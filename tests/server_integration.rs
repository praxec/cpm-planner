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
    BasicCpmPlanner, PlanServer, TOOL_ACCEPT, TOOL_ACQUIRE_COHORT, TOOL_FORCE_RELEASE,
    TOOL_HEARTBEAT, TOOL_MARK_STATUS, TOOL_STATUS, TOOL_SUBMIT,
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

// Wire shape of a status row: [id, status, attempt_count, failure_count,
// lapse_count] — trailing counters appended so positional readers of the
// historical 3-element rows keep working. An explicit mark_status failed
// shows up in failure_count (index 3), never lapse_count (index 4).
#[tokio::test]
async fn plan_status_rows_expose_failure_and_lapse_counters() {
    let server = server();
    let plan_id = submit_plan(&server).await;

    let _ = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({ "plan_id": plan_id, "caller_id": "orch-1", "max_count": 1 }),
        ))
        .await
        .expect("acquire ok");
    let _ = server
        .dispatch_call(call_args(
            TOOL_MARK_STATUS,
            json!({
                "plan_id": plan_id,
                "deliverable_id": "d1",
                "caller_id": "orch-1",
                "status": { "status": "failed", "reason": "real implementation failure" }
            }),
        ))
        .await
        .expect("mark failed ok");

    let resp = server
        .dispatch_call(call_args(TOOL_STATUS, json!({ "plan_id": plan_id })))
        .await
        .expect("status ok");
    let d1 = resp["deliverables"]
        .as_array()
        .expect("deliverables array")
        .iter()
        .find(|row| row[0].as_str() == Some("d1"))
        .expect("d1 row present");
    assert_eq!(d1.as_array().map(|r| r.len()), Some(5), "five-element row");
    assert_eq!(d1[2], json!(1), "one lease");
    assert_eq!(d1[3], json!(1), "one explicit failed attempt");
    assert_eq!(d1[4], json!(0), "no environmental lapse");
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

#[tokio::test]
async fn plan_force_release_accepts_reset_counters() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let result = server
        .dispatch_call(call_args(
            TOOL_FORCE_RELEASE,
            json!({
                "plan_id": plan_id,
                "deliverable_id": "d1",
                "reason": "reset",
                "reset_counters": true
            }),
        ))
        .await
        .expect("plan.force_release accepts reset_counters");
    assert_eq!(result["ok"], true);
}

// ── Roundtrip: plan.accept ──────────────────────────────────────────────────

#[tokio::test]
async fn plan_accept_roundtrip() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let resp = server
        .dispatch_call(call_args(
            TOOL_ACCEPT,
            json!({
                "plan_id": plan_id,
                "deliverable_id": "d1",
                "accepted_by": "owner",
                "evidence": "reviewed"
            }),
        ))
        .await
        .expect("plan.accept returns Ok");
    assert_eq!(resp["ok"], true);
}

// ── Roundtrip: plan.get ─────────────────────────────────────────────────────

#[tokio::test]
async fn plan_get_returns_submitted_graph() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let got = server
        .dispatch_call(call_args("plan.get", json!({ "plan_id": plan_id })))
        .await
        .unwrap();
    assert_eq!(got["graph"]["deliverables"], sample_graph()["deliverables"]);
}

#[tokio::test]
async fn plan_get_unknown_plan_is_an_error() {
    let server = server();
    let err = server
        .dispatch_call(call_args("plan.get", json!({ "plan_id": "plan_missing" })))
        .await
        .unwrap_err();
    assert!(err.message.contains("PLAN_NOT_FOUND"));
}

#[tokio::test]
async fn plan_get_round_trips_mixed_prerequisite_forms() {
    let server = server();
    let deliverables = json!([
        { "id": "a", "owned_files": ["a.rs"], "prerequisites": [] },
        { "id": "c", "owned_files": ["c.rs"], "prerequisites": [] },
        { "id": "d", "owned_files": ["d.rs"],
          "prerequisites": ["a", { "id": "c", "kind": "interface" }] }
    ]);
    let submitted = server
        .dispatch_call(call_args(
            TOOL_SUBMIT,
            json!({ "graph": { "deliverables": deliverables } }),
        ))
        .await
        .unwrap();
    let got = server
        .dispatch_call(call_args(
            "plan.get",
            json!({ "plan_id": submitted["plan_id"] }),
        ))
        .await
        .unwrap();
    assert_eq!(
        got["graph"]["deliverables"][2]["prerequisites"],
        deliverables[2]["prerequisites"]
    );
}

#[tokio::test]
async fn plan_get_round_trips_mixed_file_forms() {
    let server = server();
    let files = json!([
        "a.rs",
        { "path": "b.rs", "mode": "append" },
        { "path": "c.rs" },
        { "path": "d.rs", "mode": "exclusive" }
    ]);
    let submitted = server
        .dispatch_call(call_args(
            TOOL_SUBMIT,
            json!({ "graph": { "deliverables": [
                { "id": "a", "owned_files": files, "prerequisites": [] }
            ] } }),
        ))
        .await
        .unwrap();
    let got = server
        .dispatch_call(call_args(
            "plan.get",
            json!({ "plan_id": submitted["plan_id"] }),
        ))
        .await
        .unwrap();
    assert_eq!(got["graph"]["deliverables"][0]["owned_files"], files);
}

