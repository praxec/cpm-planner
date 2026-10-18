//! `r040` — MCP-surface plan selection tests (GitHub issue #77, items 1, 2, 4).
//!
//! Drives [`PlanServer::dispatch_call`] over a file-backed planner with an
//! explicit `project` on every call, so no project root is needed. Each test
//! makes one behavioural assertion about the tool surface.

use std::path::PathBuf;
use std::sync::Arc;

use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::server::{
    TOOL_ACQUIRE_COHORT, TOOL_BASELINE, TOOL_EV, TOOL_LIST, TOOL_MARK_STATUS, TOOL_STATUS,
    TOOL_SUBMIT, plan_tool_definitions,
};
use cpm_planner::{BasicCpmPlanner, PlanServer, SqlitePlanStore};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Value, json};

const PROJECT: &str = "proj";

// ── Harness ──────────────────────────────────────────────────────────────────

/// Unique temp db file, removed (with WAL sidecars) on drop.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cpm-planner-server-selector-test-{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        Self { path }
    }

    fn server(&self) -> PlanServer {
        let store = SqlitePlanStore::open(&self.path).expect("open store");
        PlanServer::new(Arc::new(BasicCpmPlanner::with_store(
            store,
            Arc::new(MemoryAuditSink::new()),
        )))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }
}

fn call_args(name: &str, args: Value) -> CallToolRequestParams {
    let map: JsonObject = match args {
        Value::Object(m) => m,
        _ => panic!("call_args expects a JSON object"),
    };
    CallToolRequestParams::new(name.to_string()).with_arguments(map)
}

/// A one-deliverable plan whose single deliverable is `id`.
fn one(id: &str) -> Value {
    json!({
        "deliverables": [
            {
                "id": id,
                "owned_files": [format!("src/{id}.rs")],
                "prerequisites": [],
                "estimated_effort_hours": 1.0
            }
        ],
        "max_chained_dispatch": null
    })
}

/// Register a named variant through `plan.submit` and return its plan id.
async fn sync_line(server: &PlanServer, name: &str, variant: &str, graph: Value) -> String {
    let req = call_args(
        TOOL_SUBMIT,
        json!({ "graph": graph, "project": PROJECT, "name": name, "variant": variant }),
    );
    let resp = server
        .dispatch_call(req)
        .await
        .expect("plan.submit of a named variant returns Ok");
    resp["plan_id"]
        .as_str()
        .expect("sync outcome carries a plan_id")
        .to_string()
}

/// Baseline a plan through the MCP surface (setup for `plan.ev`).
async fn baseline(server: &PlanServer, plan_id: &str) {
    server
        .dispatch_call(call_args(TOOL_BASELINE, json!({ "plan_id": plan_id })))
        .await
        .expect("plan.baseline returns Ok");
}

// ── plan.status: selection equivalence ───────────────────────────────────────

#[tokio::test]
async fn status_by_line_matches_explicit_plan_id() {
    let db = TempDb::new();
    let server = db.server();
    let id = sync_line(&server, "web", "main", one("a")).await;

    let by_line = server
        .dispatch_call(call_args(
            TOOL_STATUS,
            json!({ "plan": "web", "project": PROJECT }),
        ))
        .await
        .expect("plan.status by line returns Ok");
    let by_id = server
        .dispatch_call(call_args(TOOL_STATUS, json!({ "plan_id": id })))
        .await
        .expect("plan.status by id returns Ok");

    assert_eq!(by_line["plan_id"], by_id["plan_id"]);
}

// ── plan.ev: active selection ────────────────────────────────────────────────

#[tokio::test]
async fn ev_by_active_reports_the_live_plan_budget() {
    let db = TempDb::new();
    let server = db.server();
    let id = sync_line(&server, "web", "main", one("a")).await;
    baseline(&server, &id).await;

    let report = server
        .dispatch_call(call_args(
            TOOL_EV,
            json!({ "active": true, "project": PROJECT }),
        ))
        .await
        .expect("plan.ev by active returns Ok");

    assert_eq!(report["bac"], json!(1.0));
}

#[tokio::test]
async fn ev_by_active_with_no_candidate_is_no_active_plan() {
    let db = TempDb::new();
    let server = db.server();

    let err = server
        .dispatch_call(call_args(
            TOOL_EV,
            json!({ "active": true, "project": PROJECT }),
        ))
        .await
        .expect_err("no live line must fail");

    assert!(err.message.starts_with("NO_ACTIVE_PLAN"));
}

#[tokio::test]
async fn ev_by_active_with_two_candidates_is_ambiguous() {
    let db = TempDb::new();
    let server = db.server();
    let _alpha = sync_line(&server, "alpha", "main", one("a")).await;
    let _beta = sync_line(&server, "beta", "main", one("b")).await;

    let err = server
        .dispatch_call(call_args(
            TOOL_EV,
            json!({ "active": true, "project": PROJECT }),
        ))
        .await
        .expect_err("two live lines must fail");

    assert!(err.message.starts_with("AMBIGUOUS_ACTIVE_PLAN"));
}

