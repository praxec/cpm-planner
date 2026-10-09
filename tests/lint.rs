//! Library tests for `plan.lint`.

use cpm_planner::lint::{LintFinding, LintReport, Severity, lint};
use cpm_planner::plan::PlanGraph;
use serde_json::{Value, json};

fn graph(v: Value) -> PlanGraph {
    serde_json::from_value(json!({ "deliverables": v })).expect("fixture graph")
}

fn d(id: &str, prereqs: Value, files: Value, extra: Value) -> Value {
    let mut v = json!({
        "id": id,
        "owned_files": files,
        "prerequisites": prereqs,
        "metadata": { "artifact": "x" },
    });
    if let Value::Object(m) = extra {
        for (k, val) in m {
            v[k] = val;
        }
    }
    v
}

fn finding<'a>(r: &'a LintReport, code: &str) -> Option<&'a LintFinding> {
    r.findings.iter().find(|f| f.code == code)
}

fn codes(r: &LintReport) -> Vec<&str> {
    r.findings.iter().map(|f| f.code.as_str()).collect()
}

fn edge(id: &str) -> Value {
    json!({ "id": id, "consumes": "output" })
}

fn ms(id: &str, prereqs: Value) -> Value {
    d(id, prereqs, json!([]), json!({ "milestone": true }))
}

#[test]
fn cycle_finding_names_the_loop() {
    let g = graph(json!([
        d("a", json!([edge("c")]), json!([]), json!({})),
        d("b", json!([edge("a")]), json!([]), json!({})),
        d("c", json!([edge("b")]), json!([]), json!({})),
    ]));
    let r = lint(&g);
    assert_eq!(
        finding(&r, "CYCLE").map(|f| f.ids.clone()),
        Some(vec!["a".into(), "c".into(), "b".into(), "a".into()])
    );
}

#[test]
fn cycle_is_an_error() {
    let g = graph(json!([d("a", json!([edge("a")]), json!([]), json!({}))]));
    assert_eq!(
        finding(&lint(&g), "CYCLE").map(|f| f.severity),
        Some(Severity::Error)
    );
}

#[test]
fn redundant_edge_is_reported() {
    let g = graph(json!([
        d("a", json!([]), json!([]), json!({})),
        d("b", json!([edge("a")]), json!([]), json!({})),
        d("c", json!([edge("a"), edge("b")]), json!([]), json!({})),
        ms("m", json!([edge("c")])),
    ]));
    assert_eq!(
        finding(&lint(&g), "REDUNDANT_EDGE").map(|f| f.ids.clone()),
        Some(vec!["c".into(), "a".into()])
    );
}

#[test]
fn edge_without_consumes_is_reported() {
    let g = graph(json!([
        d("a", json!([]), json!([]), json!({})),
        d("b", json!(["a"]), json!([]), json!({})),
    ]));
    assert_eq!(
        finding(&lint(&g), "NO_RATIONALE").map(|f| f.ids.clone()),
        Some(vec!["b".into(), "a".into()])
    );
}

#[test]
fn legacy_metadata_consumes_counts_as_rationale() {
    let g = graph(json!([
        d("a", json!([]), json!([]), json!({})),
        d(
            "b",
            json!(["a"]),
            json!([]),
            json!({ "metadata": { "artifact": "x", "consumes": { "a": "the api" } } })
        ),
    ]));
    assert!(finding(&lint(&g), "NO_RATIONALE").is_none());
}

#[test]
fn interface_edge_to_non_contract_is_reported() {
    let g = graph(json!([
        d("a", json!([]), json!([]), json!({})),
        d(
            "b",
            json!([{ "id": "a", "consumes": "api", "kind": "interface" }]),
            json!([]),
            json!({})
        ),
    ]));
    assert!(finding(&lint(&g), "INTERFACE_EDGE_NOT_CONTRACT").is_some());
}

#[test]
fn interface_edge_to_contract_is_not_reported() {
    let g = graph(json!([
        d(
            "a",
            json!([]),
            json!([]),
            json!({ "metadata": { "artifact": "x", "contract": true } })
        ),
        d(
            "b",
            json!([{ "id": "a", "consumes": "api", "kind": "interface" }]),
            json!([]),
            json!({})
        ),
    ]));
    assert!(finding(&lint(&g), "INTERFACE_EDGE_NOT_CONTRACT").is_none());
}

#[test]
fn deliverable_feeding_no_milestone_is_reported() {
    let g = graph(json!([
        d("a", json!([]), json!([]), json!({})),
        d("stray", json!([]), json!([]), json!({})),
        ms("m", json!([edge("a")])),
    ]));
    assert_eq!(
        finding(&lint(&g), "FEEDS_NO_MILESTONE").map(|f| f.ids.clone()),
        Some(vec!["stray".into()])
    );
}