#[tokio::test]
async fn submit_rejects_unknown_prerequisite_field() {
    let server = server();
    let graph = json!({ "deliverables": [
        { "id": "a", "owned_files": ["a.rs"], "prerequisites": [] },
        { "id": "b", "owned_files": ["b.rs"], "prerequisites": [{ "id": "a", "lag_hour": 2 }] }
    ]});
    let result = server
        .dispatch_call(call_args(TOOL_SUBMIT, json!({ "graph": graph })))
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn submit_rejects_unknown_owned_file_field() {
    let server = server();
    let graph = json!({ "deliverables": [
        { "id": "a", "owned_files": [{ "path": "x", "mod": "append" }], "prerequisites": [] }
    ]});
    let result = server
        .dispatch_call(call_args(TOOL_SUBMIT, json!({ "graph": graph })))
        .await;
    assert!(result.is_err());
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

#[tokio::test]
async fn plan_acquire_cohort_accepts_ids_and_filter() {
    let server = server();
    let graph = json!({"deliverables": [
        {"id": "a", "owned_files": ["a.rs"], "prerequisites": [], "estimated_effort_hours": 1.0, "metadata": {"executor": "claude"}},
        {"id": "b", "owned_files": ["b.rs"], "prerequisites": [], "estimated_effort_hours": 1.0, "metadata": {"executor": "junior"}},
        {"id": "c", "owned_files": ["c.rs"], "prerequisites": [], "estimated_effort_hours": 1.0, "metadata": {"executor": "junior"}}
    ]});
    let sub = server
        .dispatch_call(call_args(TOOL_SUBMIT, json!({"graph": graph})))
        .await
        .unwrap();
    let plan_id = sub["plan_id"].as_str().unwrap().to_string();
    let resp = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({"plan_id": plan_id, "caller_id": "w", "max_count": 5,
                   "ids": ["a", "b"], "filter": {"metadata": {"executor": "junior"}}}),
        ))
        .await
        .unwrap();
    let ids: Vec<&str> = resp["deliverables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["b"]);
}

#[tokio::test]
async fn plan_acquire_cohort_rejects_unknown_filter_keys() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let result = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({"plan_id": plan_id, "caller_id": "w", "max_count": 1,
                   "filter": {"bogus": 1}}),
        ))
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn plan_acquire_cohort_rejects_empty_ids() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let err = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({"plan_id": plan_id, "caller_id": "w", "max_count": 1, "ids": []}),
        ))
        .await
        .expect_err("empty ids must be rejected");
    assert_eq!(err.message, "ids must be non-empty when provided");
}

#[tokio::test]
async fn plan_acquire_cohort_rejects_zero_ttl() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let err = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({"plan_id": plan_id, "caller_id": "w", "max_count": 1, "ttl_seconds": 0}),
        ))
        .await
        .expect_err("ttl_seconds 0 must be rejected");
    assert!(
        err.message.contains("ttl_seconds"),
        "zero ttl must be an invalid-params error naming ttl_seconds; got: {}",
        err.message
    );
}

#[tokio::test]
async fn plan_heartbeat_accepts_ttl_seconds() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let _ = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({"plan_id": plan_id, "caller_id": "orchestrator-001", "max_count": 4}),
        ))
        .await
        .expect("acquire ok");
    let resp = server
        .dispatch_call(call_args(
            TOOL_HEARTBEAT,
            json!({
                "plan_id": plan_id,
                "deliverable_id": "d1",
                "caller_id": "orchestrator-001",
                "ttl_seconds": 3600
            }),
        ))
        .await
        .expect("plan.heartbeat accepts ttl_seconds");
    assert_eq!(resp["ok"].as_bool(), Some(true));
}

// ── acquire response: blocked_count / needs_operator ────────────────────────

#[tokio::test]
async fn plan_acquire_cohort_reports_needs_operator_when_lapse_limited() {
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use std::sync::Mutex;

    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let now = Arc::new(Mutex::new(t0));
    let clock_now = now.clone();
    let planner = BasicCpmPlanner::with_parts(
        Arc::new(cpm_planner::audit::NullAuditSink),
        std::time::Duration::from_secs(60),
        Arc::new(move || *clock_now.lock().unwrap()),
    );
    let server = PlanServer::new(Arc::new(planner));
    let plan_id = submit_plan(&server).await;

    let acquire = || {
        call_args(
            TOOL_ACQUIRE_COHORT,
            json!({ "plan_id": plan_id, "caller_id": "w", "max_count": 1 }),
        )
    };
    for round in 1..=cpm_planner::MAX_LAPSES {
        server.dispatch_call(acquire()).await.expect("acquire ok");
        *now.lock().unwrap() = t0 + ChronoDuration::minutes(5 * i64::from(round));
    }
    let resp = server.dispatch_call(acquire()).await.expect("acquire ok");
    assert_eq!(
        (
            resp["exhausted"].clone(),
            resp["blocked_count"].clone(),
            resp["needs_operator"].clone()
        ),
        (json!(true), json!(1), json!(true))
    );
}

#[tokio::test]
async fn plan_acquire_cohort_without_blocked_does_not_need_operator() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let resp = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({ "plan_id": plan_id, "caller_id": "w", "max_count": 1 }),
        ))
        .await
        .expect("acquire ok");
    assert_eq!(
        (
            resp["blocked_count"].clone(),
            resp["needs_operator"].clone()
        ),
        (json!(0), json!(false))
    );
}

// ── Roundtrip: plan.lint / plan.schedule / plan.simulate ────────────────────

fn lintable_graph() -> Value {
    json!({
        "deliverables": [
            {
                "id": "a",
                "owned_files": ["src/a.rs"],
                "prerequisites": [],
                "estimated_effort_hours": 1.0
            },
            {
                "id": "b",
                "owned_files": ["src/b.rs"],
                "prerequisites": [{ "id": "a", "consumes": "artifact" }],
                "estimated_effort_hours": 2.0
            }
        ]
    })
}

#[tokio::test]
async fn plan_lint_inline_graph_roundtrip() {
    let server = server();
    let resp = server
        .dispatch_call(call_args("plan.lint", json!({ "graph": lintable_graph() })))
        .await
        .expect("plan.lint returns Ok");
    assert_eq!(resp["clean"], json!(true));
}

#[tokio::test]
async fn plan_lint_stored_plan_roundtrip() {
    let server = server();
    let sub = server
        .dispatch_call(call_args(TOOL_SUBMIT, json!({ "graph": lintable_graph() })))
        .await
        .unwrap();
    let plan_id = sub["plan_id"].as_str().unwrap().to_string();
    let resp = server
        .dispatch_call(call_args("plan.lint", json!({ "plan_id": plan_id })))
        .await
        .expect("plan.lint by plan_id returns Ok");
    assert_eq!(resp["clean"], json!(true));
}

#[tokio::test]
async fn plan_lint_rejects_both_graph_and_plan_id() {
    let server = server();
    let err = server
        .dispatch_call(call_args(
            "plan.lint",
            json!({ "graph": lintable_graph(), "plan_id": "plan_x" }),
        ))
        .await
        .expect_err("both graph and plan_id must be rejected");
    assert_eq!(
        err.message,
        "provide exactly one of graph, plan_id, or path"
    );
}

