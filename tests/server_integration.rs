//! SPEC §33 PA4 — roundtrip + error-mapping tests for the `PlanServer`
//! MCP façade.
//!
//! Each test constructs a `PlanServer` backed by an in-memory
//! `BasicCpmPlanner`, invokes one tool via the transport-free
//! `dispatch_call` entry point (the same pattern
//! `praxec-mcp-server`'s tests use), and asserts on the JSON
//! response shape — including the stable error-code prefixes
//! (LOCK_NOT_HELD, INVALID_GRAPH, …) on the failure paths.
//!
//! No external transport (stdio / streamable-http) is required: the
//! `dispatch_call` API is the documented test seam for this server.

use std::sync::Arc;

use cpm_planner::{
    BasicCpmPlanner, PlanServer, TOOL_ACQUIRE_COHORT, TOOL_FORCE_RELEASE, TOOL_HEARTBEAT,
    TOOL_MARK_STATUS, TOOL_STATUS, TOOL_SUBMIT,
};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Value, json};

// ── Helpers ──────────────────────────────────────────────────────────────────

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

fn sample_graph() -> Value {
    json!({
        "deliverables": [
            {
                "id": "d1",
                "owned_files": ["src/a.rs"],
                "prerequisites": [],
                "estimated_effort_hours": 1.0,
                "metadata": { "description": "first" }
            },
            {
                "id": "d2",
                "owned_files": ["src/b.rs"],
                "prerequisites": ["d1"],
                "estimated_effort_hours": 2.0,
                "metadata": { "description": "second" }
            }
        ],
        "max_chained_dispatch": null
    })
}

async fn submit_plan(server: &PlanServer) -> String {
    let req = call_args(TOOL_SUBMIT, json!({ "graph": sample_graph() }));
    let resp = server
        .dispatch_call(req)
        .await
        .expect("plan.submit returns Ok");
    resp["plan_id"]
        .as_str()
        .expect("plan_id is a string")
        .to_string()
}

// ── Roundtrip: plan.submit ──────────────────────────────────────────────────

#[tokio::test]
async fn plan_submit_roundtrip() {
    let server = server();
    let req = call_args(TOOL_SUBMIT, json!({ "graph": sample_graph() }));
    let resp = server
        .dispatch_call(req)
        .await
        .expect("plan.submit returns Ok");
    let plan_id = resp["plan_id"].as_str().expect("plan_id field present");
    assert!(!plan_id.is_empty(), "plan_id must be non-empty");
    assert!(
        plan_id.starts_with("plan_"),
        "plan_id should carry the BasicCpmPlanner `plan_<uuid>` prefix; got {plan_id}"
    );
}

// ── Roundtrip: plan.acquire_cohort ──────────────────────────────────────────

#[tokio::test]
async fn plan_acquire_cohort_roundtrip() {
    let server = server();
    let plan_id = submit_plan(&server).await;

    let req = call_args(
        TOOL_ACQUIRE_COHORT,
        json!({
            "plan_id": plan_id,
            "caller_id": "orchestrator-001",
            "max_count": 4
        }),
    );
    let resp = server
        .dispatch_call(req)
        .await
        .expect("plan.acquire_cohort returns Ok");

    assert_eq!(resp["plan_id"].as_str(), Some(plan_id.as_str()));
    let deliverables = resp["deliverables"]
        .as_array()
        .expect("deliverables is an array");
    let locks = resp["locks"].as_array().expect("locks is an array");
    // d1 is the only Ready deliverable; d2 is Pending until d1 completes.
    assert_eq!(
        deliverables.len(),
        1,
        "only d1 should be ready in the initial cohort"
    );
    assert_eq!(deliverables[0]["id"].as_str(), Some("d1"));
    assert_eq!(locks.len(), 1, "one lock per acquired deliverable");
    assert_eq!(locks[0]["deliverable_id"].as_str(), Some("d1"));
    assert_eq!(locks[0]["caller_id"].as_str(), Some("orchestrator-001"));
    // A cohort that yielded work is NOT exhausted (declarative-driver signal).
    assert_eq!(
        resp["exhausted"].as_bool(),
        Some(false),
        "a cohort with a ready deliverable is not exhausted"
    );
}

