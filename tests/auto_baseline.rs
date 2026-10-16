//! Integration tests for automatic earned-value baselines (issue #71):
//! every execution-starting call must leave the plan with a baseline, either
//! by taking one itself (`BaselineMode::Auto`, the default) or by refusing
//! with `BASELINE_REQUIRED` (`BaselineMode::Refuse`).
//!
//! Every test drives a fake clock so the automatically taken baseline's
//! `as_of` is deterministic.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::earned_value::{BaselineMode, BaselineRequest, BaselineTaken};
use cpm_planner::plan::{
    AcquireRequest, CallerId, Deliverable, DeliverableStatus, MarkStatusRequest, PlanGraph, PlanId,
    PlannerError, ReviseRequest, SyncRequest,
};
use cpm_planner::ports::Planner;
use cpm_planner::server::TOOL_LINT;
use cpm_planner::{BasicCpmPlanner, PlanServer};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Value, json};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap()
}

struct Fixture {
    planner: Arc<BasicCpmPlanner>,
    audit: Arc<MemoryAuditSink>,
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl Fixture {
    fn new(mode: BaselineMode) -> Self {
        let now = Arc::new(Mutex::new(t0()));
        let clock = now.clone();
        let audit = Arc::new(MemoryAuditSink::new());
        let planner = Arc::new(
            BasicCpmPlanner::with_parts(
                audit.clone(),
                std::time::Duration::from_secs(3600),
                Arc::new(move || *clock.lock().unwrap()),
            )
            .with_baseline_mode(mode),
        );
        Self {
            planner,
            audit,
            now,
        }
    }

    fn set_now(&self, at: DateTime<Utc>) {
        *self.now.lock().unwrap() = at;
    }

    fn server(&self) -> PlanServer {
        PlanServer::new(self.planner.clone())
    }

    fn auto() -> Self {
        Self::new(BaselineMode::Auto)
    }

    fn refuse() -> Self {
        Self::new(BaselineMode::Refuse)
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

    async fn acquire_a(&self, plan_id: &PlanId) -> Result<cpm_planner::plan::Cohort, PlannerError> {
        self.planner
            .acquire_cohort(AcquireRequest::new(plan_id.clone(), worker(), 1))
            .await
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

// ── auto mode: acquire_cohort ───────────────────────────────────────────

#[tokio::test]
async fn auto_acquire_cohort_returns_the_first_execution_baseline() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    let cohort = f.acquire_a(&plan_id).await.unwrap();
    assert_eq!(
        cohort.baseline_taken,
        Some(BaselineTaken {
            baseline_number: 1,
            as_of: t0(),
            reason: "first_execution".to_string(),
        })
    );
}

#[tokio::test]
async fn auto_acquire_cohort_enables_the_ev_report() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    f.acquire_a(&plan_id).await.unwrap();
    let report = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(report.baseline_number, 1);
}

#[tokio::test]
async fn auto_acquire_cohort_emits_the_baselined_audit_event() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    f.acquire_a(&plan_id).await.unwrap();
    assert!(
        f.audit
            .event_types()
            .contains(&"plan.ev.baselined".to_string())
    );
}

#[tokio::test]
async fn auto_acquire_cohort_on_a_baselined_plan_returns_no_baseline_taken() {
    let f = Fixture::auto();
    let plan_id = f.baselined().await;
    let cohort = f.acquire_a(&plan_id).await.unwrap();
    assert!(cohort.baseline_taken.is_none());
}

#[tokio::test]
async fn auto_acquire_cohort_on_a_baselined_plan_keeps_the_baseline_number() {
    let f = Fixture::auto();
    let plan_id = f.baselined().await;
    f.acquire_a(&plan_id).await.unwrap();
    let report = f.planner.ev(&plan_id, None).await.unwrap();
    assert_eq!(report.baseline_number, 1);
}

// ── auto mode: mark_status ──────────────────────────────────────────────

#[tokio::test]
async fn auto_lockless_complete_mark_creates_a_baseline() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    f.planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            worker(),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    let status = f.planner.status(&plan_id).await.unwrap();
    assert!(status.baseline.is_some());
}

#[tokio::test]
async fn auto_lockless_complete_mark_returns_the_taken_baseline() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    let taken = f
        .planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            worker(),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    assert_eq!(taken.map(|b| b.baseline_number), Some(1));
}