#[tokio::test]
async fn plan_schedule_roundtrip() {
    let server = server();
    let resp = server
        .dispatch_call(call_args(
            "plan.schedule",
            json!({
                "graph": sample_graph(),
                "capacities": { "unassigned": 1 }
            }),
        ))
        .await
        .expect("plan.schedule returns Ok");
    assert_eq!(resp["makespan"], json!(3.0));
}

#[tokio::test]
async fn plan_schedule_rejects_missing_capacities() {
    let server = server();
    let err = server
        .dispatch_call(call_args(
            "plan.schedule",
            json!({
                "graph": sample_graph(),
                "capacities": {}
            }),
        ))
        .await
        .expect_err("missing capacities must be rejected");
    assert!(
        err.message.contains("INVALID_CAPACITIES"),
        "expected INVALID_CAPACITIES; got: {}",
        err.message
    );
}

#[tokio::test]
async fn plan_simulate_roundtrip() {
    let server = server();
    let resp = server
        .dispatch_call(call_args(
            "plan.simulate",
            json!({ "graph": sample_graph() }),
        ))
        .await
        .expect("plan.simulate returns Ok");
    assert_eq!(resp["scorecard"]["deliverables"], json!(2));
}

#[tokio::test]
async fn plan_simulate_rejects_unknown_fields() {
    let server = server();
    let err = server
        .dispatch_call(call_args(
            "plan.simulate",
            json!({ "graph": sample_graph(), "bogus": 1 }),
        ))
        .await
        .expect_err("unknown fields must be rejected");
    assert!(
        err.message.contains("bogus") || err.message.contains("unknown field"),
        "expected unknown-field rejection; got: {}",
        err.message
    );
}

// ── Fix wave: validation, limits and read-only guarantees ──────────────────

async fn call_within(server: &PlanServer, name: &str, args: Value) -> Result<Value, String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        server.dispatch_call(call_args(name, args)),
    )
    .await
    .expect("tool call returns promptly")
    .map_err(|e| e.message.to_string())
}

fn schedule_args(deliverables: Value) -> Value {
    json!({
        "graph": { "deliverables": deliverables },
        "capacities": { "unassigned": 1 }
    })
}

#[tokio::test]
async fn plan_schedule_rejects_reserved_id() {
    let args = schedule_args(json!([
        { "id": "__start__", "owned_files": [], "prerequisites": [], "estimated_effort_hours": 1.0 }
    ]));
    let err = call_within(&server(), "plan.schedule", args)
        .await
        .unwrap_err();
    assert!(err.contains("INVALID_GRAPH"), "{err}");
}

#[tokio::test]
async fn plan_schedule_rejects_duplicate_id() {
    let args = schedule_args(json!([
        { "id": "a", "owned_files": [], "prerequisites": [], "estimated_effort_hours": 1.0 },
        { "id": "a", "owned_files": [], "prerequisites": ["b"], "estimated_effort_hours": 1.0 },
        { "id": "b", "owned_files": [], "prerequisites": ["a"], "estimated_effort_hours": 1.0 }
    ]));
    let err = call_within(&server(), "plan.schedule", args)
        .await
        .unwrap_err();
    assert!(err.contains("INVALID_GRAPH"), "{err}");
}

#[tokio::test]
async fn plan_schedule_rejects_cycle() {
    let args = schedule_args(json!([
        { "id": "a", "owned_files": [], "prerequisites": ["b"], "estimated_effort_hours": 1.0 },
        { "id": "b", "owned_files": [], "prerequisites": ["a"], "estimated_effort_hours": 1.0 }
    ]));
    let err = call_within(&server(), "plan.schedule", args)
        .await
        .unwrap_err();
    assert!(err.contains("INVALID_GRAPH: cycle detected"), "{err}");
}

#[tokio::test]
async fn plan_simulate_rejects_nested_unknown_field() {
    let args = json!({ "graph": sample_graph(), "monte_carlo": { "iteratons": 10 } });
    let err = call_within(&server(), "plan.simulate", args)
        .await
        .unwrap_err();
    assert!(err.contains("unknown field `iteratons`"), "{err}");
}

#[tokio::test]
async fn plan_schedule_rejects_nested_unknown_field_in_simulate_schedule() {
    let args = json!({
        "graph": sample_graph(),
        "schedule": { "capacities": { "unassigned": 1 }, "resource_keys": "owner" }
    });
    let err = call_within(&server(), "plan.simulate", args)
        .await
        .unwrap_err();
    assert!(err.contains("unknown field `resource_keys`"), "{err}");
}

