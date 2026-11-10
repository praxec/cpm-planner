//! Per-resource working calendars on the levelled schedule, `plan.simulate`
//! and Monte Carlo (`docs/calendars.md`, sections 2 and 3).

use chrono::{DateTime, Utc};
use cpm_planner::calendar::{CalendarArgError, CalendarSpec, ScheduleCalendar};
use cpm_planner::monte_carlo::{MonteCarloRequest, monte_carlo, monte_carlo_on_calendar};
use cpm_planner::plan::{PlanGraph, PlannerError};
use cpm_planner::resource_schedule::{
    ResourceSchedule, ScheduleRequest, resource_schedule, resource_schedule_on_calendar,
};
use cpm_planner::simulate::{SimulateRequest, simulate, simulate_on_calendar};
use serde_json::{Value, json};
use std::collections::BTreeMap;

// ── Fixtures ────────────────────────────────────────────────────────────────

const ALL_WEEK: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Section 8: agents 24/7, the owner Monday to Friday 09:00-17:00 at UTC-5.
fn worked_map() -> Value {
    json!({
        "default": {"hours_per_day": 24, "workdays": ALL_WEEK},
        "owner": {"hours_per_day": 8, "day_start": "09:00",
                  "workdays": ["mon", "tue", "wed", "thu", "fri"], "utc_offset_minutes": -300}
    })
}

fn owner_calendar() -> Value {
    worked_map()["owner"].clone()
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse().expect("rfc3339")
}

fn spec(v: Value) -> CalendarSpec {
    CalendarSpec::parse(&v).expect("calendar")
}

fn on(v: Value, start: &str) -> ScheduleCalendar {
    ScheduleCalendar::new(spec(v), at(start))
}

/// Friday 2026-11-13 14:00 at UTC-5.
const FRIDAY_14: &str = "2026-11-13T19:00:00Z";
/// Monday 2026-11-16 00:00 at UTC-5.
const MONDAY_00: &str = "2026-11-16T05:00:00Z";

fn graph(deliverables: Value) -> PlanGraph {
    serde_json::from_value(json!({ "deliverables": deliverables })).expect("graph")
}

fn work(id: &str, owner: &str, hours: f32, prereqs: &[&str]) -> Value {
    json!({"id": id, "owned_files": [format!("{id}.md")], "prerequisites": prereqs,
           "estimated_effort_hours": hours, "metadata": {"owner": owner}})
}

/// `impl` (agent, 10h) -> `review` (owner, `review_hours`) -> `release` (agent, 1h).
fn worked_graph(review_hours: f32) -> PlanGraph {
    graph(json!([
        work("impl", "agent", 10.0, &[]),
        work("review", "owner", review_hours, &["impl"]),
        work("release", "agent", 1.0, &["review"]),
    ]))
}

fn caps(pairs: &[(&str, u32)]) -> ScheduleRequest {
    ScheduleRequest {
        capacities: pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect::<BTreeMap<_, _>>(),
        resource_key: "owner".to_string(),
        project_buffer_pct: 25.0,
    }
}

fn worked_caps() -> ScheduleRequest {
    caps(&[("agent", 3), ("owner", 1)])
}

fn levelled(g: &PlanGraph, req: &ScheduleRequest, cal: &ScheduleCalendar) -> ResourceSchedule {
    resource_schedule_on_calendar(g, req, Some(cal)).expect("schedule")
}

fn worked(review_hours: f32) -> ResourceSchedule {
    levelled(
        &worked_graph(review_hours),
        &worked_caps(),
        &on(worked_map(), FRIDAY_14),
    )
}

fn row<'a>(s: &'a ResourceSchedule, id: &str) -> &'a cpm_planner::resource_schedule::ScheduledRow {
    s.rows.iter().find(|r| r.id == id).expect("row")
}

fn instants(s: &ResourceSchedule, id: &str) -> (String, String) {
    let r = row(s, id);
    let fmt = |t: Option<DateTime<Utc>>| t.expect("instant").to_rfc3339();
    (fmt(r.start_at), fmt(r.finish_at))
}

/// An agent step of `ready_after` hours from Monday 00:00 local, then a 2-hour
/// owner step: places the owner step at a chosen local ready time.
fn owner_step_ready_after(ready_after: f32) -> (String, String) {
    let g = graph(json!([
        work("lead", "agent", ready_after, &[]),
        work("step", "owner", 2.0, &["lead"]),
    ]));
    instants(
        &levelled(&g, &worked_caps(), &on(worked_map(), MONDAY_00)),
        "step",
    )
}

fn utc(s: &str) -> (String, String) {
    let (a, b) = s.split_once(' ').expect("pair");
    (at(a).to_rfc3339(), at(b).to_rfc3339())
}