#[tokio::test]
async fn auto_lockless_in_progress_mark_creates_a_baseline() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    f.planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            worker(),
            DeliverableStatus::InProgress,
        ))
        .await
        .unwrap();
    let status = f.planner.status(&plan_id).await.unwrap();
    assert!(status.baseline.is_some());
}

// ── refuse mode ─────────────────────────────────────────────────────────

#[tokio::test]
async fn refuse_acquire_cohort_without_baseline_is_baseline_required() {
    let f = Fixture::refuse();
    let plan_id = f.plan().await;
    let err = f.acquire_a(&plan_id).await.expect_err("expected refusal");
    assert!(matches!(err, PlannerError::BaselineRequired { .. }));
}

#[tokio::test]
async fn refuse_acquire_cohort_without_baseline_leaves_no_lease() {
    let f = Fixture::refuse();
    let plan_id = f.plan().await;
    let _ = f.acquire_a(&plan_id).await;
    let status = f.planner.status(&plan_id).await.unwrap();
    assert!(status.locks_held.is_empty());
}

#[tokio::test]
async fn refuse_complete_mark_without_baseline_is_baseline_required() {
    let f = Fixture::refuse();
    let plan_id = f.plan().await;
    let err = f
        .planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            worker(),
            DeliverableStatus::Complete,
        ))
        .await
        .expect_err("expected refusal");
    assert!(matches!(err, PlannerError::BaselineRequired { .. }));
}

// ── plan.status ─────────────────────────────────────────────────────────

#[tokio::test]
async fn status_without_a_baseline_reports_null() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    let status = f.planner.status(&plan_id).await.unwrap();
    assert!(status.baseline.is_none());
}

#[tokio::test]
async fn status_reports_the_baseline_number_after_it_is_taken() {
    let f = Fixture::auto();
    let plan_id = f.baselined().await;
    let status = f.planner.status(&plan_id).await.unwrap();
    assert_eq!(status.baseline.map(|b| b.baseline_number), Some(1));
}

// ── auto mode: plan.select ──────────────────────────────────────────────

