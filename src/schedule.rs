//! Shared CPM computation: turns a validated [`PlanGraph`] into a
//! [`CriticalPathResult`]. Used both when a plan is submitted and when the
//! store recomputes results cached by an older kernel version.

use crate::algorithm::CpmAlgorithm;
use crate::estimator::EffortEstimator;
use crate::plan::{Deliverable, FINISH_ID, PlanGraph, PlannerError, START_ID};
use crate::task::{CriticalPathResult, Task, TaskKind};
use std::collections::HashSet;

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
    add_endpoints(graph, &mut tasks);
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

/// Inject the synthetic zero-effort `__start__` / `__finish__` tasks.
/// Every root deliverable depends on `__start__`; `__finish__` depends on
/// every sink (a deliverable no other deliverable lists as a prerequisite).
/// Only the CPM input is touched; the stored graph never contains them.
fn add_endpoints(graph: &PlanGraph, tasks: &mut Vec<Task>) {
    let referenced: HashSet<&str> = graph
        .deliverables
        .iter()
        .flat_map(crate::graph::prerequisite_ids)
        .collect();
    for t in tasks.iter_mut().filter(|t| t.dependencies.is_empty()) {
        t.dependencies.push(START_ID.to_string());
    }
    let sinks: Vec<String> = graph
        .deliverables
        .iter()
        .filter(|d| !referenced.contains(d.id.as_str()))
        .map(|d| d.id.clone())
        .collect();
    tasks.push(endpoint_task(START_ID, Vec::new()));
    let finish_deps = if sinks.is_empty() {
        vec![START_ID.to_string()]
    } else {
        sinks
    };
    tasks.push(endpoint_task(FINISH_ID, finish_deps));
}

fn endpoint_task(id: &str, dependencies: Vec<String>) -> Task {
    Task {
        id: id.to_string(),
        name: id.to_string(),
        kind: TaskKind::Custom {
            description: String::new(),
        },
        effort_hours: 0.0,
        dependencies,
        ..Task::default()
    }
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

    // Calendar duration, when given, is the scheduled length; effort stays
    // on the deliverable as the cost basis.
    let scheduled_hours = d.duration_hours.unwrap_or(effort_hours);
    let lag_by_dependency = d
        .prerequisites
        .iter()
        .filter(|p| p.lag_hours() > 0.0)
        .map(|p| (p.id().to_string(), p.lag_hours()))
        .collect();

    Task {
        id: d.id.clone(),
        name: d.id.clone(),
        kind,
        effort_hours: scheduled_hours,
        lag_by_dependency,
        dependencies: crate::graph::prerequisite_ids(d)
            .map(str::to_string)
            .collect(),
        ..Task::default()
    }
}
