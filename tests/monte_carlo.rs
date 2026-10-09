use cpm_planner::monte_carlo::{MonteCarloRequest, monte_carlo};
use cpm_planner::plan::{PlanGraph, PlannerError};
use cpm_planner::{CpmAlgorithm, Task};
use serde_json::json;

fn graph(v: serde_json::Value) -> PlanGraph {
    serde_json::from_value(json!({ "deliverables": v })).expect("valid graph json")
}

fn est(id: &str, prereqs: &[&str], o: f32, m: f32, p: f32) -> serde_json::Value {
    json!({"id": id, "owned_files": [], "prerequisites": prereqs,
           "estimate": {"optimistic": o, "likely": m, "pessimistic": p}})
}

fn fixed(id: &str, prereqs: &[&str], hours: f32) -> serde_json::Value {
    json!({"id": id, "owned_files": [], "prerequisites": prereqs, "estimated_effort_hours": hours})
}

fn req(iterations: u32, seed: u64) -> MonteCarloRequest {
    MonteCarloRequest { iterations, seed }
}

#[test]
fn no_estimates_collapses_percentiles_to_cpm_makespan() {
    let g = graph(json!([fixed("a", &[], 3.0), fixed("b", &["a"], 4.0)]));
    let s = monte_carlo(&g, &req(50, 1)).unwrap();
    assert_eq!(
        (s.p50, s.p80, s.p95, s.mean, s.deterministic_makespan),
        (7.0, 7.0, 7.0, 7.0, 7.0)
    );
}

#[test]
fn same_seed_reproduces_output() {
    let g = graph(json!([
        est("a", &[], 2.0, 5.0, 14.0),
        fixed("b", &["a"], 1.0)
    ]));
    let r = req(300, 42);
    assert_eq!(monte_carlo(&g, &r).unwrap(), monte_carlo(&g, &r).unwrap());
}

#[test]
fn different_seed_changes_samples() {
    let g = graph(json!([est("a", &[], 2.0, 5.0, 14.0)]));
    let a = monte_carlo(&g, &req(300, 1)).unwrap();
    let b = monte_carlo(&g, &req(300, 2)).unwrap();
    assert_ne!(a.mean, b.mean);
}

#[test]
fn merge_bias_raises_p80_over_single_chain() {
    let mut branches: Vec<_> = (0..5)
        .map(|i| est(&format!("b{i}"), &[], 5.0, 10.0, 20.0))
        .collect();
    branches.push(fixed("release", &["b0", "b1", "b2", "b3", "b4"], 1.0));
    let parallel = graph(json!(branches));
    let chain = graph(json!([
        est("only", &[], 5.0, 10.0, 20.0),
        fixed("release", &["only"], 1.0)
    ]));
    let p = monte_carlo(&parallel, &req(2000, 7)).unwrap();
    let c = monte_carlo(&chain, &req(2000, 7)).unwrap();
    assert!(p.p80 > c.p80, "parallel {} vs chain {}", p.p80, c.p80);
}

#[test]
fn criticality_index_of_sole_chain_is_one() {
    let g = graph(json!([
        est("a", &[], 1.0, 2.0, 4.0),
        est("b", &["a"], 1.0, 2.0, 4.0)
    ]));
    let s = monte_carlo(&g, &req(100, 3)).unwrap();
    let got: Vec<(&str, f32)> = s
        .criticality
        .iter()
        .map(|c| (c.id.as_str(), c.index))
        .collect();
    assert_eq!(got, vec![("a", 1.0), ("b", 1.0)]);
}

#[test]
fn sensitivity_ranks_widest_estimate_first() {
    let g = graph(json!([
        est("narrow", &[], 9.0, 10.0, 11.0),
        est("wide", &["narrow"], 2.0, 10.0, 30.0),
        fixed("fixed", &["wide"], 1.0)
    ]));
    let s = monte_carlo(&g, &req(1000, 5)).unwrap();
    let ids: Vec<&str> = s.sensitivity.iter().map(|x| x.id.as_str()).collect();
    assert_eq!(ids, vec!["wide", "narrow"]);
}

