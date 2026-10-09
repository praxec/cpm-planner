//! Portfolio integration tests: named plan lines and variants (`sync_plan`,
//! `list_plans`, `revision_graph`) and in-place revision (`revise_plan`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::plan::{
    AcquireRequest, CallerId, Deliverable, DeliverableStatus, MarkStatusRequest, PlanGraph, PlanId,
    PlannerError, ReviseRequest, SyncRequest,
};
use cpm_planner::ports::Planner;
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
