//! Delivery waves: the `metadata.kind = "gate"` deliverable type, the computed
//! wave structure in `plan.lint`, and the findings that keep heavy operations
//! at milestones.

use cpm_planner::lint::{LintReport, Severity, lint};
use cpm_planner::plan::PlanGraph;
use cpm_planner::waves::{MilestoneClass, WaveReport, analyze};
use serde_json::{Value, json};

fn graph(v: Vec<Value>) -> PlanGraph {
    serde_json::from_value(json!({ "deliverables": v })).expect("fixture graph")
}

fn edges(ids: &[&str]) -> Value {
    Value::Array(
        ids.iter()
            .map(|id| json!({ "id": id, "consumes": format!("{id} output artifact") }))
            .collect(),
    )
}

fn work(id: &str, prereqs: &[&str], hours: f32) -> Value {
    json!({ "id": id, "owned_files": [], "prerequisites": edges(prereqs),
            "estimated_effort_hours": hours,
            "metadata": { "artifact": "x", "owner": "agent" } })
}

fn gate(id: &str, prereqs: &[&str], hours: f32, extra: Value) -> Value {
    let mut metadata = json!({ "artifact": "receipt", "owner": "ci", "kind": "gate" });
    if let Value::Object(m) = extra {
        for (k, v) in m {
            metadata[k] = v;
        }
    }
    json!({ "id": id, "owned_files": [], "prerequisites": edges(prereqs),
            "duration_hours": hours, "metadata": metadata })
}

fn milestone(id: &str, prereqs: &[&str]) -> Value {
    json!({ "id": id, "owned_files": [], "prerequisites": edges(prereqs), "milestone": true,
            "metadata": { "owner": "owner" } })
}

fn report(g: &PlanGraph) -> WaveReport {
    analyze(g).expect("analysable graph")
}

fn fires(r: &LintReport, code: &str) -> bool {
    r.findings.iter().any(|f| f.code == code)
}

/// a, b, c -> g1 -> m1; d, e, f (fed by g1's work) -> g2 -> m2.
fn two_waves(boundary: Value) -> PlanGraph {
    graph(vec![
        work("a", &[], 4.0),
        work("b", &[], 4.0),
        work("c", &[], 4.0),
        gate("g1", &["a", "b", "c"], 1.0, boundary),
        milestone("m1", &["g1"]),
        work("d", &["c"], 4.0),
        work("e", &["c"], 4.0),
        work("f", &["c"], 4.0),
        gate("g2", &["d", "e", "f", "m1"], 1.0, json!({})),
        milestone("m2", &["g2"]),
    ])
}

#[test]
fn a_plan_without_gates_has_no_waves() {
    let g = graph(vec![work("a", &[], 1.0), milestone("m", &["a"])]);
    assert_eq!(report(&g).wave_count, 0);
}

#[test]
fn a_milestone_with_no_gate_is_a_checkpoint() {
    let g = graph(vec![work("a", &[], 1.0), milestone("m", &["a"])]);
    assert_eq!(report(&g).milestones[0].class, MilestoneClass::Checkpoint);
}

#[test]
fn a_milestone_after_a_gate_is_a_wave_milestone() {
    let g = two_waves(json!({}));
    assert_eq!(report(&g).milestones[0].class, MilestoneClass::Wave);
}

#[test]
fn a_milestone_past_another_milestone_is_only_proved_by_gates_in_between() {
    let g = graph(vec![
        work("a", &[], 1.0),
        gate("g", &["a"], 1.0, json!({})),
        milestone("first", &["g"]),
        milestone("second", &["first"]),
    ]);
    assert_eq!(report(&g).milestones[1].class, MilestoneClass::Checkpoint);
}

#[test]
fn a_gate_claims_the_work_upstream_of_it() {
    let g = two_waves(json!({}));
    assert_eq!(report(&g).waves[0].deliverables, ["a", "b", "c"]);
}

