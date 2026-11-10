//! `plan.overview` and `plan.explain` through the MCP server
//! (docs/overview.md), and the `cpm-planner overview` / `explain` CLI
//! commands: selector, `as_of` from the planner clock, detail, limit,
//! filter, evidence target, format, and error mapping.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use cpm_planner::audit::NullAuditSink;
use cpm_planner::explain::ExplainRequest;
use cpm_planner::overview::{Detail, OverviewRequest};
use cpm_planner::plan::{PlanId, PlanRef};
use cpm_planner::{BasicCpmPlanner, PlanServer, SqlitePlanStore};
use rmcp::model::{CallToolRequestParams, ErrorCode, JsonObject};
use serde_json::{Value, json};

const AS_OF: &str = "2030-01-01T00:00:00Z";
const CLOCK: &str = "2031-06-01T08:00:00Z";

fn instant(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("rfc3339")
        .with_timezone(&Utc)
}

/// A planner whose clock is fixed at [`CLOCK`].
fn planner() -> BasicCpmPlanner {
    BasicCpmPlanner::new().with_clock(Arc::new(|| instant(CLOCK)))
}

fn server() -> PlanServer {
    PlanServer::new(Arc::new(planner()))
}

fn call_args(name: &str, args: Value) -> CallToolRequestParams {
    let map: JsonObject = match args {
        Value::Object(m) => m,
        _ => panic!("call_args expects a JSON object"),
    };
    CallToolRequestParams::new(name.to_string()).with_arguments(map)
}

async fn call(server: &PlanServer, tool: &str, args: Value) -> Value {
    server
        .dispatch_call(call_args(tool, args))
        .await
        .unwrap_or_else(|e| panic!("{tool} failed: {}", e.message))
}

async fn call_err(server: &PlanServer, tool: &str, args: Value) -> rmcp::ErrorData {
    server
        .dispatch_call(call_args(tool, args))
        .await
        .expect_err("call should fail")
}

/// `contract -> service -> ship` (a milestone), plus `approve`, a longer
/// manual sign-off that drives `service`, and `docs`, owned by `writer`.
fn graph() -> Value {
    json!({
        "deliverables": [
            { "id": "contract", "owned_files": ["api.md"], "prerequisites": [],
              "estimated_effort_hours": 2.0,
              "metadata": { "owner": "agent", "title": "Export API contract" } },
            { "id": "approve", "owned_files": [], "prerequisites": [],
              "estimated_effort_hours": 3.0,
              "metadata": { "owner": "pm", "kind": "manual" } },
            { "id": "service", "owned_files": ["src/service.rs"],
              "prerequisites": [
                  { "id": "contract", "consumes": "the export API contract" },
                  { "id": "approve", "consumes": "product sign-off" }
              ],
              "estimated_effort_hours": 4.0, "metadata": { "owner": "agent" } },
            { "id": "docs", "owned_files": ["docs/export.md"], "prerequisites": [],
              "estimated_effort_hours": 1.0, "metadata": { "owner": "writer" } },
            { "id": "ship", "owned_files": [],
              "prerequisites": [
                  { "id": "service", "consumes": "the export service" },
                  { "id": "docs", "consumes": "the export guide" }
              ],
              "milestone": true, "metadata": {} }
        ],
        "max_chained_dispatch": null
    })
}

async fn submit(server: &PlanServer) -> String {
    let resp = call(server, "plan.submit", json!({ "graph": graph() })).await;
    resp["plan_id"].as_str().expect("plan_id").to_string()
}

// ── plan.overview ────────────────────────────────────────────────────────────

#[tokio::test]
async fn overview_returns_the_schema_v1_value() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = call(&server, "plan.overview", json!({ "plan_id": plan_id })).await;
    assert_eq!(resp["schema"], json!("cpm-planner/overview/v1"));
}

#[tokio::test]
async fn overview_equals_the_library_overview_at_the_same_as_of() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = call(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "as_of": AS_OF }),
    )
    .await;
    let library = cpm_planner::overview::overview(
        &**server.planner(),
        OverviewRequest::new(PlanRef::Id(PlanId(plan_id))).with_as_of(instant(AS_OF)),
    )
    .await
    .expect("library overview");
    assert_eq!(resp, serde_json::to_value(&library).unwrap());
}

#[tokio::test]
async fn overview_as_of_defaults_to_the_planner_clock() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = call(&server, "plan.overview", json!({ "plan_id": plan_id })).await;
    assert_eq!(resp["as_of"].as_str().map(instant), Some(instant(CLOCK)));
}

#[tokio::test]
async fn overview_detail_full_equals_the_library_full_overview() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = call(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "as_of": AS_OF, "detail": "full" }),
    )
    .await;
    let library = cpm_planner::overview::overview(
        &**server.planner(),
        OverviewRequest::new(PlanRef::Id(PlanId(plan_id)))
            .with_as_of(instant(AS_OF))
            .with_detail(Detail::Full),
    )
    .await
    .expect("library overview");
    assert_eq!(resp, serde_json::to_value(&library).unwrap());
}

