//! Tests for the plan scorecard library.

use cpm_planner::lint::{LintFinding, LintReport, Severity};
use cpm_planner::metrics::{Scorecard, scorecard};
use cpm_planner::plan::PlanGraph;
use cpm_planner::resource_schedule::{ScheduleRequest, resource_schedule};
use cpm_planner::schedule::compute_cpm;
use cpm_planner::task::CriticalPathResult;
use serde_json::json;
use std::collections::BTreeMap;

fn dv(id: &str, prereqs: &[&str], extra: serde_json::Value) -> serde_json::Value {
    let mut v = json!({"id": id, "owned_files": [], "prerequisites": prereqs});
    for (k, val) in extra.as_object().expect("object") {
        v[k] = val.clone();
    }
    v
}

fn hours(id: &str, prereqs: &[&str], h: f32) -> serde_json::Value {
    dv(id, prereqs, json!({"estimated_effort_hours": h}))
}

fn graph(v: Vec<serde_json::Value>) -> PlanGraph {
    serde_json::from_value(json!({ "deliverables": v })).expect("valid graph")
}

/// CPM over the graph exactly as the planner computes it.
fn cpm(g: &PlanGraph) -> CriticalPathResult {
    compute_cpm(g).expect("valid graph")
}

fn clean() -> LintReport {
    LintReport {
        clean: true,
        findings: vec![],
    }
}

fn card(g: &PlanGraph) -> Scorecard {
    scorecard(g, &cpm(g), None, &clean())
}

fn finding(severity: Severity) -> LintFinding {
    LintFinding {
        code: "X".to_string(),
        severity,
        message: String::new(),
        ids: vec![],
        path: None,
    }
}

#[test]
fn scorecard_excludes_synthetic_endpoints_from_counts() {
    // a -> b is critical; c has 1h float. The zero-float endpoints would add
    // 2 to critical_count if they were counted.
    let g = graph(vec![
        hours("a", &[], 1.0),
        hours("b", &["a"], 1.0),
        hours("c", &[], 1.0),
    ]);
    let s = card(&g);
    assert_eq!(
        (s.deliverables, s.critical_count, s.total_float),
        (3, 2, 1.0)
    );
}

#[test]
fn scorecard_with_schedule_reports_resource_makespan_and_peak_load() {
    let owned = |id: &str| {
        dv(
            id,
            &[],
            json!({"estimated_effort_hours": 2.0, "metadata": {"owner": "dev"}}),
        )
    };
    let g = graph(vec![owned("a"), owned("b")]);
    let leveled = resource_schedule(
        &g,
        &ScheduleRequest {
            capacities: BTreeMap::from([("dev".to_string(), 1)]),
            resource_key: "owner".to_string(),
            project_buffer_pct: 0.0,
        },
    )
    .expect("schedule");
    let s = scorecard(&g, &cpm(&g), Some(&leveled), &clean());
    assert_eq!((s.resource_makespan, s.peak_load), (Some(4.0), Some(1.0)));
}

#[test]
fn empty_graph_has_zero_criticality_risk() {
    let g = graph(vec![]);
    let s = card(&g);
    assert_eq!(s.criticality_risk, 0.0);
}

#[test]
fn empty_graph_risk_band_is_not_applicable() {
    let g = graph(vec![]);
    let s = card(&g);
    assert_eq!(s.risk_band.as_str(), "not_applicable");
}

#[test]
fn merge_bias_counts_distinct_prerequisites() {
    let g = graph(vec![
        hours("a", &[], 1.0),
        hours("b", &[], 1.0),
        hours("m", &["a", "a", "b"], 1.0),
    ]);
    assert_eq!(card(&g).merge_bias_count, 0);
}

#[test]
fn cyclomatic_complexity_counts_distinct_prerequisites() {
    let once = graph(vec![hours("a", &[], 1.0), hours("b", &["a"], 1.0)]);
    let twice = graph(vec![hours("a", &[], 1.0), hours("b", &["a", "a"], 1.0)]);
    assert_eq!(
        card(&twice).cyclomatic_complexity,
        card(&once).cyclomatic_complexity
    );
}

#[test]
fn critical_path_len_counts_real_deliverables() {
    let g = graph(vec![
        hours("a", &[], 2.0),
        hours("b", &["a"], 2.0),
        hours("c", &["b"], 2.0),
    ]);
    assert_eq!(card(&g).critical_path_len, 3);
}

#[test]
fn near_critical_counts_small_positive_float() {
    // makespan 20; side has float 19 (not near), tight has float 1 (< 2.0).
    let g = graph(vec![
        hours("a", &[], 10.0),
        hours("b", &["a"], 10.0),
        hours("tight", &[], 19.0),
        hours("side", &[], 1.0),
    ]);
    assert_eq!(card(&g).near_critical_count, 1);
}

#[test]
fn merge_bias_counts_three_plus_prerequisites() {
    let g = graph(vec![
        hours("a", &[], 1.0),
        hours("b", &[], 1.0),
        hours("c", &[], 1.0),
        hours("m", &["a", "b", "c"], 1.0),
        hours("n", &["a", "b"], 1.0),
    ]);
    assert_eq!(card(&g).merge_bias_count, 1);
}

#[test]
fn parallelism_is_total_length_over_makespan() {
    let g = graph(vec![hours("a", &[], 4.0), hours("b", &[], 4.0)]);
    assert_eq!(card(&g).parallelism, 2.0);
}

#[test]
fn total_effort_ignores_duration_hours() {
    let g = graph(vec![dv(
        "a",
        &[],
        json!({"estimated_effort_hours": 3.0, "duration_hours": 10.0}),
    )]);
    assert_eq!(card(&g).total_effort, 3.0);
}

#[test]
fn max_drag_names_the_heaviest_critical_deliverable() {
    let g = graph(vec![hours("a", &[], 2.0), hours("b", &["a"], 6.0)]);
    assert_eq!(card(&g).max_drag, Some(("b".to_string(), 6.0)));
}

#[test]
fn lint_counts_are_copied_from_report() {
    let g = graph(vec![hours("a", &[], 1.0)]);
    let lint = LintReport {
        clean: false,
        findings: vec![
            finding(Severity::Error),
            finding(Severity::Error),
            finding(Severity::Warning),
            finding(Severity::Info),
        ],
    };
    let s = scorecard(&g, &cpm(&g), None, &lint);
    assert_eq!((s.lint_errors, s.lint_warnings), (2, 1));
}

#[test]
fn scorecard_is_deterministic() {
    let g = graph(vec![
        hours("a", &[], 2.0),
        hours("b", &["a"], 3.0),
        hours("c", &[], 1.0),
    ]);
    assert_eq!(card(&g), card(&g));
}