#[tokio::test]
async fn plan_simulate_rejects_zero_iterations_as_invalid_params() {
    let args = json!({ "graph": sample_graph(), "monte_carlo": { "iterations": 0 } });
    let err = server()
        .dispatch_call(call_args("plan.simulate", args))
        .await
        .unwrap_err();
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn plan_schedule_rejects_buffer_pct_above_100_as_invalid_params() {
    let mut args = schedule_args(sample_graph()["deliverables"].clone());
    args["project_buffer_pct"] = json!(101.0);
    let err = server()
        .dispatch_call(call_args("plan.schedule", args))
        .await
        .unwrap_err();
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn plan_simulate_rejects_buffer_pct_above_100_as_invalid_params() {
    let args = json!({
        "graph": sample_graph(),
        "schedule": { "capacities": { "unassigned": 1 }, "project_buffer_pct": 150.0 }
    });
    let err = server()
        .dispatch_call(call_args("plan.simulate", args))
        .await
        .unwrap_err();
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[test]
fn tool_definitions_match_tool_names() {
    let defs = cpm_planner::plan_tool_definitions();
    let names: Vec<String> = defs.iter().map(|t| t.name.to_string()).collect();
    let expected: Vec<String> = cpm_planner::PLAN_TOOL_NAMES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!((names, expected.len()), (expected, 22));
}

#[test]
fn no_tool_schema_has_top_level_combinators() {
    let defs = cpm_planner::plan_tool_definitions();
    let offending: Vec<String> = defs
        .iter()
        .filter(|t| {
            ["oneOf", "anyOf", "allOf"]
                .iter()
                .any(|key| t.input_schema.contains_key(*key))
        })
        .map(|t| t.name.to_string())
        .collect();
    assert_eq!(offending, Vec::<String>::new());
}

#[tokio::test]
async fn plan_lint_rejects_neither_graph_nor_plan_id() {
    let err = server()
        .dispatch_call(call_args("plan.lint", json!({})))
        .await
        .expect_err("neither graph nor plan_id must be rejected");
    assert_eq!(
        err.message,
        "provide exactly one of graph, plan_id, or path"
    );
}

#[tokio::test]
async fn plan_schedule_by_plan_id_roundtrip() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let resp = server
        .dispatch_call(call_args(
            "plan.schedule",
            json!({ "plan_id": plan_id, "capacities": { "unassigned": 1 } }),
        ))
        .await
        .expect("plan.schedule by plan_id returns Ok");
    assert_eq!(resp["makespan"], json!(3.0));
}

#[tokio::test]
async fn plan_simulate_by_plan_id_roundtrip() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let resp = server
        .dispatch_call(call_args("plan.simulate", json!({ "plan_id": plan_id })))
        .await
        .expect("plan.simulate by plan_id returns Ok");
    assert_eq!(resp["critical_path_hours"], json!(3.0));
}

#[tokio::test]
async fn analysis_tools_are_read_only() {
    use cpm_planner::audit::MemoryAuditSink;
    let dir = std::env::temp_dir().join(format!(
        "cpm-read-only-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("plans.db");
    let audit = Arc::new(MemoryAuditSink::new());
    let store = cpm_planner::SqlitePlanStore::open(&path).unwrap();
    let server = PlanServer::new(Arc::new(BasicCpmPlanner::with_store(store, audit.clone())));
    let plan_id = submit_plan(&server).await;
    let plans = || -> i64 {
        rusqlite::Connection::open(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM plans", [], |r| r.get(0))
            .unwrap()
    };
    let before = (plans(), audit.snapshot().len());
    for (tool, extra) in [
        ("plan.lint", json!({})),
        (
            "plan.schedule",
            json!({ "capacities": { "unassigned": 1 } }),
        ),
        (
            "plan.simulate",
            json!({ "schedule": { "capacities": { "unassigned": 1 } },
                    "monte_carlo": { "iterations": 20 } }),
        ),
    ] {
        let mut args = extra;
        args["plan_id"] = json!(plan_id);
        server.dispatch_call(call_args(tool, args)).await.unwrap();
    }
    let after = (plans(), audit.snapshot().len());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(after, before);
}

// ── Portfolio tools (Task 7) ────────────────────────────────────────────────

fn temp_project() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cpm-task7-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn rooted_server(dir: &std::path::Path) -> PlanServer {
    let root = cpm_planner::project::ProjectRoot::from_path(dir).expect("project root");
    let planner = BasicCpmPlanner::new().with_project_root(root);
    PlanServer::new(Arc::new(planner))
}

async fn sync_named(server: &PlanServer, name: &str, variant: &str) -> String {
    let resp = server
        .dispatch_call(call_args(
            "plan.sync",
            json!({ "graph": sample_graph(), "name": name, "variant": variant }),
        ))
        .await
        .expect("plan.sync returns Ok");
    resp["plan_id"].as_str().expect("plan_id").to_string()
}

async fn fork_alt(server: &PlanServer, plan_id: &str) -> String {
    let resp = server
        .dispatch_call(call_args(
            "plan.fork",
            json!({ "plan_id": plan_id, "variant": "alt" }),
        ))
        .await
        .expect("plan.fork returns Ok");
    resp["plan_id"].as_str().expect("plan_id").to_string()
}

async fn write_plan_file(dir: &std::path::Path, name: &str, variant: &str, graph: Value) {
    let root = cpm_planner::project::ProjectRoot::from_path(dir).expect("project root");
    let file = root.plan_file(name, variant).expect("plan file ref");
    let parsed: cpm_planner::plan::PlanGraph = serde_json::from_value(graph).unwrap();
    root.write_graph(&file, &parsed).unwrap();
}

#[tokio::test]
async fn plan_sync_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let resp = server
        .dispatch_call(call_args(
            "plan.sync",
            json!({ "graph": sample_graph(), "name": "web", "variant": "main" }),
        ))
        .await
        .expect("plan.sync returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["created"], json!(true));
}

#[tokio::test]
async fn plan_list_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    sync_named(&server, "web", "main").await;
    let resp = server
        .dispatch_call(call_args("plan.list", json!({})))
        .await
        .expect("plan.list returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp.as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn plan_export_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let plan_id = sync_named(&server, "web", "main").await;
    let resp = server
        .dispatch_call(call_args("plan.export", json!({ "plan_id": plan_id })))
        .await
        .expect("plan.export returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["path"], json!(".cpm-planner/plans/web/main.json"));
}

#[tokio::test]
async fn plan_revise_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let plan_id = sync_named(&server, "web", "main").await;
    let mut changed = sample_graph();
    changed["deliverables"][0]["estimated_effort_hours"] = json!(5.0);
    let resp = server
        .dispatch_call(call_args(
            "plan.revise",
            json!({ "plan_id": plan_id, "graph": changed }),
        ))
        .await
        .expect("plan.revise returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["revision"], json!(2));
}

#[tokio::test]
async fn plan_fork_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let plan_id = sync_named(&server, "web", "main").await;
    let resp = server
        .dispatch_call(call_args(
            "plan.fork",
            json!({ "plan_id": plan_id, "variant": "alt" }),
        ))
        .await
        .expect("plan.fork returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["variant"], json!("alt"));
}

#[tokio::test]
async fn plan_select_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let main = sync_named(&server, "web", "main").await;
    let alt = fork_alt(&server, &main).await;
    let resp = server
        .dispatch_call(call_args("plan.select", json!({ "plan_id": alt })))
        .await
        .expect("plan.select returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["changed"], json!(true));
}

#[tokio::test]
async fn plan_archive_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let main = sync_named(&server, "web", "main").await;
    fork_alt(&server, &main).await;
    let resp = server
        .dispatch_call(call_args(
            "plan.archive",
            json!({ "name": "web", "variant": "alt" }),
        ))
        .await
        .expect("plan.archive returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["ok"], json!(true));
}

#[tokio::test]
async fn plan_compare_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let main = sync_named(&server, "web", "main").await;
    fork_alt(&server, &main).await;
    let resp = server
        .dispatch_call(call_args("plan.compare", json!({ "plan": "web" })))
        .await
        .expect("plan.compare returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["variants"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn plan_sync_rejects_path_escape() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let err = server
        .dispatch_call(call_args("plan.sync", json!({ "path": "../evil.json" })))
        .await
        .expect_err("path escape must be rejected");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        err.message.starts_with("INVALID_PATH"),
        "got {}",
        err.message
    );
}

#[tokio::test]
async fn plan_status_reports_definition_drift() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    write_plan_file(&dir, "web", "main", sample_graph()).await;
    let sync = server
        .dispatch_call(call_args(
            "plan.sync",
            json!({ "path": ".cpm-planner/plans/web/main.json" }),
        ))
        .await
        .expect("plan.sync from path returns Ok");
    let plan_id = sync["plan_id"].as_str().unwrap().to_string();
    let mut changed = sample_graph();
    changed["deliverables"][0]["estimated_effort_hours"] = json!(9.0);
    write_plan_file(&dir, "web", "main", changed).await;
    let resp = server
        .dispatch_call(call_args("plan.status", json!({ "plan_id": plan_id })))
        .await
        .expect("plan.status returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["definition_drift"], json!(true));
}

#[tokio::test]
async fn execution_on_draft_variant_returns_variant_not_selected() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let main = sync_named(&server, "web", "main").await;
    let alt = fork_alt(&server, &main).await;
    let err = server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({ "plan_id": alt, "caller_id": "worker", "max_count": 1 }),
        ))
        .await
        .expect_err("execution on a draft is gated");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        err.message.starts_with("VARIANT_NOT_SELECTED"),
        "got {}",
        err.message
    );
}