#[test]
fn a_later_gate_claims_only_work_no_earlier_gate_claimed() {
    let g = two_waves(json!({}));
    assert_eq!(report(&g).waves[1].deliverables, ["d", "e", "f"]);
}

#[test]
fn gates_in_a_chain_with_no_new_work_share_one_wave() {
    let g = graph(vec![
        work("a", &[], 1.0),
        gate("regression", &["a"], 1.0, json!({})),
        gate("promotion", &["regression"], 1.0, json!({})),
    ]);
    assert_eq!(report(&g).wave_count, 1);
}

#[test]
fn parallel_gates_over_the_same_work_share_one_wave() {
    let g = graph(vec![
        work("a", &[], 1.0),
        gate("linux", &["a"], 1.0, json!({})),
        gate("windows", &["a"], 1.0, json!({})),
    ]);
    assert_eq!(report(&g).wave_count, 1);
}

#[test]
fn work_per_gate_hour_divides_scheduled_work_by_scheduled_gate_time() {
    let g = graph(vec![
        work("a", &[], 6.0),
        work("b", &[], 2.0),
        gate("g", &["a", "b"], 2.0, json!({})),
    ]);
    assert_eq!(report(&g).waves[0].work_per_gate_hour, Some(4.0));
}

#[test]
fn gate_fraction_reports_the_share_of_scheduled_time_in_gates() {
    let g = graph(vec![work("a", &[], 3.0), gate("g", &["a"], 1.0, json!({}))]);
    assert_eq!(report(&g).gate_fraction, 0.25);
}

#[test]
fn plan_lint_output_carries_the_wave_structure() {
    let g = two_waves(json!({}));
    assert_eq!(lint(&g).waves.expect("waves").wave_count, 2);
}

#[test]
fn a_cyclic_graph_reports_no_waves() {
    let g = graph(vec![work("a", &["b"], 1.0), work("b", &["a"], 1.0)]);
    assert!(lint(&g).waves.is_none());
}

#[test]
fn a_single_large_wave_lints_clean() {
    let g = graph(vec![
        work("a", &[], 4.0),
        work("b", &[], 4.0),
        gate("g", &["a", "b"], 1.0, json!({})),
        milestone("m", &["g"]),
    ]);
    assert!(lint(&g).clean, "{:#?}", lint(&g).findings);
}

#[test]
fn a_gate_after_every_deliverable_is_wave_too_small() {
    let g = graph(vec![
        work("a", &[], 4.0),
        gate("g1", &["a"], 1.0, json!({ "boundary_reason": "r" })),
        work("b", &[], 4.0),
        gate("g2", &["b"], 1.0, json!({})),
        milestone("m", &["g1", "g2"]),
    ]);
    assert!(fires(&lint(&g), "WAVE_TOO_SMALL"));
}

#[test]
fn a_gate_share_of_half_the_schedule_is_gate_time_dominates() {
    let g = graph(vec![
        work("a", &[], 1.0),
        gate("g1", &["a"], 1.0, json!({ "boundary_reason": "r" })),
        work("b", &[], 1.0),
        gate("g2", &["b"], 1.0, json!({})),
        milestone("m", &["g1", "g2"]),
    ]);
    assert!(fires(&lint(&g), "GATE_TIME_DOMINATES"));
}

#[test]
fn a_wave_boundary_without_a_reason_is_unjustified() {
    assert!(fires(
        &lint(&two_waves(json!({}))),
        "WAVE_BOUNDARY_UNJUSTIFIED"
    ));
}

#[test]
fn a_stated_boundary_reason_justifies_the_wave_boundary() {
    let g = two_waves(json!({ "boundary_reason": "wave 2 consumes the published crate" }));
    assert!(!fires(&lint(&g), "WAVE_BOUNDARY_UNJUSTIFIED"));
}

