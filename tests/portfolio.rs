//! Portfolio integration tests: named plan lines and variants (`sync_plan`,
//! `list_plans`, `revision_graph`), in-place revision (`revise_plan`), variant
//! selection, archiving and `VARIANT_NOT_SELECTED` execution gating.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::compare::CompareRequest;
use cpm_planner::edits::GraphEdit;
use cpm_planner::plan::{
    AcceptRequest, AcquireRequest, CallerId, ComparePlansRequest, Deliverable, DeliverableStatus,
    ForceReleaseRequest, ForkRequest, HeartbeatRequest, MarkStatusRequest, PlanGraph, PlanId,
    PlannerError, ReviseRequest, SyncRequest,
};
use cpm_planner::ports::Planner;
use cpm_planner::project::ProjectRoot;
use cpm_planner::{BasicCpmPlanner, SqlitePlanStore};

// ---------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------

const PROJECT: &str = "proj";

/// Unique temp db file, removed (with WAL sidecars) on drop.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cpm-planner-portfolio-test-{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        Self { path }
    }

    fn planner(&self) -> BasicCpmPlanner {
        let store = SqlitePlanStore::open(&self.path).expect("open store");
        BasicCpmPlanner::with_store(store, Arc::new(MemoryAuditSink::new()))
    }

    fn sql(&self, statement: &str) {
        rusqlite::Connection::open(&self.path)
            .unwrap()
            .execute_batch(statement)
            .unwrap();
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }
}

#[derive(Clone)]
struct TestClock {
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl TestClock {
    fn at(start: DateTime<Utc>) -> Self {
        Self {
            now: Arc::new(Mutex::new(start)),
        }
    }

    fn set(&self, when: DateTime<Utc>) {
        *self.now.lock().unwrap() = when;
    }
}

fn audited() -> (BasicCpmPlanner, MemoryAuditSink) {
    let sink = MemoryAuditSink::new();
    (BasicCpmPlanner::with_audit(Arc::new(sink.clone())), sink)
}

fn deliverable(id: &str, files: &[&str], prereqs: &[&str], effort: f32) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: files.iter().map(|f| (*f).into()).collect(),
        prerequisites: prereqs.iter().map(|p| (*p).into()).collect(),
        estimated_effort_hours: Some(effort),
        metadata: serde_json::Value::Null,
        duration_hours: None,
        estimate: None,
        milestone: false,
        earning_rule: None,
    }
}

fn graph(deliverables: Vec<Deliverable>) -> PlanGraph {
    PlanGraph {
        deliverables,
        max_chained_dispatch: None,
    }
}

/// `a(1) -> b(2)`.
fn chain() -> PlanGraph {
    graph(vec![
        deliverable("a", &["src/a.rs"], &[], 1.0),
        deliverable("b", &["src/b.rs"], &["a"], 2.0),
    ])
}

/// `a(1) -> b(2)` plus an independent `c(1)`.
fn chain_plus_c() -> PlanGraph {
    let mut g = chain();
    g.deliverables
        .push(deliverable("c", &["src/c.rs"], &[], 1.0));
    g
}

fn ids(g: &PlanGraph) -> Vec<String> {
    g.deliverables.iter().map(|d| d.id.clone()).collect()
}

fn sync_req(name: &str, variant: &str, g: PlanGraph) -> SyncRequest {
    SyncRequest::new(PROJECT, name, variant, g)
}

async fn sync(planner: &BasicCpmPlanner, name: &str, variant: &str, g: PlanGraph) -> PlanId {
    planner
        .sync_plan(sync_req(name, variant, g))
        .await
        .expect("sync")
        .plan_id
}

async fn acquire_a(planner: &BasicCpmPlanner, plan_id: &PlanId) {
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            CallerId("w1".into()),
            1,
        ))
        .await
        .expect("acquire");
    assert_eq!(cohort.rows[0].deliverable.id, "a", "harness: a is leased");
}

// ---------------------------------------------------------------------
// sync / list
// ---------------------------------------------------------------------

#[tokio::test]
async fn sync_creates_plan_and_first_variant_is_selected() {
    let planner = BasicCpmPlanner::new();
    sync(&planner, "web", "main", chain()).await;
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    assert_eq!(lines[0].selected_variant.as_deref(), Some("main"));
}

#[tokio::test]
async fn sync_of_new_variant_reports_created() {
    let planner = BasicCpmPlanner::new();
    let out = planner
        .sync_plan(sync_req("web", "main", chain()))
        .await
        .unwrap();
    assert!(out.created);
}

#[tokio::test]
async fn sync_same_content_is_unchanged() {
    let planner = BasicCpmPlanner::new();
    sync(&planner, "web", "main", chain()).await;
    let again = planner
        .sync_plan(sync_req("web", "main", chain()))
        .await
        .unwrap();
    assert!(!again.changed);
}

#[tokio::test]
async fn sync_same_content_keeps_the_plan_id() {
    let planner = BasicCpmPlanner::new();
    let first = sync(&planner, "web", "main", chain()).await;
    let again = sync(&planner, "web", "main", chain()).await;
    assert_eq!(first, again);
}

#[tokio::test]
async fn sync_same_content_hash_is_unchanged() {
    let planner = BasicCpmPlanner::new();
    let req = || sync_req("web", "main", chain()).with_content_hash("h1");
    planner.sync_plan(req()).await.unwrap();
    let again = planner.sync_plan(req()).await.unwrap();
    assert_eq!(again.revision, 1);
}

#[tokio::test]
async fn sync_changed_content_creates_next_revision() {
    let planner = BasicCpmPlanner::new();
    sync(&planner, "web", "main", chain()).await;
    let out = planner
        .sync_plan(sync_req("web", "main", chain_plus_c()))
        .await
        .unwrap();
    assert_eq!((out.changed, out.revision), (true, 2));
}

#[tokio::test]
async fn sync_changed_content_reports_diff() {
    let planner = BasicCpmPlanner::new();
    sync(&planner, "web", "main", chain()).await;
    let out = planner
        .sync_plan(sync_req("web", "main", chain_plus_c()))
        .await
        .unwrap();
    assert_eq!(out.diff.unwrap().added, vec!["c".to_string()]);
}

#[tokio::test]
async fn sync_changed_content_revises_in_place() {
    let planner = BasicCpmPlanner::new();
    let first = sync(&planner, "web", "main", chain()).await;
    let second = sync(&planner, "web", "main", chain_plus_c()).await;
    assert_eq!(first, second);
}

#[tokio::test]
async fn sync_rejects_invalid_graph() {
    let planner = BasicCpmPlanner::new();
    let bad = graph(vec![deliverable("a", &[], &["missing"], 1.0)]);
    let err = planner
        .sync_plan(sync_req("web", "main", bad))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidGraph { .. }));
}

#[tokio::test]
async fn sync_emits_portfolio_created_event() {
    let (planner, sink) = audited();
    sync(&planner, "web", "main", chain()).await;
    assert!(
        sink.event_types()
            .contains(&"plan.portfolio.created".to_string())
    );
}

#[tokio::test]
async fn second_variant_is_a_draft() {
    let planner = BasicCpmPlanner::new();
    sync(&planner, "web", "main", chain()).await;
    let alt = planner
        .sync_plan(sync_req("web", "alt", chain_plus_c()))
        .await
        .unwrap();
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    let alt_row = lines[0]
        .variants
        .iter()
        .find(|v| v.plan_id == alt.plan_id)
        .unwrap();
    assert!(!alt_row.selected);
}

#[tokio::test]
async fn list_reports_variants_with_selected_flag() {
    let planner = BasicCpmPlanner::new();
    sync(&planner, "web", "main", chain()).await;
    sync(&planner, "web", "alt", chain_plus_c()).await;
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    let flags: Vec<(String, bool)> = lines[0]
        .variants
        .iter()
        .map(|v| (v.variant.clone(), v.selected))
        .collect();
    assert_eq!(
        flags,
        vec![("alt".to_string(), false), ("main".to_string(), true)]
    );
}

#[tokio::test]
async fn list_reports_progress_and_makespan() {
    let planner = BasicCpmPlanner::new();
    let plan_id = sync(&planner, "web", "main", chain()).await;
    acquire_a(&planner, &plan_id).await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id,
            "a",
            CallerId("w1".into()),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    let v = &lines[0].variants[0];
    assert_eq!(
        (v.complete, v.total, v.plan_complete, v.makespan),
        (1, 2, false, 3.0)
    );
}

#[tokio::test]
async fn list_is_scoped_to_the_project() {
    let planner = BasicCpmPlanner::new();
    sync(&planner, "web", "main", chain()).await;
    let lines = planner.list_plans("other", false).await.unwrap();
    assert!(lines.is_empty());
}

#[tokio::test]
async fn list_orders_lines_by_name() {
    let planner = BasicCpmPlanner::new();
    sync(&planner, "web", "main", chain()).await;
    sync(&planner, "api", "main", chain_plus_c()).await;
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    let names: Vec<&str> = lines.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, vec!["api", "web"]);
}

