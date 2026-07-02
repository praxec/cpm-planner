//! The lock-aware [`Planner`] trait — the seam between an MCP/host caller and
//! a scheduling implementation. [`crate::planner::BasicCpmPlanner`] is the
//! textbook Critical Path Method implementation shipped by this crate.

use async_trait::async_trait;

use crate::plan::{
    CallerId, Cohort, DeliverableStatus, PlanGraph, PlanId, PlanStatus, PlannerError,
};

/// Lock-aware planner.
///
/// # Semantics
///
/// - [`Planner::submit_plan`] is idempotent on a deterministic hash of
///   `(graph, caller)`. Resubmitting an identical plan returns the existing
///   [`PlanId`] instead of creating a duplicate.
/// - [`Planner::acquire_cohort`] returns up to `max_count` deliverables that
///   are simultaneously: (a) all prerequisites complete, (b) file sets
///   mutually disjoint within the returned cohort, (c) file sets disjoint from
///   every currently held lock. The implementation MUST lock the returned
///   deliverables atomically.
/// - [`Planner::mark_status`] with [`DeliverableStatus::Complete`] or
///   [`DeliverableStatus::Failed`] releases the lock. If the supplied
///   `caller_id` does not match the lock holder, the call returns
///   [`PlannerError::LockNotHeld`].
/// - [`Planner::heartbeat`] refreshes the TTL on a held lock.
/// - [`Planner::status`] is a cheap read-only snapshot and is safe to poll.
/// - [`Planner::force_release`] is the operator escape hatch. Implementations
///   MUST emit an audit event carrying the supplied `reason`.
#[async_trait]
pub trait Planner: Send + Sync {
    /// Submit a [`PlanGraph`]. Idempotent on `(graph, caller_id)`; an
    /// identical resubmission returns the existing [`PlanId`].
    async fn submit_plan(&self, graph: PlanGraph) -> Result<PlanId, PlannerError>;

    /// Acquire up to `max_count` deliverables that are ready to run *and* have
    /// mutually disjoint `owned_files` (within the cohort and against all
    /// currently held locks). The returned [`Cohort`] carries one
    /// [`crate::plan::LockInfo`] per acquired deliverable, in the same order.
    async fn acquire_cohort(
        &self,
        plan_id: &PlanId,
        caller_id: &CallerId,
        max_count: usize,
    ) -> Result<Cohort, PlannerError>;

    /// Update the lifecycle state of a deliverable. Setting `Complete` or
    /// `Failed` releases the lock; `caller_id` MUST be the lock holder or the
    /// call is rejected with [`PlannerError::LockNotHeld`].
    async fn mark_status(
        &self,
        plan_id: &PlanId,
        deliverable_id: &str,
        caller_id: &CallerId,
        status: DeliverableStatus,
    ) -> Result<(), PlannerError>;

    /// Refresh the TTL on a held lock. Rejected with
    /// [`PlannerError::LockNotHeld`] if `caller_id` is not the holder, or with
    /// [`PlannerError::LockExpired`] if the lock already lapsed.
    async fn heartbeat(
        &self,
        plan_id: &PlanId,
        deliverable_id: &str,
        caller_id: &CallerId,
    ) -> Result<(), PlannerError>;

    /// Cheap read-only snapshot. Safe to poll on a timer.
    async fn status(&self, plan_id: &PlanId) -> Result<PlanStatus, PlannerError>;

    /// Operator escape hatch: forcibly release a lock regardless of holder or
    /// TTL. Implementations MUST emit an audit event carrying `reason`.
    async fn force_release(
        &self,
        plan_id: &PlanId,
        deliverable_id: &str,
        reason: &str,
    ) -> Result<(), PlannerError>;
}
