//! `BasicCpmPlanner` — open-source [`Planner`] implementation.
//!
//! Bridges the wire model in [`crate::plan`] to the internal
//! CPM kernel in [`crate::algorithm`], enforces the lock-aware semantics
//! described on [`Planner`], and emits an audit lifecycle for every lock
//! state transition.
//!
//! # Atomicity & persistence
//!
//! All state lives in a [`SqlitePlanStore`]. Every mutating method runs
//! its entire body — load `PlanState`, apply the existing in-memory
//! scheduling logic, write back — inside ONE `BEGIN IMMEDIATE` SQLite
//! transaction. That is what makes "acquire N disjoint deliverables
//! together" a single observable step, and because the write lock is
//! database-level it holds across OS processes, not just tasks in this
//! process: an MCP server and an external `orchestrate` CLI pointed at
//! the same database can never double-acquire.
//!
//! Constructors without an explicit store ([`BasicCpmPlanner::new`],
//! [`BasicCpmPlanner::with_audit`], [`BasicCpmPlanner::with_parts`]) use a
//! private in-memory database — the historical ephemeral behaviour. Use
//! [`BasicCpmPlanner::with_store`] with a file-backed
//! [`SqlitePlanStore`] for durable, cross-process state.
//!
//! # Audit emission
//!
//! Audit events are buffered into a `Vec<AuditEvent>` while the
//! transaction is open, then drained to the [`AuditSink`] AFTER commit.
//! A slow sink therefore never holds up concurrent acquirers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::audit::{AuditEvent, AuditSink, NullAuditSink};
use crate::plan::{
    AcceptRequest, AcquireRequest, BlockedDeliverable, CallerId, Cohort, CohortRow, Deliverable,
    DeliverableStatus, FINISH_ID, FileMode, ForceReleaseRequest, HeartbeatRequest, LockInfo,
    MarkStatusRequest, MilestoneRow, OwnedFile, PlanDefinition, PlanGraph, PlanId, PlanStatus,
    PlannerError, START_ID, ScheduleRow,
};
use crate::plan_store::SqlitePlanStore;
use crate::ports::Planner;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use execution_policy::classify::{FailureClass, RetryDecision};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::algorithm::CpmAlgorithm;
use crate::locks::{FileClaim, PlanState, add_file_claims, modes_conflict, release_file_claims};

/// Default TTL applied to newly acquired locks. Five minutes is the
/// open-source default called out in SPEC §33 PA3.
pub const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

/// Default ceiling for a per-call lease TTL (`ttl_seconds`). Eight hours
/// is generous enough for long-running work while still bounding a
/// forgotten lease; override with `CPM_MAX_TTL_SECS` on the server or
/// [`BasicCpmPlanner::with_max_ttl`] as a library.
pub const DEFAULT_MAX_TTL: Duration = Duration::from_secs(8 * 60 * 60);

/// Hard ceiling for the configurable maximum lease TTL (30 days).
pub const MAX_TTL_CEILING: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Circuit-breaker: maximum number of times a deliverable may be
/// EXPLICITLY marked failed (via [`Planner::mark_status`] with
/// `status = Failed`) before the next [`Planner::acquire_cohort`]
/// auto-fails it instead of re-leasing. Only real implementation
/// attempts count: a driver got the lease, did the work, and reported
/// failure. A lease lost environmentally (driver killed, harness
/// timeout — the lock lapses via TTL with no terminal mark) is tracked
/// separately as a lapse and NEVER burns a circuit-breaker life; see
/// [`MAX_LAPSES`] for the runaway bound on those.
///
/// This is the retry budget for the DURABLE, cross-process deliverable lease
/// (distinct from `execution_policy`'s in-process async retry): the decision to
/// keep leasing vs. circuit-break is expressed through that crate's
/// [`RetryDecision`] (`lease_retry_decision`), and a broken deliverable is
/// classified [`FailureClass::Permanent`].
pub const MAX_ATTEMPTS: u32 = 3;

/// Runaway protection for ENVIRONMENTAL lease lapses (TTL expiry with no
/// terminal mark). Deliberately generous — a lapse says nothing about the
/// deliverable's buildability, so it must not trip the failure
/// circuit-breaker — but an infinitely-crashing environment still cannot
/// spin forever: once a deliverable's `lapse_count` reaches this bound,
/// [`Planner::acquire_cohort`] refuses to re-lease it and surfaces
/// [`PlannerError::LapseLimit`] (stable `LAPSE_LIMIT:` prefix) so an
/// operator fixes the environment instead of the planner silently
/// burning leases.
pub const MAX_LAPSES: u32 = 10;

/// Decide, from a deliverable's durable explicit-`failure_count`, whether to
/// keep leasing it or circuit-break — expressed in `execution_policy`'s shared
/// vocabulary. `Retry` while under [`MAX_ATTEMPTS`]; `Stop` (→ auto-fail,
/// [`FailureClass::Permanent`]) once the budget is spent.
fn lease_retry_decision(failure_count: u32) -> RetryDecision {
    if failure_count >= MAX_ATTEMPTS {
        RetryDecision::Stop
    } else {
        RetryDecision::Retry
    }
}

/// Legacy flat fallback for missing effort estimates.
///
/// As of CMP-016 the planner no longer uses this: when a deliverable omits
/// `estimated_effort_hours`, `crate::schedule::deliverable_to_task` asks an
/// [`crate::estimator::EffortEstimator`] for a kind-aware estimate instead of substituting a
/// flat one hour. The constant is retained as a documented reference value
/// for callers that want the historical default.
pub const DEFAULT_EFFORT_HOURS: f32 = 1.0;

/// Pluggable clock. Tests inject a closure backed by a shared instant so
/// TTL expiry is deterministic without `std::thread::sleep`.
pub type ClockFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Open-source CPM planner with file-aware locking.
pub struct BasicCpmPlanner {
    /// Durable state: plans, statuses, locks, and the submit-dedup map.
    /// Also the atomicity mechanism — see the module docs.
    store: SqlitePlanStore,
    audit: Arc<dyn AuditSink>,
    ttl: Duration,
    max_ttl: Duration,
    clock: ClockFn,
}

impl BasicCpmPlanner {
    /// Construct a planner with a real-clock and the
    /// [`DEFAULT_TTL`]. Audit events are dropped on the floor (use
    /// [`Self::with_audit`] if you need them retained).
    pub fn new() -> Self {
        Self::with_audit(Arc::new(NullAuditSink))
    }

    /// Construct a planner with the supplied audit sink and the default
    /// TTL. The real `Utc::now` is used as the clock.
    pub fn with_audit(audit: Arc<dyn AuditSink>) -> Self {
        Self::with_parts(audit, DEFAULT_TTL, Arc::new(Utc::now))
    }

    /// Override the lock TTL. Useful for short-lived integration tests.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Override the maximum lease TTL a caller may request via
    /// `ttl_seconds`. Requested values above this are clamped down to it.
    /// The value is itself capped at [`MAX_TTL_CEILING`] (30 days).
    pub fn with_max_ttl(mut self, max_ttl: Duration) -> Self {
        self.max_ttl = max_ttl.min(MAX_TTL_CEILING);
        self
    }