#[tokio::test]
async fn list_omits_unnamed_plans() {
    let planner = BasicCpmPlanner::new();
    planner.submit_plan(chain()).await.unwrap();
    let lines = planner.list_plans(PROJECT, true).await.unwrap();
    assert!(lines.is_empty());
}

#[tokio::test]
async fn list_hides_archived_by_default() {
    let db = TempDb::new();
    let planner = db.planner();
    sync(&planner, "web", "main", chain()).await;
    sync(&planner, "web", "alt", chain_plus_c()).await;
    db.sql("UPDATE variants SET archived = 1 WHERE variant = 'alt'");
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    let variants: Vec<&str> = lines[0]
        .variants
        .iter()
        .map(|v| v.variant.as_str())
        .collect();
    assert_eq!(variants, vec!["main"]);
}

#[tokio::test]
async fn list_hides_archived_lines_by_default() {
    let db = TempDb::new();
    let planner = db.planner();
    sync(&planner, "web", "main", chain()).await;
    db.sql("UPDATE plan_lines SET archived = 1 WHERE name = 'web'");
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    assert!(lines.is_empty());
}

#[tokio::test]
async fn list_includes_archived_on_request() {
    let db = TempDb::new();
    let planner = db.planner();
    sync(&planner, "web", "main", chain()).await;
    sync(&planner, "web", "alt", chain_plus_c()).await;
    db.sql("UPDATE variants SET archived = 1 WHERE variant = 'alt'");
    let lines = planner.list_plans(PROJECT, true).await.unwrap();
    assert_eq!(lines[0].variants.len(), 2);
}

#[tokio::test]
async fn list_reports_source_path() {
    let planner = BasicCpmPlanner::new();
    planner
        .sync_plan(
            sync_req("web", "main", chain()).with_source_path(".cpm-planner/plans/web/main.json"),
        )
        .await
        .unwrap();
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    assert_eq!(
        lines[0].variants[0].source_path.as_deref(),
        Some(".cpm-planner/plans/web/main.json")
    );
}

// ---------------------------------------------------------------------
// Dedup scoping
// ---------------------------------------------------------------------

#[tokio::test]
async fn named_submit_dedups_within_variant_only() {
    let planner = BasicCpmPlanner::new();
    let web = sync(&planner, "web", "main", chain()).await;
    let api = sync(&planner, "api", "main", chain()).await;
    assert_ne!(web, api);
}

#[tokio::test]
async fn named_submit_does_not_reuse_unnamed_plan() {
    let planner = BasicCpmPlanner::new();
    let unnamed = planner.submit_plan(chain()).await.unwrap();
    let named = sync(&planner, "web", "main", chain()).await;
    assert_ne!(unnamed, named);
}

#[tokio::test]
async fn unnamed_submit_keeps_global_dedup() {
    let planner = BasicCpmPlanner::new();
    let first = planner.submit_plan(chain()).await.unwrap();
    sync(&planner, "web", "main", chain()).await;
    let again = planner.submit_plan(chain()).await.unwrap();
    assert_eq!(first, again);
}

#[tokio::test]
async fn unnamed_submit_does_not_reuse_named_plan() {
    let planner = BasicCpmPlanner::new();
    let named = sync(&planner, "web", "main", chain()).await;
    let unnamed = planner.submit_plan(chain()).await.unwrap();
    assert_ne!(named, unnamed);
}

#[tokio::test]
async fn unnamed_submit_of_superseded_graph_creates_a_new_plan() {
    let planner = BasicCpmPlanner::new();
    let original = planner.submit_plan(chain()).await.unwrap();
    planner
        .revise_plan(ReviseRequest::new(original.clone(), chain_plus_c()))
        .await
        .unwrap();
    let again = planner.submit_plan(chain()).await.unwrap();
    assert_ne!(original, again);
}

#[tokio::test]
async fn unnamed_submit_of_revised_graph_dedups_to_revised_plan() {
    let planner = BasicCpmPlanner::new();
    let original = planner.submit_plan(chain()).await.unwrap();
    planner
        .revise_plan(ReviseRequest::new(original.clone(), chain_plus_c()))
        .await
        .unwrap();
    let again = planner.submit_plan(chain_plus_c()).await.unwrap();
    assert_eq!(original, again);
}

// ---------------------------------------------------------------------
// revision_graph
// ---------------------------------------------------------------------

#[tokio::test]
async fn revision_graph_returns_head() {
    let planner = BasicCpmPlanner::new();
    let plan_id = sync(&planner, "web", "main", chain()).await;
    sync(&planner, "web", "main", chain_plus_c()).await;
    let (revision, head) = planner.revision_graph(&plan_id, None).await.unwrap();
    assert_eq!((revision, ids(&head)), (2, ids(&chain_plus_c())));
}

#[tokio::test]
async fn revision_graph_returns_requested_revision() {
    let planner = BasicCpmPlanner::new();
    let plan_id = sync(&planner, "web", "main", chain()).await;
    sync(&planner, "web", "main", chain_plus_c()).await;
    let (_, first) = planner.revision_graph(&plan_id, Some(1)).await.unwrap();
    assert_eq!(ids(&first), ids(&chain()));
}

#[tokio::test]
async fn revision_graph_of_unrevised_legacy_plan_is_revision_one() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(chain()).await.unwrap();
    let (revision, _) = planner.revision_graph(&plan_id, None).await.unwrap();
    assert_eq!(revision, 1);
}

#[tokio::test]
async fn revision_graph_of_unknown_revision_is_plan_not_found() {
    let planner = BasicCpmPlanner::new();
    let plan_id = sync(&planner, "web", "main", chain()).await;
    let err = planner.revision_graph(&plan_id, Some(7)).await.unwrap_err();
    assert!(matches!(err, PlannerError::PlanNotFound { .. }));
}

#[tokio::test]
async fn revision_graph_of_unknown_plan_is_plan_not_found() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .revision_graph(&PlanId("plan_missing".into()), None)
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::PlanNotFound { .. }));
}

// ---------------------------------------------------------------------
// revise_plan (store wiring)
// ---------------------------------------------------------------------

#[tokio::test]
async fn leased_deliverable_survives_revision_and_can_complete() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(chain()).await.unwrap();
    acquire_a(&planner, &plan_id).await;
    planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), chain_plus_c()))
        .await
        .unwrap();
    let done = planner
        .mark_status(MarkStatusRequest::new(
            plan_id,
            "a",
            CallerId("w1".into()),
            DeliverableStatus::Complete,
        ))
        .await;
    assert!(done.is_ok());
}

#[tokio::test]
async fn revision_number_increments() {
    let planner = BasicCpmPlanner::new();
    let plan_id = sync(&planner, "web", "main", chain()).await;
    planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), chain_plus_c()))
        .await
        .unwrap();
    let (revision, _) = planner
        .revise_plan(ReviseRequest::new(plan_id, chain()))
        .await
        .unwrap();
    assert_eq!(revision, 3);
}

#[tokio::test]
async fn first_revision_of_legacy_plan_is_revision_two() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(chain()).await.unwrap();
    let (revision, _) = planner
        .revise_plan(ReviseRequest::new(plan_id, chain_plus_c()))
        .await
        .unwrap();
    assert_eq!(revision, 2);
}

#[tokio::test]
async fn legacy_revision_backfills_original_graph_as_revision_one() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(chain()).await.unwrap();
    planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), chain_plus_c()))
        .await
        .unwrap();
    let (_, first) = planner.revision_graph(&plan_id, Some(1)).await.unwrap();
    assert_eq!(ids(&first), ids(&chain()));
}

#[tokio::test]
async fn revise_updates_variant_head_revision() {
    let planner = BasicCpmPlanner::new();
    let plan_id = sync(&planner, "web", "main", chain()).await;
    planner
        .revise_plan(ReviseRequest::new(plan_id, chain_plus_c()))
        .await
        .unwrap();
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    assert_eq!(lines[0].variants[0].head_revision, 2);
}

#[tokio::test]
async fn revise_recomputes_critical_path() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(chain()).await.unwrap();
    let longer_c = graph(vec![
        deliverable("a", &["src/a.rs"], &[], 1.0),
        deliverable("b", &["src/b.rs"], &["a"], 2.0),
        deliverable("c", &["src/c.rs"], &[], 9.0),
    ]);
    planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), longer_c))
        .await
        .unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.critical_path, vec!["__start__", "c", "__finish__"]);
}

#[tokio::test]
async fn revise_replaces_stored_graph() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(chain()).await.unwrap();
    planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), chain_plus_c()))
        .await
        .unwrap();
    let def = planner.get_plan(&plan_id).await.unwrap();
    assert_eq!(ids(&def.graph), ids(&chain_plus_c()));
}

#[tokio::test]
async fn revise_drops_removed_deliverable_status() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(chain_plus_c()).await.unwrap();
    planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), chain()))
        .await
        .unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    let listed: Vec<String> = status.deliverables.into_iter().map(|r| r.0).collect();
    assert_eq!(listed, ids(&chain()));
}

