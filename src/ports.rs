//! The lock-aware [`Planner`] trait — the seam between an MCP/host caller and
//! a scheduling implementation. [`crate::planner::BasicCpmPlanner`] is the
//! textbook Critical Path Method implementation shipped by this crate.

use async_trait::async_trait;

use crate::compare::Comparison;
use crate::plan::{
    AcceptRequest, AcquireRequest, Cohort, ComparePlansRequest, ForceReleaseRequest, ForkRequest,
    HeartbeatRequest, MarkStatusRequest, PlanDefinition, PlanGraph, PlanId, PlanLineSummary,
    PlanStatus, PlannerError, ReviseRequest, SelectOutcome, SyncOutcome, SyncRequest,
};
use crate::project::ProjectRoot;
use crate::revise::RevisionDiff;

/// Lock-aware planner.
///
/// # Semantics
///
/// - [`Planner::submit_plan`] is idempotent on a deterministic hash of
///   `(graph, caller)`. Resubmitting an identical plan returns the existing
///   [`PlanId`] instead of creating a duplicate.
/// - [`Planner::acquire_cohort`] returns up to `max_count` deliverables that
///   are simultaneously: (a) all prerequisites complete, (b) file sets
///   free of conflicting claims within the returned cohort, (c) free of
///   conflicting claims against every currently held lock (exclusive
///   conflicts with anything; append/append may share). The implementation MUST lock the returned
///   deliverables atomically.
/// - [`Planner::mark_status`] with [`DeliverableStatus::Complete`] or
///   [`DeliverableStatus::Failed`] releases the lock. If the supplied
///   `caller_id` does not match the lock holder, the call returns
///   [`PlannerError::LockNotHeld`].
/// - [`Planner::heartbeat`] refreshes the TTL on a held lock.
/// - [`Planner::status`] is a cheap read-only snapshot and is safe to poll.
/// - [`Planner::force_release`] is the operator escape hatch. Implementations
///   MUST emit an audit event carrying the supplied `reason`.
/// - Execution methods (`acquire_cohort`, `heartbeat`, `mark_status`,
///   `accept`, `force_release`) on a named variant that is not its line's
///   selected variant return [`PlannerError::VariantNotSelected`], and on
///   any variant of an archived line [`PlannerError::ArchiveRefused`];
///   unnamed plans and read/analysis methods are never gated.
#[async_trait]
pub trait Planner: Send + Sync {
    /// Submit a [`PlanGraph`]. Idempotent on `(graph, caller_id)`; an
    /// identical resubmission returns the existing [`PlanId`].
    async fn submit_plan(&self, graph: PlanGraph) -> Result<PlanId, PlannerError>;

    /// Acquire up to `max_count` deliverables that are ready to run *and* have
    /// no conflicting `owned_files` claims (within the cohort and against all
    /// currently held locks; exclusive conflicts with anything, append/append
    /// may share). The returned [`Cohort`] carries one
    /// [`crate::plan::LockInfo`] per acquired deliverable, in the same order.
    async fn acquire_cohort(&self, req: AcquireRequest) -> Result<Cohort, PlannerError>;

    /// Update the lifecycle state of a deliverable. Setting `Complete` or
    /// `Failed` releases the lock; `caller_id` MUST be the lock holder or the
    /// call is rejected with [`PlannerError::LockNotHeld`]. Optional
    /// earned-value progress fields are validated as
    /// [`PlannerError::InvalidActuals`] (see [`MarkStatusRequest`]) and
    /// stored with the mark.
    async fn mark_status(&self, req: MarkStatusRequest) -> Result<(), PlannerError>;

    /// Refresh the TTL on a held lock. Rejected with
    /// [`PlannerError::LockNotHeld`] if `caller_id` is not the holder, or with
    /// [`PlannerError::LockExpired`] if the lock already lapsed.
    async fn heartbeat(&self, req: HeartbeatRequest) -> Result<(), PlannerError>;

    /// Cheap read-only snapshot. Safe to poll on a timer. For a named
    /// variant it also reports the line name, variant, whether it is
    /// selected, and `definition_drift` (see [`PlanStatus::definition_drift`]).
    async fn status(&self, plan_id: &PlanId) -> Result<PlanStatus, PlannerError>;

    /// Return the stored definition for `plan_id` — the [`PlanGraph`]
    /// exactly as submitted.
    async fn get_plan(&self, plan_id: &PlanId) -> Result<PlanDefinition, PlannerError>;

    /// Operator escape hatch: forcibly release a lock regardless of holder or
    /// TTL. Implementations MUST emit an audit event carrying `reason`.
    async fn force_release(&self, req: ForceReleaseRequest) -> Result<(), PlannerError>;

    /// Manager/owner acceptance: mark a deliverable `Complete` without
    /// holding its lease. Requires all prerequisites `Complete`; a live
    /// lease held by another caller is refused (`LOCK_HELD`) unless
    /// `override_lock` is set. Always audited.
    async fn accept(&self, req: AcceptRequest) -> Result<(), PlannerError>;

    /// Register or update one variant of a named plan line. An unknown
    /// `(project, name, variant)` creates a plan (revision 1; the first
    /// variant of a line becomes its selected variant). A known variant
    /// whose graph is unchanged reports `changed: false`; a changed graph is
    /// revised in place exactly as [`Planner::revise_plan`]. Named plans
    /// dedup within their variant only, never against the global
    /// [`Planner::submit_plan`] dedup.
    async fn sync_plan(&self, req: SyncRequest) -> Result<SyncOutcome, PlannerError>;

