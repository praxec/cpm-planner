//! Tests for the plan scorecard library.

use cpm_planner::CpmAlgorithm;
use cpm_planner::lint::{LintFinding, LintReport, Severity};
use cpm_planner::metrics::{Scorecard, scorecard};
use cpm_planner::plan::{FINISH_ID, PlanGraph, START_ID};
use cpm_planner::task::{CriticalPathResult, Task};
use serde_json::json;
use std::collections::HashSet;

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

/// CPM over the graph with synthetic endpoints, mirroring the planner.
fn cpm(g: &PlanGraph) -> CriticalPathResult {
    let referenced: HashSet<String> = g
        .deliverables
        .iter()
        .flat_map(|d| d.prerequisites.iter().map(|p| p.id().to_string()))
        .collect();
    let mut tasks: Vec<Task> = g
        .deliverables
        .iter()
        .map(|d| Task {
            id: d.id.clone(),
            name: d.id.clone(),
            effort_hours: d.duration_hours.or(d.estimated_effort_hours).unwrap_or(0.0),
            dependencies: d.prerequisites.iter().map(|p| p.id().to_string()).collect(),
            ..Task::default()
        })
        .collect();
    for t in tasks.iter_mut().filter(|t| t.dependencies.is_empty()) {
        t.dependencies.push(START_ID.to_string());
    }
    let sinks: Vec<String> = g
        .deliverables
        .iter()
        .filter(|d| !referenced.contains(&d.id))
        .map(|d| d.id.clone())
        .collect();
    tasks.push(Task {
        id: START_ID.to_string(),
        ..Task::default()
    });
    tasks.push(Task {
        id: FINISH_ID.to_string(),
        dependencies: sinks,
        ..Task::default()
    });
    CpmAlgorithm::calculate(&mut tasks)
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
    let g = graph(vec![hours("a", &[], 1.0), hours("b", &["a"], 1.0)]);
    assert_eq!(card(&g).deliverables, g.deliverables.len());
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