// ── Selector validation ──────────────────────────────────────────────────────

#[tokio::test]
async fn plan_id_and_plan_together_are_invalid_params() {
    let db = TempDb::new();
    let server = db.server();

    let err = server
        .dispatch_call(call_args(
            TOOL_STATUS,
            json!({ "plan_id": "plan_x", "plan": "web", "project": PROJECT }),
        ))
        .await
        .expect_err("two selectors must fail");

    assert_eq!(
        err.message,
        "provide exactly one of plan_id, plan, or active: true"
    );
}

#[tokio::test]
async fn no_selector_is_invalid_params() {
    let db = TempDb::new();
    let server = db.server();

    let err = server
        .dispatch_call(call_args(TOOL_STATUS, json!({})))
        .await
        .expect_err("no selector must fail");

    assert_eq!(
        err.message,
        "provide exactly one of plan_id, plan, or active: true"
    );
}

#[tokio::test]
async fn project_with_plan_id_is_invalid_params() {
    let db = TempDb::new();
    let server = db.server();

    let err = server
        .dispatch_call(call_args(
            TOOL_STATUS,
            json!({ "plan_id": "plan_x", "project": PROJECT }),
        ))
        .await
        .expect_err("project with plan_id must fail");

    assert_eq!(err.message, "project applies only to plan or active");
}

// ── Execution tools by selector ──────────────────────────────────────────────

#[tokio::test]
async fn acquire_cohort_by_line_leases_from_the_selected_variant() {
    let db = TempDb::new();
    let server = db.server();
    let _id = sync_line(&server, "web", "main", one("a")).await;

    let cohort = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({
                "plan": "web", "project": PROJECT,
                "caller_id": "w", "max_count": 4
            }),
        ))
        .await
        .expect("plan.acquire_cohort by line returns Ok");

    assert_eq!(cohort["deliverables"][0]["id"], json!("a"));
}

#[tokio::test]
async fn mark_status_by_active_completes_a_leased_deliverable() {
    let db = TempDb::new();
    let server = db.server();
    let id = sync_line(&server, "web", "main", one("a")).await;
    server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({
                "plan_id": id, "caller_id": "w", "max_count": 4
            }),
        ))
        .await
        .expect("acquire returns Ok");

    server
        .dispatch_call(call_args(
            TOOL_MARK_STATUS,
            json!({
                "active": true, "project": PROJECT,
                "deliverable_id": "a", "caller_id": "w",
                "status": { "status": "complete" }
            }),
        ))
        .await
        .expect("plan.mark_status by active returns Ok");

    let snapshot = server
        .dispatch_call(call_args(TOOL_STATUS, json!({ "plan_id": id })))
        .await
        .expect("plan.status returns Ok");
    assert_eq!(snapshot["plan_complete"], json!(true));
}

// ── plan.list: active filter ─────────────────────────────────────────────────

#[tokio::test]
async fn list_active_omits_an_archived_line() {
    let db = TempDb::new();
    let server = db.server();
    let _live = sync_line(&server, "live", "main", one("a")).await;
    let _old = sync_line(&server, "old", "main", one("b")).await;
    server
        .dispatch_call(call_args(
            "plan.archive",
            json!({ "name": "old", "project": PROJECT }),
        ))
        .await
        .expect("plan.archive returns Ok");

    let lines = server
        .dispatch_call(call_args(
            TOOL_LIST,
            json!({ "active": true, "project": PROJECT }),
        ))
        .await
        .expect("plan.list by active returns Ok");

    let names: Vec<&str> = lines
        .as_array()
        .expect("list is an array")
        .iter()
        .map(|line| line["name"].as_str().expect("line name"))
        .collect();
    assert_eq!(names, vec!["live"]);
}

#[tokio::test]
async fn list_active_with_include_archived_is_invalid_params() {
    let db = TempDb::new();
    let server = db.server();

    let err = server
        .dispatch_call(call_args(
            TOOL_LIST,
            json!({ "active": true, "include_archived": true, "project": PROJECT }),
        ))
        .await
        .expect_err("active with include_archived must fail");

    assert_eq!(
        err.message,
        "active and include_archived are mutually exclusive"
    );
}

// ── Advertised schema ────────────────────────────────────────────────────────

#[test]
fn plan_status_schema_no_longer_requires_plan_id() {
    let defs = plan_tool_definitions();
    let status = defs
        .iter()
        .find(|t| t.name == "plan.status")
        .expect("plan.status advertised");

    let required = status
        .input_schema
        .get("required")
        .and_then(|value| value.as_array());

    assert!(!required.is_some_and(|names| names.contains(&json!("plan_id"))));
}
