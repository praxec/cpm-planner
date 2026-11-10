//! Working calendars and levelled baselines through the MCP server
//! (docs/calendars.md): `calendar` and `start` on `plan.schedule` and
//! `plan.simulate`, the new `plan.baseline` arguments, the error codes of
//! section 7, and byte-identical output without the new arguments.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use cpm_planner::calendar::{CalendarSpec, ScheduleCalendar};
use cpm_planner::monte_carlo::MonteCarloRequest;
use cpm_planner::plan::PlanGraph;
use cpm_planner::resource_schedule::ScheduleRequest;
use cpm_planner::simulate::SimulateRequest;
use cpm_planner::{BasicCpmPlanner, PlanServer};
use rmcp::model::{CallToolRequestParams, ErrorCode, JsonObject};
use serde_json::{Value, json};

/// Friday 2026-11-13 14:00 at UTC-5 (docs/calendars.md, section 8).
const START: &str = "2026-11-13T19:00:00Z";
const CLOCK: &str = "2026-11-16T14:00:00Z";

fn instant(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("rfc3339")
        .with_timezone(&Utc)
}

fn server() -> PlanServer {
    PlanServer::new(Arc::new(
        BasicCpmPlanner::new().with_clock(Arc::new(|| instant(CLOCK))),
    ))
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

/// The worked example: `impl` (agent, 10 h) -> `review` (owner, 2 h) ->
/// `release` (agent, 1 h).
fn graph() -> Value {
    json!({
        "deliverables": [
            { "id": "impl", "owned_files": ["src/impl.rs"], "prerequisites": [],
              "estimated_effort_hours": 10.0, "metadata": { "owner": "agent" } },
            { "id": "review", "owned_files": [],
              "prerequisites": [{ "id": "impl", "consumes": "the implementation" }],
              "estimated_effort_hours": 2.0, "metadata": { "owner": "owner" } },
            { "id": "release", "owned_files": [],
              "prerequisites": [{ "id": "review", "consumes": "the approved change" }],
              "estimated_effort_hours": 1.0, "metadata": { "owner": "agent" } }
        ],
        "max_chained_dispatch": null
    })
}

fn typed_graph() -> PlanGraph {
    serde_json::from_value(graph()).expect("graph decodes")
}

fn capacities() -> Value {
    json!({ "agent": 3, "owner": 1 })
}

fn schedule_request() -> ScheduleRequest {
    ScheduleRequest {
        capacities: BTreeMap::from([("agent".to_string(), 3), ("owner".to_string(), 1)]),
        resource_key: "owner".to_string(),
        project_buffer_pct: 25.0,
    }
}

/// 24/7 agents, a 09:00-17:00 Monday-to-Friday owner at UTC-5.
fn calendar_map() -> Value {
    json!({
        "default": { "hours_per_day": 24,
                     "workdays": ["mon", "tue", "wed", "thu", "fri", "sat", "sun"] },
        "owner": { "hours_per_day": 8, "day_start": "09:00",
                   "workdays": ["mon", "tue", "wed", "thu", "fri"],
                   "utc_offset_minutes": -300 }
    })
}

fn schedule_calendar(start: &str) -> ScheduleCalendar {
    ScheduleCalendar::new(
        CalendarSpec::parse(&calendar_map()).expect("calendar map"),
        instant(start),
    )
}

// ── plan.schedule ────────────────────────────────────────────────────────────

#[tokio::test]
async fn schedule_without_calendar_is_the_calendar_free_schedule() {
    let resp = ok(
        &server(),
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities() }),
    )
    .await;
    let library =
        cpm_planner::resource_schedule::resource_schedule(&typed_graph(), &schedule_request())
            .expect("schedule");
    assert_eq!(resp, serde_json::to_value(&library).unwrap());
}

#[tokio::test]
async fn schedule_without_calendar_ignores_start() {
    let server = server();
    let plain = ok(
        &server,
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities() }),
    )
    .await;
    let with_start = ok(
        &server,
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities(), "start": START }),
    )
    .await;
    assert_eq!(with_start, plain);
}

#[tokio::test]
async fn schedule_with_calendar_equals_the_library_calendar_schedule() {
    let resp = ok(
        &server(),
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities(),
                "calendar": calendar_map(), "start": START }),
    )
    .await;
    let library = cpm_planner::resource_schedule::resource_schedule_on_calendar(
        &typed_graph(),
        &schedule_request(),
        Some(&schedule_calendar(START)),
    )
    .expect("calendar schedule");
    assert_eq!(resp, serde_json::to_value(&library).unwrap());
}

#[tokio::test]
async fn schedule_with_calendar_waits_for_the_owner_window() {
    let resp = ok(
        &server(),
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities(),
                "calendar": calendar_map(), "start": START }),
    )
    .await;
    assert_eq!(resp["makespan"], json!(70.0));
}

