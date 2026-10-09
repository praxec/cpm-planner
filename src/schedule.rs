//! Shared CPM computation: turns a validated [`PlanGraph`] into a
//! [`CriticalPathResult`]. Used when a plan is submitted, when the store
//! recomputes results cached by an older kernel version, and by the
//! read-only analysis tools.
//!
//! Only [`compute_cpm`] is public; the row builders and length helpers are
//! crate-internal.

use crate::algorithm::CpmAlgorithm;
use crate::estimator::EffortEstimator;
use crate::plan::{
    Deliverable, FINISH_ID, MilestoneRow, PlanGraph, PlannerError, START_ID, ScheduleRow,
};
use crate::task::{CriticalPathResult, Task, TaskKind};
use std::collections::{HashMap, HashSet};

/// Run the CPM kernel over `graph`, exactly as `plan.submit` does.
///
/// Each deliverable's scheduled length is `duration_hours`, else
/// `estimated_effort_hours`, else `estimate.likely`, else `0` for a
/// milestone, else a default-config estimator's kind-aware effort. The
/// synthetic `__start__` / `__finish__` tasks are added, so the result's
/// `tasks` and `critical_path` include them.
///
/// `graph` should already be valid (as `plan.submit` would accept it). If
/// the kernel still leaves tasks unscheduled (a cycle or duplicate id
/// slipped through), the result surfaces as [`PlannerError::InvalidGraph`]
/// instead of a confidently-wrong schedule.
///
/// # Errors
///
/// [`PlannerError::InvalidGraph`] when any task could not be scheduled.
pub fn compute_cpm(graph: &PlanGraph) -> Result<CriticalPathResult, PlannerError> {
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
pub(crate) fn add_endpoints(graph: &PlanGraph, tasks: &mut Vec<Task>) {
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

/// Kind-aware effort derived from `metadata`, used as the last resort when
/// neither an explicit effort, a three-point estimate nor a milestone
/// zero-length rule applies.
fn derived_effort(d: &Deliverable, estimator: &EffortEstimator) -> f32 {
    let kind = TaskKind::Custom {
        description: d
            .metadata
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    };
    // Coarse complexity hint from metadata; defaults to false.
    let is_complex = d
        .metadata
        .get("complexity")
        .or_else(|| d.metadata.get("is_complex"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    estimator.estimate(&kind, is_complex)
}

/// Scheduled length in hours for a deliverable. Precedence:
/// `duration_hours`, else `estimated_effort_hours`, else `estimate.likely`,
/// else `0.0` for a milestone with no duration, else the estimator's
/// kind-aware effort.
pub(crate) fn scheduled_length(d: &Deliverable, estimator: &EffortEstimator) -> f32 {
    if let Some(hours) = d.duration_hours {
        return hours;
    }
    if let Some(hours) = d.estimated_effort_hours {
        return hours;
    }
    if let Some(estimate) = d.estimate {
        return estimate.likely;
    }
    if d.is_milestone() {
        return 0.0;
    }
    derived_effort(d, estimator)
}

/// Cost basis in hours for a deliverable: `estimated_effort_hours`, else
/// `estimate.likely`, else `0.0` for a milestone, else the estimator's
/// kind-aware effort. Unlike [`scheduled_length`] this ignores
/// `duration_hours`: calendar time is not cost.
pub(crate) fn effort_basis(d: &Deliverable, estimator: &EffortEstimator) -> f32 {
    if let Some(hours) = d.estimated_effort_hours {
        return hours;
    }
    if let Some(estimate) = d.estimate {
        return estimate.likely;
    }
    if d.is_milestone() {
        return 0.0;
    }
    derived_effort(d, estimator)
}

/// Convert each [`Deliverable`] into a [`Task`] for the CPM kernel.
///
/// The task's scheduled length comes from [`scheduled_length`]; a
/// `complexity` hint can be carried in `metadata` (boolean
/// `complexity`/`is_complex`) to opt a deliverable into the configured
/// complexity multiplier.
pub(crate) fn deliverable_to_task(d: &Deliverable, estimator: &EffortEstimator) -> Task {
    let description = d
        .metadata
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let kind = TaskKind::Custom { description };

    let scheduled_hours = scheduled_length(d, estimator);
    // A repeated prerequisite id takes its largest lag, independent of order.
    let mut lag_by_dependency: HashMap<String, f32> = HashMap::new();
    for p in d.prerequisites.iter().filter(|p| p.lag_hours() > 0.0) {
        let lag = lag_by_dependency.entry(p.id().to_string()).or_insert(0.0);
        *lag = lag.max(p.lag_hours());
    }

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

/// Schedule rows for `graph`: `__start__`, each deliverable in graph order,
/// then `__finish__`. The synthetic endpoints are flagged. Shared by
/// `plan.status` and `plan.simulate` so both report identical rows.
pub(crate) fn schedule_rows(graph: &PlanGraph, cpm: &CriticalPathResult) -> Vec<ScheduleRow> {
    let by_id = task_index(cpm);
    let task_of = |id: &str| by_id.get(id).copied();
    let row_of = |t: &Task, synthetic: bool| ScheduleRow {
        id: t.id.clone(),
        es: t.earliest_start,
        ef: t.earliest_finish,
        ls: t.latest_start,
        lf: t.latest_finish,
        float: t.float,
        critical: t.is_critical,
        synthetic,
    };
    let mut rows: Vec<ScheduleRow> = Vec::new();
    rows.extend(task_of(START_ID).map(|t| row_of(t, true)));
    rows.extend(
        graph
            .deliverables
            .iter()
            .filter_map(|d| task_of(&d.id))
            .map(|t| row_of(t, false)),
    );
    rows.extend(task_of(FINISH_ID).map(|t| row_of(t, true)));
    rows
}

/// Milestone rows for `graph`; `is_complete` supplies each milestone's
/// completion flag (`plan.simulate` has no statuses and passes `false`).
pub(crate) fn milestone_rows(
    graph: &PlanGraph,
    cpm: &CriticalPathResult,
    is_complete: impl Fn(&str) -> bool,
) -> Vec<MilestoneRow> {
    let by_id = task_index(cpm);
    graph
        .deliverables
        .iter()
        .filter(|d| d.is_milestone())
        .filter_map(|d| {
            let task = *by_id.get(d.id.as_str())?;
            Some(MilestoneRow {
                id: d.id.clone(),
                critical_path: CpmAlgorithm::trace_path_to(&cpm.tasks, &d.id),
                hours: task.earliest_finish,
                complete: is_complete(&d.id),
            })
        })
        .collect()
}

/// Id -> task lookup over a CPM result, built once per row listing.
fn task_index(cpm: &CriticalPathResult) -> HashMap<&str, &Task> {
    cpm.tasks.iter().map(|t| (t.id.as_str(), t)).collect()
}
