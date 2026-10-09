//! Planner-level tests for the earned-value operations: `baseline`,
//! `earned_value` and `snapshot` (P5 Task 3). Every test drives a fake
//! clock so baseline starts, `as_of` defaults and snapshot instants are
//! deterministic.

#![allow(clippy::float_cmp)]

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use cpm_planner::BasicCpmPlanner;
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::earned_value::{BaselineRequest, SnapshotFormat, SnapshotRequest};
use cpm_planner::plan::{
    AcquireRequest, CallerId, Deliverable, DeliverableStatus, MarkStatusRequest, PlanGraph, PlanId,
    PlannerError, SyncRequest,
};
use cpm_planner::ports::Planner;

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
