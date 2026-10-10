//! Every path that ends a lease adds `(end - acquired_at)` hours to
//! `ev_actuals.leased_hours` in the same transaction. Driven through the
//! public planner with a fake clock; read back with
//! [`crate::ev_store::load_actuals`].

#![allow(clippy::float_cmp)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};

use crate::BasicCpmPlanner;
use crate::audit::NullAuditSink;
use crate::plan::{
    AcceptRequest, AcquireRequest, CallerId, Deliverable, DeliverableStatus, ForceReleaseRequest,
    MarkStatusRequest, PlanGraph, PlanId, ReviseRequest, SyncRequest,
};
use crate::ports::Planner;

const EPS: f32 = 1e-4;

#[derive(Clone)]
struct Clock(Arc<Mutex<DateTime<Utc>>>);

impl Clock {
    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 5, 9, 0, 0).unwrap()
    }

    fn advance_minutes(&self, minutes: i64) {
        let mut now = self.0.lock().unwrap();
        *now += chrono::Duration::minutes(minutes);
    }
}

/// A planner on a fake clock (starting at [`Clock::start`]) with a one-hour
/// default lease TTL.
fn planner() -> (BasicCpmPlanner, Clock) {
    let clock = Clock(Arc::new(Mutex::new(Clock::start())));
    let reader = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        Arc::new(NullAuditSink),
        Duration::from_secs(3600),
        Arc::new(move || *reader.0.lock().unwrap()),
    );
    (planner, clock)
}

fn deliverable(id: &str) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: vec![format!("src/{id}.rs").as_str().into()],
        prerequisites: vec![],
        estimated_effort_hours: Some(1.0),
        duration_hours: None,
        estimate: None,
        metadata: serde_json::Value::Null,
        milestone: false,
        earning_rule: None,
    }
}

fn graph(ids: &[&str]) -> PlanGraph {
    PlanGraph {
        deliverables: ids.iter().map(|id| deliverable(id)).collect(),
        max_chained_dispatch: None,
    }
}

fn worker() -> CallerId {
    CallerId("w1".into())
}

async fn acquire(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str) {
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), worker(), 1).with_ids(vec![id.into()]))
        .await
        .expect("harness: acquire");
    assert_eq!(cohort.rows.len(), 1, "harness: {id} leased");
}

async fn mark(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str, status: DeliverableStatus) {
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            id,
            worker(),
            status,
        ))
        .await
        .expect("harness: mark_status");
}

fn failed() -> DeliverableStatus {
    DeliverableStatus::Failed {
        reason: "broke".into(),
    }
}

/// Stored `leased_hours` of `id` (0 when it has no actuals row).
fn leased(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str) -> f32 {
    planner
        .store()
        .read_tx(|tx| crate::ev_store::load_actuals(tx, plan_id))
        .unwrap()
        .get(id)
        .map_or(0.0, |a| a.leased_hours)
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < EPS
}

/// Named line `web`: `main` (selected) and draft `alt`, both holding `a`.
async fn main_and_alt(planner: &BasicCpmPlanner) -> (PlanId, PlanId) {
    let sync = |variant: &str, ids: &[&str]| SyncRequest::new("proj", "web", variant, graph(ids));
    let main = planner.sync_plan(sync("main", &["a"])).await.unwrap();
    let alt = planner.sync_plan(sync("alt", &["a", "b"])).await.unwrap();
    (main.plan_id, alt.plan_id)
}

