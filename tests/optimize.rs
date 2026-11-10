//! Tests for the offline plan optimizer: deterministic, never mutates, and
//! rejects edits that lose acceptance, gates, contracts, ownership or
//! milestones.

use cpm_planner::edits::{GraphEdit, apply_edits};
use cpm_planner::optimize::{
    OptimizeKind, OptimizeProposal, OptimizeReport, OptimizeRequest, RejectedCandidate, optimize,
    validate_edits,
};
use cpm_planner::plan::{PlanGraph, PlannerError};
use cpm_planner::resource_schedule::ScheduleRequest;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn graph(v: Value) -> PlanGraph {
    serde_json::from_value(json!({ "deliverables": v })).expect("valid graph")
}

fn run(g: &PlanGraph) -> OptimizeReport {
    optimize(g, &OptimizeRequest::default()).expect("optimize")
}

fn proposal<'r>(report: &'r OptimizeReport, id: &str) -> Option<&'r OptimizeProposal> {
    report.proposals.iter().find(|p| p.id == id)
}

fn rejection<'r>(report: &'r OptimizeReport, id: &str) -> Option<&'r RejectedCandidate> {
    report.rejected.iter().find(|r| r.id == id)
}

fn reasons(report: &OptimizeReport, id: &str) -> String {
    rejection(report, id)
        .map(|r| r.reasons.join(" | "))
        .unwrap_or_default()
}

fn applied(g: &PlanGraph, p: &OptimizeProposal) -> PlanGraph {
    apply_edits(g, &p.edits).expect("proposal edits apply")
}

fn deliverable<'g>(g: &'g PlanGraph, id: &str) -> &'g cpm_planner::plan::Deliverable {
    g.deliverables
        .iter()
        .find(|d| d.id == id)
        .expect("deliverable present")
}

fn prereq_ids(g: &PlanGraph, id: &str) -> Vec<String> {
    deliverable(g, id)
        .prerequisites
        .iter()
        .map(|p| p.id().to_string())
        .collect()
}

fn rejected_edits(g: &PlanGraph, edits: Vec<GraphEdit>) -> String {
    match validate_edits(g, &OptimizeRequest::default(), &edits).expect("graph simulates") {
        Ok(p) => panic!("edits accepted: {p:?}"),
        Err(r) => r.reasons.join(" | "),
    }
}

// ------------------------------------------------------------ fixtures

/// `parse` and `parse-tests` share an owner and `src/parse.rs`.
fn merge_plan(second_owner: &str) -> PlanGraph {
    graph(json!([
        {"id": "parse", "owned_files": ["src/parse.rs"], "prerequisites": [],
         "estimated_effort_hours": 2.0,
         "metadata": {"owner": "alice", "artifact": "parser in src/parse.rs"}},
        {"id": "parse-tests", "owned_files": ["src/parse.rs", "tests/parse.rs"],
         "prerequisites": [{"id": "parse", "consumes": "parser entry point"}],
         "estimated_effort_hours": 1.0,
         "metadata": {"owner": second_owner, "artifact": "parser unit tests"}},
        {"id": "cli", "owned_files": ["src/cli.rs"],
         "prerequisites": [{"id": "parse-tests", "consumes": "tested parser module"}],
         "estimated_effort_hours": 2.0,
         "metadata": {"owner": "bob", "artifact": "cli in src/cli.rs"}},
        {"id": "done", "milestone": true, "owned_files": [],
         "prerequisites": [{"id": "cli", "consumes": "shipped cli binary"}]}
    ]))
}

/// `report` waits on `ingest` only for worker availability.
fn false_dependency_plan() -> PlanGraph {
    graph(json!([
        {"id": "ingest", "owned_files": ["src/ingest.rs"], "prerequisites": [],
         "estimated_effort_hours": 2.0,
         "metadata": {"owner": "alice", "artifact": "ingest module"}},
        {"id": "report", "owned_files": ["src/report.rs"],
         "prerequisites": [{"id": "ingest", "consumes": "worker availability"}],
         "estimated_effort_hours": 3.0,
         "metadata": {"owner": "alice", "artifact": "report module"}},
        {"id": "done", "milestone": true, "owned_files": [],
         "prerequisites": [{"id": "report", "consumes": "report module merged"}]}
    ]))
}