#[test]
fn no_milestone_is_info_only() {
    let g = graph(json!([d("a", json!([]), json!([]), json!({}))]));
    let r = lint(&g);
    assert_eq!((r.clean, codes(&r)), (true, vec!["NO_MILESTONE"]));
}

#[test]
fn missing_artifact_is_info() {
    let g = graph(json!([{ "id": "a", "owned_files": [], "prerequisites": [] }]));
    assert_eq!(
        finding(&lint(&g), "NO_ARTIFACT").map(|f| f.severity),
        Some(Severity::Info)
    );
}

#[test]
fn unordered_exclusive_overlap_is_an_error() {
    let g = graph(json!([
        d("a", json!([]), json!(["f.rs"]), json!({})),
        d("b", json!([]), json!(["f.rs"]), json!({})),
    ]));
    let f = finding(&lint(&g), "UNORDERED_FILE_OVERLAP").cloned();
    assert_eq!(
        f.map(|f| (f.severity, f.path)),
        Some((Severity::Error, Some("f.rs".to_string())))
    );
}

#[test]
fn ordered_overlap_is_not_reported() {
    let g = graph(json!([
        d("a", json!([]), json!(["f.rs"]), json!({})),
        d("b", json!([edge("a")]), json!(["f.rs"]), json!({})),
    ]));
    assert!(finding(&lint(&g), "UNORDERED_FILE_OVERLAP").is_none());
}

#[test]
fn unordered_append_overlap_is_not_reported() {
    let g = graph(json!([
        d(
            "a",
            json!([]),
            json!([{ "path": "l.md", "mode": "append" }]),
            json!({})
        ),
        d(
            "b",
            json!([]),
            json!([{ "path": "l.md", "mode": "append" }]),
            json!({})
        ),
    ]));
    assert!(finding(&lint(&g), "UNORDERED_FILE_OVERLAP").is_none());
}

#[test]
fn unknown_prerequisite_is_reported_without_panicking() {
    let g = graph(json!([d(
        "a",
        json!([edge("ghost")]),
        json!([]),
        json!({})
    )]));
    assert_eq!(
        finding(&lint(&g), "UNKNOWN_PREREQUISITE").map(|f| f.ids.clone()),
        Some(vec!["a".into(), "ghost".into()])
    );
}

#[test]
fn reserved_id_is_reported() {
    let g = graph(json!([d("__start__", json!([]), json!([]), json!({}))]));
    assert!(finding(&lint(&g), "RESERVED_ID").is_some());
}

#[test]
fn duplicate_id_is_reported() {
    let g = graph(json!([
        d("a", json!([]), json!([]), json!({})),
        d("a", json!([]), json!([]), json!({})),
    ]));
    assert!(finding(&lint(&g), "DUPLICATE_ID").is_some());
}

#[test]
fn negative_effort_is_an_invalid_value() {
    let g = graph(json!([d(
        "a",
        json!([]),
        json!([]),
        json!({ "estimated_effort_hours": -1.0 })
    )]));
    assert!(finding(&lint(&g), "INVALID_VALUE").is_some());
}

#[test]
fn negative_lag_is_an_invalid_value() {
    let g = graph(json!([
        d("a", json!([]), json!([]), json!({})),
        d(
            "b",
            json!([{ "id": "a", "consumes": "x", "lag_hours": -2.0 }]),
            json!([]),
            json!({})
        ),
    ]));
    assert!(finding(&lint(&g), "INVALID_VALUE").is_some());
}

#[test]
fn clean_graph_reports_clean() {
    let g = graph(json!([
        d("a", json!([]), json!(["a.rs"]), json!({})),
        d("b", json!([edge("a")]), json!(["b.rs"]), json!({})),
        ms("m", json!([edge("b")])),
    ]));
    let r = lint(&g);
    assert_eq!((r.clean, r.findings.len()), (true, 0));
}

#[test]
fn findings_are_sorted_deterministically() {
    let g = graph(json!([
        d("z", json!(["ghost"]), json!(["f"]), json!({})),
        d("y", json!([]), json!(["f"]), json!({})),
        d("x", json!([]), json!([]), json!({})),
    ]));
    let r = lint(&g);
    let mut sorted = r.findings.clone();
    sorted.sort_by(|a, b| (a.severity, &a.code, &a.ids).cmp(&(b.severity, &b.code, &b.ids)));
    assert_eq!(r.findings, sorted);
}

