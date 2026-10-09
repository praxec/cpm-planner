//! Tests for plan variant comparison.

use cpm_planner::compare::{CompareRequest, CompareWeights, compare};
use cpm_planner::monte_carlo::MonteCarloRequest;
use cpm_planner::plan::{PlanGraph, PlanId, PlannerError};
use serde_json::json;

fn graph(items: &[(&str, &[&str], f32)]) -> PlanGraph {
    let v: Vec<_> = items
        .iter()
        .map(|(id, pre, h)| {
            json!({"id": id, "owned_files": [], "prerequisites": pre, "estimated_effort_hours": h})
        })
        .collect();
    serde_json::from_value(json!({ "deliverables": v })).expect("valid graph")
}

fn input(id: &str, g: PlanGraph) -> (PlanId, String, PlanGraph) {
    (PlanId(id.to_string()), format!("variant-{id}"), g)
}

/// makespan 10, effort 20.
fn parallel() -> PlanGraph {
    graph(&[("x", &[], 10.0), ("y", &[], 10.0)])
}
/// makespan 20, effort 20 (dominated by `parallel`).
fn chain() -> PlanGraph {
    graph(&[("x", &[], 10.0), ("y", &["x"], 10.0)])
}
/// makespan 15, effort 15.
fn single() -> PlanGraph {
    graph(&[("x", &[], 15.0)])
}

fn weights(makespan: f32, p80: f32, risk: f32, effort: f32, peak: f32) -> CompareWeights {
    CompareWeights {
        makespan,
        p80,
        criticality_risk: risk,
        total_effort: effort,
        peak_load: peak,
    }
}

fn req(weights: CompareWeights) -> CompareRequest {
    CompareRequest {
        schedule: None,
        monte_carlo: None,
        weights,
    }
}

fn three() -> Vec<(PlanId, String, PlanGraph)> {
    vec![
        input("b-chain", chain()),
        input("a-parallel", parallel()),
        input("c-single", single()),
    ]
}

fn by_id<'a>(
    c: &'a cpm_planner::compare::Comparison,
    id: &str,
) -> &'a cpm_planner::compare::VariantComparison {
    c.variants
        .iter()
        .find(|v| v.plan_id.0 == id)
        .expect("variant present")
}

#[test]
fn dominated_variant_is_not_pareto_optimal() {
    let c = compare(&three(), &CompareRequest::default()).expect("compare");
    assert!(!by_id(&c, "b-chain").pareto_optimal);
}

#[test]
fn trade_off_variants_are_both_pareto_optimal() {
    let c = compare(&three(), &CompareRequest::default()).expect("compare");
    assert!(by_id(&c, "a-parallel").pareto_optimal && by_id(&c, "c-single").pareto_optimal);
}

#[test]
fn rank_orders_by_weighted_score() {
    let c = compare(&three(), &CompareRequest::default()).expect("compare");
    let mut ranked: Vec<_> = c.variants.iter().collect();
    ranked.sort_by_key(|v| v.rank);
    let ids: Vec<_> = ranked.iter().map(|v| v.plan_id.0.as_str()).collect();
    assert_eq!(ids, ["a-parallel", "c-single", "b-chain"]);
}

#[test]
fn recommended_is_the_rank_one_variant() {
    let c = compare(&three(), &CompareRequest::default()).expect("compare");
    assert_eq!(c.recommended.0, "a-parallel");
}

#[test]
fn weights_change_ranking() {
    let r = req(weights(0.0, 0.0, 0.0, 100.0, 0.0));
    let c = compare(&three(), &r).expect("compare");
    assert_eq!(c.recommended.0, "c-single");
}

#[test]
fn zero_min_criterion_contributes_nothing() {
    // No capacities, so peak_load is 0 for every variant (min 0).
    let low = compare(&three(), &req(weights(1.0, 1.0, 1.0, 0.5, 0.0))).expect("compare");
    let high = compare(&three(), &req(weights(1.0, 1.0, 1.0, 0.5, 1000.0))).expect("compare");
    assert_eq!(
        by_id(&low, "a-parallel").score,
        by_id(&high, "a-parallel").score
    );
}

