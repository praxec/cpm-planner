//! `cpm_planner::overview`: the one-call plan overview. Each test drives a
//! fixed clock and an explicit `as_of`, and checks the overview against the
//! existing read APIs (`status`, `get_plan`, `ev`, `resolve_plan`).

#![allow(clippy::float_cmp)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use cpm_planner::BasicCpmPlanner;
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::earned_value::BaselineRequest;
use cpm_planner::overview::{
    AttentionCode, BlockerClass, Detail, EvSection, OVERVIEW_SCHEMA, Overview, OverviewFilter,
    OverviewOptions, OverviewRequest, PlanIndex, PlanSnapshot, ReadyAction, StatusLabel,
    build_overview, overview, read_snapshot, render_markdown,
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

fn as_of() -> DateTime<Utc> {
    t0() + chrono::Duration::hours(2)
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

/// contract -> approve (manual) -> service -> ship (milestone)
/// contract -> ui -> ship;   docs (independent, slack) -> ship
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
            d(
                "service",
                &["approve"],
                6.0,
                json!({"artifact": "src/service.rs with tests", "owner": "agent"}),
            ),
            d("ui", &["contract"], 2.0, json!({"owner": "agent"})),
            d(
                "docs",
                &[],
                1.0,
                json!({"description": "User guide", "owner": "writer"}),
            ),
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

async fn complete(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str) {
    lease(planner, plan_id, id).await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            id,
            worker(),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
}

fn by_id(plan_id: &PlanId) -> PlanRef {
    PlanRef::Id(plan_id.clone())
}

async fn view(planner: &BasicCpmPlanner, plan_id: &PlanId, detail: Detail) -> Overview {
    overview(
        planner,
        OverviewRequest::new(by_id(plan_id))
            .with_as_of(as_of())
            .with_detail(detail),
    )
    .await
    .unwrap()
}

async fn snapshot(planner: &BasicCpmPlanner, plan_id: &PlanId) -> PlanSnapshot {
    read_snapshot(planner, plan_id, as_of()).await.unwrap()
}

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn line_selector_reports_the_plan_resolve_plan_returns() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let line = PlanRef::Line {
        project: PROJECT.into(),
        name: "export".into(),
    };
    let o = overview(&planner, OverviewRequest::new(line).with_as_of(as_of()))
        .await
        .unwrap();
    assert_eq!(o.plan.plan_id, plan_id);
}

#[tokio::test]
async fn active_selector_reports_the_projects_active_plan() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let active = PlanRef::Active {
        project: PROJECT.into(),
    };
    let o = overview(&planner, OverviewRequest::new(active).with_as_of(as_of()))
        .await
        .unwrap();
    assert_eq!(o.plan.plan_id, plan_id);
}

#[tokio::test]
async fn unknown_line_selector_is_the_resolve_plan_error() {
    let planner = planner();
    synced(&planner).await;
    let line = PlanRef::Line {
        project: PROJECT.into(),
        name: "nope".into(),
    };
    let err = overview(&planner, OverviewRequest::new(line))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::PlanNotFound { .. }), "{err:?}");
}

// ---------------------------------------------------------------------------
// Agreement with status / get / ev
// ---------------------------------------------------------------------------

#[tokio::test]
async fn identity_matches_status_and_head_revision() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let status = planner.status(&plan_id).await.unwrap();
    let (revision, _) = planner.revision_graph(&plan_id, None).await.unwrap();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    assert_eq!(
        (
            o.plan.name,
            o.plan.variant,
            o.plan.selected,
            o.plan.revision,
            o.plan.definition_drift
        ),
        (
            status.name,
            status.variant,
            status.selected,
            revision,
            status.definition_drift
        )
    );
}

#[tokio::test]
async fn counts_match_status_rows() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let status = planner.status(&plan_id).await.unwrap();
    let complete_rows = status
        .deliverables
        .iter()
        .filter(|r| r.1 == DeliverableStatus::Complete)
        .count();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    assert_eq!(
        (o.counts.total, o.counts.complete),
        (status.deliverables.len(), complete_rows)
    );
}

#[tokio::test]
async fn critical_path_matches_status_without_endpoints() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let status = planner.status(&plan_id).await.unwrap();
    let expected: Vec<String> = status
        .critical_path
        .iter()
        .filter(|id| !id.starts_with("__"))
        .cloned()
        .collect();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let ids: Vec<String> = o
        .critical_path
        .items
        .iter()
        .map(|e| e.item.id.clone())
        .collect();
    assert_eq!(ids, expected);
}

