//! Thread affinity through the MCP server (docs/threads.md): `thread` and
//! `affinity_seconds` on `plan.acquire_cohort`, `AFFINITY_HOLD` in
//! `blocked`, and `threads` in `plan.status`.

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

async fn ok(server: &PlanServer, tool: &str, args: Value) -> Value {
    server
        .dispatch_call(call_args(tool, args))
        .await
        .unwrap_or_else(|e| panic!("{tool} failed: {}", e.message))
}

async fn err(server: &PlanServer, tool: &str, args: Value) -> rmcp::ErrorData {
    server
        .dispatch_call(call_args(tool, args))
        .await
        .expect_err("call should fail")
}

fn deliverable(id: &str, prereqs: &[&str], hours: f32, metadata: Value) -> Value {
    json!({
        "id": id,
        "owned_files": [format!("src/{id}.rs")],
        "prerequisites": prereqs
            .iter()
            .map(|p| json!({ "id": p, "consumes": format!("the {p} module") }))
            .collect::<Vec<_>>(),
        "estimated_effort_hours": hours,
        "metadata": metadata
    })
}

/// `t1 -> t2` in thread `x`, `s1` alone in thread `y`, and `u`, an
/// unthreaded long deliverable that outranks the others by priority.
fn threaded_graph() -> Value {
    json!({
        "deliverables": [
            deliverable("t1", &[], 1.0, json!({ "thread": "x" })),
            deliverable("t2", &["t1"], 1.0, json!({ "thread": "x" })),
            deliverable("s1", &[], 1.0, json!({ "thread": "y" })),
            deliverable("u", &[], 10.0, json!({}))
        ],
        "max_chained_dispatch": null
    })
}

fn unthreaded_graph() -> Value {
    json!({
        "deliverables": [
            deliverable("a", &[], 1.0, json!({})),
            deliverable("b", &[], 5.0, json!({})),
            deliverable("c", &["a"], 1.0, json!({}))
        ],
        "max_chained_dispatch": null
    })
}

async fn submit(server: &PlanServer, graph: Value) -> String {
    let resp = ok(server, "plan.submit", json!({ "graph": graph })).await;
    resp["plan_id"].as_str().expect("plan_id").to_string()
}

async fn acquire(server: &PlanServer, plan_id: &str, extra: Value) -> Value {
    let mut args = json!({ "plan_id": plan_id, "caller_id": "agent-a" });
    if let (Some(args), Value::Object(extra)) = (args.as_object_mut(), extra) {
        args.extend(extra);
    }
    ok(server, "plan.acquire_cohort", args).await
}

fn leased_ids(cohort: &Value) -> Vec<String> {
    cohort["deliverables"]
        .as_array()
        .expect("deliverables")
        .iter()
        .filter_map(|d| d["id"].as_str())
        .map(str::to_string)
        .collect()
}

/// `agent-a` leases and completes `t1`, so it last advanced thread `x`.
async fn advance_thread_x(server: &PlanServer, plan_id: &str) {
    ok(
        server,
        "plan.acquire_cohort",
        json!({ "plan_id": plan_id, "caller_id": "agent-a", "ids": ["t1"] }),
    )
    .await;
    ok(
        server,
        "plan.mark_status",
        json!({ "plan_id": plan_id, "deliverable_id": "t1", "caller_id": "agent-a",
                "status": { "status": "complete" } }),
    )
    .await;
}

#[tokio::test]
async fn acquire_with_thread_offers_that_thread_first() {
    let server = server();
    let plan_id = submit(&server, threaded_graph()).await;
    let cohort = acquire(&server, &plan_id, json!({ "thread": "y" })).await;
    assert_eq!(leased_ids(&cohort), vec!["s1"]);
}

#[tokio::test]
async fn acquire_without_thread_offers_the_callers_own_thread_first() {
    let server = server();
    let plan_id = submit(&server, threaded_graph()).await;
    advance_thread_x(&server, &plan_id).await;
    let cohort = acquire(&server, &plan_id, json!({})).await;
    assert_eq!(leased_ids(&cohort), vec!["t2"]);
}

