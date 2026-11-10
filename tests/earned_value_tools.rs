//! Planner-level tests for the earned-value operations: `baseline`,
//! `earned_value` and `snapshot` (P5 Task 3). Every test drives a fake
//! clock so baseline starts, `as_of` defaults and snapshot instants are
//! deterministic.

#![allow(clippy::float_cmp)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use cpm_planner::BasicCpmPlanner;
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::calendar::{CalendarSpec, ScheduleCalendar};
use cpm_planner::earned_value::{
    BaselineCalendar, BaselineOutcome, BaselineRequest, Calendar, EvRowStatus, ScheduleKind,
    SnapshotFormat, SnapshotRequest,
};
use cpm_planner::plan::{
    AcquireRequest, CallerId, Deliverable, DeliverableStatus, MarkStatusRequest, PlanGraph, PlanId,
    PlannerError, ReviseRequest, SyncRequest,
};
use cpm_planner::ports::Planner;
use cpm_planner::resource_schedule::{
    ScheduleRequest, resource_schedule, resource_schedule_on_calendar,
};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap()
}

struct Fixture {
    planner: BasicCpmPlanner,
    now: Arc<Mutex<DateTime<Utc>>>,
    audit: Arc<MemoryAuditSink>,
}

impl Fixture {
    fn new() -> Self {
        let now = Arc::new(Mutex::new(t0()));
        let clock = now.clone();
        let audit = Arc::new(MemoryAuditSink::new());
        let planner = BasicCpmPlanner::with_parts(
            audit.clone(),
            std::time::Duration::from_secs(3600),
            Arc::new(move || *clock.lock().unwrap()),
        );
        Self {
            planner,
            now,
            audit,
        }
    }

    fn advance(&self, hours: i64) {
        let mut now = self.now.lock().unwrap();
        *now += Duration::hours(hours);
    }

    async fn plan(&self) -> PlanId {
        self.planner.submit_plan(graph()).await.unwrap()
    }

    async fn baselined(&self) -> PlanId {
        let plan_id = self.plan().await;
        self.planner
            .baseline(BaselineRequest::new(plan_id.clone()))
            .await
            .unwrap();
        plan_id
    }

    /// Lease `a`, mark it in progress with 50% earned and `hours` spent.
    async fn report_a(&self, plan_id: &PlanId, hours: f32) {
        self.planner
            .acquire_cohort(AcquireRequest::new(plan_id.clone(), worker(), 1))
            .await
            .unwrap();
        self.planner
            .mark_status(
                MarkStatusRequest::new(
                    plan_id.clone(),
                    "a",
                    worker(),
                    DeliverableStatus::InProgress,
                )
                .with_earned_pct(50)
                .with_actual_effort_hours(hours),
            )
            .await
            .unwrap();
    }
}

fn worker() -> CallerId {
    CallerId("w1".into())
}

fn deliverable(id: &str, effort: f32, prereqs: &[&str]) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: vec![format!("src/{id}.rs").as_str().into()],
        prerequisites: prereqs.iter().map(|p| (*p).into()).collect(),
        estimated_effort_hours: Some(effort),
        duration_hours: None,
        estimate: None,
        metadata: serde_json::Value::Null,
        milestone: false,
        earning_rule: Some(cpm_planner::earned_value::EarningRule::Weighted),
    }
}

/// `a` (10h) then `b` (10h): BAC 20, finish at 20h.
fn graph() -> PlanGraph {
    PlanGraph {
        deliverables: vec![deliverable("a", 10.0, &[]), deliverable("b", 10.0, &["a"])],
        max_chained_dispatch: None,
    }
}

fn err_text<T: std::fmt::Debug>(r: Result<T, PlannerError>) -> String {
    r.expect_err("expected an error").to_string()
}

/// A named line `web` in project `p`: `main` (selected) and the draft `alt`.
async fn main_and_draft(f: &Fixture) -> (PlanId, PlanId) {
    let main = f
        .planner
        .sync_plan(SyncRequest::new("p", "web", "main", graph()))
        .await
        .unwrap()
        .plan_id;
    let mut alt_graph = graph();
    alt_graph.deliverables[1].estimated_effort_hours = Some(5.0);
    let alt = f
        .planner
        .sync_plan(SyncRequest::new("p", "web", "alt", alt_graph))
        .await
        .unwrap()
        .plan_id;
    (main, alt)
}

// ── plan.baseline ───────────────────────────────────────────────────────

#[tokio::test]
async fn first_baseline_is_number_one() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    let out = f
        .planner
        .baseline(BaselineRequest::new(plan_id))
        .await
        .unwrap();
    assert_eq!(out.baseline_number, 1);
}

#[tokio::test]
async fn baseline_start_defaults_to_the_clock() {
    let f = Fixture::new();
    f.advance(3);
    let plan_id = f.plan().await;
    let out = f
        .planner
        .baseline(BaselineRequest::new(plan_id))
        .await
        .unwrap();
    assert_eq!(out.start, t0() + Duration::hours(3));
}

#[tokio::test]
async fn baseline_reports_budget_at_completion() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    let out = f
        .planner
        .baseline(BaselineRequest::new(plan_id))
        .await
        .unwrap();
    assert_eq!(out.bac, 20.0);
}

#[tokio::test]
async fn rebaseline_without_reason_is_rejected() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    let e = err_text(f.planner.baseline(BaselineRequest::new(plan_id)).await);
    assert!(e.contains("reason"), "{e}");
}