#[tokio::test]
async fn overview_limit_bounds_every_section() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = call(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "limit": 1 }),
    )
    .await;
    assert_eq!(resp["ready"]["items"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn overview_limit_zero_is_invalid_value() {
    let server = server();
    let plan_id = submit(&server).await;
    let err = call_err(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "limit": 0 }),
    )
    .await;
    assert!(err.message.starts_with("INVALID_VALUE:"), "{}", err.message);
}

#[tokio::test]
async fn overview_owner_filter_narrows_the_ready_list() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = call(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "filter": { "owner": "writer" } }),
    )
    .await;
    let ids: Vec<&str> = resp["ready"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["docs"]);
}

#[tokio::test]
async fn overview_unknown_milestone_filter_is_deliverable_not_found() {
    let server = server();
    let plan_id = submit(&server).await;
    let err = call_err(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "filter": { "milestone": "nope" } }),
    )
    .await;
    assert!(
        err.message.starts_with("DELIVERABLE_NOT_FOUND:"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn overview_unknown_filter_field_is_invalid_params() {
    let server = server();
    let plan_id = submit(&server).await;
    let err = call_err(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "filter": { "team": "x" } }),
    )
    .await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn overview_malformed_evidence_target_is_invalid_evidence() {
    let server = server();
    let plan_id = submit(&server).await;
    let err = call_err(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "evidence_target": { "schema_version": 2 } }),
    )
    .await;
    assert!(
        err.message.starts_with("INVALID_EVIDENCE:"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn overview_malformed_evidence_target_is_refused_before_the_plan_is_read() {
    let server = server();
    let err = call_err(
        &server,
        "plan.overview",
        json!({ "plan_id": "plan_missing", "evidence_target": { "tree": "" } }),
    )
    .await;
    assert!(
        err.message.starts_with("INVALID_EVIDENCE:"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn overview_markdown_is_the_rendering_of_the_json_value() {
    let server = server();
    let plan_id = submit(&server).await;
    let json_value = call(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "as_of": AS_OF }),
    )
    .await;
    let markdown = call(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "as_of": AS_OF, "format": "markdown" }),
    )
    .await;
    let parsed: cpm_planner::overview::Overview =
        serde_json::from_value(json_value).expect("overview decodes");
    assert_eq!(
        markdown,
        json!({
            "format": "markdown",
            "markdown": cpm_planner::overview::render_markdown(&parsed)
        })
    );
}

#[tokio::test]
async fn overview_unknown_format_is_invalid_params() {
    let server = server();
    let plan_id = submit(&server).await;
    let err = call_err(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "format": "html" }),
    )
    .await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn overview_by_line_name_resolves_the_selected_variant() {
    let server = server();
    let synced = call(
        &server,
        "plan.submit",
        json!({ "graph": graph(), "name": "export", "project": "proj" }),
    )
    .await;
    let resp = call(
        &server,
        "plan.overview",
        json!({ "plan": "export", "project": "proj" }),
    )
    .await;
    assert_eq!(resp["plan"]["plan_id"], synced["plan_id"]);
}

#[tokio::test]
async fn overview_unknown_plan_is_plan_not_found() {
    let err = call_err(
        &server(),
        "plan.overview",
        json!({ "plan_id": "plan_missing" }),
    )
    .await;
    assert!(
        err.message.starts_with("PLAN_NOT_FOUND:"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn overview_without_a_selector_is_invalid_params() {
    let err = call_err(&server(), "plan.overview", json!({})).await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn overview_unknown_argument_is_invalid_params() {
    let server = server();
    let plan_id = submit(&server).await;
    let err = call_err(
        &server,
        "plan.overview",
        json!({ "plan_id": plan_id, "verbose": true }),
    )
    .await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

// ── plan.explain ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn explain_equals_the_library_explanation_at_the_same_as_of() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = call(
        &server,
        "plan.explain",
        json!({ "plan_id": plan_id, "deliverable_id": "service", "as_of": AS_OF }),
    )
    .await;
    let library = cpm_planner::explain::explain(
        &**server.planner(),
        ExplainRequest::new(PlanRef::Id(PlanId(plan_id)))
            .with_deliverable("service")
            .with_as_of(instant(AS_OF)),
    )
    .await
    .expect("library explain");
    assert_eq!(resp, serde_json::to_value(&library).unwrap());
}

#[tokio::test]
async fn explain_blocked_deliverable_reports_its_blocker_class() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = call(
        &server,
        "plan.explain",
        json!({ "plan_id": plan_id, "deliverable_id": "service" }),
    )
    .await;
    assert_eq!(
        (
            resp["deliverable"]["state"].clone(),
            resp["deliverable"]["blocked"]["class"].clone()
        ),
        (json!("blocked"), json!("manual"))
    );
}

#[tokio::test]
async fn explain_without_deliverable_returns_only_next_work() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = call(&server, "plan.explain", json!({ "plan_id": plan_id })).await;
    assert_eq!(
        (
            resp.get("deliverable").is_none(),
            resp["next_work"].is_object()
        ),
        (true, true)
    );
}

#[tokio::test]
async fn explain_as_of_defaults_to_the_planner_clock() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = call(&server, "plan.explain", json!({ "plan_id": plan_id })).await;
    assert_eq!(resp["as_of"].as_str().map(instant), Some(instant(CLOCK)));
}