#[tokio::test]
async fn revise_drops_removed_deliverable_status_row() {
    let db = TempDb::new();
    let planner = db.planner();
    let plan_id = planner.submit_plan(chain_plus_c()).await.unwrap();
    planner
        .revise_plan(ReviseRequest::new(plan_id, chain()))
        .await
        .unwrap();
    let rows: i64 = rusqlite::Connection::open(&db.path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM deliverable_statuses WHERE deliverable_id = 'c'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn revise_emits_portfolio_revised_event() {
    let (planner, sink) = audited();
    let plan_id = planner.submit_plan(chain()).await.unwrap();
    planner
        .revise_plan(ReviseRequest::new(plan_id, chain_plus_c()))
        .await
        .unwrap();
    assert!(
        sink.event_types()
            .contains(&"plan.portfolio.revised".to_string())
    );
}

#[tokio::test]
async fn revised_event_carries_revision_and_diff() {
    let (planner, sink) = audited();
    let plan_id = planner.submit_plan(chain()).await.unwrap();
    planner
        .revise_plan(ReviseRequest::new(plan_id, chain_plus_c()))
        .await
        .unwrap();
    let event = sink
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.portfolio.revised")
        .unwrap();
    assert_eq!(
        (&event.payload["revision"], &event.payload["diff"]["added"]),
        (&serde_json::json!(2), &serde_json::json!(["c"]))
    );
}

#[tokio::test]
async fn revise_removing_leased_deliverable_without_force_is_lock_held() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(chain_plus_c()).await.unwrap();
    acquire_a(&planner, &plan_id).await;
    let without_a = graph(vec![deliverable("c", &["src/c.rs"], &[], 1.0)]);
    let err = planner
        .revise_plan(ReviseRequest::new(plan_id, without_a))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::LockHeld { .. }));
}

#[tokio::test]
async fn refused_revision_leaves_the_plan_unchanged() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(chain_plus_c()).await.unwrap();
    acquire_a(&planner, &plan_id).await;
    let without_a = graph(vec![deliverable("c", &["src/c.rs"], &[], 1.0)]);
    let _ = planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), without_a))
        .await;
    let (revision, head) = planner.revision_graph(&plan_id, None).await.unwrap();
    assert_eq!((revision, ids(&head)), (1, ids(&chain_plus_c())));
}

#[tokio::test]
async fn forced_revision_releases_lock_and_audits() {
    let (planner, sink) = audited();
    let plan_id = planner.submit_plan(chain_plus_c()).await.unwrap();
    acquire_a(&planner, &plan_id).await;
    let without_a = graph(vec![deliverable("c", &["src/c.rs"], &[], 1.0)]);
    planner
        .revise_plan(ReviseRequest::new(plan_id, without_a).force(true))
        .await
        .unwrap();
    assert!(
        sink.event_types()
            .contains(&"plan.lock.released".to_string())
    );
}

#[tokio::test]
async fn forced_revision_frees_the_released_lock() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(chain_plus_c()).await.unwrap();
    acquire_a(&planner, &plan_id).await;
    let without_a = graph(vec![deliverable("c", &["src/c.rs"], &[], 1.0)]);
    planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), without_a).force(true))
        .await
        .unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    assert!(status.locks_held.is_empty());
}

#[tokio::test]
async fn revise_reaps_expired_lock_first() {
    let sink = MemoryAuditSink::new();
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::at(start);
    let reader = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        Arc::new(sink.clone()),
        Duration::from_secs(60),
        Arc::new(move || *reader.now.lock().unwrap()),
    );
    let plan_id = planner.submit_plan(chain()).await.unwrap();
    acquire_a(&planner, &plan_id).await;
    clock.set(start + chrono::Duration::seconds(120));
    planner
        .revise_plan(ReviseRequest::new(plan_id, chain_plus_c()))
        .await
        .unwrap();
    assert!(
        sink.event_types()
            .contains(&"plan.lock.expired".to_string())
    );
}

#[tokio::test]
async fn revise_of_unknown_plan_is_plan_not_found() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .revise_plan(ReviseRequest::new(PlanId("plan_missing".into()), chain()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::PlanNotFound { .. }));
}

#[tokio::test]
async fn revision_rows_survive_reopen() {
    let db = TempDb::new();
    let plan_id = sync(&db.planner(), "web", "main", chain()).await;
    db.planner()
        .revise_plan(ReviseRequest::new(plan_id.clone(), chain_plus_c()))
        .await
        .unwrap();
    let (revision, _) = db.planner().revision_graph(&plan_id, None).await.unwrap();
    assert_eq!(revision, 2);
}

// ---------------------------------------------------------------------
// Fix round 1
// ---------------------------------------------------------------------

#[tokio::test]
async fn sync_rejects_invalid_name_slug() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .sync_plan(sync_req("Web Plan", "main", chain()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

#[tokio::test]
async fn sync_rejects_invalid_variant_slug() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .sync_plan(sync_req("web", "../main", chain()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

#[tokio::test]
async fn sync_rejects_empty_project() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .sync_plan(SyncRequest::new("", "web", "main", chain()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

#[tokio::test]
async fn sync_rejects_overlong_project() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .sync_plan(SyncRequest::new("p".repeat(513), "web", "main", chain()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

#[tokio::test]
async fn identical_revise_is_a_no_op() {
    let (planner, sink) = audited();
    let plan_id = sync(&planner, "web", "main", chain()).await;
    sink.clear();
    let out = planner
        .revise_plan(ReviseRequest::new(plan_id, chain()))
        .await
        .unwrap();
    assert_eq!(
        (out.0, out.1, sink.event_types().len()),
        (1, cpm_planner::revise::RevisionDiff::default(), 0)
    );
}

#[tokio::test]
async fn revising_unnamed_plan_onto_another_plans_hash_keeps_both_reachable() {
    let planner = BasicCpmPlanner::new();
    let first = planner.submit_plan(chain()).await.unwrap();
    let second = planner.submit_plan(chain_plus_c()).await.unwrap();
    planner
        .revise_plan(ReviseRequest::new(first.clone(), chain_plus_c()))
        .await
        .unwrap();
    let dedup = planner.submit_plan(chain_plus_c()).await.unwrap();
    let first_reachable = planner.status(&first).await.is_ok();
    assert_eq!((dedup, first_reachable), (second, true));
}

#[tokio::test]
async fn revising_named_plan_never_enters_global_dedup() {
    let planner = BasicCpmPlanner::new();
    let named = sync(&planner, "web", "main", chain()).await;
    planner
        .revise_plan(ReviseRequest::new(named.clone(), chain_plus_c()))
        .await
        .unwrap();
    let unnamed = planner.submit_plan(chain_plus_c()).await.unwrap();
    assert_ne!(named, unnamed);
}

#[tokio::test]
async fn revise_that_completes_the_plan_emits_plan_completed() {
    let (planner, sink) = audited();
    let a_and_c = graph(vec![
        deliverable("a", &["src/a.rs"], &[], 1.0),
        deliverable("c", &["src/c.rs"], &[], 1.0),
    ]);
    let plan_id = planner.submit_plan(a_and_c).await.unwrap();
    acquire_a(&planner, &plan_id).await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            CallerId("w1".into()),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    let only_a = graph(vec![deliverable("a", &["src/a.rs"], &[], 1.0)]);
    planner
        .revise_plan(ReviseRequest::new(plan_id, only_a))
        .await
        .unwrap();
    assert!(sink.event_types().contains(&"plan.completed".to_string()));
}

#[tokio::test]
async fn refused_revision_emits_no_audit_events() {
    let (planner, sink) = audited();
    let plan_id = planner.submit_plan(chain_plus_c()).await.unwrap();
    acquire_a(&planner, &plan_id).await;
    sink.clear();
    let without_a = graph(vec![deliverable("c", &["src/c.rs"], &[], 1.0)]);
    let _ = planner
        .revise_plan(ReviseRequest::new(plan_id, without_a))
        .await;
    assert!(sink.event_types().is_empty());
}

#[tokio::test]
async fn unnamed_plan_without_dedup_row_regains_it_on_revise() {
    let planner = BasicCpmPlanner::new();
    let first = planner.submit_plan(chain()).await.unwrap();
    planner.submit_plan(chain_plus_c()).await.unwrap();
    // `first` loses its dedup row: the other plan owns this hash.
    planner
        .revise_plan(ReviseRequest::new(first.clone(), chain_plus_c()))
        .await
        .unwrap();
    let only_c = graph(vec![deliverable("c", &["src/c.rs"], &[], 1.0)]);
    planner
        .revise_plan(ReviseRequest::new(first.clone(), only_c.clone()))
        .await
        .unwrap();
    let dedup = planner.submit_plan(only_c).await.unwrap();
    assert_eq!(dedup, first);
}

// ---------------------------------------------------------------------
// select_variant / archive / VARIANT_NOT_SELECTED gating
// ---------------------------------------------------------------------

async fn complete_a(planner: &BasicCpmPlanner, plan_id: &PlanId) {
    acquire_a(planner, plan_id).await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            CallerId("w1".into()),
            DeliverableStatus::Complete,
        ))
        .await
        .expect("harness: complete a");
}

async fn status_of(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str) -> DeliverableStatus {
    planner
        .status(plan_id)
        .await
        .unwrap()
        .deliverables
        .into_iter()
        .find(|row| row.0 == id)
        .map(|row| row.1)
        .expect("harness: deliverable present")
}

/// `main` (selected, `chain`) and draft `alt` (`chain_plus_c`) of line `web`.
async fn main_and_alt(planner: &BasicCpmPlanner) -> (PlanId, PlanId) {
    let main = sync(planner, "web", "main", chain()).await;
    let alt = sync(planner, "web", "alt", chain_plus_c()).await;
    (main, alt)
}

fn acquire_req(plan_id: &PlanId) -> AcquireRequest {
    AcquireRequest::new(plan_id.clone(), CallerId("w1".into()), 1)
}

#[tokio::test]
async fn acquire_on_draft_variant_is_variant_not_selected() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    let err = planner.acquire_cohort(acquire_req(&alt)).await.unwrap_err();
    assert!(matches!(err, PlannerError::VariantNotSelected { .. }));
}

#[tokio::test]
async fn variant_not_selected_message_names_variant_and_selection() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    let err = planner.acquire_cohort(acquire_req(&alt)).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "VARIANT_NOT_SELECTED: plan {} is variant 'alt' of 'web'; selected is 'main'",
            alt.as_str()
        )
    );
}

#[tokio::test]
async fn mark_status_on_draft_variant_is_variant_not_selected() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    let err = planner
        .mark_status(MarkStatusRequest::new(
            alt,
            "a",
            CallerId("w1".into()),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::VariantNotSelected { .. }));
}

#[tokio::test]
async fn heartbeat_on_draft_variant_is_variant_not_selected() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    let err = planner
        .heartbeat(HeartbeatRequest::new(alt, "a", CallerId("w1".into())))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::VariantNotSelected { .. }));
}