// ── Calendar argument ───────────────────────────────────────────────────────

#[test]
fn single_calendar_without_day_start_echoes_midnight() {
    let echoed = serde_json::to_value(spec(json!({}))).expect("json");
    assert_eq!(echoed["day_start"], json!("00:00"));
}

#[test]
fn object_with_default_key_is_a_calendar_map() {
    assert!(matches!(spec(worked_map()), CalendarSpec::Map(_)));
}

#[test]
fn calendar_map_round_trips_through_json() {
    let s = spec(worked_map());
    let back: CalendarSpec =
        serde_json::from_value(serde_json::to_value(&s).expect("json")).expect("parse");
    assert_eq!(back, s);
}

#[test]
fn map_without_default_is_rejected_naming_the_default_entry() {
    let err = CalendarSpec::parse(&json!({"owner": owner_calendar()})).expect_err("no default");
    assert!(matches!(&err, CalendarArgError::Params(r) if r.contains("`default` entry")));
}

#[test]
fn single_calendar_unknown_field_keeps_argument_parsing_error() {
    let err = CalendarSpec::parse(&json!({"hours": 8})).expect_err("unknown field");
    assert!(matches!(err, CalendarArgError::Params(_)));
}

#[test]
fn single_calendar_out_of_range_hours_keeps_invalid_graph() {
    let err = CalendarSpec::parse(&json!({"hours_per_day": 25})).expect_err("range");
    assert!(matches!(
        err,
        CalendarArgError::Invalid(PlannerError::InvalidGraph { .. })
    ));
}

#[test]
fn single_calendar_malformed_day_start_is_invalid_value() {
    let err = CalendarSpec::parse(&json!({"day_start": "24:00"})).expect_err("day_start");
    assert!(matches!(
        err,
        CalendarArgError::Invalid(PlannerError::InvalidValue { .. })
    ));
}

#[test]
fn map_entry_malformed_day_start_names_its_path() {
    let mut map = worked_map();
    map["owner"]["day_start"] = json!("9:00");
    let err = CalendarSpec::parse(&map).expect_err("day_start");
    assert_eq!(
        err.to_string(),
        "INVALID_VALUE: calendar.owner.day_start must be HH:MM (00:00..23:59), got \"9:00\""
    );
}

#[test]
fn map_entry_out_of_range_hours_is_invalid_value() {
    let mut map = worked_map();
    map["owner"]["hours_per_day"] = json!(0);
    let err = CalendarSpec::parse(&map).expect_err("range");
    assert!(matches!(
        err,
        CalendarArgError::Invalid(PlannerError::InvalidValue { .. })
    ));
}

#[test]
fn map_entry_unknown_field_is_invalid_value() {
    let mut map = worked_map();
    map["owner"]["hours"] = json!(8);
    let err = CalendarSpec::parse(&map).expect_err("unknown");
    assert!(matches!(
        err,
        CalendarArgError::Invalid(PlannerError::InvalidValue { .. })
    ));
}

#[test]
fn map_entry_that_is_not_an_object_is_invalid_value() {
    let err = CalendarSpec::parse(&json!({"default": {}, "owner": 8})).expect_err("entry");
    assert!(matches!(
        err,
        CalendarArgError::Invalid(PlannerError::InvalidValue { .. })
    ));
}

#[test]
fn map_with_empty_key_is_invalid_value() {
    let err = CalendarSpec::parse(&json!({"default": {}, "": {}})).expect_err("key");
    assert!(matches!(
        err,
        CalendarArgError::Invalid(PlannerError::InvalidValue { .. })
    ));
}

#[test]
fn map_with_more_than_256_entries_is_invalid_value() {
    let mut map = serde_json::Map::new();
    map.insert("default".into(), json!({}));
    for i in 0..256 {
        map.insert(format!("r{i}"), json!({}));
    }
    let err = CalendarSpec::parse(&Value::Object(map)).expect_err("size");
    assert!(matches!(
        err,
        CalendarArgError::Invalid(PlannerError::InvalidValue { .. })
    ));
}

// ── Worked example (section 8) ──────────────────────────────────────────────

#[test]
fn worked_example_places_rows_in_wall_clock_hours() {
    let s = worked(2.0);
    let spans: Vec<(f32, f32)> = s.rows.iter().map(|r| (r.start, r.finish)).collect();
    assert_eq!(spans, [(0.0, 10.0), (67.0, 69.0), (69.0, 70.0)]);
}

#[test]
fn worked_example_review_starts_monday_nine_local() {
    assert_eq!(
        instants(&worked(2.0), "review"),
        utc("2026-11-16T14:00:00Z 2026-11-16T16:00:00Z")
    );
}

#[test]
fn worked_example_makespan_is_seventy_hours() {
    assert_eq!(worked(2.0).makespan, 70.0);
}

