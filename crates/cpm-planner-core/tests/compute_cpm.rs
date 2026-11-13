//! The kernel used on its own: a `PlanGraph` deserialized from the minimal
//! JSON shape (`id`, `owned_files`, `prerequisites`, `duration_hours`), then
//! `compute_cpm`, reading `critical_path` and `tasks[].earliest_start`.

use cpm_planner_core::plan::{PlanGraph, PlannerError};
use cpm_planner_core::schedule::compute_cpm;

fn graph() -> PlanGraph {
    serde_json::from_value(serde_json::json!({
        "deliverables": [
            {"id": "design", "owned_files": ["docs/design.md"], "prerequisites": [],
             "duration_hours": 4.0},
            {"id": "build", "owned_files": ["src/app.rs"], "prerequisites": ["design"],
             "duration_hours": 8.0},
            {"id": "docs", "owned_files": ["README.md"], "prerequisites": ["design"],
             "duration_hours": 2.0}
        ]
    }))
    .expect("the minimal deliverable shape deserializes")
}

fn earliest_start(id: &str) -> f64 {
    let cpm = compute_cpm(&graph()).expect("graph schedules");
    let task = cpm.tasks.iter().find(|t| t.id == id).expect("task present");
    f64::from(task.earliest_start)
}

#[test]
fn critical_path_runs_from_start_through_the_longest_chain_to_finish() {
    let cpm = compute_cpm(&graph()).expect("graph schedules");
    assert_eq!(
        cpm.critical_path,
        ["__start__", "design", "build", "__finish__"]
    );
}

#[test]
fn a_dependent_starts_when_its_prerequisite_finishes() {
    assert_eq!(earliest_start("build"), 4.0);
}

#[test]
fn a_root_deliverable_starts_at_zero() {
    assert_eq!(earliest_start("design"), 0.0);
}

#[test]
fn a_cyclic_graph_is_an_invalid_graph_error_with_a_display_message() {
    let cyclic: PlanGraph = serde_json::from_value(serde_json::json!({
        "deliverables": [
            {"id": "a", "owned_files": ["a.rs"], "prerequisites": ["b"], "duration_hours": 1.0},
            {"id": "b", "owned_files": ["b.rs"], "prerequisites": ["a"], "duration_hours": 1.0}
        ]
    }))
    .expect("deserializes");
    let err: PlannerError =
        cpm_planner_core::validate::validate_graph(&cyclic).expect_err("a cycle is rejected");
    assert!(
        err.to_string().starts_with("INVALID_GRAPH: cycle detected"),
        "{err}"
    );
}