#[tokio::test]
async fn accept_on_draft_variant_is_variant_not_selected() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    let err = planner
        .accept(AcceptRequest::new(alt, "a", "owner", "looks good"))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::VariantNotSelected { .. }));
}

#[tokio::test]
async fn force_release_on_draft_variant_is_variant_not_selected() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    let err = planner
        .force_release(ForceReleaseRequest::new(alt, "a", "ops"))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::VariantNotSelected { .. }));
}

#[tokio::test]
async fn status_of_draft_variant_is_not_gated() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    assert!(planner.status(&alt).await.is_ok());
}

#[tokio::test]
async fn acquire_on_selected_variant_succeeds() {
    let planner = BasicCpmPlanner::new();
    let (main, _) = main_and_alt(&planner).await;
    assert!(planner.acquire_cohort(acquire_req(&main)).await.is_ok());
}

#[tokio::test]
async fn unnamed_plans_are_never_gated() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    let unnamed = planner.submit_plan(chain_plus_c()).await.unwrap();
    assert!(planner.acquire_cohort(acquire_req(&unnamed)).await.is_ok());
}

#[tokio::test]
async fn acquire_after_select_succeeds() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    planner.select_variant(&alt, false).await.unwrap();
    assert!(planner.acquire_cohort(acquire_req(&alt)).await.is_ok());
}

#[tokio::test]
async fn select_gates_the_previously_selected_variant() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    planner.select_variant(&alt, false).await.unwrap();
    let err = planner
        .acquire_cohort(acquire_req(&main))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::VariantNotSelected { .. }));
}

#[tokio::test]
async fn select_marks_variant_selected_in_list() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    planner.select_variant(&alt, false).await.unwrap();
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    assert_eq!(lines[0].selected_variant.as_deref(), Some("alt"));
}

#[tokio::test]
async fn select_carries_complete_status_for_identical_deliverables() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    complete_a(&planner, &main).await;
    planner.select_variant(&alt, false).await.unwrap();
    assert_eq!(
        status_of(&planner, &alt, "a").await,
        DeliverableStatus::Complete
    );
}

#[tokio::test]
async fn select_rederives_dependents_of_carried_deliverables() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    complete_a(&planner, &main).await;
    planner.select_variant(&alt, false).await.unwrap();
    assert_eq!(
        status_of(&planner, &alt, "b").await,
        DeliverableStatus::Ready
    );
}

#[tokio::test]
async fn select_carries_counters_for_identical_deliverables() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    complete_a(&planner, &main).await;
    planner.select_variant(&alt, false).await.unwrap();
    let rows = planner.status(&alt).await.unwrap().deliverables;
    let attempts = rows.iter().find(|r| r.0 == "a").map(|r| r.2);
    assert_eq!(attempts, Some(1));
}

#[tokio::test]
async fn select_reports_carried_ids() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    complete_a(&planner, &main).await;
    let out = planner.select_variant(&alt, false).await.unwrap();
    assert_eq!(out.carried, vec!["a".to_string()]);
}

#[tokio::test]
async fn select_does_not_carry_status_for_changed_deliverable() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    let changed = graph(vec![
        deliverable("a", &["src/a.rs"], &[], 5.0),
        deliverable("b", &["src/b.rs"], &["a"], 2.0),
    ]);
    let alt = sync(&planner, "web", "alt", changed).await;
    complete_a(&planner, &main).await;
    planner.select_variant(&alt, false).await.unwrap();
    assert_eq!(
        status_of(&planner, &alt, "a").await,
        DeliverableStatus::Ready
    );
}

#[tokio::test]
async fn select_refuses_when_old_variant_holds_locks() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    let err = planner.select_variant(&alt, false).await.unwrap_err();
    assert!(matches!(err, PlannerError::LockHeld { .. }));
}

#[tokio::test]
async fn refused_select_keeps_the_selection() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    let _ = planner.select_variant(&alt, false).await;
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    assert_eq!(lines[0].selected_variant.as_deref(), Some("main"));
}

#[tokio::test]
async fn select_with_force_releases_old_locks() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    planner.select_variant(&alt, true).await.unwrap();
    assert!(planner.status(&main).await.unwrap().locks_held.is_empty());
}

#[tokio::test]
async fn forced_select_reports_released_locks() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    let out = planner.select_variant(&alt, true).await.unwrap();
    assert_eq!(out.released_locks, vec!["a".to_string()]);
}

#[tokio::test]
async fn forced_select_emits_released_event() {
    let (planner, sink) = audited();
    let (main, alt) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    planner.select_variant(&alt, true).await.unwrap();
    assert!(
        sink.event_types()
            .contains(&"plan.lock.released".to_string())
    );
}

#[tokio::test]
async fn forced_select_returns_released_deliverable_to_ready() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    planner.select_variant(&alt, true).await.unwrap();
    assert_eq!(
        status_of(&planner, &main, "a").await,
        DeliverableStatus::Ready
    );
}

#[tokio::test]
async fn select_reaps_expired_old_locks_instead_of_refusing() {
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::at(start);
    let reader = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        Arc::new(MemoryAuditSink::new()),
        Duration::from_secs(60),
        Arc::new(move || *reader.now.lock().unwrap()),
    );
    let (main, alt) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    clock.set(start + chrono::Duration::seconds(120));
    assert!(planner.select_variant(&alt, false).await.is_ok());
}

#[tokio::test]
async fn select_counts_reaped_lock_as_a_lapse() {
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::at(start);
    let reader = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        Arc::new(MemoryAuditSink::new()),
        Duration::from_secs(60),
        Arc::new(move || *reader.now.lock().unwrap()),
    );
    let (main, alt) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    clock.set(start + chrono::Duration::seconds(120));
    planner.select_variant(&alt, false).await.unwrap();
    let rows = planner.status(&main).await.unwrap().deliverables;
    let lapses = rows.iter().find(|r| r.0 == "a").map(|r| r.4);
    assert_eq!(lapses, Some(1));
}

#[tokio::test]
async fn select_emits_portfolio_selected_event() {
    let (planner, sink) = audited();
    let (_, alt) = main_and_alt(&planner).await;
    planner.select_variant(&alt, false).await.unwrap();
    assert!(
        sink.event_types()
            .contains(&"plan.portfolio.selected".to_string())
    );
}

#[tokio::test]
async fn selected_event_names_from_and_to() {
    let (planner, sink) = audited();
    let (_, alt) = main_and_alt(&planner).await;
    planner.select_variant(&alt, false).await.unwrap();
    let event = sink
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.portfolio.selected")
        .unwrap();
    assert_eq!(
        (&event.payload["from"], &event.payload["to"]),
        (&serde_json::json!("main"), &serde_json::json!("alt"))
    );
}

#[tokio::test]
async fn selecting_the_selected_variant_is_a_no_op() {
    let (planner, sink) = audited();
    let (main, _) = main_and_alt(&planner).await;
    planner.select_variant(&main, false).await.unwrap();
    assert!(
        !sink
            .event_types()
            .contains(&"plan.portfolio.selected".to_string())
    );
}

