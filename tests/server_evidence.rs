//! Evidence records through the MCP server (docs/evidence.md):
//! `plan.mark_status.evidence` and `plan.accept.evidence` take a note or a
//! record; the evidence error codes map like every other planner error; and
//! `plan.overview` judges records against `evidence_target`.

use std::sync::Arc;

use cpm_planner::{BasicCpmPlanner, PlanServer};
use rmcp::model::{CallToolRequestParams, ErrorCode, JsonObject};
use serde_json::{Value, json};

fn server() -> PlanServer {
    PlanServer::new(Arc::new(BasicCpmPlanner::new()))
}

fn call_args(name: &str, args: Value) -> CallToolRequestParams {
    let map: JsonObject = match args {
        Value::Object(m) => m,
        _ => panic!("call_args expects a JSON object"),
    };
    CallToolRequestParams::new(name.to_string()).with_arguments(map)
}

async fn call(server: &PlanServer, tool: &str, args: Value) -> Result<Value, rmcp::ErrorData> {
    server.dispatch_call(call_args(tool, args)).await
}

async fn ok(server: &PlanServer, tool: &str, args: Value) -> Value {
    call(server, tool, args)
        .await
        .unwrap_or_else(|e| panic!("{tool} failed: {}", e.message))
}

async fn err(server: &PlanServer, tool: &str, args: Value) -> rmcp::ErrorData {
    call(server, tool, args)
        .await
        .expect_err("call should fail")
}

fn identity(tree: &str) -> Value {
    json!({
        "schema_version": 1,
        "tree": tree,
        "dependency_lock": "sha256:lock",
        "toolchain": "rustc 1.89.0",
        "test_contract": "nextest:all-targets@sha256:tests",
        "environment": "linux-x86_64"
    })
}

fn record(scope: &str, tree: &str) -> Value {
    json!({
        "schema_version": 1,
        "scope": scope,
        "summary": format!("{scope} checks passed"),
        "identity": identity(tree)
    })
}

/// `impl` (work), `regression` (a gate verifying candidate `impl`) and
/// `signoff` (a manual approval that declares its scope).
fn graph() -> Value {
    json!({
        "deliverables": [
            { "id": "impl", "owned_files": ["src/lib.rs"], "prerequisites": [],
              "estimated_effort_hours": 2.0, "metadata": {} },
            { "id": "regression", "owned_files": [],
              "prerequisites": [{ "id": "impl", "consumes": "the integrated candidate" }],
              "estimated_effort_hours": 1.0,
              "metadata": { "kind": "gate", "evidence_scope": "gate", "candidate": "impl" } },
            { "id": "signoff", "owned_files": [], "prerequisites": [],
              "estimated_effort_hours": 0.5,
              "metadata": { "kind": "manual", "evidence_scope": "manual_approval" } }
        ],
        "max_chained_dispatch": null
    })
}

async fn submit(server: &PlanServer) -> String {
    let resp = ok(server, "plan.submit", json!({ "graph": graph() })).await;
    resp["plan_id"].as_str().expect("plan_id").to_string()
}

/// Lease `impl` and complete it with a focused record on tree `tree`.
async fn complete_impl(server: &PlanServer, plan_id: &str, tree: &str) {
    ok(
        server,
        "plan.acquire_cohort",
        json!({ "plan_id": plan_id, "caller_id": "worker", "ids": ["impl"] }),
    )
    .await;
    ok(
        server,
        "plan.mark_status",
        json!({ "plan_id": plan_id, "deliverable_id": "impl", "caller_id": "worker",
                "status": { "status": "complete" }, "evidence": record("focused", tree) }),
    )
    .await;
}

fn mark_in_progress(plan_id: &str, evidence: Value) -> Value {
    json!({ "plan_id": plan_id, "deliverable_id": "impl", "caller_id": "worker",
            "status": { "status": "in_progress" }, "evidence": evidence })
}

// ── plan.mark_status ─────────────────────────────────────────────────────────