#[tokio::test]
async fn ready_queue_matches_status_ready_in_order() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let status = planner.status(&plan_id).await.unwrap();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let ids: Vec<String> = o.ready.items.iter().map(|e| e.item.id.clone()).collect();
    assert_eq!(ids, status.ready);
}

#[tokio::test]
async fn titles_come_from_get_plan_metadata() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let def = planner.get_plan(&plan_id).await.unwrap();
    let contract = def
        .graph
        .deliverables
        .iter()
        .find(|d| d.id == "contract")
        .unwrap();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let entry = o
        .ready
        .items
        .iter()
        .find(|e| e.item.id == "contract")
        .unwrap();
    assert_eq!(
        Some(entry.item.title.as_str()),
        contract.metadata["title"].as_str()
    );
}

#[test]
fn title_falls_back_to_the_id_without_metadata() {
    let graph = graph();
    let status = cpm_planner::plan::PlanStatus {
        plan_id: PlanId("p".into()),
        deliverables: Vec::new(),
        critical_path: Vec::new(),
        critical_path_hours: 0.0,
        locks_held: Vec::new(),
        critical_ids: Vec::new(),
        schedule: Vec::new(),
        ready: Vec::new(),
        plan_complete: false,
        milestones: Vec::new(),
        name: None,
        variant: None,
        selected: None,
        archived: None,
        active_plan: None,
        definition_drift: None,
        baseline: None,
    };
    let index = PlanIndex::new(&graph, &status);
    assert_eq!(index.title("ui"), "ui");
}

#[tokio::test]
async fn ev_headline_matches_plan_ev_at_the_same_as_of() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    planner
        .baseline(BaselineRequest::new(plan_id.clone()).with_start(t0()))
        .await
        .unwrap();
    complete(&planner, &plan_id, "contract").await;
    let report = planner.ev(&plan_id, Some(as_of())).await.unwrap();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let EvSection::Baselined(ev) = o.ev else {
        panic!("expected a baselined EV section, got {:?}", o.ev);
    };
    assert_eq!(
        (ev.pv, ev.ev, ev.ac, ev.spi, ev.cpi, ev.undefined),
        (
            report.pv,
            report.ev,
            report.ac,
            report.spi,
            report.cpi,
            report.undefined
        )
    );
}

#[tokio::test]
async fn ev_before_a_baseline_says_not_baselined() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = view(&planner, &plan_id, Detail::Brief).await;
    assert!(
        matches!(&o.ev, EvSection::NotBaselined { reason } if reason.starts_with("NOT_BASELINED")),
        "{:?}",
        o.ev
    );
}

#[tokio::test]
async fn every_section_shares_the_requested_as_of() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = view(&planner, &plan_id, Detail::Brief).await;
    assert_eq!(o.as_of, as_of());
}

// ---------------------------------------------------------------------------
// Critical path, milestones, ready
// ---------------------------------------------------------------------------

#[tokio::test]
async fn critical_entry_names_the_milestone_it_feeds() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let service = o
        .critical_path
        .items
        .iter()
        .find(|e| e.item.id == "service")
        .unwrap();
    let feeds: Vec<(&str, &str)> = service
        .feeds_milestones
        .iter()
        .map(|m| (m.id.as_str(), m.title.as_str()))
        .collect();
    assert_eq!(feeds, [("ship", "Export shipped")]);
}

#[tokio::test]
async fn completed_critical_entry_has_no_remaining_hours() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let contract = o
        .critical_path
        .items
        .iter()
        .find(|e| e.item.id == "contract")
        .unwrap();
    assert_eq!(contract.remaining_hours, 0.0);
}

#[tokio::test]
async fn milestone_names_the_next_incomplete_deliverable_on_its_path() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let ship = &o.milestones.items[0];
    assert_eq!(
        ship.next_on_path.as_ref().map(|n| n.id.as_str()),
        Some("approve")
    );
}

#[tokio::test]
async fn ready_entry_names_its_next_milestone() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let docs = o.ready.items.iter().find(|e| e.item.id == "docs").unwrap();
    assert_eq!(
        docs.next_milestone.as_ref().map(|m| m.id.as_str()),
        Some("ship")
    );
}