#[test]
fn work_reaching_no_gate_is_ungated() {
    let g = graph(vec![
        work("a", &[], 1.0),
        gate("g", &["a"], 1.0, json!({})),
        work("late", &["g"], 1.0),
        milestone("m", &["g", "late"]),
    ]);
    assert!(fires(&lint(&g), "UNGATED_WORK"));
}

#[test]
fn a_gate_with_no_time_is_gate_no_duration() {
    let g = graph(vec![
        work("a", &[], 1.0),
        json!({ "id": "g", "owned_files": [], "prerequisites": edges(&["a"]),
                "metadata": { "artifact": "x", "kind": "gate", "owner": "ci" } }),
        milestone("m", &["g"]),
    ]);
    assert!(fires(&lint(&g), "GATE_NO_DURATION"));
}

#[test]
fn a_gate_proving_no_milestone_is_gate_without_milestone() {
    let g = graph(vec![work("a", &[], 1.0), gate("g", &["a"], 1.0, json!({}))]);
    assert!(fires(&lint(&g), "GATE_WITHOUT_MILESTONE"));
}

#[test]
fn work_waiting_on_a_gate_is_reported_for_information_only() {
    let g = graph(vec![
        work("a", &[], 1.0),
        gate("g", &["a"], 1.0, json!({})),
        work("next", &["g"], 1.0),
    ]);
    let found = lint(&g)
        .findings
        .into_iter()
        .find(|f| f.code == "WORK_WAITS_ON_GATE")
        .expect("finding");
    assert_eq!(found.severity, Severity::Info);
}

fn described(artifact: &str, extra: Value) -> PlanGraph {
    let mut metadata = json!({ "artifact": artifact, "owner": "agent" });
    if let Value::Object(m) = extra {
        for (k, v) in m {
            metadata[k] = v;
        }
    }
    graph(vec![
        json!({ "id": "a", "owned_files": [], "prerequisites": [],
                       "estimated_effort_hours": 1.0, "metadata": metadata }),
    ])
}

#[test]
fn an_implementation_promising_all_tests_is_an_undeclared_gate() {
    let g = described("service code with all tests passing", json!({}));
    assert!(fires(&lint(&g), "UNDECLARED_GATE"));
}

#[test]
fn a_full_regression_command_in_verify_is_an_undeclared_gate() {
    let g = described("code", json!({ "verify": ["run the full regression"] }));
    assert!(fires(&lint(&g), "UNDECLARED_GATE"));
}

#[test]
fn adding_a_regression_test_is_not_an_undeclared_gate() {
    let g = described("code with a new regression test", json!({}));
    assert!(!fires(&lint(&g), "UNDECLARED_GATE"));
}

#[test]
fn heavy_false_exempts_a_deliverable_that_only_mentions_a_heavy_operation() {
    let g = described("docs for the regression suite", json!({ "heavy": false }));
    assert!(!fires(&lint(&g), "UNDECLARED_GATE"));
}

#[test]
fn a_consumes_naming_worker_capacity_is_a_resource_constraint_edge() {
    let g = graph(vec![
        work("a", &[], 1.0),
        json!({ "id": "b", "owned_files": [], "estimated_effort_hours": 1.0,
                "prerequisites": [{ "id": "a", "consumes": "worker availability" }],
                "metadata": { "artifact": "x" } }),
    ]);
    assert!(fires(&lint(&g), "RESOURCE_CONSTRAINT_EDGE"));
}

#[test]
fn a_consumes_naming_an_artifact_is_not_a_resource_constraint_edge() {
    let g = graph(vec![
        work("a", &[], 1.0),
        json!({ "id": "b", "owned_files": [], "estimated_effort_hours": 1.0,
                "prerequisites": [{ "id": "a", "consumes": "request schema" }],
                "metadata": { "artifact": "x" } }),
    ]);
    assert!(!fires(&lint(&g), "RESOURCE_CONSTRAINT_EDGE"));
}