    /// Override the clock. Intended for deterministic TTL tests; production
    /// code should not call this.
    pub fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = clock;
        self
    }

    /// Full-parts constructor with an ephemeral in-memory store. Public
    /// for clients that want explicit control over every field at once.
    pub fn with_parts(audit: Arc<dyn AuditSink>, ttl: Duration, clock: ClockFn) -> Self {
        let store = SqlitePlanStore::open_in_memory()
            .expect("INVARIANT: opening a private in-memory sqlite database cannot fail");
        Self::with_store_parts(store, audit, ttl, clock)
    }

    /// Construct a planner backed by the supplied (typically file-backed)
    /// store, with the default TTL and real clock. This is the durable,
    /// cross-process configuration used by the MCP server binary.
    pub fn with_store(store: SqlitePlanStore, audit: Arc<dyn AuditSink>) -> Self {
        Self::with_store_parts(store, audit, DEFAULT_TTL, Arc::new(Utc::now))
    }

    /// Full-parts constructor over an explicit store.
    pub fn with_store_parts(
        store: SqlitePlanStore,
        audit: Arc<dyn AuditSink>,
        ttl: Duration,
        clock: ClockFn,
    ) -> Self {
        Self {
            store,
            audit,
            ttl,
            max_ttl: DEFAULT_MAX_TTL,
            clock,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// Effective lease TTL for one call: the caller-requested value (or
    /// the planner default) clamped to the configured maximum.
    fn effective_ttl(&self, requested: Option<Duration>) -> Duration {
        requested.unwrap_or(self.ttl).min(self.max_ttl)
    }

    /// Flush buffered audit events. Called after the mutex is dropped so a
    /// slow sink never blocks concurrent planner callers.
    async fn flush_audit(&self, events: Vec<AuditEvent>) {
        for ev in events {
            // Audit failures are intentionally swallowed at this layer:
            // the planner's invariant is "lock state stays consistent
            // even if observability fails". A `tracing::warn!` documents
            // the loss without aborting the caller's operation.
            if let Err(err) = self.audit.record(ev).await {
                tracing::warn!(error = %err, "audit sink failed to record planner event");
            }
        }
    }
}

impl Default for BasicCpmPlanner {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Graph validation + hashing
// ---------------------------------------------------------------------------

/// Deterministic content hash of a [`PlanGraph`]. Same logical graph -> same
/// hash regardless of the order `deliverables` were submitted in. This is
/// what lets `submit_plan` be idempotent.
fn hash_graph(graph: &PlanGraph) -> String {
    // Build a normalised JSON form: deliverables sorted by id; each
    // deliverable's prerequisites + owned_files sorted; metadata kept
    // as-is (callers are responsible for its determinism).
    let mut deliverables: Vec<_> = graph
        .deliverables
        .iter()
        .map(|d| {
            let mut prereqs: Vec<serde_json::Value> = d
                .prerequisites
                .iter()
                .map(|p| {
                    json!({
                        "id": p.id(),
                        "consumes": p.consumes(),
                        "kind": p.kind(),
                        "lag_hours": p.lag_hours() + 0.0,
                    })
                })
                .collect();
            // Full-key order (serialised form) so duplicate-id edges hash
            // independently of submission order.
            prereqs.sort_by_cached_key(ToString::to_string);
            let mut files: Vec<serde_json::Value> = d
                .owned_files
                .iter()
                .map(|f| json!({ "path": f.path().to_string_lossy(), "mode": f.mode() }))
                .collect();
            files.sort_by_cached_key(ToString::to_string);
            json!({
                "id": d.id,
                "owned_files": files,
                "prerequisites": prereqs,
                "estimated_effort_hours": d.estimated_effort_hours,
                "duration_hours": d.duration_hours,
                "estimate": d.estimate,
                "metadata": d.metadata,
                "milestone": d.milestone,
            })
        })
        .collect();
    deliverables.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));

    let payload = json!({
        "deliverables": deliverables,
        "max_chained_dispatch": graph.max_chained_dispatch,
    });

    // Invariant: `payload` was built from a JSON object literal whose
    // leaves are all owned `String`, primitive, or already-validated
    // `serde_json::Value` payloads. Serialisation cannot fail for these
    // inputs; the `expect` documents the invariant and aborts loudly if
    // a future refactor breaks it.
    let serialised = serde_json::to_vec(&payload)
        .expect("INVARIANT: plan-graph hash payload is JSON-serialisable");
    let mut hasher = Sha256::new();
    hasher.update(&serialised);
    format!("{:x}", hasher.finalize())
}