    /// Every plan line of `project`, sorted by name, each with its variants
    /// sorted by variant name. Archived lines and variants are omitted
    /// unless `include_archived`.
    async fn list_plans(
        &self,
        project: &str,
        include_archived: bool,
    ) -> Result<Vec<PlanLineSummary>, PlannerError>;

    /// The graph of `revision` (the head when `None`) with its revision
    /// number. A plan never revised is at revision 1.
    async fn revision_graph(
        &self,
        plan_id: &PlanId,
        revision: Option<u32>,
    ) -> Result<(u32, PlanGraph), PlannerError>;

    /// Replace a plan's graph in place, carrying progress over (see
    /// [`crate::revise`]). Returns the new revision number and the diff.
    /// Works on named and unnamed plans alike; an archived variant or a
    /// variant of an archived line is `ARCHIVE_REFUSED`.
    async fn revise_plan(&self, req: ReviseRequest) -> Result<(u32, RevisionDiff), PlannerError>;

    /// Make the named variant owning `plan_id` its line's selected (the only
    /// executable) variant. Progress carries over from the previously
    /// selected variant: every deliverable present in both with an identical
    /// canonical definition gets its `Complete` status and counters copied,
    /// unless one of its prerequisites in the new variant is not `Complete`
    /// after carrying (checked transitively), and then the new variant's
    /// open deliverables are re-derived. Expired
    /// locks on the previous variant are reaped first; live ones refuse the
    /// selection with `LOCK_HELD` unless `force`, which releases them
    /// (audited). Selecting the selected variant is a no-op. An unnamed plan
    /// is `PLAN_NOT_FOUND`; an archived variant or line is `ARCHIVE_REFUSED`.
    async fn select_variant(
        &self,
        plan_id: &PlanId,
        force: bool,
    ) -> Result<SelectOutcome, PlannerError>;

    /// Archive (`archived: true`) or unarchive a whole plan line
    /// (`variant: None`) or one variant. The line flag and the variant flags
    /// are independent: a variant is effectively archived when its line or
    /// itself is, so unarchiving a line restores each variant as it was.
    /// Archived variants stay readable but are hidden from
    /// [`Planner::list_plans`] unless `include_archived`, and refuse sync and
    /// selection; every variant of an archived line refuses execution
    /// (`ARCHIVE_REFUSED`). Archiving the selected variant alone is
    /// `ARCHIVE_REFUSED`. Archiving a line whose selected variant holds live
    /// locks (expired ones are reaped first) is `LOCK_HELD` unless `force`,
    /// which releases them (audited). Unarchiving one variant of an archived
    /// line is `ARCHIVE_REFUSED`. Unknown line/variant is `PLAN_NOT_FOUND`.
    async fn archive(
        &self,
        project: &str,
        name: &str,
        variant: Option<&str>,
        archived: bool,
        force: bool,
    ) -> Result<(), PlannerError>;

    /// Fork the named variant `req.plan_id`: copy its head graph, apply
    /// `req.edits` in order ([`crate::edits::apply_edits`]) and register the
    /// result as the new draft (not selected) variant `req.variant` of the
    /// same line, exactly as a [`Planner::sync_plan`] that creates it.
    ///
    /// With a project root for the plan's project (the request's, else the
    /// planner's own), the new variant's plan file is written first (never
    /// replacing an existing file) and the variant is synced from it (source
    /// path and file content hash); otherwise it is registered inline. A
    /// request root of another project is `INVALID_PATH`. An unnamed plan is
    /// `INVALID_PATH: fork requires a named plan`; an existing variant is
    /// `INVALID_PATH: variant '<v>' of '<name>' already exists`. Forking into
    /// an archived line is `ARCHIVE_REFUSED` before anything is written;
    /// forking from an archived variant of a live line is allowed. If the
    /// sync fails after the file was written, the file is removed (best
    /// effort, only while it still holds the bytes the fork wrote).
    async fn fork_plan(&self, req: ForkRequest) -> Result<SyncOutcome, PlannerError>;

    /// Compare plans on the scorecard ([`crate::compare::compare`]): either
    /// `plan_ids` (2..=16 distinct plans, in the given order; an unnamed plan
    /// is labelled by its id) or every non-archived variant of the line
    /// `plan` (sorted by variant). Giving both or neither, duplicate ids, or
    /// fewer than two or more than 16 variants is `INVALID_GRAPH`; an unknown
    /// plan or line is `PLAN_NOT_FOUND`. The Monte Carlo work budget is
    /// shared across variants. Read-only.
    async fn compare_plans(&self, req: ComparePlansRequest) -> Result<Comparison, PlannerError>;

    /// Write the head graph of `plan_id` under `root` and return the file's
    /// root-relative path: to `path` when given (it must be of the form
    /// `.cpm-planner/plans/<name>/<variant>.json`), else to the named
    /// variant's own file `<name>/<variant>.json`, which requires `root` to
    /// be the variant's project. An unnamed plan requires `path`. An
    /// existing file is replaced, except (unless `force`) another variant's
    /// tracked plan file, or this variant's own file while it holds local
    /// edits never synced (drifted content no revision recorded), which are
    /// `INVALID_PATH`. The written file is NOT synced, except that exporting
    /// to the variant's own tracked file records the written hash, so its
    /// drift is false afterwards.
    async fn export_plan(
        &self,
        plan_id: &PlanId,
        root: &ProjectRoot,
        path: Option<&str>,
        force: bool,
    ) -> Result<String, PlannerError>;
}