#[tokio::test]
async fn mark_status_still_accepts_a_note() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = ok(
        &server,
        "plan.mark_status",
        mark_in_progress(&plan_id, json!("wip")),
    )
    .await;
    assert_eq!(resp["ok"], json!(true));
}

#[tokio::test]
async fn mark_status_accepts_an_evidence_record() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = ok(
        &server,
        "plan.mark_status",
        mark_in_progress(&plan_id, record("focused", "git-tree:a")),
    )
    .await;
    assert_eq!(resp["ok"], json!(true));
}

#[tokio::test]
async fn mark_status_record_is_stored_with_the_deliverable() {
    let server = server();
    let plan_id = submit(&server).await;
    ok(
        &server,
        "plan.mark_status",
        mark_in_progress(&plan_id, record("focused", "git-tree:a")),
    )
    .await;
    let overview = ok(&server, "plan.overview", json!({ "plan_id": plan_id })).await;
    assert_eq!(overview["evidence_counts"]["none"], json!(2));
}

#[tokio::test]
async fn mark_status_record_without_summary_is_invalid_evidence() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.mark_status",
        mark_in_progress(
            &plan_id,
            json!({ "schema_version": 1, "scope": "focused", "identity": identity("t") }),
        ),
    )
    .await;
    assert!(e.message.starts_with("INVALID_EVIDENCE:"), "{}", e.message);
}

#[tokio::test]
async fn mark_status_record_with_a_server_field_is_invalid_evidence() {
    let server = server();
    let plan_id = submit(&server).await;
    let mut forged = record("focused", "git-tree:a");
    forged["recorded_by"] = json!("someone-else");
    let e = err(
        &server,
        "plan.mark_status",
        mark_in_progress(&plan_id, forged),
    )
    .await;
    assert!(e.message.starts_with("INVALID_EVIDENCE:"), "{}", e.message);
}

#[tokio::test]
async fn mark_status_numeric_evidence_is_invalid_params() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.mark_status",
        mark_in_progress(&plan_id, json!(42)),
    )
    .await;
    assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn mark_status_overlong_note_is_still_invalid_actuals() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.mark_status",
        mark_in_progress(&plan_id, json!("x".repeat(2049))),
    )
    .await;
    assert!(e.message.starts_with("INVALID_ACTUALS:"), "{}", e.message);
}

#[tokio::test]
async fn mark_status_gate_record_on_a_manual_approval_is_evidence_scope_mismatch() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.mark_status",
        json!({ "plan_id": plan_id, "deliverable_id": "signoff", "caller_id": "pm",
                "status": { "status": "in_progress" }, "evidence": record("gate", "t") }),
    )
    .await;
    assert!(
        e.message.starts_with("EVIDENCE_SCOPE_MISMATCH:"),
        "{}",
        e.message
    );
}

// ── plan.accept ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn accept_with_a_matching_gate_record_completes_the_gate() {
    let server = server();
    let plan_id = submit(&server).await;
    complete_impl(&server, &plan_id, "git-tree:a").await;
    let resp = ok(
        &server,
        "plan.accept",
        json!({ "plan_id": plan_id, "deliverable_id": "regression", "accepted_by": "ci",
                "evidence": record("gate", "git-tree:a") }),
    )
    .await;
    assert_eq!(resp["ok"], json!(true));
}

#[tokio::test]
async fn accept_gate_record_for_another_tree_is_evidence_stale() {
    let server = server();
    let plan_id = submit(&server).await;
    complete_impl(&server, &plan_id, "git-tree:a").await;
    let e = err(
        &server,
        "plan.accept",
        json!({ "plan_id": plan_id, "deliverable_id": "regression", "accepted_by": "ci",
                "evidence": record("gate", "git-tree:b") }),
    )
    .await;
    assert!(e.message.starts_with("EVIDENCE_STALE:"), "{}", e.message);
}

