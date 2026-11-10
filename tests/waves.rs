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
        work("candidate", &["a", "b"], 1.0),
        gate(
            "g",
            &["candidate"],
            1.0,
            json!({ "evidence_scope": "gate", "candidate": "candidate" }),
        ),
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

// Evidence scopes (docs/evidence.md, "Lint findings").

fn severity_of(r: &LintReport, code: &str) -> Option<Severity> {
    r.findings
        .iter()
        .find(|f| f.code == code)
        .map(|f| f.severity)
}

fn scoped_gate(extra: Value) -> PlanGraph {
    graph(vec![
        work("a", &[], 4.0),
        gate("g", &["a"], 1.0, extra),
        milestone("m", &["g"]),
    ])
}

fn scoped_work(extra: Value) -> PlanGraph {
    let mut metadata = json!({ "artifact": "code", "owner": "agent" });
    if let Value::Object(m) = extra {
        for (k, v) in m {
            metadata[k] = v;
        }
    }
    graph(vec![
        json!({ "id": "w", "owned_files": [], "prerequisites": [],
                       "estimated_effort_hours": 1.0, "metadata": metadata }),
    ])
}

#[test]
fn a_gate_without_an_evidence_scope_is_gate_scope_missing() {
    let g = scoped_gate(json!({ "candidate": "a" }));
    assert_eq!(
        severity_of(&lint(&g), "GATE_SCOPE_MISSING"),
        Some(Severity::Warning)
    );
}

#[test]
fn a_gate_declaring_its_scope_is_not_gate_scope_missing() {
    let g = scoped_gate(json!({ "evidence_scope": "gate", "candidate": "a" }));
    assert!(!fires(&lint(&g), "GATE_SCOPE_MISSING"));
}

#[test]
fn a_regression_gate_without_a_candidate_is_gate_candidate_missing() {
    let g = scoped_gate(json!({ "evidence_scope": "gate" }));
    assert_eq!(
        severity_of(&lint(&g), "GATE_CANDIDATE_MISSING"),
        Some(Severity::Warning)
    );
}

#[test]
fn a_gate_defaulting_to_the_gate_scope_needs_a_candidate() {
    assert!(fires(
        &lint(&scoped_gate(json!({}))),
        "GATE_CANDIDATE_MISSING"
    ));
}

#[test]
fn a_publish_gate_needs_no_candidate() {
    let g = scoped_gate(json!({ "evidence_scope": "publish" }));
    assert!(!fires(&lint(&g), "GATE_CANDIDATE_MISSING"));
}

#[test]
fn a_candidate_that_is_not_a_direct_prerequisite_is_gate_candidate_invalid() {
    let g = scoped_gate(json!({ "evidence_scope": "gate", "candidate": "m" }));
    assert_eq!(
        severity_of(&lint(&g), "GATE_CANDIDATE_INVALID"),
        Some(Severity::Error)
    );
}

#[test]
fn an_indirect_ancestor_is_not_a_valid_candidate() {
    let g = graph(vec![
        work("a", &[], 4.0),
        work("b", &["a"], 4.0),
        gate(
            "g",
            &["b"],
            1.0,
            json!({ "evidence_scope": "gate", "candidate": "a" }),
        ),
        milestone("m", &["g"]),
    ]);
    assert!(fires(&lint(&g), "GATE_CANDIDATE_INVALID"));
}

#[test]
fn a_candidate_that_is_not_a_string_is_gate_candidate_invalid() {
    let g = scoped_gate(json!({ "evidence_scope": "gate", "candidate": ["a"] }));
    assert!(fires(&lint(&g), "GATE_CANDIDATE_INVALID"));
}

#[test]
fn gate_candidate_invalid_names_the_gate_and_the_candidate() {
    let g = scoped_gate(json!({ "evidence_scope": "gate", "candidate": "m" }));
    let ids = lint(&g)
        .findings
        .into_iter()
        .find(|f| f.code == "GATE_CANDIDATE_INVALID")
        .map(|f| f.ids);
    assert_eq!(ids, Some(vec!["g".to_string(), "m".to_string()]));
}

#[test]
fn a_gate_scoped_and_bound_to_its_candidate_raises_no_scope_finding() {
    let g = scoped_gate(json!({ "evidence_scope": "gate", "candidate": "a" }));
    let scope_codes: Vec<String> = lint(&g)
        .findings
        .into_iter()
        .map(|f| f.code)
        .filter(|c| c.starts_with("GATE_") || c.contains("SCOPE"))
        .collect();
    assert!(scope_codes.is_empty(), "{scope_codes:?}");
}

#[test]
fn an_unknown_evidence_scope_is_evidence_scope_invalid() {
    let g = scoped_gate(json!({ "evidence_scope": "regression", "candidate": "a" }));
    assert_eq!(
        severity_of(&lint(&g), "EVIDENCE_SCOPE_INVALID"),
        Some(Severity::Error)
    );
}

#[test]
fn a_gate_may_not_declare_an_approval_scope() {
    let g = scoped_gate(json!({ "evidence_scope": "manual_approval" }));
    assert!(fires(&lint(&g), "EVIDENCE_SCOPE_INVALID"));
}

#[test]
fn work_may_not_declare_the_gate_scope() {
    let g = scoped_work(json!({ "evidence_scope": "gate" }));
    assert!(fires(&lint(&g), "EVIDENCE_SCOPE_INVALID"));
}

#[test]
fn work_may_declare_the_focused_scope() {
    let g = scoped_work(json!({ "evidence_scope": "focused" }));
    assert!(!fires(&lint(&g), "EVIDENCE_SCOPE_INVALID"));
}