/// Three deliverables chained only by resource-constraint edges.
fn cohort_plan() -> PlanGraph {
    graph(json!([
        {"id": "a", "owned_files": ["src/a.rs"], "prerequisites": [],
         "estimated_effort_hours": 2.0, "metadata": {"artifact": "module a"}},
        {"id": "b", "owned_files": ["src/b.rs"],
         "prerequisites": [{"id": "a", "consumes": "same agent"}],
         "estimated_effort_hours": 2.0, "metadata": {"artifact": "module b"}},
        {"id": "c", "owned_files": ["src/c.rs"],
         "prerequisites": [{"id": "b", "consumes": "one at a time"}],
         "estimated_effort_hours": 2.0, "metadata": {"artifact": "module c"}},
        {"id": "done", "milestone": true, "owned_files": [],
         "prerequisites": [{"id": "c", "consumes": "module c shipped"}]}
    ]))
}

fn work(id: &str, file: &str) -> Value {
    json!({"id": id, "owned_files": [file], "prerequisites": [],
           "estimated_effort_hours": 2.0, "metadata": {"artifact": format!("{id} code")}})
}

/// Two waves, each with three work items, a full-regression gate and a
/// milestone. `boundary` sets wave 1's `boundary_reason`.
fn two_wave_plan(boundary: Option<&str>) -> PlanGraph {
    let mut g1 = json!({"id": "regress-1", "owned_files": [], "duration_hours": 2.0,
        "estimated_effort_hours": 2.0,
        "prerequisites": [{"id": "a1", "consumes": "a1 code"}, {"id": "a2", "consumes": "a2 code"},
                          {"id": "a3", "consumes": "a3 code"}],
        "metadata": {"kind": "gate", "artifact": "wave 1 full regression green"}});
    if let Some(reason) = boundary {
        g1["metadata"]["boundary_reason"] = json!(reason);
    }
    graph(json!([
        work("a1", "src/a1.rs"), work("a2", "src/a2.rs"), work("a3", "src/a3.rs"),
        g1,
        {"id": "m1", "milestone": true, "owned_files": [],
         "prerequisites": [{"id": "regress-1", "consumes": "wave 1 regression report"}]},
        work("b1", "src/b1.rs"), work("b2", "src/b2.rs"), work("b3", "src/b3.rs"),
        {"id": "regress-2", "owned_files": [], "duration_hours": 2.0,
         "estimated_effort_hours": 2.0,
         "prerequisites": [{"id": "b1", "consumes": "b1 code"}, {"id": "b2", "consumes": "b2 code"},
                           {"id": "b3", "consumes": "b3 code"}],
         "metadata": {"kind": "gate", "artifact": "wave 2 full regression green"}},
        {"id": "m2", "milestone": true, "owned_files": [],
         "prerequisites": [{"id": "regress-2", "consumes": "wave 2 regression report"},
                           {"id": "m1", "consumes": "wave 1 accepted"}]}
    ]))
}

/// `docs` waits for the regression gate; `consumes` decides whether it
/// needs the gate's result.
fn gate_wait_plan(docs_consumes: &str) -> PlanGraph {
    graph(json!([
        work("c1", "src/c1.rs"), work("c2", "src/c2.rs"), work("c3", "src/c3.rs"),
        {"id": "regression", "owned_files": [], "duration_hours": 2.0,
         "estimated_effort_hours": 2.0,
         "prerequisites": [{"id": "c1", "consumes": "c1 code"}, {"id": "c2", "consumes": "c2 code"},
                           {"id": "c3", "consumes": "c3 code"}],
         "metadata": {"kind": "gate", "artifact": "full regression green"}},
        {"id": "docs", "owned_files": ["docs/guide.md"], "estimated_effort_hours": 1.0,
         "prerequisites": [{"id": "regression", "consumes": docs_consumes}],
         "metadata": {"artifact": "user guide"}},
        {"id": "release", "owned_files": [], "duration_hours": 1.0, "estimated_effort_hours": 1.0,
         "prerequisites": [{"id": "regression", "consumes": "green regression run"},
                           {"id": "docs", "consumes": "published user guide"}],
         "metadata": {"kind": "gate", "artifact": "release tag"}},
        {"id": "shipped", "milestone": true, "owned_files": [],
         "prerequisites": [{"id": "release", "consumes": "release tag pushed"}]}
    ]))
}

