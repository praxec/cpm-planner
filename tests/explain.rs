//! `cpm_planner::explain`: why a deliverable is blocked, what completing it
//! changes, and next-work classes. Each test drives a fixed clock and an
//! explicit `as_of`, and checks agreement with the overview, `status` and
//! `acquire_cohort` where they overlap.

#![allow(clippy::float_cmp)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use cpm_planner::BasicCpmPlanner;
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::explain::{
    DeliverableState, EXPLAIN_SCHEMA, ExplainRequest, Explanation, NextWorkClass,
    build_explanation, classify_next_work, explain, preview_cohort, render_markdown,
    slack_to_milestone,
};
use cpm_planner::overview::{
    BlockerClass, Detail, OverviewRequest, PlanIndex, PlanSnapshot, ReadyAction, overview,
    read_snapshot,
};
use cpm_planner::plan::{
    AcquireRequest, CallerId, Deliverable, DeliverableStatus, MarkStatusRequest, OwnedFile,
    PlanGraph, PlanId, PlanRef, PlannerError, Prerequisite, SyncRequest,
};
use cpm_planner::ports::Planner;
use serde_json::json;

const PROJECT: &str = "proj";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 2, 9, 0, 0).unwrap()
}

/// Inside the one-hour lease TTL.
fn as_of() -> DateTime<Utc> {
    t0() + chrono::Duration::minutes(30)
}

fn planner() -> BasicCpmPlanner {
    BasicCpmPlanner::with_parts(
        Arc::new(MemoryAuditSink::new()),
        Duration::from_secs(3600),
        Arc::new(t0),
    )
}

fn d(id: &str, prereqs: &[&str], hours: f32, metadata: serde_json::Value) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: vec![OwnedFile::from(PathBuf::from(format!("src/{id}.rs")))],
        prerequisites: prereqs.iter().map(|p| Prerequisite::from(*p)).collect(),
        estimated_effort_hours: Some(hours),
        metadata,
        duration_hours: None,
        estimate: None,
        milestone: false,
        earning_rule: None,
    }
}

fn milestone(id: &str, prereqs: &[&str], title: &str) -> Deliverable {
    let mut m = d(id, prereqs, 0.0, json!({"title": title}));
    m.owned_files = Vec::new();
    m.milestone = true;
    m
}

/// contract(4) -> approve(1, manual) -> service(6) -> ship (milestone, 11h)
/// contract -> ui(2) -> ship (5h float);   docs(1) -> ship (10h float)
fn graph() -> PlanGraph {
    PlanGraph {
        deliverables: vec![
            d(
                "contract",
                &[],
                4.0,
                json!({"title": "Export API contract", "owner": "agent"}),
            ),
            d(
                "approve",
                &["contract"],
                1.0,
                json!({"title": "Product sign-off", "owner": "pm", "kind": "manual"}),
            ),
            d("service", &["approve"], 6.0, json!({"owner": "agent"})),
            d("ui", &["contract"], 2.0, json!({"owner": "agent"})),
            d("docs", &[], 1.0, json!({"title": "User guide"})),
            milestone("ship", &["service", "ui", "docs"], "Export shipped"),
        ],
        max_chained_dispatch: None,
    }
}

async fn synced(planner: &BasicCpmPlanner) -> PlanId {
    planner
        .sync_plan(SyncRequest::new(PROJECT, "export", "main", graph()))
        .await
        .unwrap()
        .plan_id
}

fn worker() -> CallerId {
    CallerId("worker".into())
}

async fn lease(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str) {
    planner
        .acquire_cohort(
            AcquireRequest::new(plan_id.clone(), worker(), 1).with_ids(vec![id.to_string()]),
        )
        .await
        .unwrap();
}

async fn mark(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str, status: DeliverableStatus) {
    lease(planner, plan_id, id).await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            id,
            worker(),
            status,
        ))
        .await
        .unwrap();
}

async fn complete(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str) {
    mark(planner, plan_id, id, DeliverableStatus::Complete).await;
}

fn req(plan_id: &PlanId) -> ExplainRequest {
    ExplainRequest::new(PlanRef::Id(plan_id.clone())).with_as_of(as_of())
}