#[test]
fn worked_example_cpm_makespan_uses_the_same_calendars() {
    assert_eq!(worked(2.0).cpm_makespan, 70.0);
}

#[test]
fn worked_example_reports_calendar_wait_per_row() {
    let waits: Vec<Option<f32>> = worked(2.0)
        .rows
        .iter()
        .map(|r| r.calendar_wait_hours)
        .collect();
    assert_eq!(waits, [Some(0.0), Some(57.0), Some(0.0)]);
}

#[test]
fn worked_example_reports_calendar_keys_per_row() {
    let keys: Vec<Vec<String>> = worked(2.0)
        .rows
        .iter()
        .map(|r| r.calendars.clone().expect("keys"))
        .collect();
    assert_eq!(keys, [vec!["default"], vec!["owner"], vec!["default"]]);
}

#[test]
fn worked_example_driving_chain_waits_on_the_owner_calendar() {
    let chain = serde_json::to_value(&worked(2.0).driving_chain).expect("json");
    assert_eq!(
        chain,
        json!([
            {"id": "impl", "waited_on": "start"},
            {"id": "review", "waited_on": "calendar", "waited_on_calendar": "owner",
             "calendar_wait_hours": 57.0},
            {"id": "release", "waited_on": "dependency"}
        ])
    );
}

#[test]
fn worked_example_totals_calendar_wait_on_the_driving_chain() {
    assert_eq!(worked(2.0).calendar_wait_hours, Some(57.0));
}

#[test]
fn worked_example_echoes_the_plan_start() {
    assert_eq!(worked(2.0).start, Some(at(FRIDAY_14)));
}

#[test]
fn owner_utilisation_is_against_its_working_hours() {
    // [0, 70) holds Fri 14:00-17:00 and Mon 09:00-12:00 of owner time.
    let s = worked(2.0);
    let owner = s
        .load
        .iter()
        .find(|l| l.resource == "owner")
        .expect("owner");
    assert_eq!(
        (owner.available_hours, owner.utilisation),
        (Some(6.0), 2.0 / 6.0)
    );
}

#[test]
fn around_the_clock_pool_has_capacity_times_makespan_available() {
    let s = worked(2.0);
    let agent = s
        .load
        .iter()
        .find(|l| l.resource == "agent")
        .expect("agent");
    assert_eq!(agent.available_hours, Some(210.0));
}

#[test]
fn review_spanning_two_windows_pauses_overnight() {
    // L = 10: Mon 09:00-17:00, then Tue 09:00-11:00 (t 93).
    assert_eq!(row(&worked(10.0), "review").finish, 93.0);
}

// ── Placement cases on the owner calendar (section 8 table) ─────────────────

#[test]
fn ready_overnight_starts_at_next_morning() {
    // Tue 22:00 local.
    assert_eq!(
        owner_step_ready_after(46.0),
        utc("2026-11-18T14:00:00Z 2026-11-18T16:00:00Z")
    );
}

#[test]
fn ready_friday_evening_starts_monday_morning() {
    // Fri 18:00 local.
    assert_eq!(
        owner_step_ready_after(114.0),
        utc("2026-11-23T14:00:00Z 2026-11-23T16:00:00Z")
    );
}

#[test]
fn ready_before_day_start_waits_for_day_start() {
    // Wed 07:30 local.
    assert_eq!(
        owner_step_ready_after(55.5),
        utc("2026-11-18T14:00:00Z 2026-11-18T16:00:00Z")
    );
}

#[test]
fn ready_inside_a_window_spans_the_night() {
    // Wed 16:00 local: one hour Wednesday, one hour Thursday from 09:00.
    assert_eq!(
        owner_step_ready_after(64.0),
        utc("2026-11-18T21:00:00Z 2026-11-19T15:00:00Z")
    );
}

#[test]
fn ready_exactly_at_close_is_not_open() {
    // Wed 17:00 local.
    assert_eq!(
        owner_step_ready_after(65.0),
        utc("2026-11-19T14:00:00Z 2026-11-19T16:00:00Z")
    );
}

#[test]
fn work_ending_exactly_at_close_finishes_at_the_close() {
    let g = graph(json!([work("day", "owner", 8.0, &[])]));
    let s = levelled(&g, &worked_caps(), &on(worked_map(), MONDAY_00));
    assert_eq!(
        instants(&s, "day"),
        utc("2026-11-16T14:00:00Z 2026-11-16T22:00:00Z")
    );
}

#[test]
fn plan_start_outside_the_window_makes_the_first_link_a_calendar_wait() {
    let g = graph(json!([work("day", "owner", 1.0, &[])]));
    let s = levelled(&g, &worked_caps(), &on(worked_map(), MONDAY_00));
    assert_eq!(
        serde_json::to_value(&s.driving_chain).expect("json"),
        json!([{"id": "day", "waited_on": "calendar", "waited_on_calendar": "owner",
                "calendar_wait_hours": 9.0}])
    );
}

