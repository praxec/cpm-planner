//! `cpm-planner` re-exports the `cpm-planner-core` kernel at its old paths:
//! the types are the same, not copies, so values pass freely between crates.

use cpm_planner::plan::PlanGraph;

fn graph() -> PlanGraph {
    serde_json::from_value(serde_json::json!({
        "deliverables": [
            {"id": "a", "owned_files": ["a.rs"], "prerequisites": [], "duration_hours": 3.0},
            {"id": "b", "owned_files": ["b.rs"], "prerequisites": ["a"], "duration_hours": 5.0}
        ]
    }))
    .expect("deserializes")
}

#[test]
fn a_cpm_planner_plan_graph_is_scheduled_by_the_core_compute_cpm() {
    // Compiles only if `cpm_planner::plan::PlanGraph` is the core type.
    let cpm = cpm_planner_core::schedule::compute_cpm(&graph()).expect("schedules");
    assert_eq!(cpm.critical_path_duration, 8.0);
}

#[test]
fn the_old_compute_cpm_path_returns_the_core_result_type() {
    let result: cpm_planner_core::task::CriticalPathResult =
        cpm_planner::schedule::compute_cpm(&graph()).expect("schedules");
    assert_eq!(result.critical_path, ["__start__", "a", "b", "__finish__"]);
}

#[test]
fn the_planner_error_is_the_core_error_type() {
    let err: cpm_planner_core::plan::PlannerError =
        cpm_planner::plan::PlannerError::InvalidGraph { reason: "x".into() };
    assert_eq!(err.to_string(), "INVALID_GRAPH: x");
}