#[tokio::test]
async fn rebaseline_with_blank_reason_is_rejected() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    let r = f
        .planner
        .baseline(BaselineRequest::new(plan_id).with_reason("   "))
        .await;
    assert!(matches!(r, Err(PlannerError::InvalidGraph { .. })));
}

#[tokio::test]
async fn rebaseline_with_reason_takes_the_next_number() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    let out = f
        .planner
        .baseline(BaselineRequest::new(plan_id).with_reason("scope change"))
        .await
        .unwrap();
    assert_eq!(out.baseline_number, 2);
}

#[tokio::test]
async fn baseline_reason_over_2048_chars_is_rejected() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    let r = f
        .planner
        .baseline(BaselineRequest::new(plan_id).with_reason("x".repeat(2049)))
        .await;
    assert!(matches!(r, Err(PlannerError::InvalidGraph { .. })));
}

#[tokio::test]
async fn baseline_of_unknown_plan_is_plan_not_found() {
    let f = Fixture::new();
    let r = f
        .planner
        .baseline(BaselineRequest::new(PlanId("nope".into())))
        .await;
    assert!(matches!(r, Err(PlannerError::PlanNotFound { .. })));
}

#[tokio::test]
async fn baseline_on_draft_variant_is_variant_not_selected() {
    let f = Fixture::new();
    let (_, alt) = main_and_draft(&f).await;
    let r = f.planner.baseline(BaselineRequest::new(alt)).await;
    assert!(matches!(r, Err(PlannerError::VariantNotSelected { .. })));
}

#[tokio::test]
async fn baseline_is_audited_as_plan_ev_baselined() {
    let f = Fixture::new();
    f.baselined().await;
    assert!(
        f.audit
            .event_types()
            .contains(&"plan.ev.baselined".to_string())
    );
}

#[tokio::test]
async fn rebaseline_audit_carries_the_reason() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.planner
        .baseline(BaselineRequest::new(plan_id).with_reason("scope change"))
        .await
        .unwrap();
    let last = f.audit.snapshot().pop().unwrap();
    assert_eq!(last.payload["reason"], "scope change");
}

// ── plan.ev ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn baseline_then_ev_roundtrip_reports_planned_value() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.advance(5);
    let r = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(r.pv, 5.0);
}

#[tokio::test]
async fn ev_before_baseline_is_not_baselined() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    let e = err_text(f.planner.ev(&plan_id, None).await);
    assert!(e.starts_with("NOT_BASELINED:"), "{e}");
}

#[tokio::test]
async fn ev_of_unknown_plan_is_plan_not_found() {
    let f = Fixture::new();
    let r = f.planner.ev(&PlanId("nope".into()), None).await;
    assert!(matches!(r, Err(PlannerError::PlanNotFound { .. })));
}

#[tokio::test]
async fn ev_honours_an_explicit_as_of() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    let r = f
        .planner
        .ev(&plan_id, Some(t0() + Duration::hours(15)))
        .await
        .unwrap();
    assert_eq!(r.pv, 15.0);
}

#[tokio::test]
async fn ev_reports_earned_value_from_mark_status() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.report_a(&plan_id, 4.0).await;
    let r = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(r.ev, 5.0);
}

#[tokio::test]
async fn ev_right_after_baseline_with_completed_work_has_null_spi() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.complete_a(&plan_id).await;
    let r = f
        .planner
        .ev(&plan_id, Some(t0() + Duration::seconds(1)))
        .await
        .unwrap();
    assert!(r.spi.is_none());
}

#[tokio::test]
async fn rebaseline_keeps_actuals() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.report_a(&plan_id, 4.0).await;
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_reason("replan"))
        .await
        .unwrap();
    let r = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!((r.baseline_number, r.ac), (2, 4.0));
}

#[tokio::test]
async fn rebaseline_resets_the_planned_value_curve() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.advance(10);
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_reason("replan"))
        .await
        .unwrap();
    let r = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(r.pv, 0.0);
}

#[tokio::test]
async fn ev_reads_a_deselected_variant() {
    let f = Fixture::new();
    let (main, alt) = main_and_draft(&f).await;
    f.planner
        .baseline(BaselineRequest::new(main.clone()))
        .await
        .unwrap();
    f.planner.select_variant(&alt, false).await.unwrap();
    let r = f.planner.ev(&main, None).await.unwrap();
    assert_eq!(r.bac, 20.0);
}

// ── plan.snapshot ───────────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_before_baseline_is_not_baselined() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    let e = err_text(f.planner.snapshot(SnapshotRequest::new(plan_id)).await);
    assert!(e.starts_with("NOT_BASELINED:"), "{e}");
}

#[tokio::test]
async fn snapshot_markdown_contains_header_row() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    let out = f
        .planner
        .snapshot(SnapshotRequest::new(plan_id).with_format(SnapshotFormat::Markdown))
        .await
        .unwrap();
    assert!(
        out.export
            .as_str()
            .is_some_and(|md| md.contains("| date | PV | EV | AC | SPI | CPI | EAC |"))
    );
}

#[tokio::test]
async fn snapshot_json_export_lists_every_snapshot() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.planner
        .snapshot(SnapshotRequest::new(plan_id.clone()))
        .await
        .unwrap();
    let out = f
        .planner
        .snapshot(SnapshotRequest::new(plan_id))
        .await
        .unwrap();
    assert_eq!(out.export.as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn snapshot_summary_is_taken_at_the_clock() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.advance(2);
    let out = f
        .planner
        .snapshot(SnapshotRequest::new(plan_id))
        .await
        .unwrap();
    assert_eq!(out.summary.taken_at, t0() + Duration::hours(2));
}

