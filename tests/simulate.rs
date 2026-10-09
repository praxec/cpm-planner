//! Tests for the read-only plan simulation library.

use cpm_planner::audit::NullAuditSink;
use cpm_planner::monte_carlo::MonteCarloRequest;
use cpm_planner::plan::{PlanGraph, PlannerError};
use cpm_planner::plan_store::SqlitePlanStore;
use cpm_planner::planner::BasicCpmPlanner;
use cpm_planner::ports::Planner;
use cpm_planner::resource_schedule::ScheduleRequest;
use cpm_planner::simulate::{SimulateRequest, simulate};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

fn dv(id: &str, prereqs: &[&str], h: f32, resource: Option<&str>) -> serde_json::Value {
    let mut v = json!({
        "id": id,
        "owned_files": [],
        "prerequisites": prereqs,
        "estimated_effort_hours": h,
    });
    if let Some(r) = resource {
        v["metadata"] = json!({"resource": r});
    }
    v
}

fn graph(v: Vec<serde_json::Value>) -> PlanGraph {
    serde_json::from_value(json!({ "deliverables": v })).expect("valid graph")
}

fn chain() -> PlanGraph {
    graph(vec![
        dv("a", &[], 2.0, None),
        dv("b", &["a"], 3.0, None),
        dv("c", &["b"], 1.0, None),
    ])
}

fn parallel_same_resource() -> PlanGraph {
    graph(vec![
        dv("a", &[], 2.0, Some("dev")),
        dv("b", &[], 2.0, Some("dev")),
    ])
}

fn plan_count(path: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(path).expect("open db");
    conn.query_row("SELECT COUNT(*) FROM plans", [], |r| r.get(0))
        .expect("count")
}

#[tokio::test]
async fn simulate_does_not_create_a_plan() {
    let path = std::env::temp_dir().join(format!("cpm-simulate-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = SqlitePlanStore::open(&path).expect("open store");
    let planner = BasicCpmPlanner::with_store(store, Arc::new(NullAuditSink));
    let g = chain();
    simulate(&g, &SimulateRequest::default()).expect("simulate");
    let _ = planner.submit_plan(g.clone()).await.expect("submit");
    let after_submit = plan_count(&path);
    simulate(&g, &SimulateRequest::default()).expect("simulate");
    let after_simulate = plan_count(&path);
    let _ = std::fs::remove_file(&path);
    assert_eq!((after_submit, after_simulate), (1, 1));
}

#[tokio::test]
async fn simulate_leaves_a_fresh_store_empty() {
    let path = std::env::temp_dir().join(format!("cpm-simulate-empty-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = SqlitePlanStore::open(&path).expect("open store");
    let planner = BasicCpmPlanner::with_store(store, Arc::new(NullAuditSink));
    drop(planner);
    simulate(&chain(), &SimulateRequest::default()).expect("simulate");
    let count = plan_count(&path);
    let _ = std::fs::remove_file(&path);
    assert_eq!(count, 0);
}

#[tokio::test]
async fn simulate_matches_submit_critical_path() {
    let planner = BasicCpmPlanner::new();
    let g = chain();
    let id = planner.submit_plan(g.clone()).await.expect("submit");
    let status = planner.status(&id).await.expect("status");
    let sim = simulate(&g, &SimulateRequest::default()).expect("simulate");
    assert_eq!(sim.critical_path, status.critical_path);
}

#[tokio::test]
async fn simulate_rows_match_status_schedule() {
    let planner = BasicCpmPlanner::new();
    let g = chain();
    let id = planner.submit_plan(g.clone()).await.expect("submit");
    let status = planner.status(&id).await.expect("status");
    let sim = simulate(&g, &SimulateRequest::default()).expect("simulate");
    assert_eq!(sim.schedule, status.schedule);
}

#[test]
fn simulate_reports_critical_path_hours() {
    let sim = simulate(&chain(), &SimulateRequest::default()).expect("simulate");
    assert!((sim.critical_path_hours - 6.0).abs() < 1e-4);
}

#[test]
fn simulate_with_capacities_reports_resource_makespan() {
    let req = SimulateRequest {
        schedule: Some(ScheduleRequest {
            capacities: BTreeMap::from([("dev".to_string(), 1)]),
            resource_key: "resource".to_string(),
            project_buffer_pct: 0.0,
        }),
        monte_carlo: None,
    };
    let sim = simulate(&parallel_same_resource(), &req).expect("simulate");
    assert_eq!(sim.scorecard.resource_makespan, Some(4.0));
}

#[test]
fn simulate_without_schedule_omits_resource_schedule() {
    let sim = simulate(&chain(), &SimulateRequest::default()).expect("simulate");
    assert!(sim.resource_schedule.is_none());
}

#[test]
fn simulate_with_monte_carlo_fills_scorecard_summary() {
    let req = SimulateRequest {
        schedule: None,
        monte_carlo: Some(MonteCarloRequest {
            iterations: 50,
            seed: 7,
        }),
    };
    let sim = simulate(&chain(), &req).expect("simulate");
    assert!(sim.scorecard.monte_carlo.is_some());
}

#[test]
fn simulate_without_monte_carlo_leaves_summary_empty() {
    let sim = simulate(&chain(), &SimulateRequest::default()).expect("simulate");
    assert!(sim.scorecard.monte_carlo.is_none());
}

#[test]
fn simulate_rejects_cyclic_graph_with_invalid_graph() {
    let g = graph(vec![dv("a", &["b"], 1.0, None), dv("b", &["a"], 1.0, None)]);
    let err = simulate(&g, &SimulateRequest::default()).expect_err("cycle");
    assert!(matches!(&err, PlannerError::InvalidGraph { reason } if reason.contains("CYCLE")));
}

#[test]
fn simulate_error_reason_lists_offending_ids() {
    let g = graph(vec![dv("a", &["b"], 1.0, None), dv("b", &["a"], 1.0, None)]);
    let err = simulate(&g, &SimulateRequest::default()).expect_err("cycle");
    assert!(
        matches!(&err, PlannerError::InvalidGraph { reason } if reason.contains('a') && reason.contains('b'))
    );
}

#[test]
fn simulate_is_deterministic() {
    let req = SimulateRequest {
        schedule: None,
        monte_carlo: Some(MonteCarloRequest {
            iterations: 100,
            seed: 3,
        }),
    };
    let first = simulate(&chain(), &req).expect("first");
    let second = simulate(&chain(), &req).expect("second");
    assert_eq!(first, second);
}

#[test]
fn simulate_request_rejects_unknown_fields() {
    let parsed: Result<SimulateRequest, _> = serde_json::from_value(json!({"bogus": 1}));
    assert!(parsed.is_err());
}

#[tokio::test]
async fn simulate_stored_plan_by_id() {
    let planner = BasicCpmPlanner::new();
    let id = planner.submit_plan(chain()).await.expect("submit");
    let sim = planner
        .simulate_plan(&id, &SimulateRequest::default())
        .await
        .expect("simulate_plan");
    assert_eq!(sim.critical_path[1..4], ["a", "b", "c"]);
}

#[tokio::test]
async fn simulate_unknown_plan_is_plan_not_found() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .simulate_plan(
            &cpm_planner::plan::PlanId("nope".to_string()),
            &SimulateRequest::default(),
        )
        .await
        .expect_err("missing");
    assert!(matches!(err, PlannerError::PlanNotFound { .. }));
}