#[tokio::test]
async fn selecting_the_selected_variant_keeps_its_locks() {
    let planner = BasicCpmPlanner::new();
    let (main, _) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    planner.select_variant(&main, true).await.unwrap();
    assert_eq!(planner.status(&main).await.unwrap().locks_held.len(), 1);
}

#[tokio::test]
async fn select_of_unnamed_plan_is_plan_not_found() {
    let planner = BasicCpmPlanner::new();
    let unnamed = planner.submit_plan(chain()).await.unwrap();
    let err = planner.select_variant(&unnamed, false).await.unwrap_err();
    assert!(matches!(err, PlannerError::PlanNotFound { .. }));
}

#[tokio::test]
async fn archive_hides_variant_from_list() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    let variants: Vec<&str> = lines[0]
        .variants
        .iter()
        .map(|v| v.variant.as_str())
        .collect();
    assert_eq!(variants, vec!["main"]);
}

#[tokio::test]
async fn archived_variant_is_listed_on_request() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    let lines = planner.list_plans(PROJECT, true).await.unwrap();
    assert!(lines[0].variants.iter().any(|v| v.archived));
}

#[tokio::test]
async fn archived_variant_stays_readable() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    assert!(planner.get_plan(&alt).await.is_ok());
}

#[tokio::test]
async fn archiving_selected_variant_alone_is_refused() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    let err = planner
        .archive(PROJECT, "web", Some("main"), true, false)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "ARCHIVE_REFUSED: cannot archive the selected variant 'main' of 'web'; select another \
         variant first"
    );
}

#[tokio::test]
async fn archiving_a_line_hides_it_from_list() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    assert!(planner.list_plans(PROJECT, false).await.unwrap().is_empty());
}

#[tokio::test]
async fn line_archive_hides_all_variants_from_list() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    let lines = planner.list_plans(PROJECT, true).await.unwrap();
    assert!(lines[0].variants.iter().all(|v| v.archived));
}

#[tokio::test]
async fn archive_of_unknown_variant_is_plan_not_found() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    let err = planner
        .archive(PROJECT, "web", Some("nope"), true, false)
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::PlanNotFound { .. }));
}

#[tokio::test]
async fn archive_of_unknown_line_is_plan_not_found() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .archive(PROJECT, "nope", None, true, false)
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::PlanNotFound { .. }));
}

#[tokio::test]
async fn archive_emits_portfolio_archived_event() {
    let (planner, sink) = audited();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    assert!(
        sink.event_types()
            .contains(&"plan.portfolio.archived".to_string())
    );
}

#[tokio::test]
async fn sync_into_archived_variant_is_refused() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    let err = planner
        .sync_plan(sync_req("web", "alt", chain()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::ArchiveRefused { .. }));
}

#[tokio::test]
async fn sync_new_variant_into_archived_line_is_refused() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    let err = planner
        .sync_plan(sync_req("web", "third", chain()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::ArchiveRefused { .. }));
}

#[tokio::test]
async fn select_of_archived_variant_is_refused() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    let err = planner.select_variant(&alt, false).await.unwrap_err();
    assert!(matches!(err, PlannerError::ArchiveRefused { .. }));
}

#[tokio::test]
async fn execution_on_archived_line_is_refused() {
    let planner = BasicCpmPlanner::new();
    let (main, _) = main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    let err = planner
        .acquire_cohort(acquire_req(&main))
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "ARCHIVE_REFUSED: 'web' is archived; unarchive it to resume execution"
    );
}

#[tokio::test]
async fn archiving_line_with_live_lock_is_refused() {
    let planner = BasicCpmPlanner::new();
    let (main, _) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    let err = planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::LockHeld { .. }));
}

#[tokio::test]
async fn refused_line_archive_leaves_line_listed() {
    let planner = BasicCpmPlanner::new();
    let (main, _) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    let _ = planner.archive(PROJECT, "web", None, true, false).await;
    assert_eq!(planner.list_plans(PROJECT, false).await.unwrap().len(), 1);
}

#[tokio::test]
async fn archiving_line_with_expired_lock_succeeds() {
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::at(start);
    let reader = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        Arc::new(MemoryAuditSink::new()),
        Duration::from_secs(60),
        Arc::new(move || *reader.now.lock().unwrap()),
    );
    let (main, _) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    clock.set(start + chrono::Duration::seconds(120));
    assert!(
        planner
            .archive(PROJECT, "web", None, true, false)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn forced_archive_releases_locks() {
    let planner = BasicCpmPlanner::new();
    let (main, _) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    planner
        .archive(PROJECT, "web", None, true, true)
        .await
        .unwrap();
    assert!(planner.status(&main).await.unwrap().locks_held.is_empty());
}

#[tokio::test]
async fn forced_archive_emits_released_event() {
    let (planner, sink) = audited();
    let (main, _) = main_and_alt(&planner).await;
    acquire_a(&planner, &main).await;
    planner
        .archive(PROJECT, "web", None, true, true)
        .await
        .unwrap();
    assert!(
        sink.event_types()
            .contains(&"plan.lock.released".to_string())
    );
}

#[tokio::test]
async fn unarchive_restores_execution() {
    let planner = BasicCpmPlanner::new();
    let (main, _) = main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    planner
        .archive(PROJECT, "web", None, false, false)
        .await
        .unwrap();
    assert!(planner.acquire_cohort(acquire_req(&main)).await.is_ok());
}

#[tokio::test]
async fn unarchive_line_lists_its_variants_again() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    planner
        .archive(PROJECT, "web", None, false, false)
        .await
        .unwrap();
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    assert_eq!(lines[0].variants.len(), 2);
}

#[tokio::test]
async fn unarchive_variant_makes_it_selectable() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    planner
        .archive(PROJECT, "web", Some("alt"), false, false)
        .await
        .unwrap();
    assert!(planner.select_variant(&alt, false).await.is_ok());
}

#[tokio::test]
async fn unarchive_variant_in_archived_line_is_refused() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    let err = planner
        .archive(PROJECT, "web", Some("alt"), false, false)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "ARCHIVE_REFUSED: unarchive the line first");
}

#[tokio::test]
async fn unarchive_emits_portfolio_unarchived_event() {
    let (planner, sink) = audited();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    planner
        .archive(PROJECT, "web", None, false, false)
        .await
        .unwrap();
    assert!(
        sink.event_types()
            .contains(&"plan.portfolio.unarchived".to_string())
    );
}

#[tokio::test]
async fn line_archive_leaves_variant_flags_untouched() {
    let db = TempDb::new();
    let planner = db.planner();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    let flagged: i64 = rusqlite::Connection::open(&db.path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM variants WHERE archived = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(flagged, 0);
}

#[tokio::test]
async fn unarchiving_line_keeps_individually_archived_variant_archived() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    planner
        .archive(PROJECT, "web", None, false, false)
        .await
        .unwrap();
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    let variants: Vec<&str> = lines[0]
        .variants
        .iter()
        .map(|v| v.variant.as_str())
        .collect();
    assert_eq!(variants, vec!["main"]);
}

#[tokio::test]
async fn execution_on_archived_variant_is_archive_refused() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    let err = planner.acquire_cohort(acquire_req(&alt)).await.unwrap_err();
    assert!(matches!(err, PlannerError::ArchiveRefused { .. }));
}

// ---------------------------------------------------------------------
// Task 4 rulings: transitive carry-over, archived revise, project hygiene
// ---------------------------------------------------------------------

/// Lease the single next deliverable of `plan_id` and mark it `Complete`.
async fn complete_next(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str) {
    let cohort = planner
        .acquire_cohort(acquire_req(plan_id))
        .await
        .expect("harness: acquire");
    assert_eq!(cohort.rows[0].deliverable.id, id, "harness: leased id");
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            id,
            CallerId("w1".into()),
            DeliverableStatus::Complete,
        ))
        .await
        .expect("harness: complete");
}

#[tokio::test]
async fn select_does_not_carry_complete_above_uncarried_prerequisite() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    // `a` changes (not carried); `b` is identical but sits above it.
    let changed = graph(vec![
        deliverable("a", &["src/a.rs"], &[], 5.0),
        deliverable("b", &["src/b.rs"], &["a"], 2.0),
    ]);
    let alt = sync(&planner, "web", "alt", changed).await;
    complete_next(&planner, &main, "a").await;
    complete_next(&planner, &main, "b").await;
    planner.select_variant(&alt, false).await.unwrap();
    assert_eq!(
        status_of(&planner, &alt, "b").await,
        DeliverableStatus::Pending
    );
}

#[tokio::test]
async fn revise_on_archived_variant_is_refused() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    let err = planner
        .revise_plan(ReviseRequest::new(alt, chain()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::ArchiveRefused { .. }));
}

#[tokio::test]
async fn revise_on_archived_line_is_refused() {
    let planner = BasicCpmPlanner::new();
    let (main, _) = main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    let err = planner
        .revise_plan(ReviseRequest::new(main, chain_plus_c()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::ArchiveRefused { .. }));
}