#[tokio::test]
async fn snapshots_at_one_clock_instant_are_both_kept() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.planner
        .snapshot(SnapshotRequest::new(plan_id.clone()))
        .await
        .unwrap();
    let out = f
        .planner
        .snapshot(SnapshotRequest::new(plan_id))
        .await
        .unwrap();
    assert_eq!(out.snapshot_count, 2);
}

/// Baseline at t0, report `a` 50% at 5h (EV 5), then snapshot as of 10h
/// and 12h: PV 10 and 12, so SPI 0.5 and about 0.42.
async fn two_low_snapshots(f: &Fixture) -> (PlanId, Vec<String>) {
    let plan_id = f.baselined().await;
    f.report_a(&plan_id, 1.0).await;
    let mut alerts = Vec::new();
    for hours in [10, 12] {
        let out = f
            .planner
            .snapshot(
                SnapshotRequest::new(plan_id.clone()).with_as_of(t0() + Duration::hours(hours)),
            )
            .await
            .unwrap();
        alerts = out.summary.alerts;
    }
    (plan_id, alerts)
}

#[tokio::test]
async fn two_low_snapshots_raise_spi_alert() {
    let f = Fixture::new();
    let (_, alerts) = two_low_snapshots(&f).await;
    assert!(alerts.contains(&"SPI_BELOW_0_9".to_string()), "{alerts:?}");
}

#[tokio::test]
async fn ev_after_two_low_snapshots_raises_spi_alert() {
    let f = Fixture::new();
    let (plan_id, _) = two_low_snapshots(&f).await;
    let r = f.planner.ev(&plan_id, None).await.unwrap();
    assert!(r.alerts.contains(&"SPI_BELOW_0_9".to_string()));
}

#[tokio::test]
async fn first_low_snapshot_raises_no_alert() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    let out = f
        .planner
        .snapshot(SnapshotRequest::new(plan_id).with_as_of(t0() + Duration::hours(10)))
        .await
        .unwrap();
    assert!(out.summary.alerts.is_empty());
}

#[tokio::test]
async fn snapshot_on_draft_variant_is_variant_not_selected() {
    let f = Fixture::new();
    let (_, alt) = main_and_draft(&f).await;
    let r = f.planner.snapshot(SnapshotRequest::new(alt)).await;
    assert!(matches!(r, Err(PlannerError::VariantNotSelected { .. })));
}

#[tokio::test]
async fn snapshot_export_keeps_the_newest_100() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    let mut last = None;
    for _ in 0..101 {
        last = Some(
            f.planner
                .snapshot(SnapshotRequest::new(plan_id.clone()))
                .await
                .unwrap(),
        );
    }
    let last = last.unwrap();
    assert_eq!(
        (last.export.as_array().map(Vec::len), last.snapshot_count),
        (Some(100), 101)
    );
}

// ── Fix round 1: ordering, per-baseline alerts, gating ───────────────────

#[tokio::test]
async fn rebaseline_keeps_earned_value() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.report_a(&plan_id, 4.0).await;
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_reason("replan"))
        .await
        .unwrap();
    let r = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(r.ev, 5.0);
}

#[tokio::test]
async fn newly_selected_variant_takes_baseline_one_without_reason() {
    let f = Fixture::new();
    let (main, alt) = main_and_draft(&f).await;
    f.planner
        .baseline(BaselineRequest::new(main))
        .await
        .unwrap();
    f.planner.select_variant(&alt, false).await.unwrap();
    let out = f.planner.baseline(BaselineRequest::new(alt)).await.unwrap();
    assert_eq!(out.baseline_number, 1);
}

#[tokio::test]
async fn snapshot_on_archived_variant_is_archive_refused() {
    let f = Fixture::new();
    let (main, _) = main_and_draft(&f).await;
    f.planner
        .archive("p", "web", None, true, false)
        .await
        .unwrap();
    let r = f.planner.snapshot(SnapshotRequest::new(main)).await;
    assert!(matches!(r, Err(PlannerError::ArchiveRefused { .. })));
}

#[tokio::test]
async fn ev_alerts_ignore_the_current_reading() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.planner
        .snapshot(SnapshotRequest::new(plan_id.clone()).with_as_of(t0() + Duration::hours(10)))
        .await
        .unwrap();
    let r = f
        .planner
        .ev(&plan_id, Some(t0() + Duration::hours(12)))
        .await
        .unwrap();
    assert!(r.alerts.is_empty(), "{:?}", r.alerts);
}

#[tokio::test]
async fn snapshot_alerts_ignore_snapshots_from_previous_baseline() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.planner
        .snapshot(SnapshotRequest::new(plan_id.clone()).with_as_of(t0() + Duration::hours(10)))
        .await
        .unwrap();
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_reason("replan"))
        .await
        .unwrap();
    let out = f
        .planner
        .snapshot(SnapshotRequest::new(plan_id).with_as_of(t0() + Duration::hours(12)))
        .await
        .unwrap();
    assert!(out.summary.alerts.is_empty(), "{:?}", out.summary.alerts);
}

