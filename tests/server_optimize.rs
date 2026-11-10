//! `plan.optimize` through the MCP server (docs/optimize.md) and the
//! `cpm-planner optimize` CLI command: the graph selector, leveling inputs,
//! the proposal cap, and error mapping like `plan.simulate`.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;

use cpm_planner::optimize::OptimizeRequest;
use cpm_planner::plan::PlanGraph;
use cpm_planner::project::ProjectRoot;
use cpm_planner::resource_schedule::ScheduleRequest;
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

async fn optimize(server: &PlanServer, args: Value) -> Value {
    server
        .dispatch_call(call_args("plan.optimize", args))
        .await
        .unwrap_or_else(|e| panic!("plan.optimize failed: {}", e.message))
}

async fn optimize_err(server: &PlanServer, args: Value) -> rmcp::ErrorData {
    server
        .dispatch_call(call_args("plan.optimize", args))
        .await
        .expect_err("plan.optimize should fail")
}

/// `parse -> cli` joined by an edge with no stated artifact (a
/// `false_dependency` candidate), both feeding milestone `ship`.
fn graph() -> Value {
    json!({
        "deliverables": [
            { "id": "parse", "owned_files": ["src/parse.rs"], "prerequisites": [],
              "estimated_effort_hours": 3.0, "metadata": { "owner": "agent" } },
            { "id": "cli", "owned_files": ["src/cli.rs"], "prerequisites": ["parse"],
              "estimated_effort_hours": 2.0, "metadata": { "owner": "agent" } },
            { "id": "ship", "owned_files": [],
              "prerequisites": [{ "id": "cli", "consumes": "the cli binary" }],
              "milestone": true, "metadata": {} }
        ],
        "max_chained_dispatch": null
    })
}

fn typed_graph() -> PlanGraph {
    serde_json::from_value(graph()).expect("graph decodes")
}

fn library(req: &OptimizeRequest) -> Value {
    serde_json::to_value(cpm_planner::optimize::optimize(&typed_graph(), req).expect("optimize"))
        .expect("report serialises")
}

#[tokio::test]
async fn optimize_inline_graph_equals_the_library_report() {
    let resp = optimize(&server(), json!({ "graph": graph() })).await;
    assert_eq!(resp, library(&OptimizeRequest::default()));
}