// ---------------------------------------------------------- determinism

#[test]
fn identical_inputs_give_identical_reports() {
    let g = two_wave_plan(None);
    assert_eq!(run(&g), run(&g));
}

#[test]
fn identical_inputs_give_identical_serialized_reports() {
    let g = merge_plan("alice");
    let a = serde_json::to_string(&run(&g)).expect("serialize");
    let b = serde_json::to_string(&run(&g)).expect("serialize");
    assert_eq!(a, b);
}

#[test]
fn declaration_order_does_not_change_proposal_ids() {
    let g = cohort_plan();
    let mut reversed = g.clone();
    reversed.deliverables.reverse();
    let ids = |r: &OptimizeReport| r.proposals.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&run(&g)), ids(&run(&reversed)));
}

#[test]
fn optimize_leaves_the_input_graph_unchanged() {
    let g = two_wave_plan(None);
    let before = serde_json::to_value(&g).expect("serialize");
    let _ = run(&g);
    assert_eq!(serde_json::to_value(&g).expect("serialize"), before);
}

#[test]
fn optimize_rejects_a_graph_that_does_not_simulate() {
    let g = graph(json!([
        {"id": "a", "owned_files": [], "prerequisites": ["b"]},
        {"id": "b", "owned_files": [], "prerequisites": ["a"]}
    ]));
    let err = optimize(&g, &OptimizeRequest::default()).expect_err("cycle");
    assert!(matches!(err, PlannerError::InvalidGraph { .. }));
}

#[test]
fn max_proposals_caps_the_list_and_marks_it_truncated() {
    let req = OptimizeRequest {
        max_proposals: Some(0),
        ..OptimizeRequest::default()
    };
    let report = optimize(&cohort_plan(), &req).expect("optimize");
    assert!(report.truncated && report.proposals.is_empty());
}

// ---------------------------------------------------------------- merge

#[test]
fn same_owner_overlapping_files_yield_a_merge_proposal() {
    let report = run(&merge_plan("alice"));
    assert!(proposal(&report, "merge:parse+parse-tests").is_some());
}

#[test]
fn merge_proposal_removes_one_deliverable() {
    let report = run(&merge_plan("alice"));
    let p = proposal(&report, "merge:parse+parse-tests").expect("merge proposed");
    assert_eq!(p.deltas.deliverables, -1);
}

#[test]
fn merge_keeps_both_acceptance_criteria() {
    let g = merge_plan("alice");
    let report = run(&g);
    let merged = applied(
        &g,
        proposal(&report, "merge:parse+parse-tests").expect("merge"),
    );
    assert_eq!(
        deliverable(&merged, "parse").metadata["artifact"],
        json!("parser in src/parse.rs; parser unit tests")
    );
}

#[test]
fn merge_keeps_every_owned_file() {
    let g = merge_plan("alice");
    let report = run(&g);
    let merged = applied(
        &g,
        proposal(&report, "merge:parse+parse-tests").expect("merge"),
    );
    let files: Vec<String> = deliverable(&merged, "parse")
        .owned_files
        .iter()
        .map(|f| f.path().display().to_string())
        .collect();
    assert_eq!(files, ["src/parse.rs", "tests/parse.rs"]);
}

