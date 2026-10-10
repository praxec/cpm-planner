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
    AcceptRequest, AcquireRequest, BlockedDeliverable, CallerId, Cohort, CohortRow,
    ComparePlansRequest, Deliverable, DeliverableStatus, FINISH_ID, FileMode, ForceReleaseRequest,
    ForkRequest, HeartbeatRequest, LockInfo, MAX_DELIVERABLES, MarkStatusRequest, OwnedFile,
    PlanDefinition, PlanGraph, PlanId, PlanLineSummary, PlanStatus, PlannerError, ReviseRequest,
    START_ID, ScheduleRow, SelectOutcome, SyncOutcome, SyncRequest,
};
use crate::plan_store::SqlitePlanStore;
use crate::portfolio::{Archived, Revised, Selected, Synced, VariantInfo};
use crate::ports::Planner;
use crate::project::ProjectRoot;
use crate::revise::RevisionDiff;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use execution_policy::classify::{FailureClass, RetryDecision};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::locks::{FileClaim, PlanState, add_file_claims, modes_conflict};

mod ev;

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
    /// The project whose plan files this planner may read and write (drift
    /// detection, fork). `None`: file features degrade to inline/unknown.
    project_root: Option<ProjectRoot>,
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

    /// Simulate a stored plan's graph read-only: nothing is written and no
    /// audit events are emitted. See [`crate::simulate::simulate`].
    pub async fn simulate_plan(
        &self,
        plan_id: &PlanId,
        req: &crate::simulate::SimulateRequest,
    ) -> Result<crate::simulate::SimulationResult, PlannerError> {
        let graph = self.get_plan(plan_id).await?.graph;
        crate::simulate::simulate(&graph, req)
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
            project_root: None,
        }
    }

    /// Give the planner a project root. [`Planner::status`] then reports
    /// `definition_drift` for that project's file-backed variants, and
    /// [`Planner::fork_plan`] writes the new variant's plan file there when
    /// the request names no root of its own.
    pub fn with_project_root(mut self, root: ProjectRoot) -> Self {
        self.project_root = Some(root);
        self
    }

    /// The backing store, for in-crate tests that inspect tables directly.
    #[cfg(test)]
    pub(crate) fn store(&self) -> &SqlitePlanStore {
        &self.store
    }

    /// The project root given with [`Self::with_project_root`], if any. The
    /// MCP server reads its root from here (one source of truth).
    pub fn project_root(&self) -> Option<&ProjectRoot> {
        self.project_root.as_ref()
    }

    /// The `(plan id, label, head graph)` inputs of a comparison, read in one
    /// store snapshot: `plan_ids` in the given order (an unnamed plan is
    /// labelled by its id) or every non-archived variant of the line `plan`
    /// (sorted by variant). Both or neither, duplicate ids, or a count
    /// outside `2..=`[`crate::compare::MAX_COMPARE_VARIANTS`] is
    /// `INVALID_GRAPH`; an unknown plan or line is `PLAN_NOT_FOUND`. Pair it
    /// with the pure, CPU-bound [`crate::compare::compare`].
    pub fn compare_inputs(
        &self,
        req: &ComparePlansRequest,
    ) -> Result<Vec<(PlanId, String, PlanGraph)>, PlannerError> {
        if let Some(ids) = &req.plan_ids {
            let distinct: HashSet<&str> = ids.iter().map(|id| id.0.as_str()).collect();
            if distinct.len() != ids.len() {
                return Err(PlannerError::InvalidGraph {
                    reason: "plan_ids must be distinct".to_string(),
                });
            }
            crate::compare::check_variant_count(ids.len())?;
        }
        self.store.read_tx(|tx| {
            let targets: Vec<(PlanId, Option<String>)> = match (&req.plan_ids, &req.plan) {
                (Some(ids), None) => ids.iter().map(|id| (id.clone(), None)).collect(),
                (None, Some((project, name))) => {
                    crate::portfolio::live_variants(tx, project, name)?
                        .into_iter()
                        .map(|(variant, id)| (id, Some(variant)))
                        .collect()
                }
                _ => {
                    return Err(PlannerError::InvalidGraph {
                        reason: "compare takes exactly one of plan_ids or plan".to_string(),
                    });
                }
            };
            crate::compare::check_variant_count(targets.len())?;
            targets
                .into_iter()
                .map(|(id, label)| {
                    let (_, graph) = crate::portfolio::revision_graph(tx, &id, None)?;
                    let label = match label {
                        Some(variant) => variant,
                        None => crate::portfolio::variant_info(tx, &id)?
                            .map_or_else(|| id.0.clone(), |info| info.variant),
                    };
                    Ok((id, label, graph))
                })
                .collect::<Result<Vec<_>, PlannerError>>()
        })
    }

    /// Refuse an export over `file` that would clobber work (see
    /// [`Planner::export_plan`]): another variant's tracked plan file, or
    /// this variant's own file while it holds local edits that were never
    /// synced.
    fn check_export_target(
        &self,
        plan_id: &PlanId,
        root: &ProjectRoot,
        file: &crate::project::PlanFileRef,
    ) -> Result<(), PlannerError> {
        let project = root.project_key();
        let trackers = self.store.read_tx(|tx| {
            crate::portfolio::variants_tracking(tx, &project, &file.rel_path)?
                .into_iter()
                .map(|id| {
                    let info = crate::portfolio::variant_info(tx, &id)?;
                    let (_, head) = crate::portfolio::revision_graph(tx, &id, None)?;
                    let recorded = crate::portfolio::revision_content_hashes(tx, &id)?;
                    Ok((id, info, hash_graph(&head), recorded))
                })
                .collect::<Result<Vec<_>, PlannerError>>()
        })?;
        for (id, info, head_hash, mut recorded) in trackers {
            let Some(info) = info else { continue };
            if id != *plan_id {
                return Err(PlannerError::InvalidPath {
                    reason: format!(
                        "{} is the tracked plan file of variant '{}' of '{}'; pass force to \
                         overwrite it",
                        file.rel_path, info.variant, info.name
                    ),
                });
            }
            if definition_drift(Some(root), &info, &head_hash) != Some(true) {
                continue;
            }
            // Drifted: refuse only if the file holds content no revision of
            // this variant recorded (local edits). A file that merely lags
            // the head (e.g. after an inline revise) is safe to re-export.
            recorded.extend(info.content_hash.clone());
            recorded.insert(head_hash);
            if has_local_edits(root, file, &recorded) {
                return Err(PlannerError::InvalidPath {
                    reason: format!(
                        "{} has local edits that were never synced (definition drift); sync it \
                         or pass force to overwrite it",
                        file.rel_path
                    ),
                });
            }
        }
        Ok(())
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// Effective lease TTL for one call: the caller-requested value (or
    /// the planner default) clamped to the configured maximum.
    fn effective_ttl(&self, requested: Option<Duration>) -> Duration {
        requested.unwrap_or(self.ttl).min(self.max_ttl)
    }

    /// Record one audit event emitted outside the planner's own operations
    /// (e.g. `plan.review`). Sink failures are logged, never returned.
    pub async fn record_audit(&self, event: AuditEvent) {
        self.flush_audit(vec![event]).await;
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

/// Drift of a variant's plan file against its head graph (`head_hash`, the
/// [`hash_graph`] of the head read in the same snapshot as `info`).
///
/// `None` (unknown) unless there is a root for the variant's project, the
/// variant has a source path, and that file is readable within
/// [`crate::project::MAX_PLAN_FILE_BYTES`]. Otherwise: `Some(false)` when the
/// file's bytes hash to the stored content hash; else the file is parsed and
/// `Some(file graph != head graph)` (canonically, so re-formatting is not
/// drift); an unparseable file is `Some(true)`.
fn definition_drift(
    root: Option<&ProjectRoot>,
    info: &VariantInfo,
    head_hash: &str,
) -> Option<bool> {
    let root = root?;
    if root.project_key() != info.project {
        return None;
    }
    let rel = info.source_path.as_deref()?;
    let file = root.resolve_plan_file(rel).ok()?;
    let bytes = root.read_bytes(&file).ok()?;
    if info.content_hash.as_deref() == Some(crate::project::content_hash(&bytes).as_str()) {
        return Some(false);
    }
    match serde_json::from_slice::<PlanGraph>(&bytes) {
        Ok(graph) => Some(hash_graph(&graph) != head_hash),
        Err(_) => Some(true),
    }
}

/// True when `file` exists and its content matches none of `recorded` (by
/// byte hash or canonical graph hash). An unreadable file counts as none.
fn has_local_edits(
    root: &ProjectRoot,
    file: &crate::project::PlanFileRef,
    recorded: &HashSet<String>,
) -> bool {
    let Ok(bytes) = root.read_bytes(file) else {
        return false;
    };
    if recorded.contains(&crate::project::content_hash(&bytes)) {
        return false;
    }
    match serde_json::from_slice::<PlanGraph>(&bytes) {
        Ok(graph) => !recorded.contains(&hash_graph(&graph)),
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------
// Graph validation + hashing
// ---------------------------------------------------------------------------

/// Canonical, order-independent JSON form of one deliverable: prerequisites
/// and owned files sorted so declaration order never matters. Shared by
/// [`hash_graph`], plan revision's "did the definition change" test, and
/// variant comparison diffs.
pub(crate) fn canonical_deliverable(d: &Deliverable) -> serde_json::Value {
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
    let mut canonical = json!({
        "id": d.id,
        "owned_files": files,
        "prerequisites": prereqs,
        "estimated_effort_hours": d.estimated_effort_hours,
        "duration_hours": d.duration_hours,
        "estimate": d.estimate,
        "metadata": d.metadata,
        "milestone": d.milestone,
    });
    // Only when set to a non-default rule, so graphs without one keep the
    // hash they had before the field existed (dedup and inline sync stay
    // stable) and an explicit `zero_hundred` hashes like an absent rule.
    if let Some(rule) = d
        .earning_rule
        .filter(|r| *r != crate::plan::EarningRule::ZeroHundred)
    {
        canonical["earning_rule"] = json!(rule);
    }
    canonical
}

/// Deterministic content hash of a [`PlanGraph`]. Same logical graph -> same
/// hash regardless of the order `deliverables` were submitted in. This is
/// what lets `submit_plan` be idempotent, and what tells `sync_plan` whether
/// a variant's graph changed.
pub(crate) fn hash_graph(graph: &PlanGraph) -> String {
    // Build a normalised JSON form: deliverables sorted by id; each
    // deliverable's prerequisites + owned_files sorted; metadata kept
    // as-is (callers are responsible for its determinism).
    let mut deliverables: Vec<_> = graph
        .deliverables
        .iter()
        .map(canonical_deliverable)
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
/// Run by submit and by every analysis entry point (`resource_schedule`,
/// `monte_carlo`, `simulate`).
pub(crate) fn validate_graph(graph: &PlanGraph) -> Result<(), PlannerError> {
    if graph.deliverables.len() > MAX_DELIVERABLES {
        return Err(PlannerError::InvalidGraph {
            reason: format!(
                "plan has {} deliverables; maximum is {MAX_DELIVERABLES}",
                graph.deliverables.len()
            ),
        });
    }
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

    // Effort, duration, estimate and lag hours: finite, within
    // 0..=MAX_HOURS, and estimates ordered. Same messages as `plan.lint`.
    if let Some(problem) = crate::graph::value_problems(graph).into_iter().next() {
        return Err(PlannerError::InvalidGraph {
            reason: problem.message,
        });
    }

    // Prerequisite references resolve.
    let id_set: HashSet<&str> = graph.deliverables.iter().map(|d| d.id.as_str()).collect();
    for d in &graph.deliverables {
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

/// Build a new plan from a validated graph: CPM result, a fresh id, and
/// initial statuses (no prerequisites -> Ready, else Pending).
fn initial_plan(graph: PlanGraph) -> Result<(PlanId, PlanState), PlannerError> {
    let cached_result = crate::schedule::compute_cpm(&graph)?;
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
}

// ---------------------------------------------------------------------------
// Audit helpers
// ---------------------------------------------------------------------------

fn make_portfolio_created_event(
    outcome: &SyncOutcome,
    project: &str,
    selected: bool,
) -> AuditEvent {
    AuditEvent::new("plan.portfolio.created").with_payload(json!({
        "plan_id": outcome.plan_id.as_str(),
        "project": project,
        "name": outcome.name,
        "variant": outcome.variant,
        "revision": outcome.revision,
        "selected": selected,
    }))
}

fn make_portfolio_revised_event(
    plan_id: &PlanId,
    revision: u32,
    diff: &RevisionDiff,
) -> AuditEvent {
    AuditEvent::new("plan.portfolio.revised").with_payload(json!({
        "plan_id": plan_id.as_str(),
        "revision": revision,
        "diff": diff,
    }))
}

/// Audit trail of a committed revision: reaped (expired) locks, locks the
/// forced revision released, then the revision itself.
fn revision_events(plan_id: &PlanId, revised: &Revised, now: DateTime<Utc>) -> Vec<AuditEvent> {
    if revised.no_op {
        return Vec::new();
    }
    let mut events: Vec<AuditEvent> = revised
        .reaped
        .iter()
        .map(|lock| make_expired_event(lock, now))
        .collect();
    events.extend(
        revised
            .released
            .iter()
            .map(|lock| make_released_event(lock, "revision")),
    );
    events.push(make_portfolio_revised_event(
        plan_id,
        revised.revision,
        &revised.diff,
    ));
    if let Some(count) = revised.completed {
        events.push(make_plan_completed_event(plan_id, count));
    }
    events
}

fn make_portfolio_selected_event(outcome: &SelectOutcome) -> AuditEvent {
    AuditEvent::new("plan.portfolio.selected").with_payload(json!({
        "plan_id": outcome.plan_id.as_str(),
        "project": outcome.project,
        "name": outcome.name,
        "from": outcome.previous,
        "to": outcome.variant,
        "carried": outcome.carried,
        "released": outcome.released_locks,
    }))
}

/// Audit trail of a committed selection: reaped (expired) locks, locks the
/// forced selection released, the selection itself, then `plan.completed`
/// when the carry-over completed the newly selected plan. A no-op selection
/// is not audited.
fn selection_events(selected: &Selected, now: DateTime<Utc>) -> Vec<AuditEvent> {
    if !selected.outcome.changed {
        return Vec::new();
    }
    let mut events: Vec<AuditEvent> = selected
        .reaped
        .iter()
        .map(|lock| make_expired_event(lock, now))
        .collect();
    events.extend(
        selected
            .released
            .iter()
            .map(|lock| make_released_event(lock, "variant deselected")),
    );
    events.push(make_portfolio_selected_event(&selected.outcome));
    if let Some(count) = selected.completed {
        events.push(make_plan_completed_event(&selected.outcome.plan_id, count));
    }
    events
}

/// Audit trail of a committed archive (`archived`) or unarchive: reaped
/// (expired) locks, locks a forced line archive released, then
/// `plan.portfolio.archived` / `plan.portfolio.unarchived`. Nothing when no
/// flag changed.
fn archive_events(
    project: &str,
    name: &str,
    variant: Option<&str>,
    archived: bool,
    outcome: &Archived,
    now: DateTime<Utc>,
) -> Vec<AuditEvent> {
    if !outcome.changed() {
        return Vec::new();
    }
    let mut events: Vec<AuditEvent> = outcome
        .reaped
        .iter()
        .map(|lock| make_expired_event(lock, now))
        .collect();
    events.extend(
        outcome
            .released
            .iter()
            .map(|lock| make_released_event(lock, "line archived")),
    );
    let variants: Vec<serde_json::Value> = outcome
        .variants
        .iter()
        .map(|(v, plan_id)| json!({ "variant": v, "plan_id": plan_id.as_str() }))
        .collect();
    let event_type = if archived {
        "plan.portfolio.archived"
    } else {
        "plan.portfolio.unarchived"
    };
    events.push(AuditEvent::new(event_type).with_payload(json!({
        "project": project,
        "name": name,
        "variant": variant,
        "line": outcome.line,
        "variants": variants,
        "released": outcome
            .released
            .iter()
            .map(|l| l.deliverable_id.as_str())
            .collect::<Vec<_>>(),
    })));
    events
}

/// The [`SyncOutcome`] of a committed sync and its audit trail.
fn sync_outcome(
    synced: Synced,
    project: &str,
    name: String,
    variant: String,
    now: DateTime<Utc>,
) -> (SyncOutcome, Vec<AuditEvent>) {
    let outcome = |plan_id, revision, created, changed, diff| SyncOutcome {
        plan_id,
        name: name.clone(),
        variant: variant.clone(),
        revision,
        created,
        changed,
        diff,
    };
    match synced {
        Synced::Created { plan_id, selected } => {
            let out = outcome(plan_id, 1, true, true, None);
            let event = make_portfolio_created_event(&out, project, selected);
            (out, vec![event])
        }
        Synced::Unchanged { plan_id, revision } => {
            (outcome(plan_id, revision, false, false, None), Vec::new())
        }
        Synced::Revised { plan_id, revised } => {
            let events = revision_events(&plan_id, &revised, now);
            let out = outcome(plan_id, revised.revision, false, true, Some(revised.diff));
            (out, events)
        }
    }
}

/// Longest `project` key accepted by `sync_plan` (a path-derived key, not a
/// slug).
const MAX_PROJECT_LEN: usize = 512;

/// Characters refused in a project key: controls (newlines included) and
/// invisible Unicode format/separator characters that can disguise how the
/// key reads when echoed (zero-width, bidi embeddings/overrides/isolates,
/// word joiners, BOM, line/paragraph separators, interlinear annotations).
fn is_disallowed_project_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

/// Reject a malformed `(project, name, variant)` with `INVALID_PATH`. The
/// project key is echoed by tools and audit events, so control and
/// invisible format characters are refused.
fn validate_variant_key(req: &SyncRequest) -> Result<(), PlannerError> {
    if req.project.is_empty() || req.project.chars().count() > MAX_PROJECT_LEN {
        return Err(PlannerError::InvalidPath {
            reason: format!("project must be 1..={MAX_PROJECT_LEN} characters"),
        });
    }
    if req.project.chars().any(is_disallowed_project_char) {
        return Err(PlannerError::InvalidPath {
            reason: "project must not contain control or invisible format characters".to_string(),
        });
    }
    crate::project::validate_slug("name", &req.name)?;
    crate::project::validate_slug("variant", &req.variant)
}

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

/// Validate `mark_status` progress fields (`INVALID_ACTUALS`). Takes wide
/// types (any JSON number) so the server can check raw wire values with the
/// same messages: a negative, fractional or > 100 `earned_pct` is refused.
pub(crate) fn validate_actuals(
    status: &DeliverableStatus,
    earned_pct: Option<f64>,
    actual_effort_hours: Option<f64>,
    evidence: Option<&str>,
) -> Result<(), PlannerError> {
    let bad = |reason: String| Err(PlannerError::InvalidActuals { reason });
    if let Some(pct) = earned_pct {
        if !(pct.fract() == 0.0 && (0.0..=100.0).contains(&pct)) {
            return bad(format!("earned_pct must be an integer 0..=100, got {pct}"));
        }
        if !matches!(
            status,
            DeliverableStatus::InProgress | DeliverableStatus::Complete
        ) {
            return bad(
                "earned_pct is only accepted with status in_progress (or complete, where it is \
                 ignored)"
                    .to_string(),
            );
        }
    }
    if let Some(h) = actual_effort_hours
        && !(h.is_finite() && (0.0..=f64::from(crate::plan::MAX_HOURS)).contains(&h))
    {
        return bad(format!(
            "actual_effort_hours must be a finite number between 0 and 1000000, got {h}"
        ));
    }
    if let Some(e) = evidence {
        let chars = e.chars().count();
        if chars > crate::plan::MAX_EVIDENCE_CHARS {
            return bad(format!(
                "evidence must be at most {} characters, got {chars}",
                crate::plan::MAX_EVIDENCE_CHARS
            ));
        }
    }
    Ok(())
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

/// Mark a deliverable `Complete`: end any lease at `now` (dropping its file
/// claims, crediting its hours, emitting a released event), set the status, and promote dependents whose
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
    now: DateTime<Utc>,
) -> bool {
    let was_complete = all_complete(state);
    if let Some(lock) = state.end_lease(deliverable_id, now) {
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
pub(crate) fn all_complete(state: &PlanState) -> bool {
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
        self.store
            .submit_or_get(&graph_hash, move || initial_plan(graph))
    }

    async fn sync_plan(&self, req: SyncRequest) -> Result<SyncOutcome, PlannerError> {
        validate_variant_key(&req)?;
        validate_graph(&req.graph)?;
        let graph_hash = hash_graph(&req.graph);
        let now = self.now();
        let (project, name, variant) = (req.project.clone(), req.name.clone(), req.variant.clone());
        // Named plans bypass the global submit dedup: create, compare and
        // revise all happen in one immediate transaction.
        let synced = self
            .store
            .write_tx(|tx| crate::portfolio::sync(tx, req, &graph_hash, now, initial_plan))?;
        let (outcome, events) = sync_outcome(synced, &project, name, variant, now);
        self.flush_audit(events).await;
        Ok(outcome)
    }

    async fn fork_plan(&self, req: ForkRequest) -> Result<SyncOutcome, PlannerError> {
        let ForkRequest {
            plan_id,
            variant,
            edits,
            project_root,
        } = req;
        crate::project::validate_slug("variant", &variant)?;
        let (head, info, exists, line_archived) = self.store.read_tx(|tx| {
            let (_, head) = crate::portfolio::revision_graph(tx, &plan_id, None)?;
            let info = crate::portfolio::variant_info(tx, &plan_id)?.ok_or_else(|| {
                PlannerError::InvalidPath {
                    reason: "fork requires a named plan".to_string(),
                }
            })?;
            let exists = crate::portfolio::has_variant(tx, &info.project, &info.name, &variant)?;
            let line_archived = crate::portfolio::line_is_archived(tx, &info.project, &info.name)?;
            Ok((head, info, exists, line_archived))
        })?;
        // Both are checked again inside the creating transaction; failing
        // here first avoids writing a plan file for a fork that cannot be
        // registered. Forking FROM an archived variant of a live line is
        // allowed: the source stays readable and the new draft is live.
        if line_archived {
            return Err(crate::portfolio::line_archived(&info.name));
        }
        if exists {
            return Err(crate::portfolio::variant_exists(&variant, &info.name));
        }
        let graph = crate::edits::apply_edits(&head, &edits)?;
        let root = match project_root {
            Some(root) if root.project_key() != info.project => {
                return Err(PlannerError::InvalidPath {
                    reason: format!(
                        "the project root belongs to a different project than plan {}",
                        plan_id.0
                    ),
                });
            }
            Some(root) => Some(root),
            None => self
                .project_root
                .clone()
                .filter(|root| root.project_key() == info.project),
        };
        let mut sync_req = SyncRequest::new(
            info.project.clone(),
            info.name.clone(),
            variant.clone(),
            graph,
        );
        let mut written = None;
        if let Some(root) = &root {
            let file = root.plan_file(&info.name, &variant)?;
            let hash = root.write_new_graph(&file, &sync_req.graph)?;
            sync_req = sync_req
                .with_source_path(file.rel_path.clone())
                .with_content_hash(hash.clone());
            written = Some((root, file, hash));
        }
        let graph_hash = hash_graph(&sync_req.graph);
        let now = self.now();
        let synced = self
            .store
            .write_tx(|tx| crate::portfolio::sync_new(tx, sync_req, &graph_hash, now, initial_plan))
            .inspect_err(|_| {
                // The fork was never registered: do not leave its file behind.
                if let Some((root, file, hash)) = &written {
                    root.remove_plan_file_if_unchanged(file, hash);
                }
            })?;
        let (outcome, events) = sync_outcome(synced, &info.project, info.name, variant, now);
        self.flush_audit(events).await;
        Ok(outcome)
    }

    async fn compare_plans(
        &self,
        req: ComparePlansRequest,
    ) -> Result<crate::compare::Comparison, PlannerError> {
        let inputs = self.compare_inputs(&req)?;
        crate::compare::compare(&inputs, &req.request)
    }

    async fn export_plan(
        &self,
        plan_id: &PlanId,
        root: &ProjectRoot,
        path: Option<&str>,
        force: bool,
    ) -> Result<String, PlannerError> {
        let (graph, info) = self.store.read_tx(|tx| {
            let (_, graph) = crate::portfolio::revision_graph(tx, plan_id, None)?;
            Ok((graph, crate::portfolio::variant_info(tx, plan_id)?))
        })?;
        let file = match (path, &info) {
            (Some(path), _) => root.resolve_plan_file(path)?,
            (None, Some(info)) if info.project == root.project_key() => {
                root.plan_file(&info.name, &info.variant)?
            }
            (None, Some(_)) => {
                return Err(PlannerError::InvalidPath {
                    reason: format!(
                        "plan {} belongs to a different project; give a path",
                        plan_id.0
                    ),
                });
            }
            (None, None) => {
                return Err(PlannerError::InvalidPath {
                    reason: format!("plan {} is unnamed; export requires a path", plan_id.0),
                });
            }
        };
        if !force {
            self.check_export_target(plan_id, root, &file)?;
        }
        let hash = root.write_graph(&file, &graph)?;
        // Exported to its own tracked file: the file now matches the head,
        // so record its hash (drift stays false).
        if let Some(info) = &info
            && info.project == root.project_key()
            && info.source_path.as_deref() == Some(file.rel_path.as_str())
        {
            self.store
                .write_tx(|tx| crate::portfolio::set_content_hash(tx, plan_id, &hash))?;
        }
        Ok(file.rel_path)
    }

    async fn baseline(
        &self,
        req: crate::earned_value::BaselineRequest,
    ) -> Result<crate::earned_value::BaselineOutcome, PlannerError> {
        self.take_baseline(req).await
    }

    async fn ev(
        &self,
        plan_id: &PlanId,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<crate::earned_value::EvReport, PlannerError> {
        self.ev_report(plan_id, as_of)
    }

    async fn snapshot(
        &self,
        req: crate::earned_value::SnapshotRequest,
    ) -> Result<crate::earned_value::SnapshotOutcome, PlannerError> {
        self.take_snapshot(req)
    }

    async fn list_plans(
        &self,
        project: &str,
        include_archived: bool,
    ) -> Result<Vec<PlanLineSummary>, PlannerError> {
        self.store
            .read_tx(|tx| crate::portfolio::list(tx, project, include_archived))
    }

    async fn revision_graph(
        &self,
        plan_id: &PlanId,
        revision: Option<u32>,
    ) -> Result<(u32, PlanGraph), PlannerError> {
        self.store
            .read_tx(|tx| crate::portfolio::revision_graph(tx, plan_id, revision))
    }

    async fn revise_plan(&self, req: ReviseRequest) -> Result<(u32, RevisionDiff), PlannerError> {
        let ReviseRequest {
            plan_id,
            graph,
            force,
        } = req;
        let now = self.now();
        let revised = self.store.write_tx(|tx| {
            crate::portfolio::ensure_revisable(tx, &plan_id)?;
            crate::portfolio::revise(tx, &plan_id, graph, None, None, force, now)
        })?;
        self.flush_audit(revision_events(&plan_id, &revised, now))
            .await;
        Ok((revised.revision, revised.diff))
    }

    async fn select_variant(
        &self,
        plan_id: &PlanId,
        force: bool,
    ) -> Result<SelectOutcome, PlannerError> {
        let now = self.now();
        let selected = self
            .store
            .write_tx(|tx| crate::portfolio::select(tx, plan_id, force, now))?;
        self.flush_audit(selection_events(&selected, now)).await;
        Ok(selected.outcome)
    }

    async fn archive(
        &self,
        project: &str,
        name: &str,
        variant: Option<&str>,
        archived: bool,
        force: bool,
    ) -> Result<(), PlannerError> {
        let now = self.now();
        let outcome = self.store.write_tx(|tx| {
            crate::portfolio::archive(tx, project, name, variant, archived, force, now)
        })?;
        self.flush_audit(archive_events(
            project, name, variant, archived, &outcome, now,
        ))
        .await;
        Ok(())
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
        let cohort = self.store.mutate_executable_plan(&plan_id, |state| {
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
                // In progress by lease from here on: a lost lock row is an orphan.
                state.lockless.remove(&d.id);
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
            earned_pct,
            actual_effort_hours,
            evidence,
        } = req;
        validate_actuals(
            &status,
            earned_pct.map(f64::from),
            actual_effort_hours.map(f64::from),
            evidence.as_deref(),
        )?;
        let report = crate::ev_store::ReportedActuals {
            // Accepted alongside Complete, but ignored there.
            earned_pct: earned_pct.filter(|_| status == DeliverableStatus::InProgress),
            actual_hours: actual_effort_hours,
            evidence,
        };
        let deliverable_id = deliverable_id.as_str();
        let caller_id = &caller_id;
        let now = self.now();
        let mut audit_buf: Vec<AuditEvent> = Vec::new();
        self.store.mutate_executable_plan(&plan_id, |state| {
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

            // Progress fields ride along with every accepted mark (also an
            // idempotent Complete); a refused mark rolls them back.
            state.report_actuals(deliverable_id, report, now);

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
                && let Some(lock) = state.end_lease(deliverable_id, now)
            {
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

            // Lease provenance for the startup sweep: a lockless
            // in_progress mark is a legitimate persisted state (owner or
            // manual work), not an orphan of a lost lease. Any other mark,
            // or one under a held lease, clears it.
            if status == DeliverableStatus::InProgress && !state.locks.contains_key(deliverable_id)
            {
                state.lockless.insert(deliverable_id.to_string());
            } else {
                state.lockless.remove(deliverable_id);
            }

            if is_complete {
                if complete_deliverable(state, deliverable_id, &mut audit_buf, "completed", now) {
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

        self.store.mutate_executable_plan(&plan_id, |state| {
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
        let ((mut status, head_hash), info) =
            self.store.read_plan_and_variant(plan_id, |state| {
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

                let schedule = crate::schedule::schedule_rows(&state.graph, &state.cached_result);
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

                let milestones =
                    crate::schedule::milestone_rows(&state.graph, &state.cached_result, |id| {
                        matches!(state.statuses.get(id), Some(DeliverableStatus::Complete))
                    });

                let status = PlanStatus {
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
                    name: None,
                    variant: None,
                    selected: None,
                    definition_drift: None,
                };
                (status, hash_graph(&state.graph))
            })?;
        if let Some(info) = info {
            // File I/O outside the read snapshot; the head hash is from it.
            status.definition_drift =
                definition_drift(self.project_root.as_ref(), &info, &head_hash);
            status.name = Some(info.name);
            status.variant = Some(info.variant);
            status.selected = Some(info.selected);
        }
        Ok(status)
    }

    async fn accept(&self, req: AcceptRequest) -> Result<(), PlannerError> {
        let mut audit_buf: Vec<AuditEvent> = Vec::new();
        let plan_id = req.plan_id.clone();
        let deliverable_id = req.deliverable_id.as_str();
        let now = self.now();
        self.store.mutate_executable_plan(&plan_id, |state| {
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

            let plan_done =
                complete_deliverable(state, deliverable_id, &mut audit_buf, "accepted", now);
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
        let now = self.now();
        let mut audit_buf: Vec<AuditEvent> = Vec::new();
        self.store.mutate_executable_plan(&plan_id, |state| {
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

            if let Some(lock) = state.end_lease(deliverable_id, now) {
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
            earning_rule: None,
        }
    }

    #[test]
    fn explicit_default_earning_rule_hashes_like_absent() {
        let absent = deliverable("D1", Some(1.0), json!({}));
        let mut explicit = absent.clone();
        explicit.earning_rule = Some(crate::plan::EarningRule::ZeroHundred);
        let graph = |d: Deliverable| PlanGraph {
            deliverables: vec![d],
            max_chained_dispatch: None,
        };
        assert_eq!(hash_graph(&graph(explicit)), hash_graph(&graph(absent)));
    }

    #[test]
    fn canonical_form_omits_an_absent_earning_rule() {
        let d = deliverable("D1", Some(1.0), json!({}));
        assert!(canonical_deliverable(&d).get("earning_rule").is_none());
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
