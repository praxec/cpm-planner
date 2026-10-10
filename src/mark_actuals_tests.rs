//! `mark_status` progress reports (`earned_pct`, `actual_effort_hours`,
//! `evidence`) persist to `ev_actuals` in the mark's transaction.

#![allow(clippy::float_cmp)]

use crate::BasicCpmPlanner;
use crate::earned_value::Actuals;
use crate::plan::{
    AcquireRequest, CallerId, Deliverable, DeliverableStatus, MarkStatusRequest, PlanGraph, PlanId,
};
use crate::ports::Planner;

fn worker() -> CallerId {
    CallerId("w1".into())
}

/// One ready deliverable `a`, leased to `w1`.
async fn leased_a() -> (BasicCpmPlanner, PlanId) {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner
        .submit_plan(PlanGraph {
            deliverables: vec![Deliverable {
                id: "a".into(),
                owned_files: vec!["src/a.rs".into()],
                prerequisites: vec![],
                estimated_effort_hours: Some(1.0),
                duration_hours: None,
                estimate: None,
                metadata: serde_json::Value::Null,
                milestone: false,
                earning_rule: None,
            }],
            max_chained_dispatch: None,
        })
        .await
        .unwrap();
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), worker(), 1))
        .await
        .unwrap();
    (planner, plan_id)
}

fn mark(plan_id: &PlanId, status: DeliverableStatus) -> MarkStatusRequest {
    MarkStatusRequest::new(plan_id.clone(), "a", worker(), status)
}

/// Stored actuals of `a` (default when it has no row).
fn actuals(planner: &BasicCpmPlanner, plan_id: &PlanId) -> Actuals {
    planner
        .store()
        .read_tx(|tx| crate::ev_store::load_actuals(tx, plan_id))
        .unwrap()
        .remove("a")
        .unwrap_or_default()
}

#[tokio::test]
async fn actual_hours_are_persisted() {
    let (planner, plan_id) = leased_a().await;
    planner
        .mark_status(mark(&plan_id, DeliverableStatus::InProgress).with_actual_effort_hours(2.5))
        .await
        .unwrap();
    assert_eq!(actuals(&planner, &plan_id).actual_hours, Some(2.5));
}

#[tokio::test]
async fn earned_pct_with_in_progress_is_persisted() {
    let (planner, plan_id) = leased_a().await;
    planner
        .mark_status(mark(&plan_id, DeliverableStatus::InProgress).with_earned_pct(40))
        .await
        .unwrap();
    assert_eq!(actuals(&planner, &plan_id).earned_pct, Some(40));
}

#[tokio::test]
async fn earned_pct_with_complete_is_ignored() {
    let (planner, plan_id) = leased_a().await;
    planner
        .mark_status(mark(&plan_id, DeliverableStatus::Complete).with_earned_pct(40))
        .await
        .unwrap();
    assert_eq!(actuals(&planner, &plan_id).earned_pct, None);
}

#[tokio::test]
async fn evidence_is_appended() {
    let (planner, plan_id) = leased_a().await;
    for (status, note) in [
        (DeliverableStatus::InProgress, "tests written"),
        (DeliverableStatus::Complete, "merged"),
    ] {
        planner
            .mark_status(mark(&plan_id, status).with_evidence(note))
            .await
            .unwrap();
    }
    assert_eq!(
        actuals(&planner, &plan_id).evidence,
        vec!["tests written".to_string(), "merged".to_string()]
    );
}

#[tokio::test]
async fn an_idempotent_complete_still_records_its_evidence() {
    let (planner, plan_id) = leased_a().await;
    for note in ["merged", "deployed"] {
        planner
            .mark_status(mark(&plan_id, DeliverableStatus::Complete).with_evidence(note))
            .await
            .unwrap();
    }
    assert_eq!(actuals(&planner, &plan_id).evidence.len(), 2);
}

#[tokio::test]
async fn a_refused_mark_records_no_actuals() {
    let (planner, plan_id) = leased_a().await;
    let foreign = MarkStatusRequest::new(
        plan_id.clone(),
        "a",
        CallerId("intruder".into()),
        DeliverableStatus::InProgress,
    )
    .with_actual_effort_hours(9.0);
    let _ = planner.mark_status(foreign).await;
    assert_eq!(actuals(&planner, &plan_id).actual_hours, None);
}

#[tokio::test]
async fn a_mark_without_progress_fields_creates_no_actuals_row() {
    let (planner, plan_id) = leased_a().await;
    planner
        .mark_status(mark(&plan_id, DeliverableStatus::InProgress))
        .await
        .unwrap();
    let rows = planner
        .store()
        .read_tx(|tx| crate::ev_store::load_actuals(tx, &plan_id))
        .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn evidence_beyond_100_entries_is_rejected() {
    let (planner, plan_id) = leased_a().await;
    for i in 0..100 {
        planner
            .mark_status(
                mark(&plan_id, DeliverableStatus::InProgress).with_evidence(format!("e{i}")),
            )
            .await
            .unwrap();
    }
    let err = planner
        .mark_status(mark(&plan_id, DeliverableStatus::InProgress).with_evidence("e100"))
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "INVALID_ACTUALS: deliverable 'a' already has 100 evidence entries"
    );
}