#[tokio::test]
async fn plan_compare_rejects_negative_weights() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let main = sync_named(&server, "web", "main").await;
    fork_alt(&server, &main).await;
    let err = server
        .dispatch_call(call_args(
            "plan.compare",
            json!({ "plan": "web", "weights": { "makespan": -1.0 } }),
        ))
        .await
        .expect_err("negative weights must be rejected");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn plan_submit_named_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let resp = server
        .dispatch_call(call_args(
            TOOL_SUBMIT,
            json!({ "graph": sample_graph(), "name": "web" }),
        ))
        .await
        .expect("named plan.submit returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["name"], json!("web"));
}

#[tokio::test]
async fn plan_lint_path_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    write_plan_file(&dir, "web", "main", lintable_graph()).await;
    let resp = server
        .dispatch_call(call_args(
            "plan.lint",
            json!({ "path": ".cpm-planner/plans/web/main.json" }),
        ))
        .await
        .expect("plan.lint by path returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["clean"], json!(true));
}

#[tokio::test]
async fn plan_simulate_path_roundtrip() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    write_plan_file(&dir, "web", "main", sample_graph()).await;
    let resp = server
        .dispatch_call(call_args(
            "plan.simulate",
            json!({ "path": ".cpm-planner/plans/web/main.json" }),
        ))
        .await
        .expect("plan.simulate by path returns Ok");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["critical_path_hours"], json!(3.0));
}

// ── Final-review fixes ──────────────────────────────────────────────────────

fn write_raw_plan_file(dir: &std::path::Path, name: &str, variant: &str, graph: &Value) {
    let plans = dir.join(".cpm-planner/plans").join(name);
    std::fs::create_dir_all(&plans).unwrap();
    std::fs::write(
        plans.join(format!("{variant}.json")),
        serde_json::to_vec(graph).unwrap(),
    )
    .unwrap();
}

async fn call_err(server: &PlanServer, name: &str, args: Value) -> rmcp::ErrorData {
    server
        .dispatch_call(call_args(name, args))
        .await
        .expect_err("call must fail")
}

#[tokio::test]
async fn sync_path_rejects_contradicting_name() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    write_raw_plan_file(&dir, "web", "main", &sample_graph());
    let err = call_err(
        &server,
        "plan.sync",
        json!({ "path": ".cpm-planner/plans/web/main.json", "name": "other" }),
    )
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        err.message,
        "INVALID_PATH: name/variant/project must match the plan file path"
    );
}

#[tokio::test]
async fn sync_path_rejects_foreign_project() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    write_raw_plan_file(&dir, "web", "main", &sample_graph());
    let err = call_err(
        &server,
        "plan.sync",
        json!({ "path": ".cpm-planner/plans/web/main.json", "project": "/somewhere/else" }),
    )
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        err.message,
        "INVALID_PATH: name/variant/project must match the plan file path"
    );
}

#[tokio::test]
async fn sync_path_accepts_matching_name_and_variant() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    write_raw_plan_file(&dir, "web", "main", &sample_graph());
    let resp = server
        .dispatch_call(call_args(
            "plan.sync",
            json!({ "path": ".cpm-planner/plans/web/main.json", "name": "web", "variant": "main" }),
        ))
        .await
        .expect("matching name/variant is accepted");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(resp["created"], json!(true));
}

#[tokio::test]
async fn compare_rejects_duplicate_plan_ids() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let main = sync_named(&server, "web", "main").await;
    let err = call_err(&server, "plan.compare", json!({ "plan_ids": [main, main] })).await;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        (err.code, err.message.as_ref()),
        (
            rmcp::model::ErrorCode::INVALID_PARAMS,
            "plan_ids must be distinct"
        )
    );
}

#[tokio::test]
async fn compare_by_name_with_explicit_project() {
    let server = server();
    for variant in ["main", "alt"] {
        server
            .dispatch_call(call_args(
                "plan.sync",
                json!({ "graph": sample_graph(), "project": "elsewhere", "name": "web", "variant": variant }),
            ))
            .await
            .expect("plan.sync returns Ok");
    }
    let resp = server
        .dispatch_call(call_args(
            "plan.compare",
            json!({ "plan": "web", "project": "elsewhere" }),
        ))
        .await
        .expect("compare in an explicit project needs no root");
    assert_eq!(resp["variants"].as_array().map(Vec::len), Some(2));
}