#[tokio::test]
async fn backfilled_snapshot_sorts_by_as_of() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    let mut last = None;
    for hours in [10, 5] {
        last = Some(
            f.planner
                .snapshot(
                    SnapshotRequest::new(plan_id.clone()).with_as_of(t0() + Duration::hours(hours)),
                )
                .await
                .unwrap(),
        );
    }
    let export = last.unwrap().export;
    let order: Vec<serde_json::Value> = export
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["as_of"].clone())
        .collect();
    let want: Vec<serde_json::Value> = [5, 10]
        .iter()
        .map(|h| serde_json::to_value(t0() + Duration::hours(*h)).unwrap())
        .collect();
    assert_eq!(order, want);
}

#[tokio::test]
async fn snapshot_summary_explains_undefined_cpi() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    let out = f
        .planner
        .snapshot(SnapshotRequest::new(plan_id))
        .await
        .unwrap();
    assert!(out.summary.undefined.iter().any(|u| u.field == "cpi"));
}

// ── Fix round 2: backfill-correct alerts, per-baseline window ────────────

async fn snapshot_at(f: &Fixture, plan_id: &PlanId, hours: i64) -> Vec<String> {
    f.planner
        .snapshot(SnapshotRequest::new(plan_id.clone()).with_as_of(t0() + Duration::hours(hours)))
        .await
        .unwrap()
        .summary
        .alerts
}

#[tokio::test]
async fn backfilled_snapshot_alerts_use_only_earlier_readings() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    snapshot_at(&f, &plan_id, 10).await;
    snapshot_at(&f, &plan_id, 12).await;
    let backfill = snapshot_at(&f, &plan_id, 0).await;
    assert!(backfill.is_empty(), "{backfill:?}");
}

#[tokio::test]
async fn alert_window_is_not_crowded_out_by_previous_baseline_history() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    for hours in 50..150 {
        snapshot_at(&f, &plan_id, hours).await;
    }
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_reason("replan"))
        .await
        .unwrap();
    snapshot_at(&f, &plan_id, 10).await;
    let alerts = snapshot_at(&f, &plan_id, 12).await;
    assert!(alerts.contains(&"SPI_BELOW_0_9".to_string()), "{alerts:?}");
}

// ── Final fix wave: removed scope, backfill flag, calendar inheritance ───

/// Only `b`, with no prerequisites (removes `a`).
fn graph_without_a() -> PlanGraph {
    PlanGraph {
        deliverables: vec![deliverable("b", 10.0, &[])],
        max_chained_dispatch: None,
    }
}

/// Only `a` (removes `b`).
fn graph_without_b() -> PlanGraph {
    PlanGraph {
        deliverables: vec![deliverable("a", 10.0, &[])],
        max_chained_dispatch: None,
    }
}

impl Fixture {
    async fn revise(&self, plan_id: &PlanId, graph: PlanGraph) {
        self.planner
            .revise_plan(ReviseRequest::new(plan_id.clone(), graph).force(true))
            .await
            .unwrap();
    }

    async fn complete_a(&self, plan_id: &PlanId) {
        self.planner
            .acquire_cohort(AcquireRequest::new(plan_id.clone(), worker(), 1))
            .await
            .unwrap();
        self.planner
            .mark_status(MarkStatusRequest::new(
                plan_id.clone(),
                "a",
                worker(),
                DeliverableStatus::Complete,
            ))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn removed_complete_deliverable_keeps_its_earned_value() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.complete_a(&plan_id).await;
    f.revise(&plan_id, graph_without_a()).await;
    let r = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(r.ev, 10.0);
}

#[tokio::test]
async fn removed_deliverable_reports_removed_status() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.revise(&plan_id, graph_without_b()).await;
    let r = f.planner.ev(&plan_id, None).await.unwrap();
    let b = r.rows.iter().find(|row| row.id == "b").unwrap();
    assert_eq!(b.status, EvRowStatus::Removed);
}

#[tokio::test]
async fn actual_cost_survives_rebaseline_for_removed_deliverable() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.report_a(&plan_id, 4.0).await;
    f.revise(&plan_id, graph_without_a()).await;
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_reason("scope cut"))
        .await
        .unwrap();
    let r = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(r.ac, 4.0);
}

#[tokio::test]
async fn readded_deliverable_restarts_earned_progress() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.report_a(&plan_id, 4.0).await;
    f.revise(&plan_id, graph_without_a()).await;
    f.revise(&plan_id, graph()).await;
    let r = f.planner.ev(&plan_id, None).await.unwrap();
    let a = r.rows.iter().find(|row| row.id == "a").unwrap();
    assert_eq!(a.ev, 0.0);
}

#[tokio::test]
async fn snapshot_with_past_as_of_is_marked_backfilled() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.advance(5);
    let out = f
        .planner
        .snapshot(SnapshotRequest::new(plan_id).with_as_of(t0() + Duration::hours(2)))
        .await
        .unwrap();
    assert!(out.summary.backfilled);
}

#[tokio::test]
async fn alerts_ignore_backfilled_snapshots() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    f.report_a(&plan_id, 1.0).await;
    f.advance(20);
    // SPI 0.5 as of 10h, but taken at 20h: backfilled.
    snapshot_at(&f, &plan_id, 10).await;
    // SPI 0.25 as of now: the only reading alerts may use.
    let alerts = snapshot_at(&f, &plan_id, 20).await;
    assert!(alerts.is_empty(), "{alerts:?}");
}

#[tokio::test]
async fn rebaseline_without_calendar_inherits_previous_calendar() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    let calendar = Calendar {
        hours_per_day: 6.0,
        ..Calendar::default()
    };
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_calendar(calendar.clone()))
        .await
        .unwrap();
    let out = f
        .planner
        .baseline(BaselineRequest::new(plan_id).with_reason("replan"))
        .await
        .unwrap();
    assert_eq!(out.calendar, Some(CalendarSpec::Single(calendar)));
}