#[tokio::test]
async fn acquire_affinity_seconds_holds_another_callers_thread() {
    let server = server();
    let plan_id = submit(&server, threaded_graph()).await;
    advance_thread_x(&server, &plan_id).await;
    let cohort = ok(
        &server,
        "plan.acquire_cohort",
        json!({ "plan_id": plan_id, "caller_id": "agent-b", "ids": ["t2"],
                "affinity_seconds": 600 }),
    )
    .await;
    let codes: Vec<&str> = cohort["blocked"]
        .as_array()
        .expect("blocked")
        .iter()
        .filter_map(|b| b["code"].as_str())
        .collect();
    assert_eq!(codes, vec!["AFFINITY_HOLD"]);
}

#[tokio::test]
async fn acquire_affinity_seconds_zero_holds_nothing() {
    let server = server();
    let plan_id = submit(&server, threaded_graph()).await;
    advance_thread_x(&server, &plan_id).await;
    let cohort = ok(
        &server,
        "plan.acquire_cohort",
        json!({ "plan_id": plan_id, "caller_id": "agent-b", "ids": ["t2"],
                "affinity_seconds": 0 }),
    )
    .await;
    assert_eq!(leased_ids(&cohort), vec!["t2"]);
}

#[tokio::test]
async fn acquire_invalid_thread_token_is_invalid_value() {
    let server = server();
    let plan_id = submit(&server, threaded_graph()).await;
    let e = err(
        &server,
        "plan.acquire_cohort",
        json!({ "plan_id": plan_id, "caller_id": "agent-a", "thread": "-bad token" }),
    )
    .await;
    assert!(e.message.starts_with("INVALID_VALUE:"), "{}", e.message);
}

#[tokio::test]
async fn acquire_negative_affinity_seconds_is_invalid_params() {
    let server = server();
    let plan_id = submit(&server, threaded_graph()).await;
    let e = err(
        &server,
        "plan.acquire_cohort",
        json!({ "plan_id": plan_id, "caller_id": "agent-a", "affinity_seconds": -1 }),
    )
    .await;
    assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn acquire_on_an_unthreaded_plan_leases_in_the_usual_order() {
    let plain_server = server();
    let plain_id = submit(&plain_server, unthreaded_graph()).await;
    let plain = acquire(&plain_server, &plain_id, json!({ "max_count": 3 })).await;
    let affine_server = server();
    let affine_id = submit(&affine_server, unthreaded_graph()).await;
    let affine = acquire(
        &affine_server,
        &affine_id,
        json!({ "max_count": 3, "thread": "x", "affinity_seconds": 600 }),
    )
    .await;
    assert_eq!(leased_ids(&affine), leased_ids(&plain));
}

#[tokio::test]
async fn status_reports_threads_of_a_threaded_plan() {
    let server = server();
    let plan_id = submit(&server, threaded_graph()).await;
    advance_thread_x(&server, &plan_id).await;
    let status = ok(&server, "plan.status", json!({ "plan_id": plan_id })).await;
    assert_eq!(
        status["threads"][0],
        json!({ "thread": "x", "holder": null, "affine_caller": "agent-a",
                "next_ready": "t2", "completed": 1, "total": 2 })
    );
}

#[tokio::test]
async fn status_omits_threads_for_an_unthreaded_plan() {
    let server = server();
    let plan_id = submit(&server, unthreaded_graph()).await;
    let status = ok(&server, "plan.status", json!({ "plan_id": plan_id })).await;
    assert!(status.get("threads").is_none(), "{status}");
}

#[test]
fn acquire_schema_advertises_thread_and_affinity_seconds() {
    let tools = cpm_planner::server::plan_tool_definitions();
    let acquire = tools
        .iter()
        .find(|t| t.name == "plan.acquire_cohort")
        .expect("plan.acquire_cohort advertised");
    let properties = &acquire.input_schema["properties"];
    assert_eq!(
        (
            properties["thread"]["type"].clone(),
            properties["affinity_seconds"]["type"].clone()
        ),
        (json!("string"), json!("integer"))
    );
}