#[tokio::test]
async fn schedule_calendar_start_defaults_to_the_planner_clock() {
    let resp = ok(
        &server(),
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities(), "calendar": calendar_map() }),
    )
    .await;
    let library = cpm_planner::resource_schedule::resource_schedule_on_calendar(
        &typed_graph(),
        &schedule_request(),
        Some(&schedule_calendar(CLOCK)),
    )
    .expect("calendar schedule");
    assert_eq!(resp, serde_json::to_value(&library).unwrap());
}

#[tokio::test]
async fn schedule_single_calendar_with_an_unknown_field_is_invalid_params() {
    let e = err(
        &server(),
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities(),
                "calendar": { "hours_per_day": 8, "lunch": "12:00" } }),
    )
    .await;
    assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn schedule_calendar_map_without_default_is_invalid_params() {
    let e = err(
        &server(),
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities(),
                "calendar": { "owner": { "hours_per_day": 8 } } }),
    )
    .await;
    assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn schedule_bad_day_start_is_invalid_value() {
    let e = err(
        &server(),
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities(),
                "calendar": { "day_start": "9:00" } }),
    )
    .await;
    assert!(e.message.starts_with("INVALID_VALUE:"), "{}", e.message);
}

#[tokio::test]
async fn schedule_out_of_range_hours_per_day_is_invalid_graph() {
    let e = err(
        &server(),
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities(),
                "calendar": { "hours_per_day": 25 } }),
    )
    .await;
    assert!(e.message.starts_with("INVALID_GRAPH:"), "{}", e.message);
}

#[tokio::test]
async fn schedule_calendar_map_entry_error_is_invalid_value() {
    let e = err(
        &server(),
        "plan.schedule",
        json!({ "graph": graph(), "capacities": capacities(),
                "calendar": { "default": {}, "owner": { "lunch": "12:00" } } }),
    )
    .await;
    assert!(e.message.starts_with("INVALID_VALUE:"), "{}", e.message);
}

// ── plan.simulate ────────────────────────────────────────────────────────────

fn monte_carlo() -> Value {
    json!({ "iterations": 50, "seed": 7 })
}

fn simulate_request() -> SimulateRequest {
    SimulateRequest {
        schedule: Some(schedule_request()),
        monte_carlo: Some(MonteCarloRequest {
            iterations: 50,
            seed: 7,
        }),
    }
}

#[tokio::test]
async fn simulate_without_calendar_is_the_calendar_free_simulation() {
    let resp = ok(
        &server(),
        "plan.simulate",
        json!({ "graph": graph(), "schedule": { "capacities": capacities() },
                "monte_carlo": monte_carlo() }),
    )
    .await;
    let library =
        cpm_planner::simulate::simulate(&typed_graph(), &simulate_request()).expect("simulate");
    assert_eq!(resp, serde_json::to_value(&library).unwrap());
}

#[tokio::test]
async fn simulate_with_calendar_equals_the_library_calendar_simulation() {
    let resp = ok(
        &server(),
        "plan.simulate",
        json!({ "graph": graph(), "schedule": { "capacities": capacities() },
                "monte_carlo": monte_carlo(), "calendar": calendar_map(), "start": START }),
    )
    .await;
    let library = cpm_planner::simulate::simulate_on_calendar(
        &typed_graph(),
        &simulate_request(),
        Some(&schedule_calendar(START)),
    )
    .expect("calendar simulate");
    assert_eq!(resp, serde_json::to_value(&library).unwrap());
}

#[tokio::test]
async fn simulate_with_calendar_reports_the_start() {
    let resp = ok(
        &server(),
        "plan.simulate",
        json!({ "graph": graph(), "calendar": calendar_map(), "start": START }),
    )
    .await;
    assert_eq!(resp["start"].as_str().map(instant), Some(instant(START)));
}

#[tokio::test]
async fn simulate_monte_carlo_on_calendar_runs_in_wall_clock_hours() {
    let resp = ok(
        &server(),
        "plan.simulate",
        json!({ "graph": graph(), "monte_carlo": monte_carlo(),
                "calendar": calendar_map(), "start": START }),
    )
    .await;
    assert_eq!(
        resp["scorecard"]["monte_carlo"]["deterministic_makespan"],
        json!(70.0)
    );
}

#[tokio::test]
async fn simulate_calendar_with_a_wrong_type_is_invalid_params() {
    let e = err(
        &server(),
        "plan.simulate",
        json!({ "graph": graph(), "calendar": "weekdays" }),
    )
    .await;
    assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
}

// ── plan.baseline ────────────────────────────────────────────────────────────

async fn submit(server: &PlanServer) -> String {
    let resp = ok(server, "plan.submit", json!({ "graph": graph() })).await;
    resp["plan_id"].as_str().expect("plan_id").to_string()
}

async fn baseline(server: &PlanServer, args: Value) -> Value {
    ok(server, "plan.baseline", args).await
}

