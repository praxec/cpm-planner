//! The lock-aware [`Planner`] trait — the seam between an MCP/host caller and
//! a scheduling implementation. [`crate::planner::BasicCpmPlanner`] is the
//! textbook Critical Path Method implementation shipped by this crate.

use async_trait::async_trait;

use crate::plan::{
    AcceptRequest, AcquireRequest, Cohort, ForceReleaseRequest, HeartbeatRequest,
    MarkStatusRequest, PlanDefinition, PlanGraph, PlanId, PlanLineSummary, PlanStatus,
    PlannerError, ReviseRequest, SelectOutcome, SyncOutcome, SyncRequest,
};
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
    /// call is rejected with [`PlannerError::LockNotHeld`].
    async fn mark_status(&self, req: MarkStatusRequest) -> Result<(), PlannerError>;

    /// Refresh the TTL on a held lock. Rejected with
    /// [`PlannerError::LockNotHeld`] if `caller_id` is not the holder, or with
    /// [`PlannerError::LockExpired`] if the lock already lapsed.
    async fn heartbeat(&self, req: HeartbeatRequest) -> Result<(), PlannerError>;

    /// Cheap read-only snapshot. Safe to poll on a timer.
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
    /// Works on named and unnamed plans alike.
    async fn revise_plan(&self, req: ReviseRequest) -> Result<(u32, RevisionDiff), PlannerError>;

    /// Make the named variant owning `plan_id` its line's selected (the only
    /// executable) variant. Progress carries over from the previously
    /// selected variant: every deliverable present in both with an identical
    /// canonical definition gets its `Complete` status and counters copied,
    /// then the new variant's open deliverables are re-derived. Expired
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
    /// (`variant: None`: the line and all its variants) or one variant.
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
}