async fn about(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str) -> Explanation {
    explain(planner, req(plan_id).with_deliverable(id))
        .await
        .unwrap()
}

async fn snapshot(planner: &BasicCpmPlanner, plan_id: &PlanId) -> PlanSnapshot {
    read_snapshot(planner, plan_id, as_of()).await.unwrap()
}

fn ids(section: &cpm_planner::overview::Section<cpm_planner::explain::NextWorkEntry>) -> Vec<&str> {
    section.items.iter().map(|e| e.item.id.as_str()).collect()
}

/// A snapshot where `docs` also claims `src/contract.rs`. Validation never
/// stores two unordered exclusive claims, so conflicts are exercised on the
/// snapshot, as `acquire_cohort` would see them.
fn with_docs_on_contract_file(mut snap: PlanSnapshot) -> PlanSnapshot {
    let docs = snap
        .graph
        .deliverables
        .iter_mut()
        .find(|d| d.id == "docs")
        .unwrap();
    docs.owned_files = vec![OwnedFile::from(PathBuf::from("src/contract.rs"))];
    snap
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn line_selector_explains_the_resolved_plan() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let line = PlanRef::Line {
        project: PROJECT.into(),
        name: "export".into(),
    };
    let e = explain(&planner, ExplainRequest::new(line).with_as_of(as_of()))
        .await
        .unwrap();
    assert_eq!(e.plan.plan_id, plan_id);
}

#[tokio::test]
async fn unknown_deliverable_is_not_found() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let err = explain(&planner, req(&plan_id).with_deliverable("nope"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, PlannerError::DeliverableNotFound { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn zero_limit_is_invalid() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let err = explain(&planner, req(&plan_id).with_limit(0))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err:?}");
}

#[tokio::test]
async fn without_a_deliverable_only_next_work_is_returned() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = explain(&planner, req(&plan_id)).await.unwrap();
    assert!(e.deliverable.is_none());
}

// ---------------------------------------------------------------------------
// Why blocked
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deliverable_waiting_on_a_prerequisite_is_blocked() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = about(&planner, &plan_id, "service").await;
    assert_eq!(e.deliverable.unwrap().state, DeliverableState::Blocked);
}

#[tokio::test]
async fn blocked_deliverable_names_its_manual_root_and_how_to_close_it() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let e = about(&planner, &plan_id, "service").await;
    let blocked = e.deliverable.unwrap().blocked.unwrap();
    let root = &blocked.root_blockers[0];
    assert_eq!(
        (root.item.id.as_str(), root.class, root.action),
        ("approve", BlockerClass::Manual, Some(ReadyAction::Accept))
    );
}

#[tokio::test]
async fn blocked_class_follows_the_driving_chain_not_the_latest_root() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    // ship's roots are ui (finishes at 6h) and approve (5h), but the
    // driving chain runs ship <- service <- approve.
    let e = about(&planner, &plan_id, "ship").await;
    assert_eq!(
        e.deliverable.unwrap().blocked.unwrap().class,
        BlockerClass::Manual
    );
}

#[tokio::test]
async fn blocked_chain_runs_from_the_deliverable_to_the_driving_root() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = about(&planner, &plan_id, "ship").await;
    assert_eq!(
        e.deliverable.unwrap().blocked.unwrap().chain,
        ["ship", "service", "approve", "contract"]
    );
}

#[tokio::test]
async fn blocked_chain_agrees_with_the_full_overview() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = overview(
        &planner,
        OverviewRequest::new(PlanRef::Id(plan_id.clone()))
            .with_as_of(as_of())
            .with_detail(Detail::Full),
    )
    .await
    .unwrap();
    let from_overview = o
        .blocked
        .items
        .iter()
        .find(|b| b.item.id == "ship")
        .unwrap()
        .chain
        .clone();
    let e = about(&planner, &plan_id, "ship").await;
    assert_eq!(e.deliverable.unwrap().blocked.unwrap().chain, from_overview);
}

// ---------------------------------------------------------------------------
// Other states
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ready_manual_deliverable_is_closed_with_accept() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let e = about(&planner, &plan_id, "approve").await;
    let d = e.deliverable.unwrap();
    assert_eq!(
        (d.state, d.action),
        (DeliverableState::Ready, Some(ReadyAction::Accept))
    );
}