#[test]
fn merge_rewires_dependents_to_the_merged_deliverable() {
    let g = merge_plan("alice");
    let report = run(&g);
    let merged = applied(
        &g,
        proposal(&report, "merge:parse+parse-tests").expect("merge"),
    );
    assert_eq!(prereq_ids(&merged, "cli"), ["parse"]);
}

#[test]
fn merge_keeps_the_combined_effort() {
    let report = run(&merge_plan("alice"));
    let p = proposal(&report, "merge:parse+parse-tests").expect("merge");
    assert_eq!(p.deltas.effort_hours, 0.0);
}

#[test]
fn different_owners_yield_no_merge_candidate() {
    let report = run(&merge_plan("bob"));
    let any_merge = report
        .proposals
        .iter()
        .map(|p| p.kind)
        .chain(report.rejected.iter().map(|r| r.kind))
        .any(|k| k == OptimizeKind::Merge);
    assert!(!any_merge);
}

#[test]
fn merge_with_a_manual_approval_is_rejected_with_the_reason() {
    let mut g = merge_plan("alice");
    g.deliverables[1].metadata["kind"] = json!("manual");
    assert!(reasons(&run(&g), "merge:parse+parse-tests").contains("manual or external approval"));
}

#[test]
fn merge_with_an_interface_contract_is_rejected_with_the_reason() {
    let mut g = merge_plan("alice");
    g.deliverables[0].metadata["contract"] = json!(true);
    assert!(reasons(&run(&g), "merge:parse+parse-tests").contains("interface contract"));
}

// ----------------------------------------------------- false dependency

#[test]
fn resource_constraint_edge_yields_a_false_dependency_proposal() {
    let report = run(&false_dependency_plan());
    assert!(proposal(&report, "false_dependency:ingest->report").is_some());
}

#[test]
fn false_dependency_proposal_reports_the_makespan_saving() {
    let report = run(&false_dependency_plan());
    let p = proposal(&report, "false_dependency:ingest->report").expect("proposed");
    assert_eq!(p.deltas.makespan_hours, -2.0);
}

#[test]
fn false_dependency_removal_keeps_the_milestone_waiting_for_the_producer() {
    let g = false_dependency_plan();
    let report = run(&g);
    let edited = applied(
        &g,
        proposal(&report, "false_dependency:ingest->report").expect("p"),
    );
    assert!(prereq_ids(&edited, "done").contains(&"ingest".to_string()));
}

#[test]
fn false_dependency_with_a_file_collision_is_rejected_with_the_reason() {
    let mut g = false_dependency_plan();
    g.deliverables[1].owned_files.push("src/ingest.rs".into());
    assert!(reasons(&run(&g), "false_dependency:ingest->report").contains("file collision"));
}

#[test]
fn false_dependency_on_an_interface_edge_is_rejected_with_the_reason() {
    let mut g = false_dependency_plan();
    g.deliverables[0].metadata["contract"] = json!(true);
    g.deliverables[1].prerequisites = vec![
        serde_json::from_value(
            json!({"id": "ingest", "consumes": "worker availability", "kind": "interface"}),
        )
        .expect("edge"),
    ];
    assert!(reasons(&run(&g), "false_dependency:ingest->report").contains("interface contract"));
}

#[test]
fn false_dependency_from_a_manual_approval_is_rejected_with_the_reason() {
    let mut g = false_dependency_plan();
    g.deliverables[0].metadata["kind"] = json!("manual");
    assert!(reasons(&run(&g), "false_dependency:ingest->report").contains("manual or external"));
}

#[test]
fn unstated_edge_into_a_milestone_is_rejected_with_the_reason() {
    let mut g = false_dependency_plan();
    g.deliverables[2].prerequisites = vec!["report".into()];
    assert!(reasons(&run(&g), "false_dependency:report->done").contains("is a milestone"));
}