#[test]
fn window_crossing_midnight_belongs_to_its_opening_weekday() {
    // Open Monday 22:00 to Tuesday 06:00 UTC only; 10 hours from Monday 00:00
    // run Mon 22:00-Tue 06:00 and Mon 22:00-Tue 00:00 a week later.
    let map = json!({"default": {"hours_per_day": 8, "day_start": "22:00", "workdays": ["mon"]}});
    let g = graph(json!([work("night", "agent", 10.0, &[])]));
    let s = levelled(&g, &worked_caps(), &on(map, "2026-11-16T00:00:00Z"));
    assert_eq!(
        instants(&s, "night"),
        utc("2026-11-16T22:00:00Z 2026-11-24T00:00:00Z")
    );
}

#[test]
fn positive_utc_offset_shifts_windows_earlier() {
    // 09:00 at UTC+2 is 07:00 UTC.
    let map = json!({"default": {"day_start": "09:00", "utc_offset_minutes": 120}});
    let g = graph(json!([work("a", "agent", 1.0, &[])]));
    let s = levelled(&g, &worked_caps(), &on(map, "2026-11-16T00:00:00Z"));
    assert_eq!(row(&s, "a").start, 7.0);
}

// ── Resolution and kinds of work ────────────────────────────────────────────

#[test]
fn single_calendar_on_schedule_applies_to_every_resource() {
    // `impl` too works 09:00-17:00: Fri 3h, Mon 7h, finishing Mon 16:00 local.
    let s = levelled(
        &worked_graph(2.0),
        &worked_caps(),
        &on(owner_calendar(), FRIDAY_14),
    );
    assert_eq!(row(&s, "impl").finish, 74.0);
}

#[test]
fn unassigned_deliverable_uses_its_own_entry_when_present() {
    let mut map = worked_map();
    map["unassigned"] = owner_calendar();
    let g = graph(json!([
        {"id": "loose", "owned_files": ["l.md"], "prerequisites": [], "estimated_effort_hours": 1.0}
    ]));
    let s = levelled(&g, &caps(&[("unassigned", 1)]), &on(map, MONDAY_00));
    assert_eq!(
        row(&s, "loose").calendars,
        Some(vec!["unassigned".to_string()])
    );
}

#[test]
fn resource_key_selects_the_calendar_entry() {
    let g = graph(json!([
        {"id": "a", "owned_files": ["a.md"], "prerequisites": [], "estimated_effort_hours": 1.0,
         "metadata": {"team": "owner"}}
    ]));
    let req = ScheduleRequest {
        resource_key: "team".to_string(),
        ..caps(&[("owner", 1)])
    };
    let s = levelled(&g, &req, &on(worked_map(), MONDAY_00));
    assert_eq!(row(&s, "a").start, 9.0);
}

#[test]
fn multi_pool_deliverable_works_in_the_intersection() {
    let g = graph(json!([
        {"id": "pair", "owned_files": ["p.md"], "prerequisites": [], "estimated_effort_hours": 1.0,
         "metadata": {"resources": {"agent": 1, "owner": 1}}}
    ]));
    let s = levelled(&g, &worked_caps(), &on(worked_map(), MONDAY_00));
    assert_eq!(
        (
            row(&s, "pair").start,
            row(&s, "pair").calendars.clone().expect("keys")
        ),
        (9.0, vec!["default".to_string(), "owner".to_string()])
    );
}

#[test]
fn multi_pool_deliverable_with_disjoint_calendars_is_invalid_value() {
    let map = json!({
        "default": {"day_start": "00:00", "hours_per_day": 4},
        "owner": {"day_start": "12:00", "hours_per_day": 4}
    });
    let g = graph(json!([
        {"id": "pair", "owned_files": ["p.md"], "prerequisites": [], "estimated_effort_hours": 1.0,
         "metadata": {"resources": {"agent": 1, "owner": 1}}}
    ]));
    let err = resource_schedule_on_calendar(&g, &worked_caps(), Some(&on(map, MONDAY_00)))
        .expect_err("disjoint");
    assert_eq!(
        err.to_string(),
        "INVALID_VALUE: deliverable \"pair\": calendars [default, owner] never overlap"
    );
}

#[test]
fn wall_clock_only_deliverable_runs_through_closed_time() {
    let g = graph(json!([
        {"id": "soak", "owned_files": ["s.md"], "prerequisites": [], "duration_hours": 30.0,
         "metadata": {"wall_clock_only": true, "owner": "owner"}}
    ]));
    let s = levelled(&g, &worked_caps(), &on(worked_map(), FRIDAY_14));
    assert_eq!(
        (row(&s, "soak").finish, row(&s, "soak").calendars.clone()),
        (30.0, Some(Vec::new()))
    );
}