#[tokio::test]
async fn baseline_without_new_arguments_is_a_cpm_baseline() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = baseline(&server, json!({ "plan_id": plan_id, "start": START })).await;
    assert_eq!(
        (
            resp["schedule"].clone(),
            resp["calendar"].clone(),
            resp["finish_hours"].clone()
        ),
        (json!("cpm"), Value::Null, json!(13.0))
    );
}

#[tokio::test]
async fn baseline_with_capacities_is_levelled() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = baseline(
        &server,
        json!({ "plan_id": plan_id, "start": START, "capacities": capacities() }),
    )
    .await;
    assert_eq!(
        (resp["schedule"].clone(), resp["capacities"].clone()),
        (json!("levelled"), capacities())
    );
}

#[tokio::test]
async fn baseline_with_a_calendar_map_freezes_the_resource_clock() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = baseline(
        &server,
        json!({ "plan_id": plan_id, "start": START, "calendar": calendar_map() }),
    )
    .await;
    assert_eq!(resp["finish_hours"], json!(70.0));
}

#[tokio::test]
async fn baseline_with_a_single_calendar_echoes_it() {
    let server = server();
    let plan_id = submit(&server).await;
    let resp = baseline(
        &server,
        json!({ "plan_id": plan_id, "start": START, "calendar": { "hours_per_day": 8 } }),
    )
    .await;
    assert_eq!(resp["calendar"]["hours_per_day"], json!(8.0));
}

#[tokio::test]
async fn rebaseline_without_calendar_inherits_the_previous_calendar() {
    let server = server();
    let plan_id = submit(&server).await;
    baseline(
        &server,
        json!({ "plan_id": plan_id, "start": START, "calendar": { "hours_per_day": 6 } }),
    )
    .await;
    let resp = baseline(
        &server,
        json!({ "plan_id": plan_id, "start": START, "reason": "rescope" }),
    )
    .await;
    assert_eq!(resp["calendar"]["hours_per_day"], json!(6.0));
}

#[tokio::test]
async fn rebaseline_with_null_calendar_counts_wall_clock_hours() {
    let server = server();
    let plan_id = submit(&server).await;
    baseline(
        &server,
        json!({ "plan_id": plan_id, "start": START, "calendar": { "hours_per_day": 6 } }),
    )
    .await;
    let resp = baseline(
        &server,
        json!({ "plan_id": plan_id, "start": START, "reason": "drop the calendar",
                "calendar": null }),
    )
    .await;
    assert_eq!(resp["calendar"], Value::Null);
}

#[tokio::test]
async fn rebaseline_with_schedule_cpm_drops_the_levelling() {
    let server = server();
    let plan_id = submit(&server).await;
    baseline(
        &server,
        json!({ "plan_id": plan_id, "start": START, "capacities": capacities() }),
    )
    .await;
    let resp = baseline(
        &server,
        json!({ "plan_id": plan_id, "start": START, "reason": "back to cpm",
                "schedule": "cpm" }),
    )
    .await;
    assert_eq!(resp["schedule"], json!("cpm"));
}

#[tokio::test]
async fn baseline_levelled_without_capacities_is_invalid_value() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "schedule": "levelled" }),
    )
    .await;
    assert!(e.message.starts_with("INVALID_VALUE:"), "{}", e.message);
}

#[tokio::test]
async fn baseline_capacities_with_schedule_cpm_is_invalid_value() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "schedule": "cpm", "capacities": capacities() }),
    )
    .await;
    assert!(e.message.starts_with("INVALID_VALUE:"), "{}", e.message);
}

#[tokio::test]
async fn baseline_resource_key_without_effect_is_invalid_value() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "resource_key": "team" }),
    )
    .await;
    assert!(e.message.starts_with("INVALID_VALUE:"), "{}", e.message);
}

#[tokio::test]
async fn baseline_missing_capacity_is_invalid_capacities() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "capacities": { "agent": 1 } }),
    )
    .await;
    assert!(
        e.message.starts_with("INVALID_CAPACITIES:"),
        "{}",
        e.message
    );
}

#[tokio::test]
async fn baseline_negative_capacity_is_invalid_params() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "capacities": { "agent": -1, "owner": 1 } }),
    )
    .await;
    assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn baseline_unknown_schedule_kind_is_invalid_params() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "schedule": "fastest" }),
    )
    .await;
    assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn baseline_single_calendar_with_an_unknown_field_is_invalid_params() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "calendar": { "hours_per_day": 8, "lunch": true } }),
    )
    .await;
    assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn baseline_calendar_map_entry_error_is_invalid_value() {
    let server = server();
    let plan_id = submit(&server).await;
    let e = err(
        &server,
        "plan.baseline",
        json!({ "plan_id": plan_id, "calendar": { "default": {}, "owner": { "lunch": true } } }),
    )
    .await;
    assert!(e.message.starts_with("INVALID_VALUE:"), "{}", e.message);
}