#[tokio::test]
async fn ready_manual_deliverable_is_closed_with_accept() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let approve = o
        .ready
        .items
        .iter()
        .find(|e| e.item.id == "approve")
        .unwrap();
    assert_eq!(approve.action, ReadyAction::Accept);
}

#[tokio::test]
async fn ready_entries_report_owned_file_conflicts_between_candidates() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let mut snap = snapshot(&planner, &plan_id).await;
    // Validation never stores two unordered exclusive claims, so give the
    // snapshot one to exercise acquire_cohort's conflict rule.
    let docs = snap
        .graph
        .deliverables
        .iter_mut()
        .find(|d| d.id == "docs")
        .unwrap();
    docs.owned_files = vec![OwnedFile::from(PathBuf::from("src/contract.rs"))];
    let o = build_overview(&snap, &OverviewOptions::default()).unwrap();
    let docs = o.ready.items.iter().find(|e| e.item.id == "docs").unwrap();
    let conflicts: Vec<(&str, &str)> = docs
        .file_conflicts
        .iter()
        .map(|c| (c.path.as_str(), c.with.as_str()))
        .collect();
    assert_eq!(conflicts, [("src/contract.rs", "contract")]);
}

// ---------------------------------------------------------------------------
// Blocked
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blocked_item_names_its_manual_root_blocker() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let service = o
        .blocked
        .items
        .iter()
        .find(|e| e.item.id == "service")
        .unwrap();
    let roots: Vec<(&str, BlockerClass)> = service
        .root_blockers
        .iter()
        .map(|r| (r.item.id.as_str(), r.class))
        .collect();
    assert_eq!(roots, [("approve", BlockerClass::Manual)]);
}

#[tokio::test]
async fn full_detail_shows_the_driving_prerequisite_chain() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = view(&planner, &plan_id, Detail::Full).await;
    let ship = o
        .blocked
        .items
        .iter()
        .find(|e| e.item.id == "ship")
        .unwrap();
    assert_eq!(ship.chain, ["ship", "service", "approve", "contract"]);
}

#[tokio::test]
async fn brief_detail_omits_the_chain() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let ship = o
        .blocked
        .items
        .iter()
        .find(|e| e.item.id == "ship")
        .unwrap();
    assert!(ship.chain.is_empty(), "{:?}", ship.chain);
}

#[tokio::test]
async fn blocked_set_is_the_incomplete_items_with_incomplete_prerequisites() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = view(&planner, &plan_id, Detail::Full).await;
    let mut ids: Vec<&str> = o.blocked.items.iter().map(|e| e.item.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["approve", "service", "ship", "ui"]);
}

#[tokio::test]
async fn gate_root_blocker_is_classed_gate() {
    let planner = planner();
    let mut g = graph();
    g.deliverables[0].metadata["kind"] = json!("gate");
    let plan_id = planner.submit_plan(g).await.unwrap();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let ui = o.blocked.items.iter().find(|e| e.item.id == "ui").unwrap();
    assert_eq!(ui.class, BlockerClass::Gate);
}

#[tokio::test]
async fn external_root_blocker_is_classed_external() {
    let planner = planner();
    let mut g = graph();
    g.deliverables[0].metadata["external"] = json!(true);
    let plan_id = planner.submit_plan(g).await.unwrap();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let ui = o.blocked.items.iter().find(|e| e.item.id == "ui").unwrap();
    assert_eq!(ui.class, BlockerClass::External);
}

#[tokio::test]
async fn unlocks_lists_dependents_waiting_only_on_the_item() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let snap = snapshot(&planner, &plan_id).await;
    let index = PlanIndex::new(&snap.graph, &snap.status);
    assert_eq!(index.unlocks("contract"), ["approve", "ui"]);
}

// ---------------------------------------------------------------------------
// Leases and attention
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lease_past_its_ttl_at_as_of_is_expired() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    lease(&planner, &plan_id, "docs").await;
    let late = t0() + chrono::Duration::hours(5);
    let o = overview(
        &planner,
        OverviewRequest::new(by_id(&plan_id)).with_as_of(late),
    )
    .await
    .unwrap();
    assert!(o.leases.items[0].expired, "{:?}", o.leases.items);
}

