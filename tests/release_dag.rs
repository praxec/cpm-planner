use std::collections::BTreeMap;
use std::sync::Arc;

use cpm_planner::plan::{Deliverable, PlanGraph};
use cpm_planner::ports::Planner;
use cpm_planner::release_dag::{ReleaseWorkflow, compile, diff, lint_workflow};
use cpm_planner::resource_schedule::{ScheduleRequest, resource_schedule};
use cpm_planner::simulate::{SimulateRequest, simulate};
use cpm_planner::{BasicCpmPlanner, PlanServer};
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

fn input() -> Value {
    serde_json::from_str(include_str!("../examples/release-workflow.json")).unwrap()
}
fn workflow(v: Value) -> ReleaseWorkflow {
    serde_json::from_value(v).unwrap()
}
fn compiled() -> PlanGraph {
    compile(&workflow(input())).unwrap().graph
}
fn node<'a>(g: &'a PlanGraph, id: &str) -> &'a Deliverable {
    g.deliverables.iter().find(|d| d.id == id).unwrap()
}

#[test]
fn release_compiles_into_a_lint_clean_normal_simulation() {
    let result = simulate(&compiled(), &SimulateRequest::default()).unwrap();
    assert!(result.lint.clean);
}

#[test]
fn checks_fan_out_from_the_same_pr() {
    let g = compiled();
    let checks: Vec<_> = ["linux", "windows"]
        .map(|id| {
            node(&g, &format!("release/app/promotion/stage/check/{id}"))
                .prerequisites
                .iter()
                .map(|p| p.id())
                .collect::<Vec<_>>()
        })
        .into();
    assert_eq!(
        checks,
        vec![
            vec!["release/app/promotion/stage/pr"],
            vec!["release/app/promotion/stage/pr"]
        ]
    );
}

#[test]
fn join_consumes_all_required_checks() {
    let g = compiled();
    assert_eq!(
        node(&g, "release/app/promotion/stage/checks-joined")
            .prerequisites
            .iter()
            .map(|p| p.id())
            .collect::<Vec<_>>(),
        [
            "release/app/promotion/stage/check/linux",
            "release/app/promotion/stage/check/windows"
        ]
    );
}

#[test]
fn protected_promotions_are_serialized_by_merge_receipts() {
    let g = compiled();
    assert_eq!(
        node(&g, "release/app/promotion/production/pr").prerequisites[0].id(),
        "release/app/promotion/stage/merge"
    );
}

#[test]
fn owner_wait_blocks_the_approval_without_consuming_owner_capacity() {
    let g = compiled();
    let result = resource_schedule(
        &g,
        &ScheduleRequest {
            capacities: BTreeMap::from([
                ("agent".into(), 10),
                ("owner".into(), 1),
                ("external_runner".into(), 10),
            ]),
            resource_key: "owner".into(),
            project_buffer_pct: 0.0,
        },
    )
    .unwrap();
    let wait = node(&g, "release/app/promotion/stage/approval/review/wait");
    assert_eq!(
        (
            wait.duration_hours,
            wait.metadata["wall_clock_only"].as_bool(),
            result.cpm_makespan > 4.0
        ),
        (Some(2.0), Some(true), true)
    );
}

#[test]
fn retries_consume_the_failed_attempts_retained_receipt() {
    let g = compiled();
    assert_eq!(
        node(&g, "release/app/producer/proof/attempt/retry/run").prerequisites[0].id(),
        "release/app/producer/proof/attempt/first/receipt"
    );
}

#[test]
fn terminal_failed_proof_cannot_feed_acceptance() {
    let mut v = input();
    v["repositories"]["items"][0]["producers"]["items"][0]["attempts"]["items"][1]["outcome"] =
        json!("failed");
    assert!(
        compile(&workflow(v))
            .unwrap_err()
            .to_string()
            .contains("UNRESOLVED_PROOF_FAILURE")
    );
}

#[test]
fn change_skipped_campaign_keeps_a_decision_receipt_without_runner_work() {
    let mut v = input();
    let producer = &mut v["repositories"]["items"][0]["producers"]["items"][0];
    producer["trigger"] =
        json!({"matched":false,"paths":["ui/**"],"evidence":"retained/diff-42.json"});
    producer["attempts"] = json!({"cardinality":0,"items":[]});
    let g = compile(&workflow(v)).unwrap().graph;
    assert_eq!(
        g.deliverables
            .iter()
            .filter(|d| d.id.starts_with("release/app/producer/proof/"))
            .map(|d| d.id.as_str())
            .collect::<Vec<_>>(),
        [
            "release/app/producer/proof/artifact/receipt",
            "release/app/producer/proof/skipped"
        ]
    );
}

#[test]
fn hidden_cardinality_is_reported_before_expansion() {
    let mut v = input();
    v["repositories"]
        .as_object_mut()
        .unwrap()
        .remove("cardinality");
    assert_eq!(
        lint_workflow(&workflow(v)).findings[0].code,
        "HIDDEN_CARDINALITY"
    );
}