#[tokio::test]
async fn optimize_finds_the_false_dependency() {
    let resp = optimize(&server(), json!({ "graph": graph() })).await;
    let kinds: Vec<&str> = resp["proposals"]
        .as_array()
        .expect("proposals")
        .iter()
        .filter_map(|p| p["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"false_dependency"), "{resp}");
}

#[tokio::test]
async fn optimize_by_plan_id_equals_the_inline_graph() {
    let server = server();
    let submitted = server
        .dispatch_call(call_args("plan.submit", json!({ "graph": graph() })))
        .await
        .expect("submit");
    let by_id = optimize(&server, json!({ "plan_id": submitted["plan_id"] })).await;
    assert_eq!(by_id, library(&OptimizeRequest::default()));
}

#[tokio::test]
async fn optimize_by_path_reads_the_plan_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plans = dir.path().join(".cpm-planner/plans/cli");
    std::fs::create_dir_all(&plans).expect("plans dir");
    std::fs::write(plans.join("main.json"), graph().to_string()).expect("plan file");
    let root = ProjectRoot::from_path(dir.path()).expect("project root");
    let server = PlanServer::new(Arc::new(BasicCpmPlanner::new().with_project_root(root)));
    let resp = optimize(
        &server,
        json!({ "path": ".cpm-planner/plans/cli/main.json" }),
    )
    .await;
    assert_eq!(resp, library(&OptimizeRequest::default()));
}

#[tokio::test]
async fn optimize_with_capacities_equals_the_leveled_library_report() {
    let resp = optimize(
        &server(),
        json!({ "graph": graph(), "capacities": { "agent": 1, "unassigned": 1 } }),
    )
    .await;
    let req = OptimizeRequest {
        schedule: Some(ScheduleRequest {
            capacities: BTreeMap::from([("agent".to_string(), 1), ("unassigned".to_string(), 1)]),
            resource_key: "owner".to_string(),
            project_buffer_pct: 25.0,
        }),
        max_proposals: None,
    };
    assert_eq!(resp, library(&req));
}

#[tokio::test]
async fn optimize_with_capacities_reports_the_resource_makespan() {
    let resp = optimize(
        &server(),
        json!({ "graph": graph(), "capacities": { "agent": 1, "unassigned": 1 } }),
    )
    .await;
    assert!(resp["baseline"]["resource_makespan"].is_number(), "{resp}");
}

#[tokio::test]
async fn optimize_max_proposals_caps_the_proposals() {
    let resp = optimize(&server(), json!({ "graph": graph(), "max_proposals": 1 })).await;
    let req = OptimizeRequest {
        schedule: None,
        max_proposals: Some(1),
    };
    assert_eq!(resp, library(&req));
}

#[tokio::test]
async fn optimize_max_proposals_zero_is_invalid_params() {
    let err = optimize_err(&server(), json!({ "graph": graph(), "max_proposals": 0 })).await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn optimize_without_a_selector_is_invalid_params() {
    let err = optimize_err(&server(), json!({})).await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn optimize_with_two_selectors_is_invalid_params() {
    let err = optimize_err(&server(), json!({ "graph": graph(), "plan_id": "plan_x" })).await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn optimize_resource_key_without_capacities_is_invalid_params() {
    let err = optimize_err(
        &server(),
        json!({ "graph": graph(), "resource_key": "team" }),
    )
    .await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn optimize_buffer_out_of_range_is_invalid_params() {
    let err = optimize_err(
        &server(),
        json!({ "graph": graph(), "capacities": { "agent": 1 }, "project_buffer_pct": 101 }),
    )
    .await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn optimize_missing_capacity_is_invalid_capacities() {
    let err = optimize_err(
        &server(),
        json!({ "graph": graph(), "capacities": { "qa": 1 } }),
    )
    .await;
    assert!(
        err.message.starts_with("INVALID_CAPACITIES:"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn optimize_cyclic_graph_is_invalid_graph_like_simulate() {
    let cyclic = json!({
        "deliverables": [
            { "id": "a", "owned_files": [], "prerequisites": ["b"], "estimated_effort_hours": 1.0 },
            { "id": "b", "owned_files": [], "prerequisites": ["a"], "estimated_effort_hours": 1.0 }
        ],
        "max_chained_dispatch": null
    });
    let server = server();
    let optimize_message = optimize_err(&server, json!({ "graph": cyclic.clone() }))
        .await
        .message;
    let simulate_message = server
        .dispatch_call(call_args("plan.simulate", json!({ "graph": cyclic })))
        .await
        .expect_err("simulate refuses a cycle")
        .message;
    assert_eq!(optimize_message, simulate_message);
}

#[tokio::test]
async fn optimize_unknown_plan_is_plan_not_found() {
    let err = optimize_err(&server(), json!({ "plan_id": "plan_missing" })).await;
    assert!(
        err.message.starts_with("PLAN_NOT_FOUND:"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn optimize_unknown_argument_is_invalid_params() {
    let err = optimize_err(&server(), json!({ "graph": graph(), "apply": true })).await;
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
}

#[test]
fn optimize_cli_prints_the_report_for_a_graph_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("graph.json");
    std::fs::write(&file, graph().to_string()).expect("graph file");
    let out = Command::new(env!("CARGO_BIN_EXE_cpm-planner"))
        .args(["optimize", "--graph", file.to_str().expect("utf-8 path")])
        .env("CPM_PLANNER_DB", dir.path().join("unused.db"))
        .env_remove("CPM_PROJECT_ROOT")
        .env_remove("OPENROUTER_API_KEY")
        .current_dir(dir.path())
        .output()
        .expect("run cpm-planner");
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!("stderr: {}", String::from_utf8_lossy(&out.stderr));
    });
    assert_eq!(value, library(&OptimizeRequest::default()));
}

#[test]
fn optimize_cli_of_a_graph_file_creates_no_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("graph.json");
    std::fs::write(&file, graph().to_string()).expect("graph file");
    let db = dir.path().join("unused.db");
    Command::new(env!("CARGO_BIN_EXE_cpm-planner"))
        .args(["optimize", "--graph", file.to_str().expect("utf-8 path")])
        .env("CPM_PLANNER_DB", &db)
        .env_remove("CPM_PROJECT_ROOT")
        .current_dir(dir.path())
        .output()
        .expect("run cpm-planner");
    assert!(!db.exists());
}