#[tokio::test]
async fn failed_deliverable_needs_attention_with_its_reason() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    lease(&planner, &plan_id, "docs").await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "docs",
            worker(),
            DeliverableStatus::Failed {
                reason: "build broke".into(),
            },
        ))
        .await
        .unwrap();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let a = &o.attention.items[0];
    assert_eq!(
        (a.item.id.as_str(), a.code, a.reason.as_deref()),
        ("docs", AttentionCode::Failed, Some("build broke"))
    );
}

// ---------------------------------------------------------------------------
// Options and bounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn owner_filter_keeps_only_that_owners_entries() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = overview(
        &planner,
        OverviewRequest::new(by_id(&plan_id))
            .with_as_of(as_of())
            .with_filter(OverviewFilter {
                owner: Some("writer".into()),
                milestone: None,
            }),
    )
    .await
    .unwrap();
    let ids: Vec<&str> = o.ready.items.iter().map(|e| e.item.id.as_str()).collect();
    assert_eq!(ids, ["docs"]);
}

#[tokio::test]
async fn milestone_filter_naming_a_non_milestone_is_invalid() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let err = overview(
        &planner,
        OverviewRequest::new(by_id(&plan_id)).with_filter(OverviewFilter {
            owner: None,
            milestone: Some("ui".into()),
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err:?}");
}

#[tokio::test]
async fn zero_limit_is_invalid() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let err = overview(
        &planner,
        OverviewRequest::new(by_id(&plan_id)).with_limit(0),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidValue { .. }), "{err:?}");
}

/// 400 independent deliverables feeding one milestone.
fn wide_graph(n: usize) -> PlanGraph {
    let mut deliverables: Vec<Deliverable> = (0..n)
        .map(|i| {
            d(
                &format!("w{i:03}"),
                &[],
                1.0,
                json!({"title": format!("Work {i}")}),
            )
        })
        .collect();
    let ids: Vec<String> = deliverables.iter().map(|d| d.id.clone()).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    deliverables.push(milestone("done", &refs, "All done"));
    PlanGraph {
        deliverables,
        max_chained_dispatch: None,
    }
}

#[tokio::test]
async fn brief_overview_of_a_large_plan_lists_at_most_the_brief_limit() {
    let planner = planner();
    let plan_id = planner.submit_plan(wide_graph(400)).await.unwrap();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    assert_eq!(
        (o.ready.total, o.ready.items.len(), o.ready.omitted),
        (400, cpm_planner::overview::BRIEF_LIMIT, 390)
    );
}

#[tokio::test]
async fn brief_overview_of_a_large_plan_caps_root_blockers() {
    let planner = planner();
    let plan_id = planner.submit_plan(wide_graph(400)).await.unwrap();
    let o = view(&planner, &plan_id, Detail::Brief).await;
    let done = &o.blocked.items[0];
    assert_eq!(
        (done.root_blockers.len(), done.root_blockers_omitted),
        (cpm_planner::overview::BRIEF_ROOT_BLOCKERS, 397)
    );
}

#[tokio::test]
async fn brief_markdown_of_a_large_plan_stays_compact() {
    let planner = planner();
    let plan_id = planner.submit_plan(wide_graph(400)).await.unwrap();
    let md = render_markdown(&view(&planner, &plan_id, Detail::Brief).await);
    assert!(
        md.lines().count() < 60,
        "{} lines:\n{md}",
        md.lines().count()
    );
}

// ---------------------------------------------------------------------------
// Formats
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_carries_the_v1_schema_id() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let value = serde_json::to_value(view(&planner, &plan_id, Detail::Brief).await).unwrap();
    assert_eq!(value["schema"], OVERVIEW_SCHEMA);
}

#[tokio::test]
async fn json_round_trips_to_the_same_overview() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = view(&planner, &plan_id, Detail::Full).await;
    let back: Overview = serde_json::from_value(serde_json::to_value(&o).unwrap()).unwrap();
    assert_eq!(back, o);
}

#[tokio::test]
async fn json_entries_flatten_title_and_status_beside_the_id() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let value = serde_json::to_value(view(&planner, &plan_id, Detail::Brief).await).unwrap();
    let first = &value["ready"]["items"][0];
    assert_eq!(
        (&first["id"], &first["title"], &first["status"]),
        (
            &json!("contract"),
            &json!("Export API contract"),
            &json!("ready")
        )
    );
}