// ── Levelled baselines (#132, docs/calendars.md section 4) ──────────────

/// `a` and `b` (10h each, on `agent`) feed `c` (5h, on `agent`). CPM runs
/// `a` and `b` together (finish 15h); one agent levels them in sequence
/// (`a` 0-10, `b` 10-20, `c` 20-25).
fn pool_graph() -> PlanGraph {
    let on_agent = |id: &str, effort: f32, prereqs: &[&str]| Deliverable {
        metadata: serde_json::json!({"owner": "agent"}),
        ..deliverable(id, effort, prereqs)
    };
    PlanGraph {
        deliverables: vec![
            on_agent("a", 10.0, &[]),
            on_agent("b", 10.0, &[]),
            on_agent("c", 5.0, &["a", "b"]),
        ],
        max_chained_dispatch: None,
    }
}

fn one_agent() -> BTreeMap<String, u32> {
    BTreeMap::from([("agent".to_string(), 1)])
}

impl Fixture {
    async fn pool_plan(&self) -> PlanId {
        self.planner.submit_plan(pool_graph()).await.unwrap()
    }

    async fn levelled(&self) -> (PlanId, BaselineOutcome) {
        let plan_id = self.pool_plan().await;
        let out = self
            .planner
            .baseline(BaselineRequest::new(plan_id.clone()).with_capacities(one_agent()))
            .await
            .unwrap();
        (plan_id, out)
    }

    async fn rebaseline(&self, req: BaselineRequest) -> Result<BaselineOutcome, PlannerError> {
        self.planner.baseline(req.with_reason("replan")).await
    }

    async fn pv_at(&self, plan_id: &PlanId, hours: i64) -> f32 {
        self.planner
            .ev(plan_id, Some(t0() + Duration::hours(hours)))
            .await
            .unwrap()
            .pv
    }

    async fn baseline_err(&self, req: BaselineRequest) -> PlannerError {
        self.planner.baseline(req).await.unwrap_err()
    }
}

#[tokio::test]
async fn baseline_without_capacities_freezes_cpm() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    let out = f
        .planner
        .baseline(BaselineRequest::new(plan_id))
        .await
        .unwrap();
    assert_eq!(out.schedule, ScheduleKind::Cpm);
}

#[tokio::test]
async fn cpm_baseline_records_no_capacities() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    let out = f
        .planner
        .baseline(BaselineRequest::new(plan_id))
        .await
        .unwrap();
    assert_eq!(out.capacities, None);
}

#[tokio::test]
async fn baseline_with_capacities_freezes_the_levelled_schedule() {
    let f = Fixture::new();
    let (_, out) = f.levelled().await;
    assert_eq!(out.schedule, ScheduleKind::Levelled);
}

#[tokio::test]
async fn levelled_baseline_records_its_capacities() {
    let f = Fixture::new();
    let (_, out) = f.levelled().await;
    assert_eq!(out.capacities, Some(one_agent()));
}

#[tokio::test]
async fn levelled_baseline_records_the_default_resource_key() {
    let f = Fixture::new();
    let (_, out) = f.levelled().await;
    assert_eq!(out.resource_key.as_deref(), Some("owner"));
}

#[tokio::test]
async fn levelled_baseline_finish_is_the_plan_schedule_makespan() {
    let f = Fixture::new();
    let (_, out) = f.levelled().await;
    let schedule = resource_schedule(
        &pool_graph(),
        &ScheduleRequest {
            capacities: one_agent(),
            resource_key: "owner".into(),
            project_buffer_pct: 25.0,
        },
    )
    .unwrap();
    assert_eq!(out.finish_hours, schedule.makespan);
}

#[tokio::test]
async fn levelled_baseline_records_the_project_buffer() {
    let f = Fixture::new();
    let (_, out) = f.levelled().await;
    // 25% of the 25h driving chain a -> b -> c.
    assert_eq!(out.project_buffer_hours, Some(6.25));
}

#[tokio::test]
async fn cpm_pv_front_loads_parallel_work() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()))
        .await
        .unwrap();
    assert_eq!(f.pv_at(&plan_id, 10).await, 20.0);
}

#[tokio::test]
async fn levelled_pv_follows_the_levelled_schedule() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    assert_eq!(f.pv_at(&plan_id, 10).await, 10.0);
}

#[tokio::test]
async fn levelled_pv_reaches_bac_at_the_levelled_finish() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    assert_eq!(f.pv_at(&plan_id, 25).await, 25.0);
}

#[tokio::test]
async fn ev_reports_the_levelled_schedule_kind() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    let report = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(report.schedule, ScheduleKind::Levelled);
}

#[tokio::test]
async fn ev_reports_the_levelled_capacities() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    let report = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(report.capacities, Some(one_agent()));
}

#[tokio::test]
async fn status_reports_the_baseline_schedule_kind() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    let status = f.planner.status(&plan_id).await.unwrap();
    assert_eq!(
        status.baseline.map(|b| b.schedule),
        Some(ScheduleKind::Levelled)
    );
}

#[tokio::test]
async fn levelled_baseline_audit_records_the_schedule_kind() {
    let f = Fixture::new();
    f.levelled().await;
    let event = f.audit.snapshot().pop().unwrap();
    assert_eq!(event.payload["schedule"], serde_json::json!("levelled"));
}