#[tokio::test]
async fn leased_deliverable_names_its_holder() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    lease(&planner, &plan_id, "docs").await;
    let e = about(&planner, &plan_id, "docs").await;
    let d = e.deliverable.unwrap();
    assert_eq!(
        (d.state, d.lease.map(|l| l.caller_id)),
        (DeliverableState::Leased, Some("worker".to_string()))
    );
}

#[tokio::test]
async fn failed_deliverable_carries_its_reason() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    mark(
        &planner,
        &plan_id,
        "docs",
        DeliverableStatus::Failed {
            reason: "link check failed".into(),
        },
    )
    .await;
    let e = about(&planner, &plan_id, "docs").await;
    let d = e.deliverable.unwrap();
    assert_eq!(
        (d.state, d.reason.as_deref()),
        (DeliverableState::Failed, Some("link check failed"))
    );
}

#[tokio::test]
async fn complete_deliverable_has_no_schedule_impact() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "docs").await;
    let e = about(&planner, &plan_id, "docs").await;
    assert!(
        e.deliverable
            .unwrap()
            .critical_path_impact
            .starts_with("Complete"),
    );
}

// ---------------------------------------------------------------------------
// What completing it changes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unlocks_lists_the_dependents_it_alone_holds_back() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = about(&planner, &plan_id, "contract").await;
    let unlocks: Vec<String> = e
        .deliverable
        .unwrap()
        .unlocks
        .into_iter()
        .map(|n| n.id)
        .collect();
    assert_eq!(unlocks, ["approve", "ui"]);
}

#[tokio::test]
async fn unlocks_leave_out_dependents_still_waiting_on_others() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = about(&planner, &plan_id, "docs").await;
    assert!(e.deliverable.unwrap().unlocks.is_empty());
}

#[tokio::test]
async fn critical_deliverable_drives_its_milestone() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = about(&planner, &plan_id, "service").await;
    let fed = &e.deliverable.unwrap().feeds_milestones[0];
    assert_eq!(
        (fed.id.as_str(), fed.critical, fed.slack_hours),
        ("ship", true, 0.0)
    );
}

#[tokio::test]
async fn slack_deliverable_reports_hours_before_it_moves_the_milestone() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = about(&planner, &plan_id, "ui").await;
    assert_eq!(e.deliverable.unwrap().feeds_milestones[0].slack_hours, 5.0);
}

#[tokio::test]
async fn milestone_slack_counts_edge_lag() {
    let planner = planner();
    let mut g = graph();
    g.deliverables[5].prerequisites[1] = Prerequisite::Edge {
        id: "ui".into(),
        consumes: None,
        kind: None,
        lag_hours: Some(2.0),
    };
    let plan_id = planner.submit_plan(g).await.unwrap();
    let snap = snapshot(&planner, &plan_id).await;
    let index = PlanIndex::new(&snap.graph, &snap.status);
    assert_eq!(slack_to_milestone(&snap, &index, "ui", "ship"), Some(3.0));
}

#[tokio::test]
async fn critical_impact_sentence_says_it_moves_the_finish() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = about(&planner, &plan_id, "service").await;
    assert_eq!(
        e.deliverable.unwrap().critical_path_impact,
        "On the critical path: each hour it slips moves the plan finish by an hour; \
         it drives milestone `ship`."
    );
}

#[tokio::test]
async fn slack_impact_sentence_gives_float_and_milestone_slack() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = about(&planner, &plan_id, "ui").await;
    assert_eq!(
        e.deliverable.unwrap().critical_path_impact,
        "5.0h of float before a slip moves the plan finish; \
         5.0h of slack before it moves milestone `ship`."
    );
}

// ---------------------------------------------------------------------------
// Next-work classes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ready_critical_holds_ready_work_on_the_critical_path() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = explain(&planner, req(&plan_id)).await.unwrap();
    assert_eq!(ids(&e.next_work.ready_critical), ["contract"]);
}

#[tokio::test]
async fn ready_slack_holds_ready_work_with_float() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = explain(&planner, req(&plan_id)).await.unwrap();
    assert_eq!(ids(&e.next_work.ready_slack), ["docs"]);
}

