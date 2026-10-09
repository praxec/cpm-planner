//! Tests for the resource-constrained schedule library.

use cpm_planner::plan::{Deliverable, PlanGraph, PlannerError, Prerequisite};
use cpm_planner::resource_schedule::{
    ResourceSchedule, ScheduleRequest, WaitKind, resource_schedule,
};
use std::collections::BTreeMap;

fn d(id: &str, hours: f32, owner: &str, prereqs: &[&str]) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: vec![format!("{id}.rs").as_str().into()],
        prerequisites: prereqs.iter().map(|p| Prerequisite::from(*p)).collect(),
        estimated_effort_hours: None,
        duration_hours: Some(hours),
        estimate: None,
        metadata: serde_json::json!({ "owner": owner }),
        milestone: false,
    }
}

fn graph(deliverables: Vec<Deliverable>) -> PlanGraph {
    PlanGraph {
        deliverables,
        max_chained_dispatch: None,
    }
}

fn req(caps: &[(&str, u32)]) -> ScheduleRequest {
    ScheduleRequest {
        capacities: caps
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect::<BTreeMap<_, _>>(),
        resource_key: "owner".to_string(),
        project_buffer_pct: 25.0,
    }
}

fn run(g: &PlanGraph, r: &ScheduleRequest) -> ResourceSchedule {
    resource_schedule(g, r).expect("schedule")
}

fn row_start(s: &ResourceSchedule, id: &str) -> f32 {
    s.rows.iter().find(|r| r.id == id).expect("row").start
}

#[test]
fn unlimited_capacity_matches_cpm_makespan() {
    let g = graph(vec![
        d("a", 2.0, "x", &[]),
        d("b", 3.0, "x", &[]),
        d("c", 1.0, "x", &["a", "b"]),
    ]);
    let s = run(&g, &req(&[("x", 3)]));
    assert_eq!(s.makespan, s.cpm_makespan);
}

#[test]
fn single_worker_serialises_everything() {
    let g = graph(vec![
        d("a", 2.0, "x", &[]),
        d("b", 3.0, "x", &[]),
        d("c", 1.0, "x", &[]),
    ]);
    assert_eq!(run(&g, &req(&[("x", 1)])).makespan, 6.0);
}

#[test]
fn two_workers_split_independent_work() {
    let g = graph(vec![
        d("a", 2.0, "x", &[]),
        d("b", 2.0, "x", &[]),
        d("c", 2.0, "x", &[]),
        d("e", 2.0, "x", &[]),
    ]);
    assert_eq!(run(&g, &req(&[("x", 2)])).makespan, 4.0);
}

#[test]
fn priority_is_longest_remaining_tail() {
    // "short" sorts before "long" by id, but "long" heads the longer chain.
    let g = graph(vec![
        d("short", 1.0, "x", &[]),
        d("long", 1.0, "x", &[]),
        d("after", 5.0, "y", &["long"]),
    ]);
    let s = run(&g, &req(&[("x", 1), ("y", 1)]));
    assert_eq!(row_start(&s, "long"), 0.0);
}

#[test]
fn lag_delays_ready_time() {
    let mut b = d("b", 1.0, "x", &[]);
    b.prerequisites = vec![Prerequisite::Edge {
        id: "a".to_string(),
        consumes: None,
        kind: None,
        lag_hours: Some(3.0),
    }];
    let g = graph(vec![d("a", 2.0, "x", &[]), b]);
    let s = run(&g, &req(&[("x", 2)]));
    assert_eq!(row_start(&s, "b"), 5.0);
}

#[test]
fn missing_capacity_is_invalid_capacities() {
    let g = graph(vec![
        d("a", 1.0, "zeta", &[]),
        d("b", 1.0, "alpha", &[]),
        d("c", 1.0, "ok", &[]),
    ]);
    let err = resource_schedule(&g, &req(&[("ok", 1)])).expect_err("must fail");
    assert_eq!(
        err.to_string(),
        "INVALID_CAPACITIES: no capacity for resources [alpha, zeta]"
    );
}

#[test]
fn zero_capacity_is_invalid_capacities() {
    let g = graph(vec![d("a", 1.0, "x", &[])]);
    let err = resource_schedule(&g, &req(&[("x", 0)])).expect_err("must fail");
    assert!(matches!(err, PlannerError::InvalidCapacities { .. }));
}