#[test]
fn milestone_stays_at_its_instant_outside_windows() {
    let g = graph(json!([
        work("impl", "agent", 10.0, &[]),
        {"id": "gate", "owned_files": [], "prerequisites": ["impl"], "milestone": true,
         "metadata": {"owner": "owner"}}
    ]));
    let s = levelled(&g, &worked_caps(), &on(worked_map(), FRIDAY_14));
    assert_eq!(
        (row(&s, "gate").start, row(&s, "gate").finish),
        (10.0, 10.0)
    );
}

#[test]
fn lag_is_wall_clock_time() {
    // `impl` ends Fri 15:00; a 20-hour lag lands Sat 11:00; the owner starts Monday.
    let g = graph(json!([
        work("impl", "agent", 1.0, &[]),
        {"id": "review", "owned_files": ["r.md"], "estimated_effort_hours": 1.0,
         "prerequisites": [{"id": "impl", "lag_hours": 20.0}], "metadata": {"owner": "owner"}}
    ]));
    let s = levelled(&g, &worked_caps(), &on(worked_map(), FRIDAY_14));
    assert_eq!(row(&s, "review").start, 67.0);
}

#[test]
fn levelling_prefers_a_candidate_that_can_start_now() {
    // One `x` unit. `h` (longer tail) needs the owner's window too, so on a
    // Saturday start `m` takes the unit first.
    let g = graph(json!([
        {"id": "h", "owned_files": ["h.md"], "prerequisites": [], "estimated_effort_hours": 5.0,
         "metadata": {"resources": {"x": 1, "owner": 1}}},
        {"id": "m", "owned_files": ["m.md"], "prerequisites": [], "estimated_effort_hours": 1.0,
         "metadata": {"resources": {"x": 1}}}
    ]));
    let s = levelled(
        &g,
        &caps(&[("x", 1), ("owner", 1)]),
        &on(worked_map(), "2026-11-14T05:00:00Z"),
    );
    assert_eq!(row(&s, "m").start, 0.0);
}

#[test]
fn a_levelled_unit_is_held_across_closed_time() {
    // `a` spans the night on the single owner unit, so `b` waits for it.
    let g = graph(json!([
        work("a", "owner", 10.0, &[]),
        work("b", "owner", 1.0, &[]),
    ]));
    let s = levelled(&g, &worked_caps(), &on(worked_map(), MONDAY_00));
    assert_eq!(row(&s, "b").start, row(&s, "a").finish);
}

#[test]
fn calendar_schedule_keeps_capacity_errors() {
    let err = resource_schedule_on_calendar(
        &worked_graph(2.0),
        &caps(&[("agent", 1)]),
        Some(&on(worked_map(), FRIDAY_14)),
    )
    .expect_err("missing owner");
    assert!(matches!(err, PlannerError::InvalidCapacities { missing } if missing == ["owner"]));
}

// ── Monte Carlo and plan.simulate ───────────────────────────────────────────

fn mc() -> MonteCarloRequest {
    MonteCarloRequest {
        iterations: 200,
        seed: 7,
    }
}

#[test]
fn monte_carlo_deterministic_makespan_is_wall_clock_on_a_calendar() {
    let m = monte_carlo_on_calendar(
        &worked_graph(2.0),
        &mc(),
        Some(&on(worked_map(), FRIDAY_14)),
    )
    .expect("mc");
    assert_eq!(m.deterministic_makespan, 70.0);
}

#[test]
fn monte_carlo_percentiles_are_wall_clock_on_a_calendar() {
    let m = monte_carlo_on_calendar(
        &worked_graph(2.0),
        &mc(),
        Some(&on(worked_map(), FRIDAY_14)),
    )
    .expect("mc");
    assert_eq!((m.p50, m.p95), (70.0, 70.0));
}

#[test]
fn monte_carlo_sampled_owner_work_waits_for_windows() {
    // A sampled 1-3h review always lands inside Monday's window.
    let g = graph(json!([
        work("impl", "agent", 10.0, &[]),
        {"id": "review", "owned_files": ["r.md"], "prerequisites": ["impl"],
         "estimate": {"optimistic": 1.0, "likely": 2.0, "pessimistic": 3.0},
         "metadata": {"owner": "owner"}}
    ]));
    let m = monte_carlo_on_calendar(&g, &mc(), Some(&on(worked_map(), FRIDAY_14))).expect("mc");
    assert!(m.p95 > 67.0 && m.p95 <= 70.0);
}