#[test]
fn lint_output_is_repeatable() {
    let g = graph(json!([
        d("a", json!([edge("b")]), json!(["f"]), json!({})),
        d("b", json!([edge("a")]), json!(["f"]), json!({})),
    ]));
    assert_eq!(lint(&g), lint(&g));
}

#[test]
fn graph_dependent_checks_are_skipped_when_cyclic() {
    let g = graph(json!([
        d("a", json!([edge("b")]), json!([]), json!({})),
        d("b", json!([edge("a")]), json!([]), json!({})),
        ms("m", json!([])),
    ]));
    assert!(finding(&lint(&g), "FEEDS_NO_MILESTONE").is_none());
}

#[test]
fn bad_estimate_is_invalid_value() {
    let g = graph(json!([d(
        "a",
        json!([]),
        json!([]),
        json!({ "estimate": { "optimistic": 3.0, "likely": 2.0, "pessimistic": 4.0 } })
    )]));
    assert_eq!(
        finding(&lint(&g), "INVALID_VALUE").map(|f| f.message.as_str()),
        Some("deliverable 'a' estimate must satisfy 0 <= optimistic <= likely <= pessimistic")
    );
}

#[test]
fn hours_above_one_million_are_invalid_value() {
    let g = graph(json!([d(
        "a",
        json!([]),
        json!([]),
        json!({ "duration_hours": 1_000_001.0 })
    )]));
    assert_eq!(
        finding(&lint(&g), "INVALID_VALUE").map(|f| f.message.as_str()),
        Some(
            "deliverable 'a' has invalid duration_hours 1000001; must be a finite number between 0 and 1000000"
        )
    );
}

#[test]
fn estimate_above_one_million_is_invalid_value() {
    let g = graph(json!([d(
        "a",
        json!([]),
        json!([]),
        json!({ "estimate": { "optimistic": 1.0, "likely": 2.0, "pessimistic": 2_000_000.0 } })
    )]));
    assert!(finding(&lint(&g), "INVALID_VALUE").is_some());
}

#[test]
fn milestone_without_artifact_is_not_reported() {
    let g = graph(json!([
        d("a", json!([]), json!([]), json!({})),
        { "id": "m", "owned_files": [], "prerequisites": [edge("a")], "milestone": true },
    ]));
    assert!(finding(&lint(&g), "NO_ARTIFACT").is_none());
}

#[test]
fn warning_only_graph_is_not_clean() {
    let g = graph(json!([
        d("a", json!([]), json!([]), json!({})),
        d("b", json!(["a"]), json!([]), json!({})),
        ms("m", json!([edge("b")])),
    ]));
    let r = lint(&g);
    assert_eq!((r.clean, codes(&r)), (false, vec!["NO_RATIONALE"]));
}

#[test]
fn oversized_graph_reports_one_size_error() {
    let deliverables: Vec<Value> = (0..5001)
        .map(|i| json!({ "id": format!("d{i}"), "owned_files": [], "prerequisites": [] }))
        .collect();
    let r = lint(&graph(json!(deliverables)));
    assert_eq!(codes(&r), vec!["TOO_MANY_DELIVERABLES"]);
}

#[test]
fn redundant_edge_through_a_long_chain_is_reported() {
    let mut v: Vec<Value> = vec![d("n0", json!([]), json!([]), json!({}))];
    for i in 1..50 {
        v.push(d(
            &format!("n{i}"),
            json!([edge(&format!("n{}", i - 1))]),
            json!([]),
            json!({}),
        ));
    }
    v.push(d(
        "top",
        json!([edge("n0"), edge("n49")]),
        json!([]),
        json!({}),
    ));
    let r = lint(&graph(json!(v)));
    let redundant: Vec<&Vec<String>> = r
        .findings
        .iter()
        .filter(|f| f.code == "REDUNDANT_EDGE")
        .map(|f| &f.ids)
        .collect();
    assert_eq!(redundant, vec![&vec!["top".to_string(), "n0".to_string()]]);
}

#[test]
#[ignore = "timing probe: cargo test --release --test lint -- --ignored --nocapture"]
fn timing_lint_5000_node_chain() {
    let mut v: Vec<Value> = vec![d("n0", json!([]), json!([]), json!({}))];
    for i in 1..4999 {
        v.push(d(
            &format!("n{i}"),
            json!([edge(&format!("n{}", i - 1))]),
            json!([]),
            json!({}),
        ));
    }
    v.push(ms("m", json!([edge("n4998")])));
    assert_eq!(v.len(), 5000);
    let g = graph(json!(v));
    let start = std::time::Instant::now();
    let r = lint(&g);
    let elapsed = start.elapsed();
    println!("TIMING lint 5000-node chain: {elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "{elapsed:?}; {}",
        r.findings.len()
    );
}