#[test]
fn source_cannot_expand_above_its_retained_bound() {
    let mut v = input();
    v["repositories"]["items"][0]["producers"]["source"]["max_items"] = json!(0);
    assert!(
        compile(&workflow(v))
            .unwrap_err()
            .to_string()
            .contains("UNBOUNDED_CARDINALITY")
    );
}

#[test]
fn declared_cardinality_must_match_the_expanded_collection() {
    let mut v = input();
    v["repositories"]["cardinality"] = json!(2);
    assert!(
        compile(&workflow(v))
            .unwrap_err()
            .to_string()
            .contains("CARDINALITY_MISMATCH")
    );
}

#[test]
fn disallowed_promotion_edge_is_rejected() {
    let mut v = input();
    v["repositories"]["items"][0]["allowed_edges"] = json!([]);
    assert!(
        compile(&workflow(v))
            .unwrap_err()
            .to_string()
            .contains("PROMOTION_NOT_ALLOWED")
    );
}

#[test]
fn history_preserves_receipts_and_confidence_instead_of_a_silent_estimate() {
    let g = compiled();
    let n = node(&g, "release/app/promotion/stage/check/linux");
    assert_eq!(
        (
            n.duration_hours,
            n.metadata["release"]["estimate"]["kind"].as_str(),
            n.metadata["release"]["estimate"]["samples"][0]["evidence"].as_str()
        ),
        (
            Some(0.4),
            Some("retained_run_history"),
            Some("retained/run-42.json")
        )
    );
}

#[test]
fn history_estimate_changes_make_the_plan_stale_without_policy_drift() {
    let mut v = input();
    v["history"][0]["duration_hours"] = json!(0.6);
    let d = diff(&workflow(v), &compiled()).unwrap();
    assert_eq!(
        (d.stale, d.policy_changed, d.changed),
        (
            true,
            false,
            vec!["release/app/promotion/stage/check/linux".to_string()]
        )
    );
}

#[test]
fn policy_drift_shows_the_new_check_and_stales_the_definition() {
    let mut v = input();
    let checks = &mut v["repositories"]["items"][0]["promotions"]["items"][0]["checks"];
    checks["cardinality"] = json!(3);
    checks["items"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"mac","workflow":"ci/mac"}));
    let d = diff(&workflow(v), &compiled()).unwrap();
    assert_eq!(
        (d.stale, d.policy_changed, d.added),
        (
            true,
            true,
            vec!["release/app/promotion/stage/check/mac".to_string()]
        )
    );
}

#[test]
fn unchanged_policy_and_history_have_no_drift() {
    assert!(!diff(&workflow(input()), &compiled()).unwrap().stale);
}

#[test]
fn caller_edits_in_a_generated_plan_are_detected() {
    let mut g = compiled();
    g.deliverables[0].metadata["owner"] = json!("agent");
    assert!(diff(&workflow(input()), &g).unwrap().stale);
}

#[tokio::test]
async fn owner_and_runner_nodes_cannot_be_leased_by_agents() {
    let planner = BasicCpmPlanner::new();
    let id = planner.submit_plan(compiled()).await.unwrap();
    let cohort = planner
        .acquire_cohort(cpm_planner::AcquireRequest::new(
            id,
            cpm_planner::plan::CallerId("worker".into()),
            100,
        ))
        .await
        .unwrap();
    assert!(
        cohort
            .rows
            .iter()
            .all(|r| r.deliverable.metadata["owner"] == "agent")
    );
}

#[tokio::test]
async fn mcp_compile_and_diff_do_not_submit_plans() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("plans.db");
    let audit = Arc::new(cpm_planner::audit::MemoryAuditSink::new());
    let planner = Arc::new(BasicCpmPlanner::with_store(
        cpm_planner::SqlitePlanStore::open(&db).unwrap(),
        audit.clone(),
    ));
    let server = PlanServer::new(planner.clone());
    let call = |name: &str, args: Value| {
        CallToolRequestParams::new(name.to_string())
            .with_arguments(args.as_object().unwrap().clone())
    };
    let result = server
        .dispatch_call(call("plan.release.compile", json!({"workflow":input()})))
        .await
        .unwrap();
    let result = server
        .dispatch_call(call(
            "plan.release.diff",
            json!({"workflow":input(),"graph":result["graph"]}),
        ))
        .await
        .unwrap();
    assert_eq!(result["stale"], false);
    assert_eq!(
        rusqlite::Connection::open(&db)
            .unwrap()
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM plans", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert!(audit.snapshot().is_empty());
}

#[test]
fn unknown_wire_keys_fail_closed() {
    let mut v = input();
    v["repositories"]["items"][0]["producers"]["items"][0]["attemptz"] = json!([]);
    assert!(serde_json::from_value::<ReleaseWorkflow>(v).is_err());
}