#[tokio::test]
async fn sync_rejects_control_characters_in_project() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .sync_plan(SyncRequest::new("proj\nevil", "web", "main", chain()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

// ---------------------------------------------------------------------
// Variant identity + definition drift in status
// ---------------------------------------------------------------------

/// A temp project root (with `.git`), removed on drop.
fn project_dir() -> (tempfile::TempDir, ProjectRoot) {
    let dir = tempfile::Builder::new()
        .prefix("cpm-portfolio-root-")
        .tempdir()
        .unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let root = ProjectRoot::from_path(dir.path()).unwrap();
    (dir, root)
}

/// Write `g` as the plan file of `<name>/<variant>` and sync it from there.
async fn sync_file(
    planner: &BasicCpmPlanner,
    root: &ProjectRoot,
    name: &str,
    variant: &str,
    g: PlanGraph,
) -> PlanId {
    let f = root.plan_file(name, variant).unwrap();
    let hash = root.write_graph(&f, &g).unwrap();
    planner
        .sync_plan(
            SyncRequest::new(root.project_key(), name, variant, g)
                .with_source_path(f.rel_path)
                .with_content_hash(hash),
        )
        .await
        .expect("harness: sync file")
        .plan_id
}

fn rooted(root: &ProjectRoot) -> BasicCpmPlanner {
    BasicCpmPlanner::new().with_project_root(root.clone())
}

#[tokio::test]
async fn status_reports_variant_identity() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    let s = planner.status(&alt).await.unwrap();
    assert_eq!(
        (s.name.as_deref(), s.variant.as_deref(), s.selected),
        (Some("web"), Some("alt"), Some(false))
    );
}

#[tokio::test]
async fn status_of_unnamed_plan_has_no_variant_identity() {
    let planner = BasicCpmPlanner::new();
    let id = planner.submit_plan(chain()).await.unwrap();
    let s = planner.status(&id).await.unwrap();
    assert_eq!(
        (s.name, s.variant, s.selected, s.definition_drift),
        (None, None, None, None)
    );
}

#[tokio::test]
async fn status_reports_definition_drift_after_file_edit() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    let path = dir.path().join(".cpm-planner/plans/web/main.json");
    std::fs::write(&path, serde_json::to_vec(&chain_plus_c()).unwrap()).unwrap();
    let s = planner.status(&id).await.unwrap();
    assert_eq!(s.definition_drift, Some(true));
}

#[tokio::test]
async fn status_reports_no_drift_for_untouched_file() {
    let (_dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    let s = planner.status(&id).await.unwrap();
    assert_eq!(s.definition_drift, Some(false));
}

#[tokio::test]
async fn status_reports_drift_for_unparseable_file_edit() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    let path = dir.path().join(".cpm-planner/plans/web/main.json");
    std::fs::write(&path, b"{ not json").unwrap();
    let s = planner.status(&id).await.unwrap();
    assert_eq!(s.definition_drift, Some(true));
}

#[tokio::test]
async fn status_drift_is_none_without_project_root() {
    let (_dir, root) = project_dir();
    let planner = BasicCpmPlanner::new();
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    let s = planner.status(&id).await.unwrap();
    assert_eq!(s.definition_drift, None);
}

#[tokio::test]
async fn status_drift_is_none_for_inline_variant() {
    let (_dir, root) = project_dir();
    let planner = rooted(&root);
    let id = planner
        .sync_plan(SyncRequest::new(root.project_key(), "web", "main", chain()))
        .await
        .unwrap()
        .plan_id;
    let s = planner.status(&id).await.unwrap();
    assert_eq!(s.definition_drift, None);
}

#[tokio::test]
async fn status_drift_is_none_when_file_is_missing() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    std::fs::remove_file(dir.path().join(".cpm-planner/plans/web/main.json")).unwrap();
    let s = planner.status(&id).await.unwrap();
    assert_eq!(s.definition_drift, None);
}

#[tokio::test]
async fn status_drift_is_none_for_another_projects_variant() {
    let (_dir, root) = project_dir();
    let (_other_dir, other) = project_dir();
    let planner = rooted(&other);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    let s = planner.status(&id).await.unwrap();
    assert_eq!(s.definition_drift, None);
}

// ---------------------------------------------------------------------
// fork
// ---------------------------------------------------------------------

fn variant_summary(
    lines: &[cpm_planner::plan::PlanLineSummary],
    variant: &str,
) -> cpm_planner::plan::VariantSummary {
    lines[0]
        .variants
        .iter()
        .find(|v| v.variant == variant)
        .cloned()
        .expect("harness: variant listed")
}

#[tokio::test]
async fn fork_creates_draft_variant_file() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let main = sync_file(&planner, &root, "web", "main", chain()).await;
    planner
        .fork_plan(ForkRequest::new(main, "alt"))
        .await
        .unwrap();
    let lines = planner
        .list_plans(&root.project_key(), false)
        .await
        .unwrap();
    let alt = variant_summary(&lines, "alt");
    assert_eq!(
        (
            alt.selected,
            alt.source_path.as_deref(),
            dir.path().join(".cpm-planner/plans/web/alt.json").is_file()
        ),
        (false, Some(".cpm-planner/plans/web/alt.json"), true)
    );
}

#[tokio::test]
async fn fork_uses_request_project_root() {
    let (dir, root) = project_dir();
    let planner = BasicCpmPlanner::new();
    let main = sync_file(&planner, &root, "web", "main", chain()).await;
    planner
        .fork_plan(ForkRequest::new(main, "alt").with_project_root(root.clone()))
        .await
        .unwrap();
    assert!(dir.path().join(".cpm-planner/plans/web/alt.json").is_file());
}

#[tokio::test]
async fn forked_variant_file_has_no_drift() {
    let (_dir, root) = project_dir();
    let planner = rooted(&root);
    let main = sync_file(&planner, &root, "web", "main", chain()).await;
    let out = planner
        .fork_plan(ForkRequest::new(main, "alt"))
        .await
        .unwrap();
    let s = planner.status(&out.plan_id).await.unwrap();
    assert_eq!(s.definition_drift, Some(false));
}

#[tokio::test]
async fn fork_without_project_root_registers_inline_variant() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    planner
        .fork_plan(ForkRequest::new(main, "alt"))
        .await
        .unwrap();
    let lines = planner.list_plans(PROJECT, false).await.unwrap();
    let alt = variant_summary(&lines, "alt");
    assert_eq!((alt.selected, alt.source_path), (false, None));
}

#[tokio::test]
async fn fork_reports_created_outcome() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    let out = planner
        .fork_plan(ForkRequest::new(main, "alt"))
        .await
        .unwrap();
    assert_eq!(
        (out.name.as_str(), out.variant.as_str(), out.created),
        ("web", "alt", true)
    );
}

#[tokio::test]
async fn fork_existing_variant_is_rejected() {
    let planner = BasicCpmPlanner::new();
    let (main, _) = main_and_alt(&planner).await;
    let err = planner
        .fork_plan(ForkRequest::new(main, "alt"))
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "INVALID_PATH: variant 'alt' of 'web' already exists"
    );
}

#[tokio::test]
async fn fork_refuses_to_overwrite_an_unsynced_plan_file() {
    let (_dir, root) = project_dir();
    let planner = rooted(&root);
    let main = sync_file(&planner, &root, "web", "main", chain()).await;
    let f = root.plan_file("web", "alt").unwrap();
    root.write_graph(&f, &chain_plus_c()).unwrap();
    let err = planner
        .fork_plan(ForkRequest::new(main, "alt"))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

#[tokio::test]
async fn fork_applies_edits() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    let out = planner
        .fork_plan(
            ForkRequest::new(main, "alt").with_edits(vec![GraphEdit::SetEffort {
                id: "b".into(),
                hours: 7.0,
            }]),
        )
        .await
        .unwrap();
    let g = planner.get_plan(&out.plan_id).await.unwrap().graph;
    let b = g.deliverables.iter().find(|d| d.id == "b").unwrap();
    assert_eq!(b.estimated_effort_hours, Some(7.0));
}

#[tokio::test]
async fn fork_reads_the_head_revision() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    sync(&planner, "web", "main", chain_plus_c()).await;
    let out = planner
        .fork_plan(ForkRequest::new(main, "alt"))
        .await
        .unwrap();
    let g = planner.get_plan(&out.plan_id).await.unwrap().graph;
    assert_eq!(ids(&g), vec!["a", "b", "c"]);
}

#[tokio::test]
async fn fork_with_invalid_edit_is_rejected() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    let err = planner
        .fork_plan(
            ForkRequest::new(main, "alt")
                .with_edits(vec![GraphEdit::RemoveDeliverable { id: "nope".into() }]),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidGraph { .. }));
}

#[tokio::test]
async fn fork_rejects_invalid_variant_slug() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    let err = planner
        .fork_plan(ForkRequest::new(main, "Bad/Name"))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

#[tokio::test]
async fn fork_of_unnamed_plan_is_rejected() {
    let planner = BasicCpmPlanner::new();
    let id = planner.submit_plan(chain()).await.unwrap();
    let err = planner
        .fork_plan(ForkRequest::new(id, "alt"))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "INVALID_PATH: fork requires a named plan");
}