// A drained acquisition (nothing ready — d1 still locked/in-progress, d2 blocked
// on it) reports `exhausted: true`, the scalar a state-machine loop guards on to
// terminate (it cannot test array-emptiness in a guard expr).
#[tokio::test]
async fn plan_acquire_cohort_reports_exhausted_when_nothing_ready() {
    let server = server();
    let plan_id = submit_plan(&server).await;

    let acquire = |caller: &str| {
        call_args(
            TOOL_ACQUIRE_COHORT,
            json!({ "plan_id": plan_id, "caller_id": caller, "max_count": 4 }),
        )
    };

    // First acquire takes d1 (now in-progress); d2 is blocked on d1.
    let first = server.dispatch_call(acquire("orch-1")).await.unwrap();
    assert_eq!(first["exhausted"].as_bool(), Some(false));

    // Second acquire: nothing is ready → empty cohort → exhausted.
    let second = server.dispatch_call(acquire("orch-1")).await.unwrap();
    assert_eq!(
        second["deliverables"].as_array().map(|a| a.len()),
        Some(0),
        "no deliverable is ready on the second acquire"
    );
    assert_eq!(
        second["exhausted"].as_bool(),
        Some(true),
        "a drained acquire must report exhausted"
    );
}

// ── Roundtrip: plan.heartbeat ───────────────────────────────────────────────

#[tokio::test]
async fn plan_heartbeat_roundtrip() {
    let server = server();
    let plan_id = submit_plan(&server).await;

    // Acquire first so the heartbeat target lock exists.
    let _ = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({
                "plan_id": plan_id,
                "caller_id": "orchestrator-001",
                "max_count": 4
            }),
        ))
        .await
        .expect("acquire ok");

    let resp = server
        .dispatch_call(call_args(
            TOOL_HEARTBEAT,
            json!({
                "plan_id": plan_id,
                "deliverable_id": "d1",
                "caller_id": "orchestrator-001"
            }),
        ))
        .await
        .expect("plan.heartbeat returns Ok");
    assert_eq!(resp["ok"].as_bool(), Some(true));
}

// ── Roundtrip: plan.mark_status (complete releases the lock) ────────────────

#[tokio::test]
async fn plan_mark_status_complete_roundtrip() {
    let server = server();
    let plan_id = submit_plan(&server).await;

    // Acquire d1.
    let _ = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({
                "plan_id": plan_id,
                "caller_id": "orchestrator-001",
                "max_count": 4
            }),
        ))
        .await
        .expect("acquire ok");

    // Mark complete.
    let resp = server
        .dispatch_call(call_args(
            TOOL_MARK_STATUS,
            json!({
                "plan_id": plan_id,
                "deliverable_id": "d1",
                "caller_id": "orchestrator-001",
                "status": { "status": "complete" }
            }),
        ))
        .await
        .expect("plan.mark_status returns Ok");
    assert_eq!(resp["ok"].as_bool(), Some(true));

    // Verify the lock was released by inspecting status.
    let status = server
        .dispatch_call(call_args(TOOL_STATUS, json!({ "plan_id": plan_id })))
        .await
        .expect("plan.status returns Ok");
    let locks = status["locks_held"]
        .as_array()
        .expect("locks_held array present");
    assert!(
        locks.is_empty(),
        "completing d1 should have released its lock; got {locks:?}"
    );
}

// ── Roundtrip: plan.status ──────────────────────────────────────────────────

#[tokio::test]
async fn plan_status_roundtrip() {
    let server = server();
    let plan_id = submit_plan(&server).await;

    let resp = server
        .dispatch_call(call_args(TOOL_STATUS, json!({ "plan_id": plan_id })))
        .await
        .expect("plan.status returns Ok");

    assert_eq!(resp["plan_id"].as_str(), Some(plan_id.as_str()));

    let deliverables = resp["deliverables"]
        .as_array()
        .expect("deliverables is an array");
    assert_eq!(deliverables.len(), 2, "graph has two deliverables");
    // Wire format is Vec<(String, DeliverableStatus)> -> array of [id, status].
    let first = &deliverables[0];
    assert_eq!(first[0].as_str(), Some("d1"));
    assert_eq!(first[1]["status"].as_str(), Some("ready"));

    let cp = resp["critical_path"]
        .as_array()
        .expect("critical_path is an array");
    assert!(
        !cp.is_empty(),
        "critical_path must be populated for a non-empty graph"
    );
    // CPM should put d1 -> d2 on the critical path (effort 1.0 + 2.0 = 3.0).
    assert!(
        resp["critical_path_hours"]
            .as_f64()
            .map(|h| h > 0.0)
            .unwrap_or(false),
        "critical_path_hours must be positive; got {}",
        resp["critical_path_hours"]
    );
}

// ── Roundtrip: plan.force_release ───────────────────────────────────────────