#[test]
fn iterations_out_of_range_is_rejected() {
    let g = graph(json!([fixed("a", &[], 1.0)]));
    let zero = monte_carlo(&g, &req(0, 1)).unwrap_err();
    let big = monte_carlo(&g, &req(50_001, 1)).unwrap_err();
    assert!(matches!(
        (zero, big),
        (
            PlannerError::InvalidGraph { .. },
            PlannerError::InvalidGraph { .. }
        )
    ));
}

#[test]
fn request_defaults_are_2000_and_0xc0ffee() {
    let r: MonteCarloRequest = serde_json::from_value(json!({})).unwrap();
    assert_eq!((r.iterations, r.seed), (2000, 0xC0FFEE));
}

#[test]
fn duration_hours_overrides_estimate_and_is_not_sampled() {
    let g = graph(
        json!([{"id": "a", "owned_files": [], "prerequisites": [], "duration_hours": 6.0,
        "estimate": {"optimistic": 1.0, "likely": 2.0, "pessimistic": 50.0}}]),
    );
    let s = monte_carlo(&g, &req(50, 1)).unwrap();
    assert_eq!((s.p95, s.sensitivity.len()), (6.0, 0));
}

#[test]
fn deterministic_graph_with_lag_and_parallel_sinks_matches_cpm() {
    let g = graph(json!([
        fixed("a", &[], 3.0),
        json!({"id": "b", "owned_files": [], "estimated_effort_hours": 2.0,
               "prerequisites": [{"id": "a", "lag_hours": 4.0}]}),
        fixed("c", &["a"], 5.0),
    ]));
    let s = monte_carlo(&g, &req(20, 1)).unwrap();
    // b finishes at 3 + 4 + 2 = 9, c at 8: two sinks, lag decides the makespan.
    let mut b = Task {
        id: "b".into(),
        effort_hours: 2.0,
        dependencies: vec!["a".into()],
        ..Task::default()
    };
    b.lag_by_dependency.insert("a".into(), 4.0);
    let mut tasks = vec![
        Task {
            id: "a".into(),
            effort_hours: 3.0,
            ..Task::default()
        },
        b,
        Task {
            id: "c".into(),
            effort_hours: 5.0,
            dependencies: vec!["a".into()],
            ..Task::default()
        },
    ];
    let cpm = CpmAlgorithm::calculate(&mut tasks).critical_path_duration;
    assert_eq!((s.p50, s.deterministic_makespan, cpm), (9.0, 9.0, 9.0));
}

#[test]
fn explicit_effort_suppresses_sampling() {
    let g = graph(
        json!([{"id": "a", "owned_files": [], "prerequisites": [], "estimated_effort_hours": 4.0,
        "estimate": {"optimistic": 1.0, "likely": 2.0, "pessimistic": 50.0}}]),
    );
    let s = monte_carlo(&g, &req(50, 1)).unwrap();
    assert_eq!((s.p95, s.sensitivity.len()), (4.0, 0));
}

#[test]
fn summary_echoes_iterations_and_seed() {
    let g = graph(json!([est("a", &[], 1.0, 2.0, 3.0)]));
    let s = monte_carlo(&g, &req(37, 991)).unwrap();
    assert_eq!((s.iterations, s.seed), (37, 991));
}

#[test]
fn work_budget_is_enforced() {
    // 2001 deliverables + 2000 edges = 4001 nodes+edges; × 50 000 > 200 000 000.
    let mut v = vec![fixed("d0", &[], 1.0)];
    for i in 1..2001 {
        let prev = format!("d{}", i - 1);
        v.push(fixed(&format!("d{i}"), &[prev.as_str()], 1.0));
    }
    let err = monte_carlo(&graph(json!(v)), &req(50_000, 1)).unwrap_err();
    assert_eq!(
        err.to_string(),
        "INVALID_GRAPH: monte carlo budget exceeded (50000 iterations × 4001 nodes+edges > 200000000)"
    );
}

#[test]
fn cyclic_graph_is_invalid_graph() {
    let g = graph(json!([fixed("a", &["b"], 1.0), fixed("b", &["a"], 1.0)]));
    let err = monte_carlo(&g, &req(10, 1)).unwrap_err();
    assert!(
        err.to_string().starts_with("INVALID_GRAPH: cycle detected"),
        "{err}"
    );
}

#[test]
fn request_rejects_unknown_fields() {
    let parsed: Result<MonteCarloRequest, _> = serde_json::from_value(json!({"iteratons": 10}));
    assert!(parsed.is_err());
}