#[tokio::test]
async fn rebaseline_without_capacities_reuses_the_previous_capacities() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    let out = f.rebaseline(BaselineRequest::new(plan_id)).await.unwrap();
    assert_eq!(out.capacities, Some(one_agent()));
}

#[tokio::test]
async fn rebaseline_without_capacities_stays_levelled() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    let out = f.rebaseline(BaselineRequest::new(plan_id)).await.unwrap();
    assert_eq!(out.schedule, ScheduleKind::Levelled);
}

#[tokio::test]
async fn rebaseline_reuses_the_previous_project_buffer_pct() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    f.planner
        .baseline(
            BaselineRequest::new(plan_id.clone())
                .with_capacities(one_agent())
                .with_project_buffer_pct(10.0),
        )
        .await
        .unwrap();
    let out = f.rebaseline(BaselineRequest::new(plan_id)).await.unwrap();
    assert_eq!(out.project_buffer_pct, Some(10.0));
}

#[tokio::test]
async fn rebaseline_with_new_capacities_replaces_the_previous_ones() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    let two = BTreeMap::from([("agent".to_string(), 2)]);
    let out = f
        .rebaseline(BaselineRequest::new(plan_id).with_capacities(two))
        .await
        .unwrap();
    // Two agents run a and b together: finish 15h, as CPM.
    assert_eq!(out.finish_hours, 15.0);
}

#[tokio::test]
async fn rebaseline_with_schedule_cpm_returns_to_cpm() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    let out = f
        .rebaseline(BaselineRequest::new(plan_id).with_schedule(ScheduleKind::Cpm))
        .await
        .unwrap();
    assert_eq!(out.schedule, ScheduleKind::Cpm);
}

#[tokio::test]
async fn rebaseline_after_cpm_reset_stays_cpm() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    f.rebaseline(BaselineRequest::new(plan_id.clone()).with_schedule(ScheduleKind::Cpm))
        .await
        .unwrap();
    let out = f.rebaseline(BaselineRequest::new(plan_id)).await.unwrap();
    assert_eq!(out.schedule, ScheduleKind::Cpm);
}

#[tokio::test]
async fn capacities_with_schedule_cpm_are_invalid_value() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    let err = f
        .baseline_err(
            BaselineRequest::new(plan_id)
                .with_capacities(one_agent())
                .with_schedule(ScheduleKind::Cpm),
        )
        .await;
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err}");
}

#[tokio::test]
async fn schedule_levelled_without_capacities_is_invalid_value() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    let err = f
        .baseline_err(BaselineRequest::new(plan_id).with_schedule(ScheduleKind::Levelled))
        .await;
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err}");
}

#[tokio::test]
async fn schedule_levelled_with_inherited_capacities_is_accepted() {
    let f = Fixture::new();
    let (plan_id, _) = f.levelled().await;
    let out = f
        .rebaseline(BaselineRequest::new(plan_id).with_schedule(ScheduleKind::Levelled))
        .await
        .unwrap();
    assert_eq!(out.capacities, Some(one_agent()));
}

#[tokio::test]
async fn resource_key_without_capacities_is_invalid_value() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    let err = f
        .baseline_err(BaselineRequest::new(plan_id).with_resource_key("team"))
        .await;
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err}");
}

#[tokio::test]
async fn project_buffer_pct_with_schedule_cpm_is_invalid_value() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    let err = f
        .baseline_err(
            BaselineRequest::new(plan_id)
                .with_project_buffer_pct(10.0)
                .with_schedule(ScheduleKind::Cpm),
        )
        .await;
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err}");
}

#[tokio::test]
async fn empty_resource_key_is_invalid_value() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    let err = f
        .baseline_err(
            BaselineRequest::new(plan_id)
                .with_capacities(one_agent())
                .with_resource_key(""),
        )
        .await;
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err}");
}

#[tokio::test]
async fn project_buffer_pct_out_of_range_is_invalid_value() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    let err = f
        .baseline_err(
            BaselineRequest::new(plan_id)
                .with_capacities(one_agent())
                .with_project_buffer_pct(101.0),
        )
        .await;
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err}");
}

#[tokio::test]
async fn missing_capacity_matches_the_plan_schedule_refusal() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    let none = BTreeMap::from([("human".to_string(), 1)]);
    let err = f
        .baseline_err(BaselineRequest::new(plan_id).with_capacities(none.clone()))
        .await;
    let expected = resource_schedule(
        &pool_graph(),
        &ScheduleRequest {
            capacities: none,
            resource_key: "owner".into(),
            project_buffer_pct: 25.0,
        },
    )
    .unwrap_err();
    assert_eq!(err.to_string(), expected.to_string());
}

#[tokio::test]
async fn refused_levelled_baseline_stores_nothing() {
    let f = Fixture::new();
    let plan_id = f.pool_plan().await;
    let none = BTreeMap::from([("human".to_string(), 1)]);
    f.baseline_err(BaselineRequest::new(plan_id.clone()).with_capacities(none))
        .await;
    let err = f.planner.ev(&plan_id, None).await.unwrap_err();
    assert!(matches!(err, PlannerError::NotBaselined { .. }), "{err}");
}

// ── calendar: null and per-resource maps ────────────────────────────────

#[tokio::test]
async fn rebaseline_with_null_calendar_returns_to_wall_clock_hours() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_calendar(Calendar::default()))
        .await
        .unwrap();
    let out = f
        .rebaseline(BaselineRequest::new(plan_id).with_calendar_input(BaselineCalendar::WallClock))
        .await
        .unwrap();
    assert_eq!(out.calendar, None);
}