#[tokio::test]
async fn repeated_overviews_at_one_as_of_are_identical() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let a = view(&planner, &plan_id, Detail::Full).await;
    let b = view(&planner, &plan_id, Detail::Full).await;
    assert_eq!(a, b);
}

#[tokio::test]
async fn markdown_shows_titles_and_ids() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let md = render_markdown(&view(&planner, &plan_id, Detail::Brief).await);
    assert!(md.contains("Export API contract (`contract`)"), "{md}");
}

#[tokio::test]
async fn markdown_names_the_blocker_class() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete(&planner, &plan_id, "contract").await;
    let md = render_markdown(&view(&planner, &plan_id, Detail::Brief).await);
    assert!(md.contains("| `approve` (ready) | manual |"), "{md}");
}

#[test]
fn status_label_matches_the_deliverable_status_wire_tag() {
    let wire = serde_json::to_value(DeliverableStatus::SupersededRevision).unwrap();
    let label = serde_json::to_value(StatusLabel::SupersededRevision).unwrap();
    assert_eq!(wire["status"], label);
}

// ---------------------------------------------------------------------------
// Evidence (docs/evidence.md verdicts)
// ---------------------------------------------------------------------------

use cpm_planner::evidence::{EvidenceRecord, Identity, IdentityField, Scope, Verdict};
use cpm_planner::overview::evidence_text;

fn identity(lock: &str) -> Identity {
    Identity::new()
        .with(IdentityField::Tree, "tree:abc")
        .with(IdentityField::DependencyLock, lock)
        .with(IdentityField::Toolchain, "rustc 1.99.1")
        .with(IdentityField::TestContract, "nextest:overview@v1")
        .with(IdentityField::Environment, "linux-x86_64")
}

fn focused(lock: &str) -> EvidenceRecord {
    EvidenceRecord::new(Scope::Focused, "cargo nextest run --test overview: green")
        .with_identity(identity(lock))
}

/// Lease `id` and mark it `status` carrying `evidence`.
async fn mark_with(
    planner: &BasicCpmPlanner,
    plan_id: &PlanId,
    id: &str,
    status: DeliverableStatus,
    evidence: impl Into<cpm_planner::evidence::Evidence>,
) {
    lease(planner, plan_id, id).await;
    planner
        .mark_status(
            MarkStatusRequest::new(plan_id.clone(), id, worker(), status).with_evidence(evidence),
        )
        .await
        .unwrap();
}

async fn complete_contract_with(planner: &BasicCpmPlanner, plan_id: &PlanId, lock: &str) {
    mark_with(
        planner,
        plan_id,
        "contract",
        DeliverableStatus::Complete,
        focused(lock),
    )
    .await;
}

async fn targeted(
    planner: &BasicCpmPlanner,
    plan_id: &PlanId,
    target: Option<Identity>,
) -> Overview {
    let mut req = OverviewRequest::new(by_id(plan_id)).with_as_of(as_of());
    if let Some(target) = target {
        req = req.with_evidence_target(target);
    }
    overview(planner, req).await.unwrap()
}

fn contract_evidence(o: &Overview) -> cpm_planner::evidence::DeliverableEvidenceStatus {
    o.critical_path
        .items
        .iter()
        .find(|e| e.item.id == "contract")
        .unwrap()
        .item
        .evidence
        .clone()
        .unwrap()
}

/// Re-sync the export line with `contract`'s title edited: a metadata-only
/// definition change that keeps its status.
async fn retitle_contract(planner: &BasicCpmPlanner) {
    let mut g = graph();
    g.deliverables[0].metadata["title"] = json!("Export API contract v2");
    planner
        .sync_plan(SyncRequest::new(PROJECT, "export", "main", g))
        .await
        .unwrap();
}

#[tokio::test]
async fn evidence_matching_the_target_is_current() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    let o = targeted(&planner, &plan_id, Some(identity("sha256:l1"))).await;
    assert_eq!(contract_evidence(&o).verdict, Verdict::Current);
}

#[tokio::test]
async fn changed_dependency_lock_makes_evidence_stale_naming_the_field() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    let o = targeted(&planner, &plan_id, Some(identity("sha256:l2"))).await;
    let changed: Vec<(IdentityField, String, String)> = contract_evidence(&o)
        .record
        .unwrap()
        .changed
        .into_iter()
        .map(|c| (c.field, c.recorded, c.target))
        .collect();
    assert_eq!(
        changed,
        [(
            IdentityField::DependencyLock,
            "sha256:l1".to_string(),
            "sha256:l2".to_string()
        )]
    );
}