#[tokio::test]
async fn explain_unknown_deliverable_is_deliverable_not_found() {
    let server = server();
    let plan_id = submit(&server).await;
    let err = call_err(
        &server,
        "plan.explain",
        json!({ "plan_id": plan_id, "deliverable_id": "nope" }),
    )
    .await;
    assert!(
        err.message.starts_with("DELIVERABLE_NOT_FOUND:"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn explain_limit_out_of_range_is_invalid_value() {
    let server = server();
    let plan_id = submit(&server).await;
    let err = call_err(
        &server,
        "plan.explain",
        json!({ "plan_id": plan_id, "limit": 1001 }),
    )
    .await;
    assert!(err.message.starts_with("INVALID_VALUE:"), "{}", err.message);
}

#[tokio::test]
async fn explain_markdown_is_the_rendering_of_the_json_value() {
    let server = server();
    let plan_id = submit(&server).await;
    let args = json!({ "plan_id": plan_id, "deliverable_id": "service", "as_of": AS_OF });
    let json_value = call(&server, "plan.explain", args.clone()).await;
    let mut markdown_args = args;
    markdown_args["format"] = json!("markdown");
    let markdown = call(&server, "plan.explain", markdown_args).await;
    let parsed: cpm_planner::explain::Explanation =
        serde_json::from_value(json_value).expect("explanation decodes");
    assert_eq!(
        markdown["markdown"],
        json!(cpm_planner::explain::render_markdown(&parsed))
    );
}

#[tokio::test]
async fn explain_with_two_selectors_is_invalid_params() {
    let server = server();
    let plan_id = submit(&server).await;
    let err = call_err(
        &server,
        "plan.explain",
        json!({ "plan_id": plan_id, "active": true }),
    )
    .await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

// ── CLI ──────────────────────────────────────────────────────────────────────

/// A temporary planner database holding one submitted plan.
struct StoredPlan {
    dir: tempfile::TempDir,
    db: PathBuf,
    plan_id: String,
}

async fn stored_plan() -> StoredPlan {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("planner.db");
    let store = SqlitePlanStore::open(&db).expect("open store");
    let server = PlanServer::new(Arc::new(BasicCpmPlanner::with_store(
        store,
        Arc::new(NullAuditSink),
    )));
    let plan_id = submit(&server).await;
    StoredPlan { dir, db, plan_id }
}

fn cli(plan: &StoredPlan, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cpm-planner"))
        .args(args)
        .env("CPM_PLANNER_DB", &plan.db)
        .env("CPM_PROJECT_ROOT", plan.dir.path())
        .env_remove("CPM_PLANNER_STATE_MODE")
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .expect("run cpm-planner")
}

#[tokio::test]
async fn overview_cli_prints_markdown_by_default() {
    let plan = stored_plan().await;
    let out = cli(&plan, &["overview", "--plan-id", &plan.plan_id]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("Export API contract") && !stdout.starts_with('{'),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn overview_cli_format_json_prints_the_schema_v1_value() {
    let plan = stored_plan().await;
    let out = cli(
        &plan,
        &["overview", "--plan-id", &plan.plan_id, "--format", "json"],
    );
    let value: Value = serde_json::from_slice(&out.stdout).expect("json on stdout");
    assert_eq!(value["schema"], json!("cpm-planner/overview/v1"));
}

#[tokio::test]
async fn explain_cli_reports_the_deliverable() {
    let plan = stored_plan().await;
    let out = cli(
        &plan,
        &[
            "explain",
            "--plan-id",
            &plan.plan_id,
            "--deliverable",
            "service",
            "--format",
            "json",
        ],
    );
    let value: Value = serde_json::from_slice(&out.stdout).expect("json on stdout");
    assert_eq!(value["deliverable"]["id"], json!("service"));
}

#[tokio::test]
async fn overview_cli_error_exits_one_with_the_error_code() {
    let plan = stored_plan().await;
    let out = cli(&plan, &["overview", "--plan-id", "plan_missing"]);
    assert_eq!(
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).contains("PLAN_NOT_FOUND")
        ),
        (Some(1), true)
    );
}

#[tokio::test]
async fn overview_cli_unknown_flag_exits_with_usage() {
    let plan = stored_plan().await;
    let out = cli(&plan, &["overview", "--verbose"]);
    assert_eq!(out.status.code(), Some(2));
}