#[tokio::test]
async fn calendar_input_overrides_the_calendar_shorthand() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    let out = f
        .planner
        .baseline(
            BaselineRequest::new(plan_id)
                .with_calendar(Calendar::default())
                .with_calendar_input(BaselineCalendar::WallClock),
        )
        .await
        .unwrap();
    assert_eq!(out.calendar, None);
}

#[tokio::test]
async fn invalid_calendar_map_entry_is_invalid_value() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    let map = BaselineCalendar::from_json(Some(serde_json::json!({
        "default": {"hours_per_day": 24},
        "owner": {"hours_per_day": 25}
    })))
    .unwrap();
    let err = f
        .baseline_err(BaselineRequest::new(plan_id).with_calendar_input(map))
        .await;
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err}");
}

#[tokio::test]
async fn single_calendar_with_out_of_range_value_keeps_invalid_graph() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    let calendar = Calendar {
        hours_per_day: 25.0,
        ..Calendar::default()
    };
    let err = f
        .baseline_err(
            BaselineRequest::new(plan_id).with_calendar_input(BaselineCalendar::Single(calendar)),
        )
        .await;
    assert!(matches!(err, PlannerError::InvalidGraph { .. }), "{err}");
}

// ── Per-resource calendars on the resource clock (#131) ─────────────────

#[tokio::test]
async fn single_calendar_with_malformed_day_start_is_invalid_value() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    let calendar = Calendar {
        day_start: "9:00".into(),
        ..Calendar::default()
    };
    let err = f
        .baseline_err(BaselineRequest::new(plan_id).with_calendar(calendar))
        .await;
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err}");
}

#[tokio::test]
async fn single_calendar_day_start_delays_plan_clock_pv() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    // Monday 00:00 UTC start, window 09:00-17:00: nothing is planned
    // before 09:00, one hour by 10:00.
    let calendar = Calendar {
        day_start: "09:00".into(),
        ..Calendar::default()
    };
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_calendar(calendar))
        .await
        .unwrap();
    assert_eq!(f.pv_at(&plan_id, 10).await, 1.0);
}

/// Working calendar used for the equivalence property: 09:00-17:00 at
/// UTC-5, Monday to Friday.
fn office() -> Calendar {
    Calendar {
        hours_per_day: 8.0,
        day_start: "09:00".into(),
        utc_offset_minutes: -300,
        ..Calendar::default()
    }
}

async fn pv_curve(f: &Fixture, plan_id: &PlanId) -> Vec<f32> {
    let mut out = Vec::new();
    for h in (0..=24 * 14).step_by(3) {
        out.push(f.pv_at(plan_id, h).await);
    }
    out
}

#[tokio::test]
async fn uniform_map_pv_equals_single_calendar_pv() {
    let f = Fixture::new();
    let plan_id = f.plan().await;
    f.planner
        .baseline(
            BaselineRequest::new(plan_id.clone())
                .with_start(t0())
                .with_calendar(office()),
        )
        .await
        .unwrap();
    let single = pv_curve(&f, &plan_id).await;
    let map =
        BaselineCalendar::from_json(Some(serde_json::json!({ "default": office() }))).unwrap();
    f.rebaseline(
        BaselineRequest::new(plan_id.clone())
            .with_start(t0())
            .with_calendar_input(map),
    )
    .await
    .unwrap();
    let uniform = pv_curve(&f, &plan_id).await;
    let worst = single
        .iter()
        .zip(&uniform)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f32, f32::max);
    assert!(worst < 1e-3, "single {single:?} uniform {uniform:?}");
}

// The worked example of docs/calendars.md, section 8: agents 24/7, the
// owner Monday to Friday 09:00-17:00 at UTC-5, start Friday 14:00 local.

fn example_start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 13, 19, 0, 0).unwrap()
}

fn example_calendar() -> serde_json::Value {
    serde_json::json!({
        "default": {"hours_per_day": 24, "workdays": ["mon","tue","wed","thu","fri","sat","sun"]},
        "owner": {"hours_per_day": 8, "day_start": "09:00",
                  "workdays": ["mon","tue","wed","thu","fri"], "utc_offset_minutes": -300}
    })
}

fn example_graph() -> PlanGraph {
    let on = |owner: &str, id: &str, effort: f32, prereqs: &[&str]| Deliverable {
        metadata: serde_json::json!({ "owner": owner }),
        ..deliverable(id, effort, prereqs)
    };
    PlanGraph {
        deliverables: vec![
            on("agent", "impl", 10.0, &[]),
            on("owner", "review", 2.0, &["impl"]),
            on("agent", "release", 1.0, &["review"]),
        ],
        max_chained_dispatch: None,
    }
}

fn example_capacities() -> BTreeMap<String, u32> {
    BTreeMap::from([("agent".to_string(), 3), ("owner".to_string(), 1)])
}

impl Fixture {
    /// The example plan, baselined on the calendar map; levelled when
    /// `levelled`.
    async fn example(&self, levelled: bool) -> (PlanId, BaselineOutcome) {
        let plan_id = self.planner.submit_plan(example_graph()).await.unwrap();
        let mut req = BaselineRequest::new(plan_id.clone())
            .with_start(example_start())
            .with_calendar_input(BaselineCalendar::from_json(Some(example_calendar())).unwrap());
        if levelled {
            req = req.with_capacities(example_capacities());
        }
        let out = self.planner.baseline(req).await.unwrap();
        (plan_id, out)
    }