#[tokio::test]
async fn blocked_manual_holds_work_behind_an_owner_action() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let e = explain(&planner, req(&plan_id)).await.unwrap();
    assert_eq!(ids(&e.next_work.blocked_manual), ["service", "ship"]);
}

#[tokio::test]
async fn blocked_dependency_holds_work_behind_ordinary_work() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = explain(&planner, req(&plan_id)).await.unwrap();
    assert_eq!(
        ids(&e.next_work.blocked_dependency),
        ["approve", "service", "ship", "ui"]
    );
}

#[tokio::test]
async fn blocked_gate_holds_work_behind_a_gate() {
    let planner = planner();
    let mut g = graph();
    g.deliverables[0].metadata["kind"] = json!("gate");
    let plan_id = planner.submit_plan(g).await.unwrap();
    let e = explain(&planner, req(&plan_id)).await.unwrap();
    assert_eq!(
        ids(&e.next_work.blocked_gate),
        ["approve", "service", "ship", "ui"]
    );
}

#[tokio::test]
async fn blocked_external_holds_work_behind_a_sign_off() {
    let planner = planner();
    let mut g = graph();
    g.deliverables[0].metadata["kind"] = json!("external");
    let plan_id = planner.submit_plan(g).await.unwrap();
    let e = explain(&planner, req(&plan_id)).await.unwrap();
    assert_eq!(
        ids(&e.next_work.blocked_external),
        ["approve", "service", "ship", "ui"]
    );
}

#[tokio::test]
async fn blocked_entry_names_its_driving_blocker() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let e = explain(&planner, req(&plan_id)).await.unwrap();
    let ship = e
        .next_work
        .blocked_manual
        .items
        .iter()
        .find(|n| n.item.id == "ship")
        .unwrap();
    assert_eq!(
        ship.blocked_by.as_ref().map(|b| b.id.as_str()),
        Some("approve")
    );
}

#[tokio::test]
async fn work_driving_an_incomplete_milestone_is_ready_critical() {
    let planner = planner();
    let mut g = graph();
    // An intermediate milestone driven by docs alone.
    g.deliverables
        .push(milestone("guide-out", &["docs"], "Guide published"));
    let plan_id = planner.submit_plan(g).await.unwrap();
    let e = explain(&planner, req(&plan_id)).await.unwrap();
    assert_eq!(ids(&e.next_work.ready_critical), ["contract", "docs"]);
}

#[tokio::test]
async fn leased_and_failed_work_has_no_class() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    lease(&planner, &plan_id, "contract").await;
    mark(
        &planner,
        &plan_id,
        "docs",
        DeliverableStatus::Failed { reason: "x".into() },
    )
    .await;
    let snap = snapshot(&planner, &plan_id).await;
    let index = PlanIndex::new(&snap.graph, &snap.status);
    let preview = preview_cohort(&snap, &index);
    let classified: Vec<String> = classify_next_work(&snap, &index, &preview)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(classified, ["approve", "service", "ship", "ui"]);
}

#[tokio::test]
async fn every_classified_deliverable_has_exactly_one_class() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let snap = snapshot(&planner, &plan_id).await;
    let index = PlanIndex::new(&snap.graph, &snap.status);
    let preview = preview_cohort(&snap, &index);
    let mut classified: Vec<String> = classify_next_work(&snap, &index, &preview)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    classified.sort();
    assert_eq!(classified, ["approve", "docs", "service", "ship", "ui"]);
}

// ---------------------------------------------------------------------------
// File conflicts (acquire_cohort semantics, read-only)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cohort_preview_leases_what_acquire_cohort_leases() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let snap = snapshot(&planner, &plan_id).await;
    let index = PlanIndex::new(&snap.graph, &snap.status);
    let preview = preview_cohort(&snap, &index);
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), worker(), 100))
        .await
        .unwrap();
    let leased: Vec<String> = cohort
        .rows
        .iter()
        .map(|r| r.deliverable.id.clone())
        .collect();
    assert_eq!(preview.leasable, leased);
}