#[tokio::test]
async fn plan_force_release_roundtrip() {
    let server = server();
    let plan_id = submit_plan(&server).await;

    // Acquire d1 first so there is a lock to force-release.
    let _ = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({
                "plan_id": plan_id,
                "caller_id": "orchestrator-001",
                "max_count": 4
            }),
        ))
        .await
        .expect("acquire ok");

    let resp = server
        .dispatch_call(call_args(
            TOOL_FORCE_RELEASE,
            json!({
                "plan_id": plan_id,
                "deliverable_id": "d1",
                "reason": "orchestrator crashed; releasing manually"
            }),
        ))
        .await
        .expect("plan.force_release returns Ok");
    assert_eq!(resp["ok"].as_bool(), Some(true));

    // Confirm the lock is gone.
    let status = server
        .dispatch_call(call_args(TOOL_STATUS, json!({ "plan_id": plan_id })))
        .await
        .expect("status ok");
    let locks = status["locks_held"]
        .as_array()
        .expect("locks_held array present");
    assert!(
        locks.is_empty(),
        "force_release should have removed the lock; got {locks:?}"
    );
}

// ── Error mapping: INVALID_GRAPH on cycles ──────────────────────────────────

#[tokio::test]
async fn plan_invalid_graph_returns_error() {
    let server = server();
    let cyclic = json!({
        "deliverables": [
            { "id": "a", "owned_files": ["src/a.rs"], "prerequisites": ["b"] },
            { "id": "b", "owned_files": ["src/b.rs"], "prerequisites": ["a"] }
        ]
    });
    let err = server
        .dispatch_call(call_args(TOOL_SUBMIT, json!({ "graph": cyclic })))
        .await
        .expect_err("cyclic graph must be rejected");
    assert!(
        err.message.contains("INVALID_GRAPH"),
        "MCP error must carry the INVALID_GRAPH prefix; got: {}",
        err.message
    );
}

// ── Error mapping: LOCK_NOT_HELD on wrong caller ────────────────────────────

#[tokio::test]
async fn plan_wrong_caller_returns_lock_not_held() {
    let server = server();
    let plan_id = submit_plan(&server).await;

    // Acquire under one caller.
    let _ = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({
                "plan_id": plan_id,
                "caller_id": "owner-001",
                "max_count": 4
            }),
        ))
        .await
        .expect("acquire ok");

    // Attempt to mark complete from a different caller.
    let err = server
        .dispatch_call(call_args(
            TOOL_MARK_STATUS,
            json!({
                "plan_id": plan_id,
                "deliverable_id": "d1",
                "caller_id": "imposter-002",
                "status": { "status": "complete" }
            }),
        ))
        .await
        .expect_err("wrong caller must be rejected");
    assert!(
        err.message.contains("LOCK_NOT_HELD"),
        "MCP error must carry the LOCK_NOT_HELD prefix; got: {}",
        err.message
    );
}

// ── Wire format: deny_unknown_fields enforced ───────────────────────────────

#[tokio::test]
async fn plan_submit_rejects_unknown_fields() {
    let server = server();
    let err = server
        .dispatch_call(call_args(
            TOOL_SUBMIT,
            json!({
                "graph": sample_graph(),
                "stray_field": "should be rejected"
            }),
        ))
        .await
        .expect_err("unknown fields must be rejected at the wire boundary");
    // The MCP layer wraps serde errors as invalid_params; the message
    // should mention the offending field.
    assert!(
        err.message.contains("stray_field") || err.message.contains("unknown field"),
        "expected wire-level rejection of unknown field; got: {}",
        err.message
    );
}

// ── DeliverableStatus::Failed round-trip ────────────────────────────────────

#[tokio::test]
async fn plan_mark_status_failed_carries_reason() {
    let server = server();
    let plan_id = submit_plan(&server).await;

    // Acquire so we hold the lock with the expected caller_id.
    let _ = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({
                "plan_id": plan_id,
                "caller_id": "orchestrator-001",
                "max_count": 4
            }),
        ))
        .await
        .expect("acquire ok");

    // Mark the deliverable failed with a structured reason.
    let resp = server
        .dispatch_call(call_args(
            TOOL_MARK_STATUS,
            json!({
                "plan_id": plan_id,
                "deliverable_id": "d1",
                "caller_id": "orchestrator-001",
                "status": { "status": "failed", "reason": "intentional test failure" }
            }),
        ))
        .await
        .expect("mark_status failed must succeed when caller holds the lock");
    assert_eq!(resp["ok"], json!(true));

    // Status reflects the Failed variant with the reason preserved.
    let status_resp = server
        .dispatch_call(call_args(TOOL_STATUS, json!({ "plan_id": plan_id })))
        .await
        .expect("status ok");
    let deliverables = status_resp["deliverables"]
        .as_array()
        .expect("deliverables array present");
    let d1 = deliverables
        .iter()
        .find(|row| row[0].as_str() == Some("d1"))
        .expect("d1 entry present");
    assert_eq!(d1[1]["status"], json!("failed"));
    assert_eq!(d1[1]["reason"], json!("intentional test failure"));
    // Third element of each status row is the lease attempt_count —
    // d1 was leased exactly once before being marked failed.
    assert_eq!(d1[2], json!(1));
}