#[test]
fn compare_schema_caps_plan_ids_at_16() {
    let tools = cpm_planner::server::plan_tool_definitions();
    let compare = tools
        .iter()
        .find(|t| t.name == "plan.compare")
        .expect("plan.compare advertised");
    assert_eq!(
        compare.input_schema["properties"]["plan_ids"]["maxItems"],
        json!(16)
    );
}

#[tokio::test]
async fn export_force_overwrites_unsynced_local_edits() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    write_raw_plan_file(&dir, "web", "main", &sample_graph());
    let sync = server
        .dispatch_call(call_args(
            "plan.sync",
            json!({ "path": ".cpm-planner/plans/web/main.json" }),
        ))
        .await
        .expect("plan.sync from path");
    let mut edited = sample_graph();
    edited["deliverables"][0]["estimated_effort_hours"] = json!(7.0);
    write_raw_plan_file(&dir, "web", "main", &edited);
    let resp = server
        .dispatch_call(call_args(
            "plan.export",
            json!({ "plan_id": sync["plan_id"], "force": true }),
        ))
        .await;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(resp.is_ok(), "got {resp:?}");
}

#[tokio::test]
async fn sync_path_without_root_is_invalid_path() {
    let err = call_err(
        &server(),
        "plan.sync",
        json!({ "path": ".cpm-planner/plans/web/main.json" }),
    )
    .await;
    assert!(
        err.message.starts_with("INVALID_PATH"),
        "got {}",
        err.message
    );
}

#[tokio::test]
async fn lint_path_without_root_is_invalid_path() {
    let err = call_err(
        &server(),
        "plan.lint",
        json!({ "path": ".cpm-planner/plans/web/main.json" }),
    )
    .await;
    assert!(
        err.message.starts_with("INVALID_PATH"),
        "got {}",
        err.message
    );
}

#[tokio::test]
async fn export_without_root_is_invalid_path() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let err = call_err(&server, "plan.export", json!({ "plan_id": plan_id })).await;
    assert!(
        err.message.starts_with("INVALID_PATH"),
        "got {}",
        err.message
    );
}