#[test]
fn monte_carlo_criticality_uses_the_calendar_backward_pass() {
    // `impl` could start 57 hours later and `review` would still start Monday
    // 09:00, so it has float; `review` and `release` have none.
    let m = monte_carlo_on_calendar(
        &worked_graph(2.0),
        &mc(),
        Some(&on(worked_map(), FRIDAY_14)),
    )
    .expect("mc");
    let index: Vec<f32> = m.criticality.iter().map(|c| c.index).collect();
    assert_eq!(index, [0.0, 1.0, 1.0]);
}

#[test]
fn monte_carlo_without_calendar_is_unchanged() {
    let g = worked_graph(2.0);
    assert_eq!(
        monte_carlo_on_calendar(&g, &mc(), None).expect("mc"),
        monte_carlo(&g, &mc()).expect("mc")
    );
}

#[test]
fn simulate_levels_on_the_calendar() {
    let req = SimulateRequest {
        schedule: Some(worked_caps()),
        monte_carlo: None,
    };
    let sim = simulate_on_calendar(&worked_graph(2.0), &req, Some(&on(worked_map(), FRIDAY_14)))
        .expect("simulate");
    assert_eq!(sim.scorecard.resource_makespan, Some(70.0));
}

#[test]
fn simulate_monte_carlo_uses_the_schedule_resource_key_for_calendars() {
    let g = graph(json!([
        {"id": "a", "owned_files": ["a.md"], "prerequisites": [], "estimated_effort_hours": 1.0,
         "metadata": {"team": "owner"}}
    ]));
    let req = SimulateRequest {
        schedule: Some(ScheduleRequest {
            resource_key: "team".to_string(),
            ..caps(&[("owner", 1)])
        }),
        monte_carlo: Some(mc()),
    };
    let sim = simulate_on_calendar(&g, &req, Some(&on(worked_map(), MONDAY_00))).expect("simulate");
    assert_eq!(
        sim.scorecard
            .monte_carlo
            .expect("mc")
            .deterministic_makespan,
        10.0
    );
}

#[test]
fn simulate_keeps_the_cpm_section_calendar_free() {
    let req = SimulateRequest::default();
    let sim = simulate_on_calendar(&worked_graph(2.0), &req, Some(&on(worked_map(), FRIDAY_14)))
        .expect("simulate");
    assert_eq!(sim.critical_path_hours, 13.0);
}

// ── Without a calendar: byte-for-byte today's output ────────────────────────

/// Lags, a multi-pool claim, `wall_clock_only`, sampled estimates, an
/// unassigned deliverable and a milestone.
fn legacy_fixture() -> PlanGraph {
    graph(json!([
        {"id": "design", "owned_files": ["a"], "prerequisites": [],
         "estimate": {"optimistic": 2.0, "likely": 3.5, "pessimistic": 9.0}, "metadata": {"owner": "agent"}},
        {"id": "build", "owned_files": ["b"], "prerequisites": [{"id": "design", "lag_hours": 1.5}],
         "estimated_effort_hours": 7.25, "metadata": {"owner": "agent"}},
        {"id": "docs", "owned_files": ["c"], "prerequisites": ["design"],
         "estimate": {"optimistic": 1.0, "likely": 2.0, "pessimistic": 6.0}, "metadata": {"owner": "agent"}},
        {"id": "pair", "owned_files": ["d"], "prerequisites": ["design"], "duration_hours": 2.5,
         "metadata": {"resources": {"agent": 1, "owner": 1}}},
        {"id": "soak", "owned_files": ["e"], "prerequisites": ["build"], "duration_hours": 4.0,
         "metadata": {"wall_clock_only": true}},
        {"id": "review", "owned_files": ["f"], "prerequisites": ["soak", "docs", "pair"],
         "estimated_effort_hours": 1.0, "metadata": {"owner": "owner"}},
        {"id": "loose", "owned_files": ["g"], "prerequisites": [], "estimated_effort_hours": 3.0},
        {"id": "ship", "owned_files": [], "prerequisites": ["review", "loose"], "milestone": true}
    ]))
}

fn legacy_caps() -> ScheduleRequest {
    caps(&[("agent", 2), ("owner", 1), ("unassigned", 1)])
}

fn legacy_simulate_request() -> SimulateRequest {
    SimulateRequest {
        schedule: Some(legacy_caps()),
        monte_carlo: Some(MonteCarloRequest {
            iterations: 300,
            seed: 9,
        }),
    }
}