#[tokio::test]
async fn definition_change_after_recording_makes_evidence_stale() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    retitle_contract(&planner).await;
    let o = targeted(&planner, &plan_id, Some(identity("sha256:l1"))).await;
    let fields: Vec<IdentityField> = contract_evidence(&o)
        .record
        .unwrap()
        .changed
        .into_iter()
        .map(|c| c.field)
        .collect();
    assert_eq!(fields, [IdentityField::Definition]);
}

#[tokio::test]
async fn plain_note_evidence_is_unbound() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    mark_with(
        &planner,
        &plan_id,
        "contract",
        DeliverableStatus::Complete,
        "all green",
    )
    .await;
    let o = targeted(&planner, &plan_id, None).await;
    assert_eq!(contract_evidence(&o).verdict, Verdict::Unbound);
}

#[tokio::test]
async fn deliverable_without_evidence_reports_none() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let o = targeted(&planner, &plan_id, None).await;
    assert_eq!(contract_evidence(&o).verdict, Verdict::None);
}

#[tokio::test]
async fn without_a_target_identity_fields_are_unknown() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    let o = targeted(&planner, &plan_id, None).await;
    assert_eq!(
        contract_evidence(&o).record.unwrap().unknown,
        IdentityField::IDENTITY
    );
}

#[tokio::test]
async fn target_argument_is_reported_as_the_target_source() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    let o = targeted(&planner, &plan_id, Some(identity("sha256:l1"))).await;
    assert_eq!(
        contract_evidence(&o)
            .record
            .unwrap()
            .target_source
            .as_deref(),
        Some("argument")
    );
}

#[tokio::test]
async fn overview_evidence_agrees_with_planner_evidence_status() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    let target = identity("sha256:l2");
    let expected = planner
        .evidence_status(&plan_id, Some(&target))
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.deliverable_id == "contract")
        .unwrap();
    let o = targeted(&planner, &plan_id, Some(target)).await;
    assert_eq!(contract_evidence(&o), expected);
}

#[tokio::test]
async fn invalid_evidence_target_is_refused() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    let mut bad = identity("sha256:l1");
    bad.schema_version = 2;
    let err = overview(
        &planner,
        OverviewRequest::new(by_id(&plan_id)).with_evidence_target(bad),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, PlannerError::InvalidEvidence { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn stale_evidence_list_puts_complete_work_first_then_id() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    mark_with(
        &planner,
        &plan_id,
        "docs",
        DeliverableStatus::InProgress,
        focused("sha256:l1"),
    )
    .await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    let o = targeted(&planner, &plan_id, Some(identity("sha256:l2"))).await;
    let ids: Vec<&str> = o
        .stale_evidence
        .items
        .iter()
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(ids, ["contract", "docs"]);
}

#[tokio::test]
async fn evidence_counts_cover_the_whole_plan() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    let o = targeted(&planner, &plan_id, Some(identity("sha256:l2"))).await;
    assert_eq!(
        (o.evidence_counts.stale, o.evidence_counts.none),
        (1, o.counts.total - 1)
    );
}

#[tokio::test]
async fn evidence_text_names_changed_fields_in_contract_order() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    retitle_contract(&planner).await;
    let o = targeted(&planner, &plan_id, Some(identity("sha256:l2"))).await;
    assert_eq!(
        evidence_text(&contract_evidence(&o)).as_deref(),
        Some("evidence: stale (dependency_lock, definition)")
    );
}

#[tokio::test]
async fn markdown_rows_show_compact_evidence_text() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    let md = render_markdown(&targeted(&planner, &plan_id, Some(identity("sha256:l2"))).await);
    assert!(
        md.contains("Export API contract (`contract`) · evidence: stale (dependency_lock)"),
        "{md}"
    );
}

#[tokio::test]
async fn snapshot_carries_the_stored_evidence() {
    let planner = planner();
    let plan_id = synced(&planner).await;
    complete_contract_with(&planner, &plan_id, "sha256:l1").await;
    let snap = read_snapshot(&planner, &plan_id, as_of()).await.unwrap();
    assert_eq!(snap.evidence, planner.evidence(&plan_id).await.unwrap());
}