#[test]
fn leveled_resource_wait_is_reported_in_the_baseline() {
    let mut g = cohort_plan();
    for d in g.deliverables.iter_mut().filter(|d| !d.is_milestone()) {
        d.metadata["owner"] = json!("dev");
    }
    for d in &mut g.deliverables[1..3] {
        d.prerequisites.clear();
    }
    let req = OptimizeRequest {
        schedule: Some(ScheduleRequest {
            capacities: BTreeMap::from([("dev".to_string(), 1)]),
            resource_key: "owner".to_string(),
            project_buffer_pct: 0.0,
        }),
        max_proposals: None,
    };
    let report = optimize(&g, &req).expect("optimize");
    assert_eq!(report.baseline.resource_wait_hours, 4.0);
}

// ------------------------------------------------------ parallel cohort

#[test]
fn chain_of_resource_edges_yields_a_parallel_cohort() {
    let report = run(&cohort_plan());
    assert!(proposal(&report, "parallel_cohort:a,b,c").is_some());
}

#[test]
fn parallel_cohort_shortens_the_makespan_to_one_member() {
    let report = run(&cohort_plan());
    let p = proposal(&report, "parallel_cohort:a,b,c").expect("cohort");
    assert_eq!(p.after.makespan, 2.0);
}

#[test]
fn parallel_cohort_ranks_above_its_single_edges() {
    let report = run(&cohort_plan());
    assert_eq!(report.proposals[0].id, "parallel_cohort:a,b,c");
}

#[test]
fn parallel_cohort_with_a_file_collision_is_rejected_with_the_reason() {
    let mut g = cohort_plan();
    g.deliverables[2].owned_files.push("src/a.rs".into());
    assert!(reasons(&run(&g), "parallel_cohort:a,b,c").contains("file collision on 'src/a.rs'"));
}

// ---------------------------------------------------- gate consolidation

#[test]
fn two_unjustified_waves_yield_a_gate_consolidation() {
    let report = run(&two_wave_plan(None));
    assert!(proposal(&report, "gate_consolidation:regress-1->regress-2").is_some());
}

#[test]
fn gate_consolidation_saves_the_first_gate_hours() {
    let report = run(&two_wave_plan(None));
    let p = proposal(&report, "gate_consolidation:regress-1->regress-2").expect("p");
    assert_eq!(p.deltas.gate_hours, -2.0);
}

#[test]
fn gate_consolidation_carries_the_removed_gate_acceptance() {
    let g = two_wave_plan(None);
    let report = run(&g);
    let edited = applied(
        &g,
        proposal(&report, "gate_consolidation:regress-1->regress-2").expect("p"),
    );
    assert_eq!(
        deliverable(&edited, "regress-2").metadata["artifact"],
        json!("wave 2 full regression green; wave 1 full regression green")
    );
}

#[test]
fn gate_consolidation_keeps_the_first_milestone_behind_the_remaining_gate() {
    let g = two_wave_plan(None);
    let report = run(&g);
    let edited = applied(
        &g,
        proposal(&report, "gate_consolidation:regress-1->regress-2").expect("p"),
    );
    assert!(prereq_ids(&edited, "m1").contains(&"regress-2".to_string()));
}

#[test]
fn justified_wave_boundary_rejects_the_consolidation_with_the_reason() {
    let report = run(&two_wave_plan(Some("wave 1 ships to the customer first")));
    assert!(
        reasons(&report, "gate_consolidation:regress-1->regress-2")
            .contains("wave 1 ships to the customer first")
    );
}

#[test]
fn milestone_upstream_of_the_next_wave_rejects_the_consolidation() {
    let mut g = two_wave_plan(None);
    let b1 = g
        .deliverables
        .iter_mut()
        .find(|d| d.id == "b1")
        .expect("b1");
    b1.prerequisites = vec![
        serde_json::from_value(json!({"id": "m1", "consumes": "wave 1 sign-off record"}))
            .expect("edge"),
    ];
    assert!(
        reasons(&run(&g), "gate_consolidation:regress-1->regress-2")
            .contains("'m1' would stop waiting")
    );
}

// ------------------------------------------------------------ gate bypass

#[test]
fn work_consuming_code_through_a_gate_yields_a_bypass() {
    let report = run(&gate_wait_plan("parser source code"));
    assert!(proposal(&report, "gate_bypass:regression->docs").is_some());
}