    /// Plan-start-relative EV report of the example.
    async fn example_ev(
        &self,
        plan_id: &PlanId,
        hours: i64,
    ) -> cpm_planner::earned_value::EvReport {
        self.planner
            .ev(plan_id, Some(example_start() + Duration::hours(hours)))
            .await
            .unwrap()
    }

    async fn complete(&self, plan_id: &PlanId, id: &str) {
        self.planner
            .acquire_cohort(
                AcquireRequest::new(plan_id.clone(), worker(), 1).with_ids(vec![id.to_string()]),
            )
            .await
            .unwrap();
        self.planner
            .mark_status(MarkStatusRequest::new(
                plan_id.clone(),
                id,
                worker(),
                DeliverableStatus::Complete,
            ))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn example_baseline_finishes_monday_noon_local() {
    let f = Fixture::new();
    let (_, out) = f.example(true).await;
    assert_eq!(out.finish_hours, 70.0);
}

#[tokio::test]
async fn example_baseline_records_the_calendar_map() {
    let f = Fixture::new();
    let (_, out) = f.example(true).await;
    assert!(matches!(out.calendar, Some(CalendarSpec::Map(_))));
}

#[tokio::test]
async fn example_cpm_baseline_also_waits_for_monday() {
    let f = Fixture::new();
    let (_, out) = f.example(false).await;
    assert_eq!(out.finish_hours, 70.0);
}

#[tokio::test]
async fn example_cpm_baseline_records_the_resource_key() {
    let f = Fixture::new();
    let (_, out) = f.example(false).await;
    assert_eq!(out.resource_key.as_deref(), Some("owner"));
}

#[tokio::test]
async fn example_pv_is_flat_over_the_weekend() {
    let f = Fixture::new();
    let (plan_id, _) = f.example(true).await;
    // Saturday 12:00 local.
    assert_eq!(f.example_ev(&plan_id, 22).await.pv, 10.0);
}

#[tokio::test]
async fn example_pv_accrues_agent_work_around_the_clock() {
    let f = Fixture::new();
    let (plan_id, _) = f.example(true).await;
    // Friday 19:00 local, after office hours.
    assert_eq!(f.example_ev(&plan_id, 5).await.pv, 5.0);
}

#[tokio::test]
async fn example_pv_accrues_review_inside_the_owner_window() {
    let f = Fixture::new();
    let (plan_id, _) = f.example(true).await;
    // Monday 10:00 local: one of review's two hours.
    assert_eq!(f.example_ev(&plan_id, 68).await.pv, 11.0);
}

#[tokio::test]
async fn example_spi_is_one_when_execution_matches_the_calendar() {
    let f = Fixture::new();
    let (plan_id, _) = f.example(true).await;
    f.complete(&plan_id, "impl").await;
    assert_eq!(f.example_ev(&plan_id, 22).await.spi, Some(1.0));
}

#[tokio::test]
async fn example_review_row_reports_its_planned_calendar_wait() {
    let f = Fixture::new();
    let (plan_id, _) = f.example(true).await;
    let report = f.example_ev(&plan_id, 22).await;
    let review = report.rows.iter().find(|r| r.id == "review").unwrap();
    assert_eq!(review.calendar_wait_hours, 57.0);
}

#[tokio::test]
async fn example_total_calendar_wait_is_the_driving_chain_sum() {
    let f = Fixture::new();
    let (plan_id, _) = f.example(true).await;
    let schedule = resource_schedule_on_calendar(
        &example_graph(),
        &ScheduleRequest {
            capacities: example_capacities(),
            resource_key: "owner".into(),
            project_buffer_pct: 25.0,
        },
        Some(&ScheduleCalendar::new(
            CalendarSpec::parse(&example_calendar()).unwrap(),
            example_start(),
        )),
    )
    .unwrap();
    let report = f.example_ev(&plan_id, 22).await;
    assert_eq!(
        Some(report.calendar_wait_hours),
        schedule.calendar_wait_hours
    );
}

#[tokio::test]
async fn example_pv_reaches_bac_at_the_calendar_finish() {
    let f = Fixture::new();
    let (plan_id, _) = f.example(true).await;
    assert_eq!(f.example_ev(&plan_id, 70).await.pv, 13.0);
}

#[tokio::test]
async fn rebaseline_without_calendar_reuses_the_calendar_map() {
    let f = Fixture::new();
    let (plan_id, _) = f.example(true).await;
    let out = f.rebaseline(BaselineRequest::new(plan_id)).await.unwrap();
    assert!(matches!(out.calendar, Some(CalendarSpec::Map(_))));
}

#[tokio::test]
async fn resource_key_with_a_calendar_map_and_no_capacities_is_accepted() {
    let f = Fixture::new();
    let plan_id = f.planner.submit_plan(example_graph()).await.unwrap();
    let out = f
        .planner
        .baseline(
            BaselineRequest::new(plan_id)
                .with_start(example_start())
                .with_resource_key("owner")
                .with_calendar_input(
                    BaselineCalendar::from_json(Some(example_calendar())).unwrap(),
                ),
        )
        .await
        .unwrap();
    assert_eq!(out.schedule, ScheduleKind::Cpm);
}

#[tokio::test]
async fn plan_clock_baseline_reports_no_calendar_wait() {
    let f = Fixture::new();
    let plan_id = f.baselined().await;
    let report = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(report.calendar_wait_hours, 0.0);
}