/// `resource_schedule` output captured before calendars existed.
const LEGACY_SCHEDULE: &str = r#"{"makespan":17.75,"cpm_makespan":17.25,"rows":[{"id":"design","resource":"agent","resources":{"agent":1},"start":0.0,"finish":3.5},{"id":"build","resource":"agent","resources":{"agent":1},"start":5.5,"finish":12.75},{"id":"docs","resource":"agent","resources":{"agent":1},"start":3.5,"finish":5.5},{"id":"pair","resource":"agent+owner","resources":{"agent":1,"owner":1},"start":3.5,"finish":6.0},{"id":"soak","resource":"wall_clock","start":12.75,"finish":16.75},{"id":"review","resource":"owner","resources":{"owner":1},"start":16.75,"finish":17.75},{"id":"loose","resource":"unassigned","resources":{"unassigned":1},"start":0.0,"finish":3.0},{"id":"ship","resource":"unassigned","resources":{"unassigned":1},"start":17.75,"finish":17.75}],"load":[{"resource":"agent","capacity":2,"busy_hours":15.25,"utilisation":0.42957747},{"resource":"owner","capacity":1,"busy_hours":3.5,"utilisation":0.1971831},{"resource":"unassigned","capacity":1,"busy_hours":3.0,"utilisation":0.16901408}],"driving_chain":[{"id":"design","waited_on":"start"},{"id":"docs","waited_on":"dependency"},{"id":"build","waited_on":"resource","waited_on_resource":"agent"},{"id":"soak","waited_on":"dependency"},{"id":"review","waited_on":"dependency"}],"project_buffer_hours":4.4375,"feeding_buffers":[{"joins":"review","from_chain":["pair"],"buffer_hours":0.625}]}"#;