#[test]
fn driving_chain_marks_resource_waits() {
    let g = graph(vec![d("a", 2.0, "x", &[]), d("b", 2.0, "x", &[])]);
    let s = run(&g, &req(&[("x", 1)]));
    assert_eq!(s.driving_chain[1].waited_on, WaitKind::Resource);
}

#[test]
fn driving_chain_marks_dependency_waits() {
    let g = graph(vec![d("a", 2.0, "x", &[]), d("b", 2.0, "y", &["a"])]);
    let s = run(&g, &req(&[("x", 1), ("y", 1)]));
    assert_eq!(s.driving_chain[1].waited_on, WaitKind::Dependency);
}

#[test]
fn driving_chain_starts_with_start() {
    let g = graph(vec![d("a", 2.0, "x", &[]), d("b", 2.0, "x", &["a"])]);
    let s = run(&g, &req(&[("x", 1)]));
    assert_eq!(s.driving_chain[0].waited_on, WaitKind::Start);
}

#[test]
fn project_buffer_is_pct_of_driving_chain() {
    let g = graph(vec![d("a", 2.0, "x", &[]), d("b", 6.0, "x", &["a"])]);
    let s = run(&g, &req(&[("x", 1)]));
    assert_eq!(s.project_buffer_hours, 2.0);
}

#[test]
fn feeding_buffer_reported_where_side_chain_joins() {
    let g = graph(vec![
        d("main", 8.0, "x", &[]),
        d("side1", 2.0, "y", &[]),
        d("side2", 2.0, "y", &["side1"]),
        d("join", 1.0, "x", &["main", "side2"]),
    ]);
    let s = run(&g, &req(&[("x", 1), ("y", 1)]));
    let fb = &s.feeding_buffers;
    assert_eq!(
        (
            fb[0].joins.as_str(),
            fb[0].from_chain.clone(),
            fb[0].buffer_hours
        ),
        ("join", vec!["side1".to_string(), "side2".to_string()], 1.0)
    );
}

#[test]
fn milestones_consume_no_resource_time() {
    let mut m = d("m", 0.0, "x", &["a"]);
    m.milestone = true;
    let g = graph(vec![d("a", 2.0, "x", &[]), m, d("b", 2.0, "x", &[])]);
    let s = run(&g, &req(&[("x", 1)]));
    assert_eq!(s.makespan, 4.0);
}

#[test]
fn rows_exclude_synthetic_endpoints() {
    let g = graph(vec![d("a", 1.0, "x", &[]), d("b", 1.0, "x", &["a"])]);
    let ids: Vec<_> = run(&g, &req(&[("x", 1)]))
        .rows
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec!["a", "b"]);
}

#[test]
fn rows_default_unowned_work_to_unassigned() {
    let mut a = d("a", 1.0, "x", &[]);
    a.metadata = serde_json::Value::Null;
    let s = run(&graph(vec![a]), &req(&[("unassigned", 1)]));
    assert_eq!(s.rows[0].resource, "unassigned");
}

#[test]
fn load_reports_utilisation_per_resource() {
    let g = graph(vec![d("a", 2.0, "x", &[]), d("b", 2.0, "y", &["a"])]);
    let s = run(&g, &req(&[("x", 1), ("y", 1)]));
    assert_eq!(s.load[0].utilisation, 0.5);
}

#[test]
fn huge_capacity_does_not_allocate_per_unit() {
    let g = graph(vec![
        d("a", 2.0, "agent", &[]),
        d("b", 3.0, "agent", &[]),
        d("c", 1.0, "agent", &[]),
    ]);
    let s = run(&g, &req(&[("agent", u32::MAX)]));
    assert_eq!(s.makespan, s.cpm_makespan);
}

#[test]
fn negative_project_buffer_pct_is_rejected() {
    let g = graph(vec![d("a", 1.0, "x", &[])]);
    let mut r = req(&[("x", 1)]);
    r.project_buffer_pct = -1.0;
    let err = resource_schedule(&g, &r).expect_err("must fail");
    assert!(err.to_string().starts_with("INVALID_GRAPH"));
}

#[test]
fn output_is_deterministic() {
    let g = graph(vec![
        d("a", 2.0, "x", &[]),
        d("b", 2.0, "x", &[]),
        d("c", 2.0, "x", &["a"]),
        d("e", 1.0, "y", &["b"]),
    ]);
    let r = req(&[("x", 1), ("y", 1)]);
    let first = serde_json::to_string(&run(&g, &r)).expect("json");
    let second = serde_json::to_string(&run(&g, &r)).expect("json");
    assert_eq!(first, second);
}
