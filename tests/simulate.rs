//! Tests for the read-only plan simulation library.

use cpm_planner::monte_carlo::MonteCarloRequest;
use cpm_planner::plan::{PlanGraph, PlannerError};
use cpm_planner::planner::BasicCpmPlanner;
use cpm_planner::ports::Planner;
use cpm_planner::resource_schedule::ScheduleRequest;
use cpm_planner::simulate::{SimulateRequest, simulate};
use serde_json::json;
use std::collections::BTreeMap;

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
    assert_eq!(
        err.to_string(),
        "INVALID_GRAPH: lint errors: CYCLE [a, b, a]: prerequisite cycle: a -> b -> a"
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

#[test]
fn simulate_rejects_oversized_graph() {
    let v: Vec<serde_json::Value> = (0..5001)
        .map(|i| dv(&format!("d{i}"), &[], 1.0, None))
        .collect();
    let err = simulate(&graph(v), &SimulateRequest::default()).expect_err("too large");
    assert_eq!(
        err.to_string(),
        "INVALID_GRAPH: lint errors: TOO_MANY_DELIVERABLES []: plan has 5001 deliverables; maximum is 5000"
    );
}