#[tokio::test]
async fn select_of_a_progressed_variant_creates_a_baseline() {
    let f = Fixture::auto();
    let (main, alt) = main_and_draft(&f).await;
    f.planner
        .acquire_cohort(AcquireRequest::new(main.clone(), worker(), 1))
        .await
        .unwrap();
    f.planner
        .mark_status(MarkStatusRequest::new(
            main.clone(),
            "a",
            worker(),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    f.planner.select_variant(&alt, false).await.unwrap();
    let status = f.planner.status(&alt).await.unwrap();
    assert!(status.baseline.is_some());
}

#[tokio::test]
async fn select_of_a_progressed_variant_reports_the_taken_baseline() {
    let f = Fixture::auto();
    let (main, alt) = main_and_draft(&f).await;
    f.planner
        .acquire_cohort(AcquireRequest::new(main.clone(), worker(), 1))
        .await
        .unwrap();
    f.planner
        .mark_status(MarkStatusRequest::new(
            main.clone(),
            "a",
            worker(),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    let outcome = f.planner.select_variant(&alt, false).await.unwrap();
    assert_eq!(outcome.baseline_taken.map(|b| b.baseline_number), Some(1));
}

#[tokio::test]
async fn select_back_keeps_an_existing_baseline() {
    let f = Fixture::auto();
    let (main, alt) = main_and_draft(&f).await;
    f.planner
        .acquire_cohort(AcquireRequest::new(main.clone(), worker(), 1))
        .await
        .unwrap();
    f.planner
        .mark_status(MarkStatusRequest::new(
            main.clone(),
            "a",
            worker(),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    f.planner.select_variant(&alt, false).await.unwrap();
    f.planner.select_variant(&main, false).await.unwrap();
    let status = f.planner.status(&main).await.unwrap();
    assert_eq!(status.baseline.map(|b| b.baseline_number), Some(1));
}

#[tokio::test]
async fn revise_keeps_an_existing_baseline() {
    let f = Fixture::auto();
    let plan_id = f.baselined().await;
    let mut revised = graph();
    revised.deliverables.push(deliverable("c", 3.0, &["b"]));
    f.planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), revised))
        .await
        .unwrap();
    let status = f.planner.status(&plan_id).await.unwrap();
    assert_eq!(status.baseline.map(|b| b.baseline_number), Some(1));
}

#[tokio::test]
async fn refuse_select_of_a_progressed_variant_without_baseline_is_baseline_required() {
    let f = Fixture::refuse();
    let (main, alt) = main_and_draft(&f).await;
    f.planner
        .baseline(BaselineRequest::new(main.clone()))
        .await
        .unwrap();
    f.planner
        .acquire_cohort(AcquireRequest::new(main.clone(), worker(), 1))
        .await
        .unwrap();
    f.planner
        .mark_status(MarkStatusRequest::new(
            main.clone(),
            "a",
            worker(),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    let err = f
        .planner
        .select_variant(&alt, false)
        .await
        .expect_err("expected refusal");
    assert!(matches!(err, PlannerError::BaselineRequired { .. }));
}

// ── plan.lint: NO_BASELINE ───────────────────────────────────────────────

async fn lint(f: &Fixture, args: Value) -> Value {
    let map: JsonObject = match args {
        Value::Object(m) => m,
        _ => panic!("lint args must be an object"),
    };
    f.server()
        .dispatch_call(CallToolRequestParams::new(TOOL_LINT).with_arguments(map))
        .await
        .expect("plan.lint returns Ok")
}

fn has_finding(report: &Value, code: &str) -> bool {
    report["findings"]
        .as_array()
        .is_some_and(|findings| findings.iter().any(|f| f["code"] == code))
}

#[tokio::test]
async fn lint_by_plan_id_reports_no_baseline_for_a_progressed_unbaselined_plan() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    f.planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            worker(),
            DeliverableStatus::Failed {
                reason: "boom".into(),
            },
        ))
        .await
        .unwrap();
    let report = lint(&f, json!({ "plan_id": plan_id.0 })).await;
    assert!(has_finding(&report, "NO_BASELINE"));
}

#[tokio::test]
async fn lint_by_plan_id_reports_no_no_baseline_for_a_baselined_plan() {
    let f = Fixture::auto();
    let plan_id = f.baselined().await;
    let report = lint(&f, json!({ "plan_id": plan_id.0 })).await;
    assert!(!has_finding(&report, "NO_BASELINE"));
}

#[tokio::test]
async fn lint_by_graph_never_reports_no_baseline() {
    let f = Fixture::auto();
    let report = lint(&f, json!({ "graph": graph() })).await;
    assert!(!has_finding(&report, "NO_BASELINE"));
}

// ── plan.baseline: backfill_from ─────────────────────────────────────────

#[tokio::test]
async fn baseline_backfill_from_sets_the_baseline_start() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    // `plans.created_at_us` is the store's real `Utc::now`, so the valid
    // backfill instant must be after submission; the fake clock then moves
    // past it so it is not "in the future".
    let start = Utc::now() + Duration::seconds(1);
    f.set_now(start + Duration::days(1));
    let outcome = f
        .planner
        .baseline(BaselineRequest::new(plan_id).with_backfill_from(start))
        .await
        .unwrap();
    assert_eq!(outcome.start, start);
}

#[tokio::test]
async fn plan_ev_reports_a_backfilled_baseline_as_backfilled() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    let start = Utc::now() + Duration::seconds(1);
    f.set_now(start + Duration::days(1));
    f.planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_backfill_from(start))
        .await
        .unwrap();
    let report = f.planner.ev(&plan_id, None).await.unwrap();
    assert!(report.backfilled);
}

#[tokio::test]
async fn baseline_backfill_from_in_the_future_is_rejected() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    let err = f
        .planner
        .baseline(BaselineRequest::new(plan_id).with_backfill_from(t0() + Duration::hours(1)))
        .await
        .expect_err("future backfill must be refused");
    assert!(matches!(err, PlannerError::InvalidValue { .. }));
}

#[tokio::test]
async fn baseline_backfill_from_before_the_plan_creation_is_rejected() {
    let f = Fixture::auto();
    let plan_id = f.plan().await;
    f.set_now(t0() + Duration::days(1));
    let err = f
        .planner
        .baseline(BaselineRequest::new(plan_id).with_backfill_from(t0() - Duration::hours(1)))
        .await
        .expect_err("backfill before creation must be refused");
    assert!(matches!(err, PlannerError::InvalidValue { .. }));
}