#[tokio::test]
async fn sync_with_both_path_and_graph_is_invalid_params() {
    let err = call_err(
        &server(),
        "plan.sync",
        json!({ "path": ".cpm-planner/plans/web/main.json", "graph": sample_graph(), "name": "web" }),
    )
    .await;
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn sync_with_neither_path_nor_graph_is_invalid_params() {
    let err = call_err(&server(), "plan.sync", json!({})).await;
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn compare_with_both_plan_ids_and_plan_is_invalid_params() {
    let err = call_err(
        &server(),
        "plan.compare",
        json!({ "plan_ids": ["a", "b"], "plan": "web" }),
    )
    .await;
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn compare_with_neither_plan_ids_nor_plan_is_invalid_params() {
    let err = call_err(&server(), "plan.compare", json!({})).await;
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn lint_path_on_cyclic_file_reports_cycle_finding() {
    let dir = temp_project();
    let server = rooted_server(&dir);
    let mut cyclic = sample_graph();
    cyclic["deliverables"][0]["prerequisites"] = json!(["d2"]);
    write_raw_plan_file(&dir, "cyc", "main", &cyclic);
    let resp = server
        .dispatch_call(call_args(
            "plan.lint",
            json!({ "path": ".cpm-planner/plans/cyc/main.json" }),
        ))
        .await
        .expect("lint reports a cycle as findings, not an error");
    let _ = std::fs::remove_dir_all(&dir);
    let codes: Vec<&str> = resp["findings"]
        .as_array()
        .map(|f| f.iter().filter_map(|x| x["code"].as_str()).collect())
        .unwrap_or_default();
    assert!(codes.contains(&"CYCLE"), "got {resp}");
}

// ── plan.mark_status progress fields (earned value) ─────────────────────────

/// A server with `d1` of the sample plan leased to `w1`; returns the plan id.
async fn server_with_d1_leased(server: &PlanServer) -> String {
    let plan_id = submit_plan(server).await;
    server
        .dispatch_call(call_args(
            TOOL_ACQUIRE_COHORT,
            json!({ "plan_id": plan_id, "caller_id": "w1", "max_count": 1 }),
        ))
        .await
        .expect("acquire ok");
    plan_id
}

fn mark_d1(plan_id: &str, extra: Value) -> Value {
    let mut args = json!({
        "plan_id": plan_id,
        "deliverable_id": "d1",
        "caller_id": "w1",
        "status": { "status": "in_progress" }
    });
    for (k, v) in extra.as_object().expect("extra is an object") {
        args[k] = v.clone();
    }
    args
}

#[tokio::test]
async fn mark_status_accepts_progress_fields() {
    let server = server();
    let plan_id = server_with_d1_leased(&server).await;
    let resp = server
        .dispatch_call(call_args(
            TOOL_MARK_STATUS,
            mark_d1(
                &plan_id,
                json!({ "earned_pct": 40, "actual_effort_hours": 1.5, "evidence": "tests pass" }),
            ),
        ))
        .await
        .expect("mark_status ok");
    assert_eq!(resp["ok"], json!(true));
}

#[tokio::test]
async fn mark_status_earned_pct_beyond_a_byte_is_invalid_actuals() {
    let server = server();
    let plan_id = server_with_d1_leased(&server).await;
    let err = call_err(
        &server,
        TOOL_MARK_STATUS,
        mark_d1(&plan_id, json!({ "earned_pct": 300 })),
    )
    .await;
    assert_eq!(
        err.message,
        "INVALID_ACTUALS: earned_pct must be an integer 0..=100, got 300"
    );
}

#[tokio::test]
async fn mark_status_actual_hours_beyond_f32_is_invalid_actuals() {
    let server = server();
    let plan_id = server_with_d1_leased(&server).await;
    let err = call_err(
        &server,
        TOOL_MARK_STATUS,
        mark_d1(&plan_id, json!({ "actual_effort_hours": 1e300 })),
    )
    .await;
    assert!(
        err.message
            .starts_with("INVALID_ACTUALS: actual_effort_hours"),
        "{}",
        err.message
    );
}

#[test]
fn mark_status_schema_bounds_earned_pct() {
    let tools = cpm_planner::server::plan_tool_definitions();
    let mark = tools
        .iter()
        .find(|t| t.name == "plan.mark_status")
        .expect("plan.mark_status advertised");
    assert_eq!(
        mark.input_schema["properties"]["earned_pct"],
        json!({
            "type": "integer",
            "minimum": 0,
            "maximum": 100,
            "description": "Percent complete (used by the weighted earning rule). Only with status in_progress; accepted and ignored with complete."
        })
    );
}

#[test]
fn mark_status_schema_caps_evidence_length() {
    let tools = cpm_planner::server::plan_tool_definitions();
    let mark = tools
        .iter()
        .find(|t| t.name == "plan.mark_status")
        .expect("plan.mark_status advertised");
    assert_eq!(
        mark.input_schema["properties"]["evidence"]["maxLength"],
        json!(2048)
    );
}

#[tokio::test]
async fn fractional_earned_pct_is_invalid_actuals() {
    let server = server();
    let plan_id = server_with_d1_leased(&server).await;
    let err = call_err(
        &server,
        TOOL_MARK_STATUS,
        mark_d1(&plan_id, json!({ "earned_pct": 40.5 })),
    )
    .await;
    assert_eq!(
        err.message,
        "INVALID_ACTUALS: earned_pct must be an integer 0..=100, got 40.5"
    );
}

#[tokio::test]
async fn negative_earned_pct_is_invalid_actuals() {
    let server = server();
    let plan_id = server_with_d1_leased(&server).await;
    let err = call_err(
        &server,
        TOOL_MARK_STATUS,
        mark_d1(&plan_id, json!({ "earned_pct": -1 })),
    )
    .await;
    assert_eq!(
        err.message,
        "INVALID_ACTUALS: earned_pct must be an integer 0..=100, got -1"
    );
}

// ── Earned value: plan.baseline / plan.ev / plan.snapshot (P5) ──────────────

fn ev_t0() -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    chrono::Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap()
}

/// A server whose planner clock is frozen at [`ev_t0`] plus `hours`.
fn clocked_server(hours: i64) -> PlanServer {
    let now = ev_t0() + chrono::Duration::hours(hours);
    let planner = BasicCpmPlanner::with_parts(
        Arc::new(cpm_planner::audit::NullAuditSink),
        std::time::Duration::from_secs(3600),
        Arc::new(move || now),
    );
    PlanServer::new(Arc::new(planner))
}

async fn call_ok(server: &PlanServer, name: &str, args: Value) -> Value {
    server
        .dispatch_call(call_args(name, args))
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {}", e.message))
}

async fn baselined_plan(server: &PlanServer) -> String {
    let plan_id = submit_plan(server).await;
    call_ok(
        server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "start": "2026-01-05T00:00:00Z" }),
    )
    .await;
    plan_id
}

#[tokio::test]
async fn plan_baseline_then_ev_roundtrip() {
    let server = clocked_server(1);
    let plan_id = submit_plan(&server).await;
    let baseline = call_ok(&server, "plan.baseline", json!({ "plan_id": plan_id })).await;
    let ev = call_ok(&server, "plan.ev", json!({ "plan_id": plan_id })).await;
    assert_eq!(
        (ev["baseline_number"].clone(), ev["bac"].clone()),
        (baseline["baseline_number"].clone(), baseline["bac"].clone())
    );
}

#[tokio::test]
async fn plan_ev_before_baseline_returns_not_baselined() {
    let server = clocked_server(0);
    let plan_id = submit_plan(&server).await;
    let err = call_err(&server, "plan.ev", json!({ "plan_id": plan_id })).await;
    assert!(err.message.starts_with("NOT_BASELINED:"), "{}", err.message);
}

#[tokio::test]
async fn plan_ev_reports_planned_value_at_as_of() {
    let server = clocked_server(0);
    let plan_id = baselined_plan(&server).await;
    let ev = call_ok(
        &server,
        "plan.ev",
        json!({ "plan_id": plan_id, "as_of": "2026-01-05T02:00:00Z" }),
    )
    .await;
    assert_eq!(ev["pv"], json!(2.0));
}

#[tokio::test]
async fn plan_ev_rejects_unparseable_as_of_as_invalid_params() {
    let server = clocked_server(0);
    let plan_id = baselined_plan(&server).await;
    let err = call_err(
        &server,
        "plan.ev",
        json!({ "plan_id": plan_id, "as_of": "yesterday" }),
    )
    .await;
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn plan_baseline_rejects_rebaseline_without_reason() {
    let server = clocked_server(0);
    let plan_id = baselined_plan(&server).await;
    let err = call_err(&server, "plan.baseline", json!({ "plan_id": plan_id })).await;
    assert!(err.message.starts_with("INVALID_GRAPH:"), "{}", err.message);
}

#[tokio::test]
async fn plan_baseline_with_reason_returns_the_next_number() {
    let server = clocked_server(0);
    let plan_id = baselined_plan(&server).await;
    let out = call_ok(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "reason": "scope change" }),
    )
    .await;
    assert_eq!(out["baseline_number"], json!(2));
}

#[tokio::test]
async fn plan_baseline_accepts_a_calendar() {
    let server = clocked_server(0);
    let plan_id = submit_plan(&server).await;
    let out = call_ok(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "calendar": { "hours_per_day": 6, "workdays": ["mon"] } }),
    )
    .await;
    assert_eq!(out["calendar"]["hours_per_day"], json!(6.0));
}

