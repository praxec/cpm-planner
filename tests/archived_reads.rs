//! Archived-variant reads: `plan.status` and `plan.ev` report whether the
//! plan belongs to an effectively archived variant and, when it does, name
//! the project's single active plan line. Reads stay allowed; `plan.snapshot`
//! keeps refusing with `ARCHIVE_REFUSED`.

use std::path::PathBuf;
use std::sync::Arc;

use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::earned_value::{BaselineRequest, SnapshotRequest};
use cpm_planner::plan::{Deliverable, OwnedFile, PlanGraph, PlanId, SyncRequest};
use cpm_planner::ports::Planner;
use cpm_planner::{BasicCpmPlanner, SqlitePlanStore};

const PROJECT: &str = "proj";

// ---------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------

/// Unique temp db file, removed (with WAL sidecars) on drop.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cpm-planner-archived-reads-test-{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        Self { path }
    }

    fn planner(&self) -> BasicCpmPlanner {
        let store = SqlitePlanStore::open(&self.path).expect("open store");
        BasicCpmPlanner::with_store(store, Arc::new(MemoryAuditSink::new()))
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

/// A one-deliverable, not-yet-complete plan.
fn graph() -> PlanGraph {
    PlanGraph {
        deliverables: vec![Deliverable {
            id: "a".to_string(),
            owned_files: vec![OwnedFile::from(PathBuf::from("src/a.rs"))],
            prerequisites: Vec::new(),
            estimated_effort_hours: Some(1.0),
            metadata: serde_json::Value::Null,
            duration_hours: None,
            estimate: None,
            milestone: false,
            earning_rule: None,
        }],
        max_chained_dispatch: None,
    }
}

/// Sync `variant` of the line `name`, returning its plan id.
async fn sync(planner: &BasicCpmPlanner, name: &str, variant: &str) -> PlanId {
    planner
        .sync_plan(SyncRequest::new(PROJECT, name, variant, graph()))
        .await
        .expect("sync")
        .plan_id
}

/// Sync the selected variant `main` of a live line, baselined (baselines
/// refuse archived variants, so callers archive only afterwards).
async fn baselined_line(planner: &BasicCpmPlanner, name: &str) -> PlanId {
    let plan_id = sync(planner, name, "main").await;
    planner
        .baseline(BaselineRequest::new(plan_id.clone()))
        .await
        .expect("baseline");
    plan_id
}

/// Baseline and then archive the whole line `old`; adds the live lines in
/// `active` (each with an incomplete plan).
async fn archived_line_with_active(planner: &BasicCpmPlanner, active: &[&str]) -> PlanId {
    let old = baselined_line(planner, "old").await;
    planner
        .archive(PROJECT, "old", None, true, false)
        .await
        .expect("archive line");
    for name in active {
        sync(planner, name, "main").await;
    }
    old
}

// ---------------------------------------------------------------------
// plan.status
// ---------------------------------------------------------------------

#[tokio::test]
async fn status_of_archived_line_variant_reports_archived_true() {
    let db = TempDb::new();
    let planner = db.planner();
    let old = archived_line_with_active(&planner, &[]).await;

    let status = planner.status(&old).await.expect("status");

    assert_eq!(status.archived, Some(true));
}

#[tokio::test]
async fn status_of_variant_archived_alone_reports_archived_true() {
    let db = TempDb::new();
    let planner = db.planner();
    let main = sync(&planner, "web", "main").await;
    let alt = sync(&planner, "web", "alt").await;
    planner
        .select_variant(&alt, false)
        .await
        .expect("select alt");
    planner
        .archive(PROJECT, "web", Some("main"), true, false)
        .await
        .expect("archive main");

    let status = planner.status(&main).await.expect("status");

    assert_eq!(status.archived, Some(true));
}

#[tokio::test]
async fn status_of_live_selected_variant_reports_archived_false() {
    let db = TempDb::new();
    let planner = db.planner();
    let live = sync(&planner, "web", "main").await;

    let status = planner.status(&live).await.expect("status");

    assert_eq!(status.archived, Some(false));
}

#[tokio::test]
async fn status_of_unnamed_plan_reports_archived_none() {
    let db = TempDb::new();
    let planner = db.planner();
    let plan_id = planner.submit_plan(graph()).await.expect("submit");

    let status = planner.status(&plan_id).await.expect("status");

    assert_eq!(status.archived, None);
}

#[tokio::test]
async fn status_of_archived_variant_names_the_single_active_line() {
    let db = TempDb::new();
    let planner = db.planner();
    let old = archived_line_with_active(&planner, &["current"]).await;

    let status = planner.status(&old).await.expect("status");

    assert_eq!(status.active_plan.as_deref(), Some("current"));
}

#[tokio::test]
async fn status_active_plan_is_none_when_the_project_has_two_active_lines() {
    let db = TempDb::new();
    let planner = db.planner();
    let old = archived_line_with_active(&planner, &["one", "two"]).await;

    let status = planner.status(&old).await.expect("status");

    assert_eq!(status.active_plan, None);
}

#[tokio::test]
async fn status_active_plan_is_none_for_a_live_variant() {
    let db = TempDb::new();
    let planner = db.planner();
    let live = sync(&planner, "web", "main").await;

    let status = planner.status(&live).await.expect("status");

    assert_eq!(status.active_plan, None);
}

// ---------------------------------------------------------------------
// plan.ev
// ---------------------------------------------------------------------

#[tokio::test]
async fn ev_of_archived_variant_reports_archived_true() {
    let db = TempDb::new();
    let planner = db.planner();
    let old = archived_line_with_active(&planner, &[]).await;

    let report = planner.ev(&old, None).await.expect("ev");

    assert!(report.archived);
}

#[tokio::test]
async fn ev_of_archived_variant_names_the_active_line() {
    let db = TempDb::new();
    let planner = db.planner();
    let old = archived_line_with_active(&planner, &["current"]).await;

    let report = planner.ev(&old, None).await.expect("ev");

    assert_eq!(report.active_plan.as_deref(), Some("current"));
}

#[tokio::test]
async fn ev_of_live_variant_reports_not_archived() {
    let db = TempDb::new();
    let planner = db.planner();
    let live = baselined_line(&planner, "web").await;

    let report = planner.ev(&live, None).await.expect("ev");

    assert!(!report.archived);
}

#[tokio::test]
async fn ev_of_live_variant_reports_no_active_plan() {
    let db = TempDb::new();
    let planner = db.planner();
    let live = baselined_line(&planner, "web").await;

    let report = planner.ev(&live, None).await.expect("ev");

    assert_eq!(report.active_plan, None);
}

// ---------------------------------------------------------------------
// plan.snapshot stays refused
// ---------------------------------------------------------------------

#[tokio::test]
async fn snapshot_on_archived_variant_still_fails_archive_refused() {
    let db = TempDb::new();
    let planner = db.planner();
    let old = archived_line_with_active(&planner, &[]).await;

    let error = planner
        .snapshot(SnapshotRequest::new(old))
        .await
        .expect_err("archived snapshot refused");

    assert!(error.to_string().starts_with("ARCHIVE_REFUSED:"));
}