#[tokio::test]
async fn accept_focused_record_on_a_gate_is_evidence_scope_mismatch() {
    let server = server();
    let plan_id = submit(&server).await;
    complete_impl(&server, &plan_id, "git-tree:a").await;
    let e = err(
        &server,
        "plan.accept",
        json!({ "plan_id": plan_id, "deliverable_id": "regression", "accepted_by": "ci",
                "evidence": record("focused", "git-tree:a") }),
    )
    .await;
    assert!(
        e.message.starts_with("EVIDENCE_SCOPE_MISMATCH:"),
        "{}",
        e.message
    );
}

#[tokio::test]
async fn accept_declared_scope_with_only_a_note_is_evidence_required() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.accept",
        json!({ "plan_id": plan_id, "deliverable_id": "signoff", "accepted_by": "pm",
                "evidence": "looks good" }),
    )
    .await;
    assert!(e.message.starts_with("EVIDENCE_REQUIRED:"), "{}", e.message);
}

#[tokio::test]
async fn accept_manual_approval_record_completes_the_sign_off() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = ok(
        &server,
        "plan.accept",
        json!({ "plan_id": plan_id, "deliverable_id": "signoff", "accepted_by": "pm",
                "evidence": { "schema_version": 1, "scope": "manual_approval",
                              "summary": "approved the export contract" } }),
    )
    .await;
    assert_eq!(resp["ok"], json!(true));
}

#[tokio::test]
async fn accept_null_evidence_is_invalid_params() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.accept",
        json!({ "plan_id": plan_id, "deliverable_id": "signoff", "accepted_by": "pm",
                "evidence": null }),
    )
    .await;
    assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn accept_malformed_record_is_invalid_evidence() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.accept",
        json!({ "plan_id": plan_id, "deliverable_id": "signoff", "accepted_by": "pm",
                "evidence": { "schema_version": 1, "scope": "blessing", "summary": "ok" } }),
    )
    .await;
    assert!(e.message.starts_with("INVALID_EVIDENCE:"), "{}", e.message);
}

// ── plan.overview evidence_target ────────────────────────────────────────────

#[tokio::test]
async fn overview_evidence_target_is_the_argument_target() {
    let server = server();
    let plan_id = submit(&server).await;
    complete_impl(&server, &plan_id, "git-tree:a").await;
    let overview = ok(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "detail": "full", "evidence_target": identity("git-tree:b") }),
    )
    .await;
    let stale: Vec<&str> = overview["stale_evidence"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert_eq!(stale, vec!["impl"]);
}

#[tokio::test]
async fn overview_matching_evidence_target_leaves_no_stale_evidence() {
    let server = server();
    let plan_id = submit(&server).await;
    complete_impl(&server, &plan_id, "git-tree:a").await;
    let overview = ok(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "evidence_target": identity("git-tree:a") }),
    )
    .await;
    assert_eq!(overview["stale_evidence"]["total"], json!(0));
}

// ── Schemas ──────────────────────────────────────────────────────────────────

fn evidence_schema(tool: &str) -> Value {
    let tools = cpm_planner::server::plan_tool_definitions();
    let def = tools
        .iter()
        .find(|t| t.name == tool)
        .unwrap_or_else(|| panic!("{tool} advertised"));
    def.input_schema["properties"]["evidence"].clone()
}

fn one_of_types(schema: &Value) -> Vec<Value> {
    schema["oneOf"]
        .as_array()
        .expect("oneOf")
        .iter()
        .map(|s| s["type"].clone())
        .collect()
}

#[test]
fn mark_status_evidence_schema_is_a_string_or_a_record() {
    assert_eq!(
        one_of_types(&evidence_schema("plan.mark_status")),
        vec![json!("string"), json!("object")]
    );
}

#[test]
fn accept_evidence_schema_is_a_string_or_a_record() {
    assert_eq!(
        one_of_types(&evidence_schema("plan.accept")),
        vec![json!("string"), json!("object")]
    );
}