/// `simulate` output captured before calendars existed.
const LEGACY_SIMULATE: &str = r#"{"lint":{"clean":false,"findings":[{"code":"NO_RATIONALE","severity":"warning","message":"'build' depends on 'design' without saying what it consumes","ids":["build","design"]},{"code":"NO_RATIONALE","severity":"warning","message":"'docs' depends on 'design' without saying what it consumes","ids":["docs","design"]},{"code":"NO_RATIONALE","severity":"warning","message":"'pair' depends on 'design' without saying what it consumes","ids":["pair","design"]},{"code":"NO_RATIONALE","severity":"warning","message":"'review' depends on 'docs' without saying what it consumes","ids":["review","docs"]},{"code":"NO_RATIONALE","severity":"warning","message":"'review' depends on 'pair' without saying what it consumes","ids":["review","pair"]},{"code":"NO_RATIONALE","severity":"warning","message":"'review' depends on 'soak' without saying what it consumes","ids":["review","soak"]},{"code":"NO_RATIONALE","severity":"warning","message":"'ship' depends on 'loose' without saying what it consumes","ids":["ship","loose"]},{"code":"NO_RATIONALE","severity":"warning","message":"'ship' depends on 'review' without saying what it consumes","ids":["ship","review"]},{"code":"NO_RATIONALE","severity":"warning","message":"'soak' depends on 'build' without saying what it consumes","ids":["soak","build"]},{"code":"NO_ARTIFACT","severity":"info","message":"'build' declares no metadata.artifact","ids":["build"]},{"code":"NO_ARTIFACT","severity":"info","message":"'design' declares no metadata.artifact","ids":["design"]},{"code":"NO_ARTIFACT","severity":"info","message":"'docs' declares no metadata.artifact","ids":["docs"]},{"code":"NO_ARTIFACT","severity":"info","message":"'loose' declares no metadata.artifact","ids":["loose"]},{"code":"NO_ARTIFACT","severity":"info","message":"'pair' declares no metadata.artifact","ids":["pair"]},{"code":"NO_ARTIFACT","severity":"info","message":"'review' declares no metadata.artifact","ids":["review"]},{"code":"NO_ARTIFACT","severity":"info","message":"'soak' declares no metadata.artifact","ids":["soak"]}],"waves":{"gate_count":0,"wave_count":0,"work_hours":23.25,"gate_hours":0.0,"gate_fraction":0.0,"waves":[],"milestones":[{"id":"ship","class":"checkpoint"}],"ungated":[]}},"critical_path":["__start__","design","build","soak","review","ship","__finish__"],"critical_path_hours":17.25,"schedule":[{"id":"__start__","es":0.0,"ef":0.0,"ls":0.0,"lf":0.0,"float":0.0,"critical":true,"synthetic":true},{"id":"design","es":0.0,"ef":3.5,"ls":0.0,"lf":3.5,"float":0.0,"critical":true,"synthetic":false},{"id":"build","es":5.0,"ef":12.25,"ls":5.0,"lf":12.25,"float":0.0,"critical":true,"synthetic":false},{"id":"docs","es":3.5,"ef":5.5,"ls":14.25,"lf":16.25,"float":10.75,"critical":false,"synthetic":false},{"id":"pair","es":3.5,"ef":6.0,"ls":13.75,"lf":16.25,"float":10.25,"critical":false,"synthetic":false},{"id":"soak","es":12.25,"ef":16.25,"ls":12.25,"lf":16.25,"float":0.0,"critical":true,"synthetic":false},{"id":"review","es":16.25,"ef":17.25,"ls":16.25,"lf":17.25,"float":0.0,"critical":true,"synthetic":false},{"id":"loose","es":0.0,"ef":3.0,"ls":14.25,"lf":17.25,"float":14.25,"critical":false,"synthetic":false},{"id":"ship","es":17.25,"ef":17.25,"ls":17.25,"lf":17.25,"float":0.0,"critical":true,"synthetic":false},{"id":"__finish__","es":17.25,"ef":17.25,"ls":17.25,"lf":17.25,"float":0.0,"critical":true,"synthetic":true}],"milestones":[{"id":"ship","critical_path":["__start__","design","build","soak","review","ship"],"hours":17.25,"complete":false}],"resource_schedule":{"makespan":17.75,"cpm_makespan":17.25,"rows":[{"id":"design","resource":"agent","resources":{"agent":1},"start":0.0,"finish":3.5},{"id":"build","resource":"agent","resources":{"agent":1},"start":5.5,"finish":12.75},{"id":"docs","resource":"agent","resources":{"agent":1},"start":3.5,"finish":5.5},{"id":"pair","resource":"agent+owner","resources":{"agent":1,"owner":1},"start":3.5,"finish":6.0},{"id":"soak","resource":"wall_clock","start":12.75,"finish":16.75},{"id":"review","resource":"owner","resources":{"owner":1},"start":16.75,"finish":17.75},{"id":"loose","resource":"unassigned","resources":{"unassigned":1},"start":0.0,"finish":3.0},{"id":"ship","resource":"unassigned","resources":{"unassigned":1},"start":17.75,"finish":17.75}],"load":[{"resource":"agent","capacity":2,"busy_hours":15.25,"utilisation":0.42957747},{"resource":"owner","capacity":1,"busy_hours":3.5,"utilisation":0.1971831},{"resource":"unassigned","capacity":1,"busy_hours":3.0,"utilisation":0.16901408}],"driving_chain":[{"id":"design","waited_on":"start"},{"id":"docs","waited_on":"dependency"},{"id":"build","waited_on":"resource","waited_on_resource":"agent"},{"id":"soak","waited_on":"dependency"},{"id":"review","waited_on":"dependency"}],"project_buffer_hours":4.4375,"feeding_buffers":[{"joins":"review","from_chain":["pair"],"buffer_hours":0.625}]},"scorecard":{"deliverables":8,"makespan":17.25,"resource_makespan":17.75,"critical_path_len":5,"critical_count":5,"near_critical_count":0,"total_float":35.25,"criticality_risk":0.71875,"risk_band":"in_target","max_drag":["build",7.25],"diameter":17.25,"cyclomatic_complexity":3,"complexity_flag":"in_target","merge_bias_count":1,"total_effort":24.75,"parallelism":1.3478261,"peak_load":0.42957747,"lint_errors":0,"lint_warnings":9,"monte_carlo":{"iterations":300,"seed":9,"p50":17.823689,"p80":18.855799,"p95":19.948006,"mean":17.899313,"deterministic_makespan":17.25,"criticality":[{"id":"design","index":1.0},{"id":"build","index":1.0},{"id":"docs","index":0.0},{"id":"pair","index":0.0},{"id":"soak","index":1.0},{"id":"review","index":1.0},{"id":"loose","index":0.0},{"id":"ship","index":1.0}],"sensitivity":[{"id":"design","correlation":1.0},{"id":"docs","correlation":-0.022913447}]}}}"#;

#[test]
fn schedule_without_calendar_is_byte_for_byte_unchanged() {
    let s = resource_schedule(&legacy_fixture(), &legacy_caps()).expect("schedule");
    assert_eq!(serde_json::to_string(&s).expect("json"), LEGACY_SCHEDULE);
}

#[test]
fn schedule_on_no_calendar_is_byte_for_byte_unchanged() {
    let s =
        resource_schedule_on_calendar(&legacy_fixture(), &legacy_caps(), None).expect("schedule");
    assert_eq!(serde_json::to_string(&s).expect("json"), LEGACY_SCHEDULE);
}

#[test]
fn simulate_without_calendar_is_byte_for_byte_unchanged() {
    let sim = simulate(&legacy_fixture(), &legacy_simulate_request()).expect("simulate");
    assert_eq!(serde_json::to_string(&sim).expect("json"), LEGACY_SIMULATE);
}

#[test]
fn simulate_on_no_calendar_is_byte_for_byte_unchanged() {
    let sim = simulate_on_calendar(&legacy_fixture(), &legacy_simulate_request(), None)
        .expect("simulate");
    assert_eq!(serde_json::to_string(&sim).expect("json"), LEGACY_SIMULATE);
}