#[tokio::test]
async fn fork_emits_portfolio_created_event() {
    let (planner, sink) = audited();
    let main = sync(&planner, "web", "main", chain()).await;
    planner
        .fork_plan(ForkRequest::new(main, "alt"))
        .await
        .unwrap();
    let created = sink
        .event_types()
        .iter()
        .filter(|t| *t == "plan.portfolio.created")
        .count();
    assert_eq!(created, 2);
}

// ---------------------------------------------------------------------
// compare_plans
// ---------------------------------------------------------------------

#[tokio::test]
async fn compare_by_plan_name_uses_non_archived_variants() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    sync(&planner, "web", "third", chain()).await;
    planner
        .archive(PROJECT, "web", Some("third"), true, false)
        .await
        .unwrap();
    let c = planner
        .compare_plans(ComparePlansRequest::by_plan(
            PROJECT,
            "web",
            CompareRequest::default(),
        ))
        .await
        .unwrap();
    let names: Vec<&str> = c.variants.iter().map(|v| v.variant.as_str()).collect();
    assert_eq!(names, vec!["alt", "main"]);
}

#[tokio::test]
async fn compare_by_plan_ids_labels_variants() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    let c = planner
        .compare_plans(ComparePlansRequest::by_ids(
            vec![main, alt],
            CompareRequest::default(),
        ))
        .await
        .unwrap();
    let names: Vec<&str> = c.variants.iter().map(|v| v.variant.as_str()).collect();
    assert_eq!(names, vec!["main", "alt"]);
}

#[tokio::test]
async fn compare_labels_unnamed_plan_by_its_id() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    let unnamed = planner.submit_plan(chain_plus_c()).await.unwrap();
    let c = planner
        .compare_plans(ComparePlansRequest::by_ids(
            vec![main, unnamed.clone()],
            CompareRequest::default(),
        ))
        .await
        .unwrap();
    assert_eq!(c.variants[1].variant, unnamed.0);
}

#[tokio::test]
async fn compare_plan_ids_and_name_together_is_rejected() {
    let planner = BasicCpmPlanner::new();
    let (main, alt) = main_and_alt(&planner).await;
    let mut req = ComparePlansRequest::by_ids(vec![main, alt], CompareRequest::default());
    req.plan = Some((PROJECT.to_string(), "web".to_string()));
    let err = planner.compare_plans(req).await.unwrap_err();
    assert!(matches!(err, PlannerError::InvalidGraph { .. }));
}

#[tokio::test]
async fn compare_without_plan_ids_or_name_is_rejected() {
    let planner = BasicCpmPlanner::new();
    let req = ComparePlansRequest {
        plan_ids: None,
        plan: None,
        request: CompareRequest::default(),
    };
    let err = planner.compare_plans(req).await.unwrap_err();
    assert!(matches!(err, PlannerError::InvalidGraph { .. }));
}

#[tokio::test]
async fn compare_of_unknown_plan_id_is_plan_not_found() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    let err = planner
        .compare_plans(ComparePlansRequest::by_ids(
            vec![main, PlanId("nope".into())],
            CompareRequest::default(),
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::PlanNotFound { .. }));
}

#[tokio::test]
async fn compare_of_unknown_plan_line_is_plan_not_found() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .compare_plans(ComparePlansRequest::by_plan(
            PROJECT,
            "nope",
            CompareRequest::default(),
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::PlanNotFound { .. }));
}

// ---------------------------------------------------------------------
// export
// ---------------------------------------------------------------------

#[tokio::test]
async fn export_writes_head_graph_to_variant_file() {
    let (_dir, root) = project_dir();
    let planner = BasicCpmPlanner::new();
    let key = root.project_key();
    let id = planner
        .sync_plan(SyncRequest::new(key.clone(), "web", "main", chain()))
        .await
        .unwrap()
        .plan_id;
    planner
        .sync_plan(SyncRequest::new(key, "web", "main", chain_plus_c()))
        .await
        .unwrap();
    let rel = planner.export_plan(&id, &root, None, false).await.unwrap();
    let (written, _) = root
        .read_graph(&root.resolve_plan_file(&rel).unwrap())
        .unwrap();
    assert_eq!(ids(&written), vec!["a", "b", "c"]);
}

#[tokio::test]
async fn export_returns_the_variant_file_path() {
    let (_dir, root) = project_dir();
    let planner = BasicCpmPlanner::new();
    let id = planner
        .sync_plan(SyncRequest::new(root.project_key(), "web", "main", chain()))
        .await
        .unwrap()
        .plan_id;
    let rel = planner.export_plan(&id, &root, None, false).await.unwrap();
    assert_eq!(rel, ".cpm-planner/plans/web/main.json");
}

#[tokio::test]
async fn export_does_not_sync_the_written_file() {
    let (_dir, root) = project_dir();
    let planner = BasicCpmPlanner::new();
    let id = planner
        .sync_plan(SyncRequest::new(root.project_key(), "web", "main", chain()))
        .await
        .unwrap()
        .plan_id;
    planner.export_plan(&id, &root, None, false).await.unwrap();
    let lines = planner
        .list_plans(&root.project_key(), false)
        .await
        .unwrap();
    assert_eq!(variant_summary(&lines, "main").source_path, None);
}

#[tokio::test]
async fn export_of_unnamed_plan_requires_path() {
    let (_dir, root) = project_dir();
    let planner = BasicCpmPlanner::new();
    let id = planner.submit_plan(chain()).await.unwrap();
    let err = planner
        .export_plan(&id, &root, None, false)
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

#[tokio::test]
async fn export_of_unnamed_plan_writes_given_path() {
    let (dir, root) = project_dir();
    let planner = BasicCpmPlanner::new();
    let id = planner.submit_plan(chain()).await.unwrap();
    planner
        .export_plan(
            &id,
            &root,
            Some(".cpm-planner/plans/adhoc/draft.json"),
            false,
        )
        .await
        .unwrap();
    assert!(
        dir.path()
            .join(".cpm-planner/plans/adhoc/draft.json")
            .is_file()
    );
}

#[tokio::test]
async fn export_rejects_path_escape() {
    let (_dir, root) = project_dir();
    let planner = BasicCpmPlanner::new();
    let id = planner.submit_plan(chain()).await.unwrap();
    let err = planner
        .export_plan(&id, &root, Some("../outside.json"), false)
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

#[tokio::test]
async fn export_of_another_projects_variant_needs_a_path() {
    let (_dir, root) = project_dir();
    let planner = BasicCpmPlanner::new();
    let id = sync(&planner, "web", "main", chain()).await;
    let err = planner
        .export_plan(&id, &root, None, false)
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

// ---------------------------------------------------------------------
// Final-review fixes: drift, fork safety, export guards, carry-over
// ---------------------------------------------------------------------

/// Write `g` as compact (non-pretty) JSON to `<name>/<variant>` and sync it
/// from there, as a hand-edited file would be.
async fn sync_compact_file(
    planner: &BasicCpmPlanner,
    dir: &tempfile::TempDir,
    root: &ProjectRoot,
    name: &str,
    variant: &str,
    g: PlanGraph,
) -> PlanId {
    let f = root.plan_file(name, variant).unwrap();
    let path = dir.path().join(&f.rel_path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = serde_json::to_vec(&g).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    planner
        .sync_plan(
            SyncRequest::new(root.project_key(), name, variant, g)
                .with_source_path(f.rel_path)
                .with_content_hash(cpm_planner::project::content_hash(&bytes)),
        )
        .await
        .expect("harness: sync compact file")
        .plan_id
}

fn plan_path(dir: &tempfile::TempDir, name: &str, variant: &str) -> PathBuf {
    dir.path()
        .join(format!(".cpm-planner/plans/{name}/{variant}.json"))
}

#[tokio::test]
async fn identical_inline_sync_keeps_drift_false() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_compact_file(&planner, &dir, &root, "web", "main", chain()).await;
    planner
        .sync_plan(SyncRequest::new(root.project_key(), "web", "main", chain()))
        .await
        .unwrap();
    assert_eq!(
        planner.status(&id).await.unwrap().definition_drift,
        Some(false)
    );
}

#[tokio::test]
async fn export_to_own_source_keeps_drift_false() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_compact_file(&planner, &dir, &root, "web", "main", chain()).await;
    planner.export_plan(&id, &root, None, false).await.unwrap();
    assert_eq!(
        planner.status(&id).await.unwrap().definition_drift,
        Some(false)
    );
}

#[tokio::test]
async fn export_to_own_source_records_the_written_hash() {
    let (dir, root) = project_dir();
    let db = TempDb::new();
    let planner = db.planner().with_project_root(root.clone());
    let id = sync_compact_file(&planner, &dir, &root, "web", "main", chain()).await;
    planner.export_plan(&id, &root, None, false).await.unwrap();
    let written =
        cpm_planner::project::content_hash(&std::fs::read(plan_path(&dir, "web", "main")).unwrap());
    let stored: String = rusqlite::Connection::open(&db.path)
        .unwrap()
        .query_row(
            "SELECT content_hash FROM variants WHERE plan_id = ?1",
            [&id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, written);
}

#[tokio::test]
async fn reformatted_file_with_same_graph_is_not_drift() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    std::fs::write(
        plan_path(&dir, "web", "main"),
        serde_json::to_vec(&chain()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        planner.status(&id).await.unwrap().definition_drift,
        Some(false)
    );
}

#[tokio::test]
async fn inline_revise_of_file_backed_variant_reports_drift() {
    let (_dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    planner
        .revise_plan(ReviseRequest::new(id.clone(), chain_plus_c()))
        .await
        .unwrap();
    assert_eq!(
        planner.status(&id).await.unwrap().definition_drift,
        Some(true)
    );
}

#[tokio::test]
async fn status_drift_is_none_for_oversized_file() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    std::fs::write(
        plan_path(&dir, "web", "main"),
        vec![b' '; 8 * 1024 * 1024 + 1],
    )
    .unwrap();
    assert_eq!(planner.status(&id).await.unwrap().definition_drift, None);
}

#[tokio::test]
async fn sync_rejects_bidi_override_in_project() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .sync_plan(SyncRequest::new("proj\u{202E}lmth", "web", "main", chain()))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }));
}