#[test]
fn unsupported_schema_versions_are_refused() {
    let mut v = input();
    v["schema_version"] = json!(2);
    assert!(
        compile(&workflow(v))
            .unwrap_err()
            .to_string()
            .contains("RELEASE_SCHEMA_VERSION")
    );
}

#[test]
fn retained_runs_use_observed_range_and_lower_median() {
    let mut v = input();
    v["history"] = json!([
        {"node_id":"release/app/promotion/stage/pr","duration_hours":1,"outcome":"succeeded","evidence":"run-1.json","confidence":0.8},
        {"node_id":"release/app/promotion/stage/pr","duration_hours":3,"outcome":"failed","evidence":"run-2.json","confidence":0.5},
        {"node_id":"release/app/promotion/stage/pr","duration_hours":2,"outcome":"cancelled","evidence":"run-3.json","confidence":0.6},
        {"node_id":"release/app/promotion/stage/pr","duration_hours":0,"outcome":"skipped","evidence":"run-4.json","confidence":1}
    ]);
    let g = compile(&workflow(v)).unwrap().graph;
    let n = node(&g, "release/app/promotion/stage/pr");
    assert_eq!(
        (
            n.estimate.unwrap(),
            n.metadata["release"]["estimate"]["confidence"].as_f64()
        ),
        (
            cpm_planner::Estimate {
                optimistic: 1.0,
                likely: 2.0,
                pessimistic: 3.0
            },
            Some(0.5)
        )
    );
}

#[test]
fn history_cannot_silently_attach_to_an_unknown_node() {
    let mut v = input();
    v["history"][0]["node_id"] = json!("missing");
    assert!(
        compile(&workflow(v))
            .unwrap_err()
            .to_string()
            .contains("UNKNOWN_HISTORY_NODE")
    );
}

#[test]
fn missing_estimates_are_not_replaced_with_implicit_defaults() {
    let mut v = input();
    v["estimates"].as_object_mut().unwrap().remove("check");
    assert!(
        compile(&workflow(v))
            .unwrap_err()
            .to_string()
            .contains("MISSING_RELEASE_ESTIMATE")
    );
}

#[test]
fn total_expansion_is_bounded_even_when_individual_collections_fit() {
    let mut v = input();
    let checks = &mut v["repositories"]["items"][0]["promotions"]["items"][0]["checks"];
    checks["items"] = json!(
        (0..5000)
            .map(|i| json!({"id":format!("c{i}"),"workflow":format!("ci/{i}")}))
            .collect::<Vec<_>>()
    );
    checks["cardinality"] = json!(5000);
    assert!(
        compile(&workflow(v))
            .unwrap_err()
            .to_string()
            .contains("TOO_MANY_DELIVERABLES")
    );
}

#[test]
fn central_acceptance_cannot_name_a_different_producer_promotion() {
    let mut v = input();
    v["acceptance"]["dependencies"]["items"][0]["promotion"] = json!("stage");
    assert!(
        compile(&workflow(v))
            .unwrap_err()
            .to_string()
            .contains("INVALID_ACCEPTANCE_DEPENDENCY")
    );
}

#[test]
fn every_leaf_must_feed_central_acceptance() {
    let mut v = input();
    v["repositories"]["items"][0]["producers"]["items"][0]["artifacts"]["items"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"unaccepted","path":"release/unaccepted.json"}));
    v["repositories"]["items"][0]["producers"]["items"][0]["artifacts"]["cardinality"] = json!(2);
    assert!(
        compile(&workflow(v))
            .unwrap_err()
            .to_string()
            .contains("FEEDS_NO_MILESTONE")
    );
}

#[test]
fn owner_runner_and_observation_waits_are_manual_work() {
    assert!(
        compiled()
            .deliverables
            .iter()
            .filter(|d| d.metadata["owner"] != "agent")
            .all(|d| d.metadata["kind"] == "manual")
    );
}

#[test]
fn independent_repositories_join_only_at_central_acceptance() {
    let mut v = input();
    let mut second = v["repositories"]["items"][0].clone();
    second["id"] = json!("library");
    second["repository"] = json!("example/library");
    v["repositories"]["items"]
        .as_array_mut()
        .unwrap()
        .push(second);
    v["repositories"]["cardinality"] = json!(2);
    let mut dep = v["acceptance"]["dependencies"]["items"][0].clone();
    dep["repository"] = json!("library");
    v["acceptance"]["dependencies"]["items"]
        .as_array_mut()
        .unwrap()
        .push(dep);
    v["acceptance"]["dependencies"]["cardinality"] = json!(2);
    let g = compile(&workflow(v)).unwrap().graph;
    assert_eq!(
        node(&g, "release/acceptance/production")
            .prerequisites
            .iter()
            .map(|p| p.id())
            .collect::<Vec<_>>(),
        [
            "release/app/producer/proof/artifact/receipt",
            "release/library/producer/proof/artifact/receipt"
        ]
    );
}
