//! `BasicCpmPlanner` — open-source [`Planner`] implementation.
//!
//! Bridges the wire model in [`crate::plan`] to the internal
//! CPM kernel in [`crate::algorithm`], enforces the lock-aware semantics
//! described on [`Planner`], and emits an audit lifecycle for every lock
//! state transition.
//!
//! # Atomicity
//!
//! All mutating methods take the single top-level
//! `tokio::sync::Mutex<HashMap<PlanId, PlanState>>`. Holding that mutex
//! for the entirety of [`acquire_cohort`][Planner::acquire_cohort] is what
//! makes "acquire N disjoint deliverables together" a single observable
//! step: no other concurrent acquirer can witness a half-applied lock map.
//!
//! # Audit emission
//!
//! Audit events are buffered into a `Vec<AuditEvent>` while the mutex is
//! held, then drained to the [`AuditSink`] AFTER the mutex is dropped.
//! A slow sink therefore never holds up concurrent acquirers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::audit::{AuditEvent, AuditSink, NullAuditSink};
use crate::plan::{
    CallerId, Cohort, CohortRow, Deliverable, DeliverableStatus, LockInfo, PlanGraph, PlanId,
    PlanStatus, PlannerError,
};
use crate::ports::Planner;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::algorithm::CpmAlgorithm;
use crate::estimator::EffortEstimator;
use crate::locks::PlanState;
use crate::task::{Task, TaskKind};

/// Default TTL applied to newly acquired locks. Five minutes is the
/// open-source default called out in SPEC §33 PA3.
pub const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

/// Legacy flat fallback for missing effort estimates.
///
/// As of CMP-016 the planner no longer uses this: when a deliverable omits
/// `estimated_effort_hours`, [`deliverable_to_task`] asks an
/// [`EffortEstimator`] for a kind-aware estimate instead of substituting a
/// flat one hour. The constant is retained as a documented reference value
/// for callers that want the historical default.
pub const DEFAULT_EFFORT_HOURS: f32 = 1.0;

/// Pluggable clock. Tests inject a closure backed by a shared instant so
/// TTL expiry is deterministic without `std::thread::sleep`.
pub type ClockFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Open-source CPM planner with file-aware locking.
pub struct BasicCpmPlanner {
    plans: Arc<Mutex<HashMap<PlanId, PlanState>>>,
    /// `graph_hash -> PlanId` map used by [`submit_plan`] to make
    /// resubmissions idempotent without re-running CPM.
    dedup: Arc<Mutex<HashMap<String, PlanId>>>,
    audit: Arc<dyn AuditSink>,
    ttl: Duration,
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