#[tokio::test]
async fn completing_a_lease_records_its_hours() {
    let (planner, clock) = planner();
    let plan = planner.submit_plan(graph(&["a"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(30);
    mark(&planner, &plan, "a", DeliverableStatus::Complete).await;
    assert!(near(leased(&planner, &plan, "a"), 0.5));
}

#[tokio::test]
async fn failing_a_lease_records_its_hours() {
    let (planner, clock) = planner();
    let plan = planner.submit_plan(graph(&["a"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(45);
    mark(&planner, &plan, "a", failed()).await;
    assert!(near(leased(&planner, &plan, "a"), 0.75));
}

#[tokio::test]
async fn leased_hours_accumulate_across_two_leases() {
    let (planner, clock) = planner();
    let plan = planner.submit_plan(graph(&["a"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(15);
    mark(&planner, &plan, "a", failed()).await;
    clock.advance_minutes(60);
    mark(&planner, &plan, "a", DeliverableStatus::Ready).await;
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(30);
    mark(&planner, &plan, "a", DeliverableStatus::Complete).await;
    assert!(near(leased(&planner, &plan, "a"), 0.75));
}

#[tokio::test]
async fn a_held_lease_records_nothing_yet() {
    let (planner, clock) = planner();
    let plan = planner.submit_plan(graph(&["a"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(30);
    planner.status(&plan).await.unwrap();
    assert_eq!(leased(&planner, &plan, "a"), 0.0);
}

#[tokio::test]
async fn force_release_records_the_lease_hours() {
    let (planner, clock) = planner();
    let plan = planner.submit_plan(graph(&["a"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(12);
    planner
        .force_release(ForceReleaseRequest::new(plan.clone(), "a", "stuck"))
        .await
        .unwrap();
    assert!(near(leased(&planner, &plan, "a"), 0.2));
}

#[tokio::test]
async fn reaped_lease_records_its_hours_up_to_expiry() {
    let (planner, clock) = planner();
    let plan = planner.submit_plan(graph(&["a"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(180);
    // The next acquire reaps the lapsed one-hour lease (and re-leases a).
    acquire(&planner, &plan, "a").await;
    assert!(near(leased(&planner, &plan, "a"), 1.0));
}

#[tokio::test]
async fn store_quarantine_records_expired_lease_hours_up_to_expiry() {
    let (planner, _clock) = planner();
    let plan = planner.submit_plan(graph(&["a"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    planner
        .store()
        .quarantine_expired(Clock::start() + chrono::Duration::hours(5))
        .unwrap();
    assert!(near(leased(&planner, &plan, "a"), 1.0));
}

#[tokio::test]
async fn accept_override_records_the_overridden_lease_hours() {
    let (planner, clock) = planner();
    let plan = planner.submit_plan(graph(&["a"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(20);
    planner
        .accept(AcceptRequest::new(plan.clone(), "a", "lead", "reviewed").override_lock(true))
        .await
        .unwrap();
    assert!(near(leased(&planner, &plan, "a"), 1.0 / 3.0));
}

#[tokio::test]
async fn forced_revise_records_the_released_lease_hours() {
    let (planner, clock) = planner();
    let plan = planner.submit_plan(graph(&["a", "b"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(30);
    planner
        .revise_plan(ReviseRequest::new(plan.clone(), graph(&["b"])).force(true))
        .await
        .unwrap();
    assert!(near(leased(&planner, &plan, "a"), 0.5));
}

#[tokio::test]
async fn revise_records_a_lapsed_lease_up_to_expiry() {
    let (planner, clock) = planner();
    let plan = planner.submit_plan(graph(&["a", "b"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(150);
    planner
        .revise_plan(ReviseRequest::new(plan.clone(), graph(&["a", "c"])))
        .await
        .unwrap();
    assert!(near(leased(&planner, &plan, "a"), 1.0));
}

#[tokio::test]
async fn revise_keeps_a_surviving_lease_running() {
    let (planner, clock) = planner();
    let plan = planner.submit_plan(graph(&["a", "b"])).await.unwrap();
    acquire(&planner, &plan, "a").await;
    clock.advance_minutes(30);
    planner
        .revise_plan(ReviseRequest::new(plan.clone(), graph(&["a", "c"])))
        .await
        .unwrap();
    assert_eq!(leased(&planner, &plan, "a"), 0.0);
}

#[tokio::test]
async fn forced_select_records_the_released_lease_hours() {
    let (planner, clock) = planner();
    let (main, alt) = main_and_alt(&planner).await;
    acquire(&planner, &main, "a").await;
    clock.advance_minutes(6);
    planner.select_variant(&alt, true).await.unwrap();
    assert!(near(leased(&planner, &main, "a"), 0.1));
}

#[tokio::test]
async fn forced_archive_records_the_released_lease_hours() {
    let (planner, clock) = planner();
    let (main, _) = main_and_alt(&planner).await;
    acquire(&planner, &main, "a").await;
    clock.advance_minutes(36);
    planner
        .archive("proj", "web", None, true, true)
        .await
        .unwrap();
    assert!(near(leased(&planner, &main, "a"), 0.6));
}

fn actuals(
    planner: &BasicCpmPlanner,
    plan_id: &PlanId,
    id: &str,
) -> Option<crate::earned_value::Actuals> {
    planner
        .store()
        .read_tx(|tx| crate::ev_store::load_actuals(tx, plan_id))
        .unwrap()
        .remove(id)
}

#[tokio::test]
async fn select_carries_actuals_for_carried_deliverables() {
    let (planner, clock) = planner();
    let (main, alt) = main_and_alt(&planner).await;
    acquire(&planner, &main, "a").await;
    clock.advance_minutes(30);
    planner
        .mark_status(
            MarkStatusRequest::new(main.clone(), "a", worker(), DeliverableStatus::Complete)
                .with_actual_effort_hours(2.0)
                .with_evidence("merged"),
        )
        .await
        .unwrap();
    planner.select_variant(&alt, false).await.unwrap();
    assert_eq!(actuals(&planner, &alt, "a"), actuals(&planner, &main, "a"));
}

#[tokio::test]
async fn select_does_not_carry_actuals_for_uncarried_deliverables() {
    let (planner, clock) = planner();
    let main = planner
        .sync_plan(SyncRequest::new("proj", "web", "main", graph(&["a"])))
        .await
        .unwrap()
        .plan_id;
    let mut changed = graph(&["a"]);
    changed.deliverables[0].estimated_effort_hours = Some(5.0);
    let alt = planner
        .sync_plan(SyncRequest::new("proj", "web", "alt", changed))
        .await
        .unwrap()
        .plan_id;
    acquire(&planner, &main, "a").await;
    clock.advance_minutes(30);
    mark(&planner, &main, "a", DeliverableStatus::Complete).await;
    planner.select_variant(&alt, false).await.unwrap();
    assert_eq!(actuals(&planner, &alt, "a"), None);
}