#[test]
fn gate_bypass_depends_on_the_gate_prerequisites() {
    let g = gate_wait_plan("parser source code");
    let report = run(&g);
    let edited = applied(
        &g,
        proposal(&report, "gate_bypass:regression->docs").expect("p"),
    );
    assert_eq!(prereq_ids(&edited, "docs"), ["c1", "c2", "c3"]);
}

#[test]
fn gate_bypass_reports_the_makespan_saving() {
    let report = run(&gate_wait_plan("parser source code"));
    let p = proposal(&report, "gate_bypass:regression->docs").expect("p");
    assert_eq!(p.deltas.makespan_hours, -1.0);
}

#[test]
fn work_consuming_the_gate_result_yields_no_bypass() {
    let report = run(&gate_wait_plan("regression report for the guide"));
    let id = "gate_bypass:regression->docs";
    assert!(proposal(&report, id).is_none() && rejection(&report, id).is_none());
}

// ------------------------------------------------------- validate_edits

#[test]
fn caller_edits_that_drop_acceptance_criteria_are_rejected() {
    let g = false_dependency_plan();
    let edits = vec![
        GraphEdit::RemoveEdge {
            from: "ingest".into(),
            to: "report".into(),
        },
        GraphEdit::SetMetadata {
            id: "report".into(),
            key: "artifact".into(),
            value: json!(""),
        },
    ];
    assert!(rejected_edits(&g, edits).contains("acceptance criteria of 'report' lost"));
}

#[test]
fn caller_edits_that_remove_a_manual_approval_are_rejected() {
    let g = graph(json!([
        {"id": "code", "owned_files": ["src/x.rs"], "prerequisites": [],
         "estimated_effort_hours": 2.0, "metadata": {"artifact": "x"}},
        {"id": "sign-off", "owned_files": [], "estimated_effort_hours": 1.0,
         "prerequisites": [{"id": "code", "consumes": "x module for review"}],
         "metadata": {"kind": "manual", "artifact": "owner approval"}}
    ]));
    let edits = vec![GraphEdit::RemoveDeliverable {
        id: "sign-off".into(),
    }];
    assert!(rejected_edits(&g, edits).contains("'sign-off' is removed"));
}

#[test]
fn caller_edits_that_turn_a_gate_into_work_are_rejected() {
    let g = gate_wait_plan("parser source code");
    let edits = vec![GraphEdit::SetMetadata {
        id: "regression".into(),
        key: "kind".into(),
        value: json!("work"),
    }];
    assert!(rejected_edits(&g, edits).contains("'regression' is a gate"));
}

#[test]
fn caller_edits_that_change_an_owner_are_rejected() {
    let g = merge_plan("alice");
    let edits = vec![GraphEdit::SetMetadata {
        id: "cli".into(),
        key: "owner".into(),
        value: json!("carol"),
    }];
    assert!(rejected_edits(&g, edits).contains("owner of 'cli' would change"));
}

#[test]
fn caller_edits_that_drop_an_interface_edge_are_rejected() {
    let mut g = false_dependency_plan();
    g.deliverables[0].metadata["contract"] = json!(true);
    g.deliverables[1].prerequisites = vec![
        serde_json::from_value(
            json!({"id": "ingest", "consumes": "ingest trait", "kind": "interface"}),
        )
        .expect("edge"),
    ];
    let edits = vec![GraphEdit::RemoveEdge {
        from: "ingest".into(),
        to: "report".into(),
    }];
    assert!(
        rejected_edits(&g, edits).contains("interface contract edge 'ingest' -> 'report' lost")
    );
}

#[test]
fn caller_edits_that_lengthen_the_schedule_are_rejected() {
    let g = false_dependency_plan();
    let edits = vec![GraphEdit::SetEffort {
        id: "report".into(),
        hours: 9.0,
    }];
    assert!(rejected_edits(&g, edits).contains("makespan grows by 6 h"));
}