#[test]
fn a_manual_deliverable_may_declare_external_signoff() {
    let g = scoped_work(json!({ "kind": "manual", "evidence_scope": "external_signoff" }));
    assert!(!fires(&lint(&g), "EVIDENCE_SCOPE_INVALID"));
}

#[test]
fn a_manual_deliverable_may_not_declare_the_focused_scope() {
    let g = scoped_work(json!({ "kind": "manual", "evidence_scope": "focused" }));
    assert!(fires(&lint(&g), "EVIDENCE_SCOPE_INVALID"));
}

#[test]
fn a_non_string_evidence_scope_is_evidence_scope_invalid() {
    let g = scoped_work(json!({ "evidence_scope": true }));
    assert!(fires(&lint(&g), "EVIDENCE_SCOPE_INVALID"));
}

#[test]
fn evidence_scope_invalid_lists_the_scopes_the_kind_may_declare() {
    let g = scoped_gate(json!({ "evidence_scope": "focused", "candidate": "a" }));
    let message = lint(&g)
        .findings
        .into_iter()
        .find(|f| f.code == "EVIDENCE_SCOPE_INVALID")
        .map(|f| f.message)
        .unwrap_or_default();
    assert!(
        message.contains("'gate', 'merge', 'publish', 'activation'"),
        "{message}"
    );
}

#[test]
fn work_that_needs_an_owner_approval_is_scope_folded() {
    let g = scoped_work(json!({ "acceptance": "owner approval of the new schema" }));
    assert_eq!(
        severity_of(&lint(&g), "SCOPE_FOLDED"),
        Some(Severity::Warning)
    );
}

#[test]
fn work_that_needs_an_external_sign_off_is_scope_folded() {
    let g = scoped_work(json!({ "artifact": "API spec with partner sign-off" }));
    assert!(fires(&lint(&g), "SCOPE_FOLDED"));
}

#[test]
fn work_that_deploys_to_production_is_scope_folded() {
    let g = scoped_work(json!({ "verify": ["deploy to production and smoke test"] }));
    assert!(fires(&lint(&g), "SCOPE_FOLDED"));
}

#[test]
fn work_that_merges_to_main_is_scope_folded() {
    let g = scoped_work(json!({ "description": "fix the bug and merge to main" }));
    assert!(fires(&lint(&g), "SCOPE_FOLDED"));
}

#[test]
fn a_regression_gate_that_also_publishes_is_scope_folded() {
    let g = scoped_gate(json!({ "evidence_scope": "gate", "candidate": "a",
                                "verify": ["full suite, then tag the release"] }));
    assert!(fires(&lint(&g), "SCOPE_FOLDED"));
}

#[test]
fn a_publish_gate_naming_its_own_publish_step_is_not_scope_folded() {
    let g = scoped_gate(json!({ "evidence_scope": "publish", "artifact": "publish the crate" }));
    assert!(!fires(&lint(&g), "SCOPE_FOLDED"));
}

#[test]
fn a_publish_dry_run_is_not_scope_folded() {
    let g = scoped_work(json!({ "verify": ["cargo publish --dry-run --locked"] }));
    assert!(!fires(&lint(&g), "SCOPE_FOLDED"));
}

#[test]
fn a_word_containing_a_phrase_is_not_scope_folded() {
    let g = scoped_work(
        json!({ "artifact": "approver records with disapproval reasons, republished nightly" }),
    );
    assert!(!fires(&lint(&g), "SCOPE_FOLDED"));
}

#[test]
fn a_manual_deliverable_is_never_scope_folded() {
    let g = scoped_work(json!({ "kind": "manual", "artifact": "owner approval, then publish" }));
    assert!(!fires(&lint(&g), "SCOPE_FOLDED"));
}

#[test]
fn scope_folded_false_exempts_a_deliverable_that_only_mentions_the_operation() {
    let g = scoped_work(json!({ "artifact": "docs on how to publish", "scope_folded": false }));
    assert!(!fires(&lint(&g), "SCOPE_FOLDED"));
}

#[test]
fn scope_folded_names_each_folded_scope() {
    let g = scoped_work(json!({ "artifact": "code, owner approval and roll out" }));
    let message = lint(&g)
        .findings
        .into_iter()
        .find(|f| f.code == "SCOPE_FOLDED")
        .map(|f| f.message)
        .unwrap_or_default();
    assert!(
        message.contains("'approval' (manual_approval), 'roll out' (activation)"),
        "{message}"
    );
}

#[test]
fn scope_findings_do_not_rewrite_the_plan() {
    let g = scoped_gate(json!({ "evidence_scope": "nonsense", "candidate": "m" }));
    let before = serde_json::to_value(&g).expect("serialisable graph");
    let _ = lint(&g);
    assert_eq!(
        serde_json::to_value(&g).expect("serialisable graph"),
        before
    );
}

#[test]
fn scope_findings_do_not_depend_on_declaration_order() {
    let nodes = vec![
        work("a", &[], 4.0),
        gate(
            "g2",
            &["a"],
            1.0,
            json!({ "artifact": "approve and publish" }),
        ),
        gate(
            "g1",
            &["a"],
            1.0,
            json!({ "evidence_scope": "bogus", "candidate": 1 }),
        ),
        milestone("m", &["g1", "g2"]),
    ];
    let reversed: Vec<Value> = nodes.iter().rev().cloned().collect();
    assert_eq!(
        lint(&graph(nodes)).findings,
        lint(&graph(reversed)).findings
    );
}