/// Reject graphs that fail any structural invariant. Returns
/// [`PlannerError::InvalidGraph`] with a precise `reason` on first failure.
pub(crate) fn validate_graph(graph: &PlanGraph) -> Result<(), PlannerError> {
    for d in &graph.deliverables {
        if d.id == START_ID || d.id == FINISH_ID {
            return Err(PlannerError::InvalidGraph {
                reason: format!("deliverable id '{}' is reserved", d.id),
            });
        }
    }

    // Duplicate ids.
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for d in &graph.deliverables {
        if !seen_ids.insert(d.id.as_str()) {
            return Err(PlannerError::InvalidGraph {
                reason: format!("duplicate deliverable id '{}'", d.id),
            });
        }
    }

    // Effort estimates, when present, must be finite and non-negative.
    for d in &graph.deliverables {
        if let Some(h) = d.estimated_effort_hours
            && (h < 0.0 || !h.is_finite())
        {
            return Err(PlannerError::InvalidGraph {
                reason: format!(
                    "deliverable '{}' has invalid estimated_effort_hours {h}; must be a finite number >= 0",
                    d.id
                ),
            });
        }
    }

    // Calendar durations, when present, must be finite and non-negative.
    for d in &graph.deliverables {
        if let Some(h) = d.duration_hours
            && (h < 0.0 || !h.is_finite())
        {
            return Err(PlannerError::InvalidGraph {
                reason: format!(
                    "deliverable '{}' has invalid duration_hours {h}; must be a finite number >= 0",
                    d.id
                ),
            });
        }
    }

    // Three-point estimates, when present, must be finite, non-negative
    // and ordered optimistic <= likely <= pessimistic.
    for d in &graph.deliverables {
        if let Some(e) = d.estimate {
            let ordered = e.optimistic.is_finite()
                && e.likely.is_finite()
                && e.pessimistic.is_finite()
                && e.optimistic >= 0.0
                && e.optimistic <= e.likely
                && e.likely <= e.pessimistic;
            if !ordered {
                return Err(PlannerError::InvalidGraph {
                    reason: format!(
                        "deliverable '{}' estimate must satisfy 0 <= optimistic <= likely <= pessimistic",
                        d.id
                    ),
                });
            }
        }
    }

    // Prerequisite references resolve.
    let id_set: HashSet<&str> = graph.deliverables.iter().map(|d| d.id.as_str()).collect();
    for d in &graph.deliverables {
        for p in &d.prerequisites {
            let lag = p.lag_hours();
            if !lag.is_finite() || lag < 0.0 {
                return Err(PlannerError::InvalidGraph {
                    reason: format!(
                        "prerequisite '{}' of deliverable '{}' has invalid lag_hours {lag}; must be a finite number >= 0",
                        p.id(),
                        d.id
                    ),
                });
            }
        }
        for p in crate::graph::prerequisite_ids(d) {
            if !id_set.contains(p) {
                return Err(PlannerError::InvalidGraph {
                    reason: format!(
                        "prerequisite '{p}' for deliverable '{}' does not exist",
                        d.id
                    ),
                });
            }
        }
    }

    // Shared files: any pair of claimants where at least one claim is
    // exclusive must be ordered by prerequisites, so one can never run
    // while the other holds the file. Append/append needs no ordering.
    let mut reach: Option<HashMap<String, HashSet<String>>> = None;
    let mut claimants: HashMap<&Path, Vec<(&str, FileMode)>> = HashMap::new();
    let mut path_order: Vec<&Path> = Vec::new();
    for d in &graph.deliverables {
        for f in &d.owned_files {
            let entry = claimants.entry(f.path()).or_insert_with(|| {
                path_order.push(f.path());
                Vec::new()
            });
            entry.push((d.id.as_str(), f.mode()));
        }
    }
    for path in path_order {
        let list = &claimants[path];
        for (i, &(x, xm)) in list.iter().enumerate() {
            for &(y, ym) in &list[i + 1..] {
                if x == y || (xm == FileMode::Append && ym == FileMode::Append) {
                    continue;
                }
                let reach = reach.get_or_insert_with(|| crate::graph::reachability(graph));
                let ordered = reach.get(x).is_some_and(|r| r.contains(y))
                    || reach.get(y).is_some_and(|r| r.contains(x));
                if !ordered {
                    return Err(PlannerError::InvalidGraph {
                        reason: format!(
                            "file '{}' is claimed by '{x}' and '{y}', which are not ordered by prerequisites (one could run while the other holds it)",
                            path.display(),
                        ),
                    });
                }
            }
        }
    }

    // Cycle detection via Kahn's algorithm on the prerequisite DAG.
    let mut indeg: HashMap<&str, usize> = HashMap::new();
    let mut succs: HashMap<&str, Vec<&str>> = HashMap::new();
    for d in &graph.deliverables {
        indeg.entry(d.id.as_str()).or_insert(0);
        succs.entry(d.id.as_str()).or_default();
    }
    for d in &graph.deliverables {
        for p in crate::graph::prerequisite_ids(d) {
            *indeg.entry(d.id.as_str()).or_insert(0) += 1;
            succs.entry(p).or_default().push(d.id.as_str());
        }
    }
    let mut queue: Vec<&str> = indeg
        .iter()
        .filter_map(|(k, v)| if *v == 0 { Some(*k) } else { None })
        .collect();
    let mut popped = 0_usize;
    while let Some(node) = queue.pop() {
        popped += 1;
        if let Some(s) = succs.get(node).cloned() {
            for next in s {
                if let Some(deg) = indeg.get_mut(next) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push(next);
                    }
                }
            }
        }
    }
    if popped < graph.deliverables.len() {
        // SPEC §33 audit fixup (F6 STUB-007) — name the cycle members.
        // After Kahn's terminates, any node with residual in-degree > 0
        // is part of (or downstream of) at least one cycle. Listing
        // them sorted gives operators a starting set to debug from
        // instead of "somewhere in your 50-deliverable graph there's
        // a cycle, good luck."
        let mut cycle_members: Vec<&str> = indeg
            .iter()
            .filter_map(|(k, v)| if *v > 0 { Some(*k) } else { None })
            .collect();
        cycle_members.sort_unstable();
        return Err(PlannerError::InvalidGraph {
            reason: format!(
                "cycle detected in prerequisite graph involving deliverables: [{}]",
                cycle_members.join(", ")
            ),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Audit helpers
// ---------------------------------------------------------------------------

fn make_acquired_event(lock: &LockInfo, owned_files: &[OwnedFile]) -> AuditEvent {
    AuditEvent::new("plan.lock.acquired")
        .with_actor(lock.caller_id.as_str())
        .with_payload(json!({
            "plan_id": lock.plan_id.as_str(),
            "deliverable_id": lock.deliverable_id,
            "caller_id": lock.caller_id.as_str(),
            "acquired_at": lock.acquired_at,
            "expires_at": lock.expires_at,
            "owned_files": owned_files,
        }))
}

fn make_released_event(lock: &LockInfo, reason: &str) -> AuditEvent {
    AuditEvent::new("plan.lock.released")
        .with_actor(lock.caller_id.as_str())
        .with_payload(json!({
            "plan_id": lock.plan_id.as_str(),
            "deliverable_id": lock.deliverable_id,
            "caller_id": lock.caller_id.as_str(),
            "reason": reason,
        }))
}

fn make_expired_event(lock: &LockInfo, expired_at: DateTime<Utc>) -> AuditEvent {
    AuditEvent::new("plan.lock.expired")
        .with_actor(lock.caller_id.as_str())
        .with_payload(json!({
            "plan_id": lock.plan_id.as_str(),
            "deliverable_id": lock.deliverable_id,
            "last_caller_id": lock.caller_id.as_str(),
            "expired_at": expired_at,
        }))
}

fn make_circuit_break_event(
    plan_id: &PlanId,
    deliverable_id: &str,
    failure_count: u32,
    class: FailureClass,
    reason: &str,
) -> AuditEvent {
    AuditEvent::new("plan.deliverable.circuit_broken").with_payload(json!({
        "plan_id": plan_id.as_str(),
        "deliverable_id": deliverable_id,
        "failure_count": failure_count,
        "max_attempts": MAX_ATTEMPTS,
        "failure_class": format!("{class:?}"),
        "retry_decision": format!("{:?}", RetryDecision::Stop),
        "reason": reason,
    }))
}

/// Missing (non-Complete) prerequisites of `deliverable_id`.
fn incomplete_prerequisites(state: &PlanState, deliverable_id: &str) -> Vec<String> {
    state
        .graph
        .deliverables
        .iter()
        .find(|d| d.id == deliverable_id)
        .map(|d| {
            crate::graph::prerequisite_ids(d)
                .filter(|p| !matches!(state.statuses.get(*p), Some(DeliverableStatus::Complete)))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Mark a deliverable `Complete`: release any lock (and its file index,
/// emitting a released event), set the status, and promote dependents whose
/// prerequisites are now all complete. Shared by `mark_status` and `accept`.
///
/// Returns true when this completion made every deliverable `Complete`
/// (the false -> true flip of `plan_complete`), so the caller can emit
/// `plan.completed` exactly once.
fn complete_deliverable(
    state: &mut PlanState,
    deliverable_id: &str,
    audit_buf: &mut Vec<AuditEvent>,
    release_reason: &str,
) -> bool {
    let was_complete = all_complete(state);
    if let Some(lock) = state.locks.remove(deliverable_id) {
        // Callers verified the deliverable exists; a held lock implies the
        // graph entry exists.
        let owned_files: Vec<OwnedFile> = match state
            .graph
            .deliverables
            .iter()
            .find(|d| d.id == deliverable_id)
        {
            Some(d) => d.owned_files.clone(),
            None => unreachable!(
                "deliverable {deliverable_id} present in locks but missing from graph — \
                 invariant broken"
            ),
        };
        release_file_claims(&mut state.file_claims, deliverable_id, &owned_files);
        audit_buf.push(make_released_event(&lock, release_reason));
    }

    state
        .statuses
        .insert(deliverable_id.to_string(), DeliverableStatus::Complete);

    let dependents: Vec<String> = state
        .graph
        .deliverables
        .iter()
        .filter(|d| crate::graph::prerequisite_ids(d).any(|p| p == deliverable_id))
        .map(|d| d.id.clone())
        .collect();
    for dep_id in dependents {
        let dep = match state.graph.deliverables.iter().find(|d| d.id == dep_id) {
            Some(d) => d,
            None => unreachable!("dependent id {dep_id} present in graph but not findable"),
        };
        let all_done = crate::graph::prerequisite_ids(dep)
            .all(|p| matches!(state.statuses.get(p), Some(DeliverableStatus::Complete)));
        let currently_pending = matches!(
            state.statuses.get(&dep_id),
            Some(DeliverableStatus::Pending)
        );
        if all_done && currently_pending {
            state.statuses.insert(dep_id, DeliverableStatus::Ready);
        }
    }
    !was_complete && all_complete(state)
}

/// True when every deliverable in the plan is `Complete`.
fn all_complete(state: &PlanState) -> bool {
    state
        .graph
        .deliverables
        .iter()
        .all(|d| matches!(state.statuses.get(&d.id), Some(DeliverableStatus::Complete)))
}

fn make_plan_completed_event(plan_id: &PlanId, deliverable_count: usize) -> AuditEvent {
    AuditEvent::new("plan.completed").with_payload(json!({
        "plan_id": plan_id.as_str(),
        "deliverable_count": deliverable_count,
    }))
}

fn make_accepted_event(
    req: &AcceptRequest,
    overrode_lock_of: Option<&str>,
    previous_status: &DeliverableStatus,
) -> AuditEvent {
    AuditEvent::new("plan.deliverable.accepted")
        .with_actor(req.accepted_by.as_str())
        .with_payload(json!({
            "plan_id": req.plan_id.as_str(),
            "deliverable_id": req.deliverable_id,
            "accepted_by": req.accepted_by,
            "evidence": req.evidence,
            "overrode_lock_of": overrode_lock_of,
            "previous_status": previous_status,
        }))
}

fn make_completed_without_lease_event(
    plan_id: &PlanId,
    deliverable_id: &str,
    caller_id: &CallerId,
    previous_status: &DeliverableStatus,
) -> AuditEvent {
    AuditEvent::new("plan.deliverable.completed_without_lease")
        .with_actor(caller_id.as_str())
        .with_payload(json!({
            "plan_id": plan_id.as_str(),
            "deliverable_id": deliverable_id,
            "caller_id": caller_id.as_str(),
            "previous_status": previous_status,
        }))
}

fn make_marked_without_lease_event(
    plan_id: &PlanId,
    deliverable_id: &str,
    caller_id: &CallerId,
    status: &DeliverableStatus,
    previous_status: &DeliverableStatus,
) -> AuditEvent {
    AuditEvent::new("plan.deliverable.marked_without_lease")
        .with_actor(caller_id.as_str())
        .with_payload(json!({
            "plan_id": plan_id.as_str(),
            "deliverable_id": deliverable_id,
            "caller_id": caller_id.as_str(),
            "status": status,
            "previous_status": previous_status,
        }))
}

fn make_force_released_event(lock: &LockInfo, reason: &str) -> AuditEvent {
    AuditEvent::new("plan.lock.force_released")
        .with_actor(lock.caller_id.as_str())
        .with_payload(json!({
            "plan_id": lock.plan_id.as_str(),
            "deliverable_id": lock.deliverable_id,
            "last_caller_id": lock.caller_id.as_str(),
            "reason": reason,
        }))
}

fn make_counters_reset_event(
    plan_id: &PlanId,
    deliverable_id: &str,
    reason: &str,
    lapse_count: u32,
    failure_count: u32,
) -> AuditEvent {
    AuditEvent::new("plan.deliverable.counters_reset")
        .with_actor("operator")
        .with_payload(json!({
            "plan_id": plan_id.as_str(),
            "deliverable_id": deliverable_id,
            "reason": reason,
            "previous": { "lapse_count": lapse_count, "failure_count": failure_count },
        }))
}

// ---------------------------------------------------------------------------
// Priority ordering for cohort selection
// ---------------------------------------------------------------------------

/// Sort key for the ready-set priority pass: smallest latest start first
/// (longest remaining tail leads), then least total float, then id for
/// determinism.
fn priority_key(
    deliverable_id: &str,
    sched_by_id: &HashMap<&str, (f32, f32)>,
) -> (i64, i64, String) {
    let (latest_start, float) = match sched_by_id.get(deliverable_id) {
        Some(&v) => v,
        None => unreachable!(
            "deliverable '{deliverable_id}' is in the ready set but absent from the cached \
             CPM schedule — ready set and CPM result are out of sync"
        ),
    };
    let scale = |h: f32| (h * 1000.0).round() as i64;
    (
        scale(latest_start),
        scale(float),
        deliverable_id.to_string(),
    )
}

// ---------------------------------------------------------------------------
// Planner impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Planner for BasicCpmPlanner {
    async fn submit_plan(&self, graph: PlanGraph) -> Result<PlanId, PlannerError> {
        validate_graph(&graph)?;
        let graph_hash = hash_graph(&graph);

        // The dedup check + build + insert all happen inside ONE immediate
        // sqlite transaction, so identical concurrent submissions — even
        // from different processes — resolve to a single PlanId. The build
        // closure only runs on a dedup miss.
        self.store.submit_or_get(&graph_hash, move || {
            let cached_result = crate::schedule::compute_cpm(&graph)?;

            // Initialise per-deliverable status: zero-prereq -> Ready, else Pending.
            let mut statuses: HashMap<String, DeliverableStatus> =
                HashMap::with_capacity(graph.deliverables.len());
            for d in &graph.deliverables {
                let status = if d.prerequisites.is_empty() {
                    DeliverableStatus::Ready
                } else {
                    DeliverableStatus::Pending
                };
                statuses.insert(d.id.clone(), status);
            }

            let plan_id = PlanId(format!("plan_{}", uuid::Uuid::new_v4().simple()));
            Ok((plan_id, PlanState::new(graph, statuses, cached_result)))
        })
    }

    async fn acquire_cohort(&self, req: AcquireRequest) -> Result<Cohort, PlannerError> {
        let AcquireRequest {
            plan_id,
            caller_id,
            max_count,
            ids,
            metadata_filter,
            ttl,
        } = req;
        let now = self.now();
        let expires_at = now
            + chrono::Duration::from_std(self.effective_ttl(ttl))
                .expect("INVARIANT: planner TTL fits in chrono::Duration");

        // Whole acquire body runs inside one immediate sqlite transaction —
        // that's what gives us atomicity against concurrent acquirers,
        // including acquirers in other OS processes.
        let mut audit_buf: Vec<AuditEvent> = Vec::new();
        let cohort = self.store.mutate_plan(&plan_id, |state| {
            // 1. Reap expired locks, emitting expiry events.
            let reaped = state.reap_expired(now);
            for lock in &reaped {
                audit_buf.push(make_expired_event(lock, now));
            }

            // 2a. Targeting: validate requested ids, then work out which
            //     deliverables this acquire considers. Manual deliverables
            //     (metadata.kind == "manual") are never leased; a metadata
            //     filter silently narrows the field.
            if let Some(req_ids) = &ids {
                for rid in req_ids {
                    if !state.graph.deliverables.iter().any(|d| &d.id == rid) {
                        return Err(PlannerError::DeliverableNotFound {
                            plan_id: plan_id.0.clone(),
                            deliverable_id: rid.clone(),
                        });
                    }
                }
            }
            let is_requested = |id: &str| ids.as_ref().is_some_and(|v| v.iter().any(|r| r == id));
            let is_manual =
                |d: &Deliverable| d.metadata.get("kind").and_then(|k| k.as_str()) == Some("manual");
            let matches_filter = |d: &Deliverable| {
                metadata_filter
                    .as_ref()
                    .is_none_or(|f| f.iter().all(|(k, v)| d.metadata.get(k) == Some(v)))
            };
            let in_scope =
                |d: &Deliverable| (ids.is_none() || is_requested(&d.id)) && matches_filter(d);

            // 2b-prelim. Lapse bound: a Ready deliverable whose lease has lapsed
            //     environmentally MAX_LAPSES times is evidence of a broken
            //     ENVIRONMENT (drivers keep getting killed before they can
            //     report), not a broken deliverable. Do NOT auto-fail it —
            //     that would misdiagnose a healthy deliverable — but stop
            //     re-leasing it: skip it, leave the rest of the plan
            //     leasable, and report it in `blocked` so an operator
            //     intervenes (force_release with reset_counters).

            // 2b. Circuit-break: a Ready deliverable that has already been
            //     EXPLICITLY marked failed MAX_ATTEMPTS times is a poison
            //     item — every prior lease ended with the driver reporting
            //     a real implementation failure, and re-leasing it would
            //     loop forever. Transition it to Failed and skip it. It
            //     holds no lock (it is Ready), it is never handed out
            //     again (Failed is terminal), and its dependents simply
            //     never become Ready — the plan converges to `exhausted`
            //     instead of retrying unboundedly. Environmental lapses
            //     deliberately do NOT feed this counter.
            let poisoned: Vec<(String, u32)> = state
                .graph
                .deliverables
                .iter()
                .filter(|d| {
                    matches!(state.statuses.get(&d.id), Some(DeliverableStatus::Ready))
                        && lease_retry_decision(state.failure_count(&d.id)) == RetryDecision::Stop
                })
                .map(|d| (d.id.clone(), state.failure_count(&d.id)))
                .collect();
            for (id, failures) in poisoned {
                // A spent retry budget is a PERMANENT failure (not retryable) in
                // execution_policy's classification — the deliverable is out.
                let reason = format!("circuit-break: exceeded {MAX_ATTEMPTS} failed attempts");
                audit_buf.push(make_circuit_break_event(
                    &plan_id,
                    &id,
                    failures,
                    FailureClass::Permanent,
                    &reason,
                ));
                state
                    .statuses
                    .insert(id, DeliverableStatus::Failed { reason });
            }

            // 2c. Lapse-limited set, computed AFTER the circuit-break so a
            //     deliverable at both caps is reported only as Failed.
            let lapse_blocked: Vec<BlockedDeliverable> = state
                .graph
                .deliverables
                .iter()
                .filter(|d| {
                    in_scope(d)
                        && !is_manual(d)
                        && matches!(state.statuses.get(&d.id), Some(DeliverableStatus::Ready))
                        && !state.locks.contains_key(&d.id)
                        && state.lapse_count(&d.id) >= MAX_LAPSES
                })
                .map(|d| BlockedDeliverable {
                    id: d.id.clone(),
                    code: "LAPSE_LIMIT".to_string(),
                    reason: format!(
                        "lease lapsed {} times (limit {MAX_LAPSES}); clear with \
                         plan.force_release {{reset_counters: true}}",
                        state.lapse_count(&d.id)
                    ),
                })
                .collect();

            // 3. Build the (latest_start, float) lookup table.
            let sched_by_id: HashMap<&str, (f32, f32)> = state
                .cached_result
                .tasks
                .iter()
                .map(|t| (t.id.as_str(), (t.latest_start, t.float)))
                .collect();

            // 4. Build the ready set (in scope, non-manual, Ready, unlocked,
            //    not lapse-limited) and, for requested ids, classify every id
            //    that cannot be leased. Without `ids`, only LAPSE_LIMIT is
            //    reported (manual deliverables are skipped quietly).
            let mut blocked: Vec<BlockedDeliverable> = Vec::new();
            let mut ready: Vec<&Deliverable> = Vec::new();
            if let Some(req_ids) = &ids {
                let mut seen: HashSet<&str> = HashSet::new();
                for rid in req_ids {
                    if !seen.insert(rid.as_str()) {
                        continue;
                    }
                    let d = state
                        .graph
                        .deliverables
                        .iter()
                        .find(|d| &d.id == rid)
                        .expect("INVARIANT: requested ids validated above");
                    if !matches_filter(d) {
                        continue;
                    }
                    let mut block = |code: &str, reason: String| {
                        blocked.push(BlockedDeliverable {
                            id: d.id.clone(),
                            code: code.to_string(),
                            reason,
                        });
                    };
                    let status = state.statuses.get(&d.id);
                    if is_manual(d) {
                        block("MANUAL", "manual deliverable; never leased".to_string());
                    } else if state.locks.contains_key(&d.id) {
                        block("LOCKED", "held by an active lease".to_string());
                    } else if !matches!(status, Some(DeliverableStatus::Ready)) {
                        block(
                            "NOT_READY",
                            format!(
                                "status is {}",
                                status.map_or("unknown".to_string(), |s| format!("{s:?}"))
                            ),
                        );
                    } else if let Some(lb) = lapse_blocked.iter().find(|b| b.id == d.id) {
                        block("LAPSE_LIMIT", lb.reason.clone());
                    } else {
                        ready.push(d);
                    }
                }
            } else {
                blocked = lapse_blocked.clone();
                ready.extend(state.graph.deliverables.iter().filter(|d| {
                    in_scope(d)
                        && !is_manual(d)
                        && matches!(state.statuses.get(&d.id), Some(DeliverableStatus::Ready))
                        && !state.locks.contains_key(&d.id)
                        && !lapse_blocked.iter().any(|b| b.id == d.id)
                }));
            }
            ready.sort_by_key(|d| priority_key(&d.id, &sched_by_id));

            // 5. Greedy fill with file-disjointness check. Requested ids that
            //    are skipped are reported as FILE_CONFLICT / MAX_COUNT.
            let mut selected: Vec<Deliverable> = Vec::new();
            let mut selected_files: HashMap<PathBuf, FileMode> = HashMap::new();
            for candidate in ready {
                if selected.len() == max_count {
                    if ids.is_none() {
                        break;
                    }
                    blocked.push(BlockedDeliverable {
                        id: candidate.id.clone(),
                        code: "MAX_COUNT".to_string(),
                        reason: format!("cohort already holds max_count ({max_count})"),
                    });
                    continue;
                }
                let conflict = candidate.owned_files.iter().any(|f| {
                    selected_files
                        .get(f.path())
                        .is_some_and(|m| modes_conflict(f.mode(), *m))
                        || state
                            .file_claims
                            .get(f.path())
                            .is_some_and(|c| c.conflicts_with(f.mode()))
                });
                if conflict {
                    if ids.is_some() {
                        blocked.push(BlockedDeliverable {
                            id: candidate.id.clone(),
                            code: "FILE_CONFLICT".to_string(),
                            reason: "owned files overlap a held lock or an earlier pick"
                                .to_string(),
                        });
                    }
                    continue;
                }
                for f in &candidate.owned_files {
                    // Exclusive wins if a path is somehow listed twice.
                    let e = selected_files
                        .entry(f.path().to_path_buf())
                        .or_insert(f.mode());
                    if f.mode() == FileMode::Exclusive {
                        *e = FileMode::Exclusive;
                    }
                }
                selected.push(candidate.clone());
            }

            // 6. Atomically acquire: status -> InProgress, locks inserted,
            //    file index updated, attempt counted, audit events buffered.
            //
            // F5 INTERFACE_GAP-001: build `Vec<CohortRow>` directly so
            // the pairing invariant is type-enforced — pre-F5 we
            // collected into `Vec<(Deliverable, LockInfo)>` then
            // unzipped into two parallel vectors that were doc-only
            // aligned.
            let mut rows: Vec<CohortRow> = Vec::with_capacity(selected.len());
            for d in selected {
                let lock = LockInfo {
                    plan_id: plan_id.clone(),
                    deliverable_id: d.id.clone(),
                    caller_id: caller_id.clone(),
                    acquired_at: now,
                    expires_at,
                };
                state
                    .statuses
                    .insert(d.id.clone(), DeliverableStatus::InProgress);
                // Increment on LEASE, not on candidate evaluation: only a
                // deliverable actually handed to a driver counts as an
                // attempt (a file-conflict skip above does not).
                *state.attempt_counts.entry(d.id.clone()).or_insert(0) += 1;
                add_file_claims(&mut state.file_claims, &d.id, &d.owned_files);
                state.locks.insert(d.id.clone(), lock.clone());
                audit_buf.push(make_acquired_event(&lock, &d.owned_files));
                rows.push(CohortRow {
                    deliverable: d,
                    lock,
                });
            }

            // Append paths in this cohort that two or more deliverables hold
            // (claims were just recorded, so the set includes cohort members).
            let mut shared_paths: Vec<PathBuf> = rows
                .iter()
                .flat_map(|r| r.deliverable.owned_files.iter())
                .filter(|f| f.mode() == FileMode::Append)
                .filter(|f| {
                    matches!(state.file_claims.get(f.path()),
                        Some(FileClaim::Append(set)) if set.len() >= 2)
                })
                .map(|f| f.path().to_path_buf())
                .collect();
            shared_paths.sort();
            shared_paths.dedup();

            Ok(Cohort {
                plan_id: plan_id.clone(),
                rows,
                blocked,
                shared_paths,
            })
        })?;

        self.flush_audit(audit_buf).await;
        Ok(cohort)
    }

    async fn mark_status(&self, req: MarkStatusRequest) -> Result<(), PlannerError> {
        let MarkStatusRequest {
            plan_id,
            deliverable_id,
            caller_id,
            status,
        } = req;
        let deliverable_id = deliverable_id.as_str();
        let caller_id = &caller_id;
        let mut audit_buf: Vec<AuditEvent> = Vec::new();
        self.store.mutate_plan(&plan_id, |state| {
            // Deliverable existence.
            if !state
                .graph
                .deliverables
                .iter()
                .any(|d| d.id == deliverable_id)
            {
                return Err(PlannerError::DeliverableNotFound {
                    plan_id: plan_id.0.clone(),
                    deliverable_id: deliverable_id.to_string(),
                });
            }

            // If a lock exists it must belong to caller_id.
            if let Some(lock) = state.locks.get(deliverable_id)
                && lock.caller_id != *caller_id
            {
                return Err(PlannerError::LockNotHeld {
                    caller_id: caller_id.0.clone(),
                    deliverable_id: deliverable_id.to_string(),
                });
            }

            let is_complete = matches!(status, DeliverableStatus::Complete);
            let previous_status = state
                .statuses
                .get(deliverable_id)
                .cloned()
                .unwrap_or(DeliverableStatus::Pending);

            // No lock and already Complete: only an idempotent Complete is
            // allowed; a late holder of an overridden lease cannot undo an
            // accept.
            if !state.locks.contains_key(deliverable_id)
                && matches!(previous_status, DeliverableStatus::Complete)
                && !is_complete
            {
                return Err(PlannerError::LockNotHeld {
                    caller_id: caller_id.0.clone(),
                    deliverable_id: deliverable_id.to_string(),
                });
            }

            // Completing without a lease: every prerequisite must be done,
            // and the bypass is audited.
            if is_complete && !state.locks.contains_key(deliverable_id) {
                // Already Complete: idempotent no-op, nothing to audit.
                if matches!(
                    state.statuses.get(deliverable_id),
                    Some(DeliverableStatus::Complete)
                ) {
                    return Ok(());
                }
                let missing = incomplete_prerequisites(state, deliverable_id);
                if !missing.is_empty() {
                    return Err(PlannerError::PrerequisitesIncomplete {
                        plan_id: plan_id.0.clone(),
                        deliverable_id: deliverable_id.to_string(),
                        missing,
                    });
                }
                audit_buf.push(make_completed_without_lease_event(
                    &plan_id,
                    deliverable_id,
                    caller_id,
                    &previous_status,
                ));
            } else if !is_complete && !state.locks.contains_key(deliverable_id) {
                // Putting a deliverable back to work without a lease must
                // not bypass dependency order.
                if matches!(
                    status,
                    DeliverableStatus::Ready | DeliverableStatus::InProgress
                ) {
                    let missing = incomplete_prerequisites(state, deliverable_id);
                    if !missing.is_empty() {
                        return Err(PlannerError::PrerequisitesIncomplete {
                            plan_id: plan_id.0.clone(),
                            deliverable_id: deliverable_id.to_string(),
                            missing,
                        });
                    }
                }
                audit_buf.push(make_marked_without_lease_event(
                    &plan_id,
                    deliverable_id,
                    caller_id,
                    &status,
                    &previous_status,
                ));
            }

            // Lock release on Failed.
            if matches!(status, DeliverableStatus::Failed { .. })
                && let Some(lock) = state.locks.remove(deliverable_id)
            {
                // Deliverable existence was verified at the top of
                // `mark_status`; `.find()` is guaranteed to succeed.
                let owned_files: Vec<OwnedFile> = match state
                    .graph
                    .deliverables
                    .iter()
                    .find(|d| d.id == deliverable_id)
                {
                    Some(d) => d.owned_files.clone(),
                    None => unreachable!(
                        "deliverable {deliverable_id} present in locks but missing from \
                         graph — invariant broken"
                    ),
                };
                release_file_claims(&mut state.file_claims, deliverable_id, &owned_files);
                audit_buf.push(make_released_event(&lock, "failed"));
            }

            // An EXPLICIT Failed mark is a real implementation attempt —
            // this (and only this) feeds the failure circuit-breaker.
            // Guarded on the prior status so an idempotent re-mark of an
            // already-Failed deliverable does not double-charge, and the
            // acquire-path auto-fail (which writes Failed directly) never
            // routes through here.
            if matches!(status, DeliverableStatus::Failed { .. })
                && !matches!(
                    state.statuses.get(deliverable_id),
                    Some(DeliverableStatus::Failed { .. })
                )
            {
                *state
                    .failure_counts
                    .entry(deliverable_id.to_string())
                    .or_insert(0) += 1;
            }

            if is_complete {
                if complete_deliverable(state, deliverable_id, &mut audit_buf, "completed") {
                    audit_buf.push(make_plan_completed_event(
                        &plan_id,
                        state.graph.deliverables.len(),
                    ));
                }
            } else {
                state
                    .statuses
                    .insert(deliverable_id.to_string(), status.clone());
            }

            Ok(())
        })?;

        // SPEC §33 audit fixup (F6 ORPHAN-001): the previous
        // `let _ = status_recompute_needed;` extension marker was
        // computed but never consumed. YAGNI — the CPM algorithm
        // doesn't drift with status alone, and a future "filter on
        // non-complete tasks" refactor can recompute the flag when
        // it actually needs it.

        self.flush_audit(audit_buf).await;
        Ok(())
    }

    async fn heartbeat(&self, req: HeartbeatRequest) -> Result<(), PlannerError> {
        let HeartbeatRequest {
            plan_id,
            deliverable_id,
            caller_id,
            ttl,
        } = req;
        let deliverable_id = deliverable_id.as_str();
        let caller_id = &caller_id;
        let now = self.now();
        let explicit_ttl = ttl.is_some();
        let expires_at = now
            + chrono::Duration::from_std(self.effective_ttl(ttl))
                .expect("INVARIANT: planner TTL fits in chrono::Duration");

        self.store.mutate_plan(&plan_id, |state| {
            let lock =
                state
                    .locks
                    .get_mut(deliverable_id)
                    .ok_or_else(|| PlannerError::LockNotHeld {
                        caller_id: caller_id.0.clone(),
                        deliverable_id: deliverable_id.to_string(),
                    })?;

            if lock.caller_id != *caller_id {
                return Err(PlannerError::LockNotHeld {
                    caller_id: caller_id.0.clone(),
                    deliverable_id: deliverable_id.to_string(),
                });
            }

            // TTL already lapsed at the moment of the heartbeat — surface it so
            // the caller knows their work item may have been reclaimed.
            if lock.expires_at < now {
                return Err(PlannerError::LockExpired {
                    deliverable_id: deliverable_id.to_string(),
                    expired_at: lock.expires_at,
                });
            }

            // A heartbeat without an explicit ttl never shortens a lease
            // (e.g. one acquired with a long ttl_seconds); an explicit ttl
            // sets now + ttl.
            lock.expires_at = if explicit_ttl {
                expires_at
            } else {
                lock.expires_at.max(expires_at)
            };
            Ok(())
        })
    }

    async fn get_plan(&self, plan_id: &PlanId) -> Result<PlanDefinition, PlannerError> {
        self.store.read_plan(plan_id, |state| PlanDefinition {
            plan_id: plan_id.clone(),
            graph: state.graph.clone(),
        })
    }

    async fn status(&self, plan_id: &PlanId) -> Result<PlanStatus, PlannerError> {
        self.store.read_plan(plan_id, |state| {
            // Preserve insertion order from the original graph for stable UI.
            let deliverables: Vec<(String, DeliverableStatus, u32, u32, u32)> = state
                .graph
                .deliverables
                .iter()
                .map(|d| {
                    let status = state
                        .statuses
                        .get(&d.id)
                        .cloned()
                        .unwrap_or(DeliverableStatus::Pending);
                    (
                        d.id.clone(),
                        status,
                        state.attempt_count(&d.id),
                        state.failure_count(&d.id),
                        state.lapse_count(&d.id),
                    )
                })
                .collect();

            let task_of = |id: &str| state.cached_result.tasks.iter().find(|t| t.id == id);
            let row_of = |t: &crate::task::Task, synthetic: bool| ScheduleRow {
                id: t.id.clone(),
                es: t.earliest_start,
                ef: t.earliest_finish,
                ls: t.latest_start,
                lf: t.latest_finish,
                float: t.float,
                critical: t.is_critical,
                synthetic,
            };
            let mut schedule: Vec<ScheduleRow> = Vec::new();
            schedule.extend(task_of(START_ID).map(|t| row_of(t, true)));
            schedule.extend(
                state
                    .graph
                    .deliverables
                    .iter()
                    .filter_map(|d| task_of(&d.id))
                    .map(|t| row_of(t, false)),
            );
            schedule.extend(task_of(FINISH_ID).map(|t| row_of(t, true)));
            let mut ready_rows: Vec<&ScheduleRow> = schedule
                .iter()
                .filter(|r| {
                    matches!(state.statuses.get(&r.id), Some(DeliverableStatus::Ready))
                        && !state.locks.contains_key(&r.id)
                })
                .collect();
            // Same ordering as acquire_cohort (shared priority_key). Membership is a
            // superset: acquire may still skip deliverables at the failure or lapse
            // cap, manual deliverables, or ones whose files overlap a held lock.
            let sched_by_id: HashMap<&str, (f32, f32)> = state
                .cached_result
                .tasks
                .iter()
                .map(|t| (t.id.as_str(), (t.latest_start, t.float)))
                .collect();
            ready_rows.sort_by_key(|r| priority_key(&r.id, &sched_by_id));
            let ready: Vec<String> = ready_rows.iter().map(|r| r.id.clone()).collect();

            let milestones: Vec<MilestoneRow> = state
                .graph
                .deliverables
                .iter()
                .filter(|d| d.is_milestone())
                .filter_map(|d| {
                    let task = task_of(&d.id)?;
                    Some(MilestoneRow {
                        id: d.id.clone(),
                        critical_path: CpmAlgorithm::trace_path_to(
                            &state.cached_result.tasks,
                            &d.id,
                        ),
                        hours: task.earliest_finish,
                        complete: matches!(
                            state.statuses.get(&d.id),
                            Some(DeliverableStatus::Complete)
                        ),
                    })
                })
                .collect();

            PlanStatus {
                plan_id: plan_id.clone(),
                milestones,
                critical_ids: state
                    .cached_result
                    .critical_ids
                    .iter()
                    .filter(|id| id.as_str() != START_ID && id.as_str() != FINISH_ID)
                    .cloned()
                    .collect(),
                plan_complete: all_complete(state),
                schedule,
                ready,
                deliverables,
                critical_path: state.cached_result.critical_path.clone(),
                critical_path_hours: state.cached_result.critical_path_duration,
                locks_held: state.locks.values().cloned().collect(),
            }
        })
    }

    async fn accept(&self, req: AcceptRequest) -> Result<(), PlannerError> {
        let mut audit_buf: Vec<AuditEvent> = Vec::new();
        let plan_id = req.plan_id.clone();
        let deliverable_id = req.deliverable_id.as_str();
        let now = self.now();
        self.store.mutate_plan(&plan_id, |state| {
            if !state
                .graph
                .deliverables
                .iter()
                .any(|d| d.id == deliverable_id)
            {
                return Err(PlannerError::DeliverableNotFound {
                    plan_id: plan_id.0.clone(),
                    deliverable_id: deliverable_id.to_string(),
                });
            }

            // Only LIVE leases block acceptance: reap expired ones first.
            for lock in &state.reap_expired(now) {
                audit_buf.push(make_expired_event(lock, now));
            }

            // Already Complete: idempotent no-op (no state change, no audit).
            let previous_status = state
                .statuses
                .get(deliverable_id)
                .cloned()
                .unwrap_or(DeliverableStatus::Pending);
            if matches!(previous_status, DeliverableStatus::Complete) {
                return Ok(());
            }

            // ANY live lease needs an explicit override, whoever the
            // acceptor claims to be.
            let mut overrode: Option<String> = None;
            if let Some(lock) = state.locks.get(deliverable_id) {
                if !req.override_lock {
                    return Err(PlannerError::LockHeld {
                        plan_id: plan_id.0.clone(),
                        deliverable_id: deliverable_id.to_string(),
                        holder: lock.caller_id.0.clone(),
                    });
                }
                overrode = Some(lock.caller_id.0.clone());
            }

            let missing = incomplete_prerequisites(state, deliverable_id);
            if !missing.is_empty() {
                return Err(PlannerError::PrerequisitesIncomplete {
                    plan_id: plan_id.0.clone(),
                    deliverable_id: deliverable_id.to_string(),
                    missing,
                });
            }

            let plan_done = complete_deliverable(state, deliverable_id, &mut audit_buf, "accepted");
            audit_buf.push(make_accepted_event(
                &req,
                overrode.as_deref(),
                &previous_status,
            ));
            if plan_done {
                audit_buf.push(make_plan_completed_event(
                    &plan_id,
                    state.graph.deliverables.len(),
                ));
            }
            Ok(())
        })?;

        self.flush_audit(audit_buf).await;
        Ok(())
    }

    async fn force_release(&self, req: ForceReleaseRequest) -> Result<(), PlannerError> {
        let ForceReleaseRequest {
            plan_id,
            deliverable_id,
            reason,
            reset_counters,
        } = req;
        let deliverable_id = deliverable_id.as_str();
        let reason = reason.as_str();
        let mut audit_buf: Vec<AuditEvent> = Vec::new();
        self.store.mutate_plan(&plan_id, |state| {
            if !state
                .graph
                .deliverables
                .iter()
                .any(|d| d.id == deliverable_id)
            {
                return Err(PlannerError::DeliverableNotFound {
                    plan_id: plan_id.0.clone(),
                    deliverable_id: deliverable_id.to_string(),
                });
            }

            if let Some(lock) = state.locks.remove(deliverable_id) {
                // Deliverable existence was verified above; the held lock
                // implies the graph entry exists.
                let owned_files: Vec<OwnedFile> = match state
                    .graph
                    .deliverables
                    .iter()
                    .find(|d| d.id == deliverable_id)
                {
                    Some(d) => d.owned_files.clone(),
                    None => unreachable!(
                        "deliverable {deliverable_id} present in locks but missing from graph"
                    ),
                };
                release_file_claims(&mut state.file_claims, deliverable_id, &owned_files);
                state
                    .statuses
                    .insert(deliverable_id.to_string(), DeliverableStatus::Ready);
                audit_buf.push(make_force_released_event(&lock, reason));
            }

            if reset_counters {
                let lapse_count = state.lapse_counts.remove(deliverable_id).unwrap_or(0);
                let failure_count = state.failure_counts.remove(deliverable_id).unwrap_or(0);
                // A circuit-broken deliverable is Failed; revive it.
                if matches!(
                    state.statuses.get(deliverable_id),
                    Some(DeliverableStatus::Failed { .. })
                ) {
                    let prereqs_complete = state
                        .graph
                        .deliverables
                        .iter()
                        .find(|d| d.id == deliverable_id)
                        .is_some_and(|d| {
                            crate::graph::prerequisite_ids(d).all(|p| {
                                matches!(state.statuses.get(p), Some(DeliverableStatus::Complete))
                            })
                        });
                    let revived = if prereqs_complete {
                        DeliverableStatus::Ready
                    } else {
                        DeliverableStatus::Pending
                    };
                    state.statuses.insert(deliverable_id.to_string(), revived);
                }
                audit_buf.push(make_counters_reset_event(
                    &plan_id,
                    deliverable_id,
                    reason,
                    lapse_count,
                    failure_count,
                ));
            }

            Ok(())
        })?;

        self.flush_audit(audit_buf).await;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::estimator::EffortEstimator;
    use crate::schedule::deliverable_to_task;
    use crate::task::TaskKind;

    fn deliverable(id: &str, effort: Option<f32>, metadata: serde_json::Value) -> Deliverable {
        Deliverable {
            id: id.to_string(),
            owned_files: Vec::new(),
            prerequisites: Vec::new(),
            estimated_effort_hours: effort,
            metadata,
            duration_hours: None,
            estimate: None,
            milestone: false,
        }
    }

    #[test]
    fn explicit_effort_wins_over_estimator() {
        let estimator = EffortEstimator::new();
        let d = deliverable("D1", Some(2.5), json!({}));
        let task = deliverable_to_task(&d, &estimator);
        assert_eq!(task.effort_hours, 2.5);
    }

    #[test]
    fn missing_effort_uses_estimator_not_flat_default() {
        // No explicit estimate -> estimator's Custom base (default 4.0),
        // which must differ from the legacy flat DEFAULT_EFFORT_HOURS (1.0).
        let estimator = EffortEstimator::new();
        let d = deliverable("D1", None, json!({}));
        let task = deliverable_to_task(&d, &estimator);
        let expected = estimator.estimate(
            &TaskKind::Custom {
                description: String::new(),
            },
            false,
        );
        assert_eq!(task.effort_hours, expected);
        assert_ne!(task.effort_hours, DEFAULT_EFFORT_HOURS);
    }

    #[test]
    fn complexity_metadata_hint_raises_estimate() {
        let estimator = EffortEstimator::new();
        let simple = deliverable_to_task(&deliverable("S", None, json!({})), &estimator);
        let complex = deliverable_to_task(
            &deliverable("C", None, json!({ "complexity": true })),
            &estimator,
        );
        assert!(complex.effort_hours > simple.effort_hours);
    }

    fn sched<'a>(entries: &[(&'a str, f32, f32)]) -> HashMap<&'a str, (f32, f32)> {
        entries
            .iter()
            .map(|&(id, latest_start, float)| (id, (latest_start, float)))
            .collect()
    }

    #[test]
    fn priority_key_orders_lower_latest_start_first() {
        let s = sched(&[("A", 5.0, 0.0), ("B", 0.0, 9.0)]);
        assert!(priority_key("B", &s) < priority_key("A", &s));
    }

    #[test]
    fn priority_key_breaks_latest_start_ties_by_float() {
        let s = sched(&[("X", 1.0, 3.0), ("Y", 1.0, 1.0)]);
        assert!(priority_key("Y", &s) < priority_key("X", &s));
    }

    #[test]
    fn priority_key_breaks_float_ties_by_id() {
        let s = sched(&[("X", 1.0, 1.0), ("Y", 1.0, 1.0)]);
        assert!(priority_key("X", &s) < priority_key("Y", &s));
    }

    #[test]
    #[should_panic(expected = "absent from the cached CPM schedule")]
    fn priority_key_missing_entry_is_invariant_breach() {
        let s: HashMap<&str, (f32, f32)> = HashMap::new();
        let _ = priority_key("ghost", &s);
    }
}