#[test]
fn p80_falls_back_to_deterministic_without_monte_carlo() {
    // Only the p80 weight is non-zero: score is makespan / min makespan.
    let c = compare(&three(), &req(weights(0.0, 1.0, 0.0, 0.0, 0.0))).expect("compare");
    assert!((by_id(&c, "c-single").score - 1.5).abs() < 1e-5);
}

#[test]
fn monte_carlo_request_fills_the_scorecard_summary() {
    let r = CompareRequest {
        monte_carlo: Some(MonteCarloRequest {
            iterations: 20,
            seed: 7,
        }),
        ..CompareRequest::default()
    };
    let c = compare(&three(), &r).expect("compare");
    assert!(by_id(&c, "a-parallel").scorecard.monte_carlo.is_some());
}

#[test]
fn scorecard_has_no_monte_carlo_when_not_requested() {
    let c = compare(&three(), &CompareRequest::default()).expect("compare");
    assert!(by_id(&c, "a-parallel").scorecard.monte_carlo.is_none());
}

#[test]
fn diff_vs_first_lists_structural_changes() {
    let base = graph(&[("a", &[], 5.0), ("b", &["a"], 5.0), ("keep", &[], 1.0)]);
    let other = graph(&[("a", &[], 9.0), ("c", &["a"], 5.0), ("keep", &[], 1.0)]);
    let c = compare(
        &[input("base", base), input("other", other)],
        &CompareRequest::default(),
    )
    .expect("compare");
    let d = &by_id(&c, "other").diff_vs_first;
    assert_eq!(
        (&d.added, &d.removed, &d.changed),
        (
            &vec!["c".to_string()],
            &vec!["b".to_string()],
            &vec!["a".to_string()]
        )
    );
}

#[test]
fn first_variant_has_an_empty_diff() {
    let c = compare(&three(), &CompareRequest::default()).expect("compare");
    let d = &by_id(&c, "b-chain").diff_vs_first;
    assert!(d.added.is_empty() && d.removed.is_empty() && d.changed.is_empty());
}

#[test]
fn compare_diff_leaves_runtime_fields_empty() {
    let base = graph(&[("a", &[], 5.0), ("b", &["a"], 5.0)]);
    let other = graph(&[("a", &[], 9.0)]);
    let c = compare(
        &[input("base", base), input("other", other)],
        &CompareRequest::default(),
    )
    .expect("compare");
    let d = &by_id(&c, "other").diff_vs_first;
    assert!(d.reopened.is_empty() && d.released_locks.is_empty());
}

#[test]
fn compare_requires_two_variants() {
    let err = compare(&[input("only", parallel())], &CompareRequest::default()).expect_err("err");
    assert!(
        matches!(err, PlannerError::InvalidGraph { reason } if reason == "compare needs at least two variants")
    );
}

#[test]
fn lint_errors_name_the_offending_variant() {
    let cyclic = graph(&[("p", &["q"], 1.0), ("q", &["p"], 1.0)]);
    let inputs = vec![input("ok", parallel()), input("bad", cyclic)];
    let err = compare(&inputs, &CompareRequest::default()).expect_err("err");
    assert!(matches!(err, PlannerError::InvalidGraph { reason } if reason.contains("variant-bad")));
}

#[test]
fn rationale_names_the_best_criteria() {
    let c = compare(&three(), &CompareRequest::default()).expect("compare");
    let r = &by_id(&c, "c-single").rationale;
    assert!(r.contains("best: total_effort"), "{r}");
}

#[test]
fn rationale_names_the_worst_criteria() {
    let c = compare(&three(), &CompareRequest::default()).expect("compare");
    let r = &by_id(&c, "b-chain").rationale;
    assert!(r.contains("worst: makespan"), "{r}");
}

#[test]
fn compare_is_deterministic() {
    let r = CompareRequest {
        monte_carlo: Some(MonteCarloRequest {
            iterations: 20,
            seed: 3,
        }),
        ..CompareRequest::default()
    };
    assert_eq!(
        compare(&three(), &r).expect("first"),
        compare(&three(), &r).expect("second")
    );
}