    /// Override the clock. Intended for deterministic TTL tests; production
    /// code should not call this.
    pub fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = clock;
        self
    }

    /// Full-parts constructor. Public for clients that want explicit
    /// control over every field at once.
    pub fn with_parts(audit: Arc<dyn AuditSink>, ttl: Duration, clock: ClockFn) -> Self {
        Self {
            plans: Arc::new(Mutex::new(HashMap::new())),
            dedup: Arc::new(Mutex::new(HashMap::new())),
            audit,
            ttl,
            clock,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
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
            let mut prereqs = d.prerequisites.clone();
            prereqs.sort();
            let mut files: Vec<String> = d
                .owned_files
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            files.sort();
            json!({
                "id": d.id,
                "owned_files": files,
                "prerequisites": prereqs,
                "estimated_effort_hours": d.estimated_effort_hours,
                "metadata": d.metadata,
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
fn validate_graph(graph: &PlanGraph) -> Result<(), PlannerError> {
    // Duplicate ids.
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for d in &graph.deliverables {
        if !seen_ids.insert(d.id.as_str()) {
            return Err(PlannerError::InvalidGraph {
                reason: format!("duplicate deliverable id '{}'", d.id),
            });
        }
    }

    // Prerequisite references resolve.
    let id_set: HashSet<&str> = graph.deliverables.iter().map(|d| d.id.as_str()).collect();
    for d in &graph.deliverables {
        for p in &d.prerequisites {
            if !id_set.contains(p.as_str()) {
                return Err(PlannerError::InvalidGraph {
                    reason: format!(
                        "prerequisite '{p}' for deliverable '{}' does not exist",
                        d.id
                    ),
                });
            }
        }
    }

    // Disjoint owned_files at graph level.
    let mut file_owner: HashMap<&PathBuf, &str> = HashMap::new();
    for d in &graph.deliverables {
        for f in &d.owned_files {
            if let Some(other) = file_owner.insert(f, d.id.as_str()) {
                return Err(PlannerError::InvalidGraph {
                    reason: format!(
                        "file '{}' is owned by both '{}' and '{}'",
                        f.display(),
                        other,
                        d.id
                    ),
                });
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
        for p in &d.prerequisites {
            *indeg.entry(d.id.as_str()).or_insert(0) += 1;
            succs.entry(p.as_str()).or_default().push(d.id.as_str());
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

/// Convert each [`Deliverable`] into a [`Task`] for the CPM kernel.
///
/// Effort precedence: an explicit `estimated_effort_hours` on the
/// deliverable always wins. When it is absent we ask `estimator` to derive
/// a kind-aware estimate rather than falling back to the flat
/// [`DEFAULT_EFFORT_HOURS`] placeholder. A `complexity` hint can be carried
/// in `metadata` (boolean `complexity`/`is_complex`) to opt a deliverable
/// into the configured complexity multiplier.
fn deliverable_to_task(d: &Deliverable, estimator: &EffortEstimator) -> Task {
    let description = d
        .metadata
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let kind = TaskKind::Custom { description };

    let effort_hours = match d.estimated_effort_hours {
        Some(explicit) => explicit,
        None => {
            // Coarse complexity hint from metadata; defaults to false.
            let is_complex = d
                .metadata
                .get("complexity")
                .or_else(|| d.metadata.get("is_complex"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            estimator.estimate(&kind, is_complex)
        }
    };

    Task {
        id: d.id.clone(),
        name: d.id.clone(),
        kind,
        effort_hours,
        dependencies: d.prerequisites.clone(),
        ..Task::default()
    }
}

// ---------------------------------------------------------------------------
// Audit helpers
// ---------------------------------------------------------------------------

fn make_acquired_event(lock: &LockInfo, owned_files: &[PathBuf]) -> AuditEvent {
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

// ---------------------------------------------------------------------------
// Priority ordering for cohort selection
// ---------------------------------------------------------------------------

/// Sort key for the ready-set priority pass. Critical-path tasks come first
/// (in CP execution order), then non-critical tasks by ascending
/// `earliest_start`. Ties broken by `deliverable_id` for determinism.
fn priority_key(
    deliverable_id: &str,
    cp_positions: &HashMap<&str, usize>,
    es_by_id: &HashMap<&str, f32>,
) -> (u8, i64, String) {
    if let Some(pos) = cp_positions.get(deliverable_id) {
        // Tier 0 = critical path; position drives order.
        (0, *pos as i64, deliverable_id.to_string())
    } else {
        // Tier 1 = non-critical; ES drives order (scaled to integer for Ord).
        // Every deliverable in the ready set was turned into a Task and fed
        // through CPM, so its id MUST be present in `es_by_id`. A miss means
        // the ready set and the cached CPM result have diverged — an
        // invariant breach, not a "default to time 0" situation, since
        // defaulting would confidently mis-order the cohort.
        let es = match es_by_id.get(deliverable_id) {
            Some(&es) => es,
            None => unreachable!(
                "deliverable '{deliverable_id}' is in the ready set but absent from the cached \
                 CPM earliest-start table — ready set and CPM result are out of sync"
            ),
        };
        let es_scaled = (es * 1000.0).round() as i64;
        (1, es_scaled, deliverable_id.to_string())
    }
}

// ---------------------------------------------------------------------------
// Planner impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Planner for BasicCpmPlanner {
    async fn submit_plan(&self, graph: PlanGraph) -> Result<PlanId, PlannerError> {
        validate_graph(&graph)?;
        let graph_hash = hash_graph(&graph);

        // Fast path: dedup hit -> return existing PlanId.
        {
            let dedup = self.dedup.lock().await;
            if let Some(existing) = dedup.get(&graph_hash) {
                return Ok(existing.clone());
            }
        }

        // Build the CPM kernel input and run the algorithm. A single
        // default-config estimator fills in effort for deliverables that
        // omit an explicit `estimated_effort_hours`.
        let estimator = EffortEstimator::new();
        let mut tasks: Vec<Task> = graph
            .deliverables
            .iter()
            .map(|d| deliverable_to_task(d, &estimator))
            .collect();
        let cached_result = CpmAlgorithm::calculate(&mut tasks);

        // `validate_graph` above already rejected cyclic graphs, so the CPM
        // kernel must have scheduled every task. If `unscheduled` is non-empty
        // here, the two cycle detectors disagree — a correctness bug, not bad
        // input. Surface it as an InvalidGraph rather than caching and serving
        // a confidently-wrong plan.
        if !cached_result.unscheduled.is_empty() {
            return Err(PlannerError::InvalidGraph {
                reason: format!(
                    "internal CPM inconsistency: deliverables passed cycle validation but could \
                     not be scheduled: [{}]",
                    cached_result.unscheduled.join(", ")
                ),
            });
        }

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

        // Mint a fresh PlanId and insert. Re-check dedup under both locks to
        // avoid a TOCTOU race between the read above and the insert below
        // when two callers submit identical graphs concurrently.
        let plan_id = PlanId(format!("plan_{}", uuid::Uuid::new_v4().simple()));
        let state = PlanState::new(graph, statuses, cached_result);

        let mut dedup = self.dedup.lock().await;
        if let Some(existing) = dedup.get(&graph_hash) {
            return Ok(existing.clone());
        }
        let mut plans = self.plans.lock().await;
        dedup.insert(graph_hash, plan_id.clone());
        plans.insert(plan_id.clone(), state);
        Ok(plan_id)
    }

    async fn acquire_cohort(
        &self,
        plan_id: &PlanId,
        caller_id: &CallerId,
        max_count: usize,
    ) -> Result<Cohort, PlannerError> {
        let now = self.now();
        let expires_at = now
            + chrono::Duration::from_std(self.ttl)
                .expect("INVARIANT: planner TTL fits in chrono::Duration");

        // Whole acquire body runs under the top-level mutex — that's what
        // gives us atomicity against concurrent acquirers.
        let (cohort, audit_buf) = {
            let mut plans = self.plans.lock().await;
            let state = plans
                .get_mut(plan_id)
                .ok_or_else(|| PlannerError::PlanNotFound {
                    plan_id: plan_id.0.clone(),
                })?;

            let mut audit_buf: Vec<AuditEvent> = Vec::new();

            // 1. Reap expired locks, emitting expiry events.
            let reaped = state.reap_expired(now);
            for lock in &reaped {
                audit_buf.push(make_expired_event(lock, now));
            }

            // 2. Build CP priority + ES lookup tables.
            let cp_positions: HashMap<&str, usize> = state
                .cached_result
                .critical_path
                .iter()
                .enumerate()
                .map(|(i, id)| (id.as_str(), i))
                .collect();
            let es_by_id: HashMap<&str, f32> = state
                .cached_result
                .tasks
                .iter()
                .map(|t| (t.id.as_str(), t.earliest_start))
                .collect();

            // 3. Build the ready set: status=Ready AND no lock currently held.
            let mut ready: Vec<&Deliverable> = state
                .graph
                .deliverables
                .iter()
                .filter(|d| {
                    matches!(state.statuses.get(&d.id), Some(DeliverableStatus::Ready))
                        && !state.locks.contains_key(&d.id)
                })
                .collect();
            ready.sort_by_key(|d| priority_key(&d.id, &cp_positions, &es_by_id));

            // 4. Greedy fill with file-disjointness check.
            let mut selected: Vec<Deliverable> = Vec::new();
            let mut selected_files: HashSet<PathBuf> = HashSet::new();
            for candidate in ready {
                if selected.len() == max_count {
                    break;
                }
                let conflict = candidate.owned_files.iter().any(|f| {
                    selected_files.contains(f) || state.file_to_deliverable.contains_key(f)
                });
                if conflict {
                    continue;
                }
                for f in &candidate.owned_files {
                    selected_files.insert(f.clone());
                }
                selected.push(candidate.clone());
            }

            // 5. Atomically acquire: status -> InProgress, locks inserted,
            //    file index updated, audit events buffered.
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
                for f in &d.owned_files {
                    state.file_to_deliverable.insert(f.clone(), d.id.clone());
                }
                state.locks.insert(d.id.clone(), lock.clone());
                audit_buf.push(make_acquired_event(&lock, &d.owned_files));
                rows.push(CohortRow {
                    deliverable: d,
                    lock,
                });
            }

            let cohort = Cohort {
                plan_id: plan_id.clone(),
                rows,
            };

            (cohort, audit_buf)
        };

        self.flush_audit(audit_buf).await;
        Ok(cohort)
    }

    async fn mark_status(
        &self,
        plan_id: &PlanId,
        deliverable_id: &str,
        caller_id: &CallerId,
        status: DeliverableStatus,
    ) -> Result<(), PlannerError> {
        let audit_buf = {
            let mut plans = self.plans.lock().await;
            let state = plans
                .get_mut(plan_id)
                .ok_or_else(|| PlannerError::PlanNotFound {
                    plan_id: plan_id.0.clone(),
                })?;

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
            if let Some(lock) = state.locks.get(deliverable_id) {
                if lock.caller_id != *caller_id {
                    return Err(PlannerError::LockNotHeld {
                        caller_id: caller_id.0.clone(),
                        deliverable_id: deliverable_id.to_string(),
                    });
                }
            }

            let mut audit_buf: Vec<AuditEvent> = Vec::new();

            // Lock release on terminal status.
            let release_reason: Option<&'static str> = match &status {
                DeliverableStatus::Complete => Some("completed"),
                DeliverableStatus::Failed { .. } => Some("failed"),
                _ => None,
            };

            if let Some(reason) = release_reason {
                if let Some(lock) = state.locks.remove(deliverable_id) {
                    // Deliverable existence was verified at the top of
                    // `mark_status`; `.find()` is guaranteed to succeed.
                    let owned_files: Vec<PathBuf> = match state
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
                    for f in &owned_files {
                        state.file_to_deliverable.remove(f);
                    }
                    audit_buf.push(make_released_event(&lock, reason));
                }
            }

            // Set status.
            state
                .statuses
                .insert(deliverable_id.to_string(), status.clone());

            // Advance dependents to Ready if all their prereqs are Complete.
            if matches!(status, DeliverableStatus::Complete) {
                let dependents: Vec<String> = state
                    .graph
                    .deliverables
                    .iter()
                    .filter(|d| d.prerequisites.iter().any(|p| p == deliverable_id))
                    .map(|d| d.id.clone())
                    .collect();
                for dep_id in dependents {
                    let dep = match state.graph.deliverables.iter().find(|d| d.id == dep_id) {
                        Some(d) => d,
                        None => {
                            unreachable!("dependent id {dep_id} present in graph but not findable")
                        }
                    };
                    let all_done = dep.prerequisites.iter().all(|p| {
                        matches!(state.statuses.get(p), Some(DeliverableStatus::Complete))
                    });
                    let currently_pending = matches!(
                        state.statuses.get(&dep_id),
                        Some(DeliverableStatus::Pending)
                    );
                    if all_done && currently_pending {
                        state.statuses.insert(dep_id, DeliverableStatus::Ready);
                    }
                }
            }

            audit_buf
        };

        // SPEC §33 audit fixup (F6 ORPHAN-001): the previous
        // `let _ = status_recompute_needed;` extension marker was
        // computed but never consumed. YAGNI — the CPM algorithm
        // doesn't drift with status alone, and a future "filter on
        // non-complete tasks" refactor can recompute the flag when
        // it actually needs it.

        self.flush_audit(audit_buf).await;
        Ok(())
    }

    async fn heartbeat(
        &self,
        plan_id: &PlanId,
        deliverable_id: &str,
        caller_id: &CallerId,
    ) -> Result<(), PlannerError> {
        let now = self.now();
        let expires_at = now
            + chrono::Duration::from_std(self.ttl)
                .expect("INVARIANT: planner TTL fits in chrono::Duration");

        let mut plans = self.plans.lock().await;
        let state = plans
            .get_mut(plan_id)
            .ok_or_else(|| PlannerError::PlanNotFound {
                plan_id: plan_id.0.clone(),
            })?;

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

        lock.expires_at = expires_at;
        Ok(())
    }

    async fn status(&self, plan_id: &PlanId) -> Result<PlanStatus, PlannerError> {
        let plans = self.plans.lock().await;
        let state = plans
            .get(plan_id)
            .ok_or_else(|| PlannerError::PlanNotFound {
                plan_id: plan_id.0.clone(),
            })?;

        // Preserve insertion order from the original graph for stable UI.
        let deliverables: Vec<(String, DeliverableStatus)> = state
            .graph
            .deliverables
            .iter()
            .map(|d| {
                let status = state
                    .statuses
                    .get(&d.id)
                    .cloned()
                    .unwrap_or(DeliverableStatus::Pending);
                (d.id.clone(), status)
            })
            .collect();

        Ok(PlanStatus {
            plan_id: plan_id.clone(),
            deliverables,
            critical_path: state.cached_result.critical_path.clone(),
            critical_path_hours: state.cached_result.critical_path_duration,
            locks_held: state.locks.values().cloned().collect(),
        })
    }

    async fn force_release(
        &self,
        plan_id: &PlanId,
        deliverable_id: &str,
        reason: &str,
    ) -> Result<(), PlannerError> {
        let audit_buf = {
            let mut plans = self.plans.lock().await;
            let state = plans
                .get_mut(plan_id)
                .ok_or_else(|| PlannerError::PlanNotFound {
                    plan_id: plan_id.0.clone(),
                })?;

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

            let mut audit_buf: Vec<AuditEvent> = Vec::new();
            if let Some(lock) = state.locks.remove(deliverable_id) {
                // Deliverable existence was verified above; the held lock
                // implies the graph entry exists.
                let owned_files: Vec<PathBuf> = match state
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
                for f in &owned_files {
                    state.file_to_deliverable.remove(f);
                }
                state
                    .statuses
                    .insert(deliverable_id.to_string(), DeliverableStatus::Ready);
                audit_buf.push(make_force_released_event(&lock, reason));
            }

            audit_buf
        };

        self.flush_audit(audit_buf).await;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn deliverable(id: &str, effort: Option<f32>, metadata: serde_json::Value) -> Deliverable {
        Deliverable {
            id: id.to_string(),
            owned_files: Vec::new(),
            prerequisites: Vec::new(),
            estimated_effort_hours: effort,
            metadata,
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

    #[test]
    fn priority_key_critical_tier_orders_by_position() {
        let mut cp = HashMap::new();
        cp.insert("A", 0usize);
        cp.insert("B", 1usize);
        let es: HashMap<&str, f32> = HashMap::new();
        let ka = priority_key("A", &cp, &es);
        let kb = priority_key("B", &cp, &es);
        assert!(ka < kb);
    }

    #[test]
    fn priority_key_noncritical_uses_es() {
        let cp: HashMap<&str, usize> = HashMap::new();
        let mut es = HashMap::new();
        es.insert("X", 1.0_f32);
        es.insert("Y", 3.0_f32);
        let kx = priority_key("X", &cp, &es);
        let ky = priority_key("Y", &cp, &es);
        // Both tier 1, X has earlier ES so sorts first.
        assert_eq!(kx.0, 1);
        assert!(kx < ky);
    }

    #[test]
    #[should_panic(expected = "absent from the cached CPM earliest-start table")]
    fn priority_key_missing_es_is_invariant_breach() {
        let cp: HashMap<&str, usize> = HashMap::new();
        let es: HashMap<&str, f32> = HashMap::new();
        // Non-critical deliverable with no ES entry must panic, not default.
        let _ = priority_key("ghost", &cp, &es);
    }
}