#[tokio::test]
async fn fork_into_archived_line_writes_no_file() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let main = sync_file(&planner, &root, "web", "main", chain()).await;
    planner
        .archive(&root.project_key(), "web", None, true, false)
        .await
        .unwrap();
    let _ = planner.fork_plan(ForkRequest::new(main, "alt")).await;
    assert!(!plan_path(&dir, "web", "alt").exists());
}

#[tokio::test]
async fn fork_on_archived_line_is_refused() {
    let (_dir, root) = project_dir();
    let planner = rooted(&root);
    let main = sync_file(&planner, &root, "web", "main", chain()).await;
    planner
        .archive(&root.project_key(), "web", None, true, false)
        .await
        .unwrap();
    let err = planner
        .fork_plan(ForkRequest::new(main, "alt"))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::ArchiveRefused { .. }));
}

#[tokio::test]
async fn fork_from_archived_variant_of_live_line_is_allowed() {
    let planner = BasicCpmPlanner::new();
    let (_, alt) = main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", Some("alt"), true, false)
        .await
        .unwrap();
    let outcome = planner
        .fork_plan(ForkRequest::new(alt, "third"))
        .await
        .unwrap();
    assert!(outcome.created);
}

#[tokio::test]
async fn fork_sync_failure_removes_written_file() {
    let (dir, root) = project_dir();
    let db = TempDb::new();
    let planner = db.planner().with_project_root(root.clone());
    let main = sync_file(&planner, &root, "web", "main", chain()).await;
    // Every later variant insert fails, after the fork wrote its file.
    db.sql(
        "CREATE TRIGGER inject_fork_failure BEFORE INSERT ON variants
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );
    let _ = planner.fork_plan(ForkRequest::new(main, "alt")).await;
    assert!(!plan_path(&dir, "web", "alt").exists());
}

#[tokio::test]
async fn fork_with_foreign_root_does_not_echo_project_keys() {
    let (_dir, root) = project_dir();
    let (_other_dir, other) = project_dir();
    let planner = BasicCpmPlanner::new();
    let main = sync_file(&planner, &root, "web", "main", chain()).await;
    let err = planner
        .fork_plan(ForkRequest::new(main, "alt").with_project_root(other.clone()))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, PlannerError::InvalidPath { reason }
            if reason.contains("a different project") && !reason.contains(&other.project_key())),
        "got {err}"
    );
}

#[tokio::test]
async fn foreign_project_root_export_is_invalid_path() {
    let (_dir, root) = project_dir();
    let (_other_dir, other) = project_dir();
    let planner = BasicCpmPlanner::new();
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    let err = planner
        .export_plan(&id, &other, None, false)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, PlannerError::InvalidPath { reason }
            if reason.contains("a different project") && !reason.contains(&root.project_key())),
        "got {err}"
    );
}

#[tokio::test]
async fn export_refuses_another_variants_tracked_file() {
    let (_dir, root) = project_dir();
    let planner = rooted(&root);
    sync_file(&planner, &root, "web", "main", chain()).await;
    let other = planner.submit_plan(chain_plus_c()).await.unwrap();
    let err = planner
        .export_plan(
            &other,
            &root,
            Some(".cpm-planner/plans/web/main.json"),
            false,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }), "got {err}");
}

#[tokio::test]
async fn export_refuses_to_overwrite_unsynced_local_edits() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    std::fs::write(
        plan_path(&dir, "web", "main"),
        serde_json::to_vec(&chain_plus_c()).unwrap(),
    )
    .unwrap();
    let err = planner
        .export_plan(&id, &root, None, false)
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidPath { .. }), "got {err}");
}

#[tokio::test]
async fn forced_export_overwrites_unsynced_local_edits() {
    let (dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    std::fs::write(
        plan_path(&dir, "web", "main"),
        serde_json::to_vec(&chain_plus_c()).unwrap(),
    )
    .unwrap();
    planner.export_plan(&id, &root, None, true).await.unwrap();
    let (written, _) = root
        .read_graph(&root.plan_file("web", "main").unwrap())
        .unwrap();
    assert_eq!(ids(&written), vec!["a", "b"]);
}

#[tokio::test]
async fn export_after_inline_revise_refreshes_the_lagging_file() {
    let (_dir, root) = project_dir();
    let planner = rooted(&root);
    let id = sync_file(&planner, &root, "web", "main", chain()).await;
    planner
        .revise_plan(ReviseRequest::new(id.clone(), chain_plus_c()))
        .await
        .unwrap();
    planner.export_plan(&id, &root, None, false).await.unwrap();
    assert_eq!(
        planner.status(&id).await.unwrap().definition_drift,
        Some(false)
    );
}

#[tokio::test]
async fn un_carry_restores_failed_status() {
    let planner = BasicCpmPlanner::new();
    let main = sync(&planner, "web", "main", chain()).await;
    let changed_a = graph(vec![
        deliverable("a", &["src/a.rs"], &[], 5.0),
        deliverable("b", &["src/b.rs"], &["a"], 2.0),
    ]);
    let alt = sync(&planner, "web", "alt", changed_a).await;
    let failed = DeliverableStatus::Failed {
        reason: "flaky".into(),
    };
    planner.select_variant(&alt, false).await.unwrap();
    planner
        .mark_status(MarkStatusRequest::new(
            alt.clone(),
            "b",
            CallerId("w1".into()),
            failed.clone(),
        ))
        .await
        .unwrap();
    planner.select_variant(&main, false).await.unwrap();
    complete_next(&planner, &main, "a").await;
    complete_next(&planner, &main, "b").await;
    // `b` is carried Complete, then un-carried above the changed `a`.
    planner.select_variant(&alt, false).await.unwrap();
    assert_eq!(status_of(&planner, &alt, "b").await, failed);
}

#[tokio::test]
async fn three_level_transitive_un_carry() {
    let planner = BasicCpmPlanner::new();
    let three = |a_effort| {
        graph(vec![
            deliverable("a", &["src/a.rs"], &[], a_effort),
            deliverable("b", &["src/b.rs"], &["a"], 2.0),
            deliverable("c", &["src/c.rs"], &["b"], 1.0),
        ])
    };
    let main = sync(&planner, "web", "main", three(1.0)).await;
    let alt = sync(&planner, "web", "alt", three(5.0)).await;
    for id in ["a", "b", "c"] {
        complete_next(&planner, &main, id).await;
    }
    planner.select_variant(&alt, false).await.unwrap();
    assert_eq!(
        status_of(&planner, &alt, "c").await,
        DeliverableStatus::Pending
    );
}

#[tokio::test]
async fn compare_by_name_with_fewer_than_two_live_variants_is_rejected() {
    let planner = BasicCpmPlanner::new();
    sync(&planner, "web", "main", chain()).await;
    let err = planner
        .compare_plans(ComparePlansRequest::by_plan(
            PROJECT,
            "web",
            CompareRequest::default(),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(err, PlannerError::InvalidGraph { .. }),
        "got {err}"
    );
}

#[tokio::test]
async fn compare_by_name_on_archived_line_is_rejected() {
    let planner = BasicCpmPlanner::new();
    main_and_alt(&planner).await;
    planner
        .archive(PROJECT, "web", None, true, false)
        .await
        .unwrap();
    let err = planner
        .compare_plans(ComparePlansRequest::by_plan(
            PROJECT,
            "web",
            CompareRequest::default(),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(err, PlannerError::InvalidGraph { .. }),
        "got {err}"
    );
}

#[tokio::test]
async fn compare_by_ids_rejects_duplicates() {
    let planner = BasicCpmPlanner::new();
    let (main, _) = main_and_alt(&planner).await;
    let err = planner
        .compare_plans(ComparePlansRequest::by_ids(
            vec![main.clone(), main],
            CompareRequest::default(),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(err, PlannerError::InvalidGraph { .. }),
        "got {err}"
    );
}