#[tokio::test]
async fn lower_priority_candidate_sharing_a_file_is_file_conflicting() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let snap = with_docs_on_contract_file(snapshot(&planner, &plan_id).await);
    let e = build_explanation(&snap, None, 10).unwrap();
    let conflict = &e.next_work.file_conflicting.items[0];
    assert_eq!(
        (
            conflict.item.id.as_str(),
            conflict.file_conflicts[0].with.as_str(),
            conflict.file_conflicts[0].leased
        ),
        ("docs", "contract", false)
    );
}

#[tokio::test]
async fn candidate_sharing_a_file_with_a_live_lease_is_file_conflicting() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    lease(&planner, &plan_id, "contract").await;
    let snap = with_docs_on_contract_file(snapshot(&planner, &plan_id).await);
    let index = PlanIndex::new(&snap.graph, &snap.status);
    let preview = preview_cohort(&snap, &index);
    assert!(preview.conflicting["docs"][0].leased);
}

#[tokio::test]
async fn expired_lease_holds_no_file_claim() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    lease(&planner, &plan_id, "contract").await;
    let late = t0() + chrono::Duration::hours(2);
    let snap = with_docs_on_contract_file(read_snapshot(&planner, &plan_id, late).await.unwrap());
    let index = PlanIndex::new(&snap.graph, &snap.status);
    assert!(preview_cohort(&snap, &index).conflicting.is_empty());
}

#[tokio::test]
async fn cohort_preview_skips_manual_work() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let snap = snapshot(&planner, &plan_id).await;
    let index = PlanIndex::new(&snap.graph, &snap.status);
    assert!(
        !preview_cohort(&snap, &index)
            .leasable
            .contains(&"approve".to_string())
    );
}

// ---------------------------------------------------------------------------
// Formats and determinism
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_carries_the_v1_schema_id() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let value = serde_json::to_value(about(&planner, &plan_id, "ui").await).unwrap();
    assert_eq!(value["schema"], EXPLAIN_SCHEMA);
}

#[tokio::test]
async fn json_round_trips_to_the_same_explanation() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let e = about(&planner, &plan_id, "ship").await;
    let back: Explanation = serde_json::from_value(serde_json::to_value(&e).unwrap()).unwrap();
    assert_eq!(back, e);
}

#[tokio::test]
async fn json_deliverable_flattens_title_and_state() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let value = serde_json::to_value(about(&planner, &plan_id, "contract").await).unwrap();
    let d = &value["deliverable"];
    assert_eq!(
        (&d["title"], &d["state"]),
        (&json!("Export API contract"), &json!("ready"))
    );
}

#[tokio::test]
async fn repeated_explanations_at_one_as_of_are_identical() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let a = about(&planner, &plan_id, "ship").await;
    let b = about(&planner, &plan_id, "ship").await;
    assert_eq!(a, b);
}

#[tokio::test]
async fn class_enum_serialises_to_the_documented_names() {
    let names: Vec<serde_json::Value> = [
        NextWorkClass::ReadyCritical,
        NextWorkClass::ReadySlack,
        NextWorkClass::BlockedDependency,
        NextWorkClass::BlockedManual,
        NextWorkClass::BlockedExternal,
        NextWorkClass::BlockedGate,
        NextWorkClass::FileConflicting,
    ]
    .iter()
    .map(|c| serde_json::to_value(c).unwrap())
    .collect();
    assert_eq!(
        names,
        [
            "ready_critical",
            "ready_slack",
            "blocked_dependency",
            "blocked_manual",
            "blocked_external",
            "blocked_gate",
            "file_conflicting"
        ]
    );
}

#[tokio::test]
async fn markdown_says_why_and_what_to_move() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let md = render_markdown(&about(&planner, &plan_id, "service").await);
    assert!(
        md.contains("Blocked (manual) waiting on `approve`.\n- move Product sign-off (`approve`) (ready, manual: accept)"),
        "{md}"
    );
}

#[tokio::test]
async fn markdown_lists_next_work_by_class() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let md = render_markdown(&explain(&planner, req(&plan_id)).await.unwrap());
    assert!(
        md.contains("- **Ready, critical** (1): Export API contract (`contract`) acquire"),
        "{md}"
    );
}
