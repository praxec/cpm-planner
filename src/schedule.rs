//! Shared CPM computation: turns a validated [`PlanGraph`] into a
//! [`CriticalPathResult`]. Used both when a plan is submitted and when the
//! store recomputes results cached by an older kernel version.

use crate::algorithm::CpmAlgorithm;
use crate::estimator::EffortEstimator;
use crate::plan::{Deliverable, PlanGraph, PlannerError};
use crate::task::{CriticalPathResult, Task, TaskKind};

/// Run the CPM kernel over `graph`. A default-config estimator fills in
/// effort for deliverables that omit `estimated_effort_hours`.
///
/// Graphs are expected to have passed cycle validation already; if the kernel
/// still leaves tasks unscheduled the two cycle detectors disagree, which is
/// a correctness bug rather than bad input, so it surfaces as
/// [`PlannerError::InvalidGraph`] instead of a confidently-wrong result.
pub(crate) fn compute_cpm(graph: &PlanGraph) -> Result<CriticalPathResult, PlannerError> {
    let estimator = EffortEstimator::new();
    let mut tasks: Vec<Task> = graph
        .deliverables
        .iter()
        .map(|d| deliverable_to_task(d, &estimator))
        .collect();
    let result = CpmAlgorithm::calculate(&mut tasks);
    if !result.unscheduled.is_empty() {
        return Err(PlannerError::InvalidGraph {
            reason: format!(
                "internal CPM inconsistency: deliverables passed cycle validation but \
                 could not be scheduled: [{}]",
                result.unscheduled.join(", ")
            ),
        });
    }
    Ok(result)
}

/// Convert each [`Deliverable`] into a [`Task`] for the CPM kernel.
///
/// Effort precedence: an explicit `estimated_effort_hours` on the
/// deliverable always wins. When it is absent we ask `estimator` to derive
/// a kind-aware estimate rather than falling back to the flat
/// [`DEFAULT_EFFORT_HOURS`] placeholder. A `complexity` hint can be carried
/// in `metadata` (boolean `complexity`/`is_complex`) to opt a deliverable
/// into the configured complexity multiplier.
pub(crate) fn deliverable_to_task(d: &Deliverable, estimator: &EffortEstimator) -> Task {
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