#[tokio::test]
async fn plan_baseline_rejects_unknown_calendar_field_as_invalid_params() {
    let server = clocked_server(0);
    let plan_id = submit_plan(&server).await;
    let err = call_err(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "calendar": { "hours": 8 } }),
    )
    .await;
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn plan_baseline_on_draft_variant_returns_variant_not_selected() {
    let server = clocked_server(0);
    let main = call_ok(
        &server,
        "plan.sync",
        json!({ "graph": sample_graph(), "project": "p", "name": "web" }),
    )
    .await;
    let alt = call_ok(
        &server,
        "plan.fork",
        json!({ "plan_id": main["plan_id"], "variant": "alt" }),
    )
    .await;
    let err = call_err(
        &server,
        "plan.baseline",
        json!({ "plan_id": alt["plan_id"] }),
    )
    .await;
    assert!(
        err.message.starts_with("VARIANT_NOT_SELECTED"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn plan_snapshot_markdown_contains_header_row() {
    let server = clocked_server(1);
    let plan_id = baselined_plan(&server).await;
    let out = call_ok(
        &server,
        "plan.snapshot",
        json!({ "plan_id": plan_id, "format": "markdown" }),
    )
    .await;
    assert!(
        out["export"]
            .as_str()
            .is_some_and(|md| md.starts_with("| date | PV | EV | AC | SPI | CPI | EAC |")),
        "{out}"
    );
}

#[tokio::test]
async fn plan_snapshot_defaults_to_json_export() {
    let server = clocked_server(1);
    let plan_id = baselined_plan(&server).await;
    let out = call_ok(&server, "plan.snapshot", json!({ "plan_id": plan_id })).await;
    assert_eq!(out["export"][0]["pv"], out["summary"]["pv"]);
}

#[tokio::test]
async fn plan_snapshot_rejects_unknown_format_as_invalid_params() {
    let server = clocked_server(1);
    let plan_id = baselined_plan(&server).await;
    let err = call_err(
        &server,
        "plan.snapshot",
        json!({ "plan_id": plan_id, "format": "csv" }),
    )
    .await;
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn plan_snapshots_with_low_spi_raise_alert() {
    let server = clocked_server(0);
    let plan_id = baselined_plan(&server).await;
    let mut last = Value::Null;
    for as_of in ["2026-01-05T02:00:00Z", "2026-01-05T03:00:00Z"] {
        last = call_ok(
            &server,
            "plan.snapshot",
            json!({ "plan_id": plan_id, "as_of": as_of }),
        )
        .await;
    }
    assert_eq!(last["summary"]["alerts"], json!(["SPI_BELOW_0_9"]));
}

/// Every `null` in an EV report must be a ratio its `undefined` list
/// explains; returns the offending JSON paths.
fn unexplained_nulls(report: &Value) -> Vec<String> {
    fn walk(v: &Value, path: String, out: &mut Vec<String>) {
        match v {
            Value::Null => out.push(path),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk(item, format!("{path}[{i}]"), out);
                }
            }
            Value::Object(map) => {
                for (k, item) in map {
                    walk(item, format!("{path}.{k}"), out);
                }
            }
            _ => {}
        }
    }
    let explained: Vec<String> = report["undefined"]
        .as_array()
        .map(|u| {
            u.iter()
                .filter_map(|e| e["field"].as_str().map(|f| format!(".{f}")))
                .collect()
        })
        .unwrap_or_default();
    let mut nulls = Vec::new();
    walk(report, String::new(), &mut nulls);
    nulls.retain(|p| !explained.contains(p));
    let text = report.to_string();
    if text.contains("NaN") || text.contains("inf") {
        nulls.push(format!("non-finite literal in {text}"));
    }
    nulls
}

#[tokio::test]
async fn plan_ev_never_serialises_unexplained_null_or_nan() {
    let mut seed: u64 = 0x5eed_cafe;
    let mut next = |n: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % n
    };
    let rules = ["zero_hundred", "fifty_fifty", "weighted"];
    let mut failures = Vec::new();
    for case in 0..40 {
        let server = clocked_server(0);
        let count = 1 + next(5) as usize;
        let deliverables: Vec<Value> = (0..count)
            .map(|i| {
                let prereqs: Vec<String> = (0..i)
                    .filter(|_| next(3) == 0)
                    .map(|p| format!("d{p}"))
                    .collect();
                let effort = [0.0, 0.5, 3.0, 12.0][next(4) as usize];
                let rule = rules[next(3) as usize];
                let mut d = json!({
                    "id": format!("d{i}"),
                    "owned_files": [format!("src/f{i}.rs")],
                    "prerequisites": prereqs,
                    "estimated_effort_hours": effort,
                    "earning_rule": rule,
                });
                if next(2) == 0 {
                    let rate = [0.0, 0.5, 2.0][next(3) as usize];
                    d["metadata"] = json!({ "cost_rate": rate });
                }
                d
            })
            .collect();
        let plan = call_ok(
            &server,
            TOOL_SUBMIT,
            json!({ "graph": { "deliverables": deliverables } }),
        )
        .await;
        let plan_id = plan["plan_id"].as_str().unwrap().to_string();
        let mut baseline = json!({ "plan_id": plan_id, "start": "2026-01-05T00:00:00Z" });
        if next(2) == 0 {
            baseline["calendar"] = json!({ "hours_per_day": 8, "workdays": ["mon", "tue"] });
        }
        call_ok(&server, "plan.baseline", baseline).await;
        let cohort = call_ok(
            &server,
            TOOL_ACQUIRE_COHORT,
            json!({ "plan_id": plan_id, "caller_id": "w", "max_count": 10 }),
        )
        .await;
        for d in cohort["deliverables"].as_array().into_iter().flatten() {
            let mut mark = json!({
                "plan_id": plan_id,
                "deliverable_id": d["id"],
                "caller_id": "w",
                "status": { "status": "in_progress" },
                "earned_pct": next(101),
            });
            if next(2) == 0 {
                let hours = [0.0, 1.5, 40.0][next(3) as usize];
                mark["actual_effort_hours"] = json!(hours);
            }
            call_ok(&server, TOOL_MARK_STATUS, mark).await;
        }
        let as_of = ev_t0() + chrono::Duration::hours(next(120) as i64 - 10);
        let report = call_ok(
            &server,
            "plan.ev",
            json!({ "plan_id": plan_id, "as_of": as_of.to_rfc3339() }),
        )
        .await;
        let bad = unexplained_nulls(&report);
        if !bad.is_empty() {
            failures.push((case, bad));
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
}

#[test]
fn instructions_mention_every_tool() {
    use rmcp::ServerHandler;
    let text = server().get_info().instructions.unwrap_or_default();
    let missing: Vec<&str> = cpm_planner::PLAN_TOOL_NAMES
        .iter()
        .copied()
        .filter(|name| !text.contains(&format!("  {name} ")))
        .collect();
    assert_eq!(missing, Vec::<&str>::new());
}
