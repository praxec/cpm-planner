//! Plan selector integration tests: resolving a stored plan by id, by plan-line
//! name, or as a project's single active line (`PlanRef`, `resolve_plan`), and
//! listing a project's active plan lines (`list_active_plans`).

use std::path::PathBuf;
use std::sync::Arc;

use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::plan::{
    AcceptRequest, Deliverable, OwnedFile, PlanGraph, PlanId, PlanRef, PlannerError, SyncRequest,
};
use cpm_planner::ports::Planner;
use cpm_planner::{BasicCpmPlanner, SqlitePlanStore};

// ---------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------

const PROJECT: &str = "proj";
const OTHER_PROJECT: &str = "other";

/// Unique temp db file, removed (with WAL sidecars) on drop.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cpm-planner-plan-selector-test-{}.db",
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

fn deliverable(id: &str) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: vec![OwnedFile::from(PathBuf::from(format!("src/{id}.rs")))],
        prerequisites: Vec::new(),
        estimated_effort_hours: Some(1.0),
        metadata: serde_json::Value::Null,
        duration_hours: None,
        estimate: None,
        milestone: false,
        earning_rule: None,
    }
}

/// A one-deliverable plan.
fn one(id: &str) -> PlanGraph {
    PlanGraph {
        deliverables: vec![deliverable(id)],
        max_chained_dispatch: None,
    }
}

fn sign_in(project: &str, name: &str, variant: &str) -> impl Fn(PlanGraph) -> SyncRequest {
    move |graph| {
        SyncRequest::new(
            project.to_string(),
            name.to_string(),
            variant.to_string(),
            graph,
        )
    }
}

async fn sync(
    planner: &BasicCpmPlanner,
    project: &str,
    name: &str,
    variant: &str,
    graph: PlanGraph,
) -> PlanId {
    planner
        .sync_plan(sign_in(project, name, variant)(graph))
        .await
        .expect("sync")
        .plan_id
}

/// Accept the plan's only deliverable, leaving the plan all-complete.
async fn complete_only(planner: &BasicCpmPlanner, plan_id: &PlanId) {
    planner
        .accept(AcceptRequest::new(plan_id.clone(), "a", "owner", "done"))
        .await
        .expect("accept");
}

async fn line_names(planner: &BasicCpmPlanner, project: &str) -> Vec<String> {
    planner
        .list_active_plans(project)
        .await
        .expect("list active plans")
        .into_iter()
        .map(|line| line.name)
        .collect()
}

// ---------------------------------------------------------------------
// PlanRef::Id
// ---------------------------------------------------------------------

#[tokio::test]
async fn id_resolves_to_itself_without_a_lookup() {
    let db = TempDb::new();
    let planner = db.planner();
    let id = PlanId("never-stored".into());

    let resolved = planner
        .resolve_plan(PlanRef::Id(id.clone()))
        .await
        .expect("resolve id");

    assert_eq!(resolved, id);
}

// ---------------------------------------------------------------------
// PlanRef::Line
// ---------------------------------------------------------------------

#[tokio::test]
async fn line_resolves_to_its_selected_variant() {
    let db = TempDb::new();
    let planner = db.planner();
    let main = sync(&planner, PROJECT, "web", "main", one("a")).await;

    let resolved = planner
        .resolve_plan(PlanRef::Line {
            project: PROJECT.into(),
            name: "web".into(),
        })
        .await
        .expect("resolve line");

    assert_eq!(resolved, main);
}

#[tokio::test]
async fn line_resolves_to_the_variant_selected_after_a_switch() {
    let db = TempDb::new();
    let planner = db.planner();
    let _main = sync(&planner, PROJECT, "web", "main", one("a")).await;
    let alt = sync(&planner, PROJECT, "web", "alt", one("b")).await;
    planner
        .select_variant(&alt, false)
        .await
        .expect("select alt");

    let resolved = planner
        .resolve_plan(PlanRef::Line {
            project: PROJECT.into(),
            name: "web".into(),
        })
        .await
        .expect("resolve line");

    assert_eq!(resolved, alt);
}

#[tokio::test]
async fn unknown_line_is_plan_not_found() {
    let db = TempDb::new();
    let planner = db.planner();

    let err = planner
        .resolve_plan(PlanRef::Line {
            project: PROJECT.into(),
            name: "missing".into(),
        })
        .await
        .expect_err("unknown line must fail");

    assert!(matches!(err, PlannerError::PlanNotFound { .. }));
}

// ---------------------------------------------------------------------
// PlanRef::Active
// ---------------------------------------------------------------------

#[tokio::test]
async fn active_with_one_live_line_resolves_to_it() {
    let db = TempDb::new();
    let planner = db.planner();
    let only = sync(&planner, PROJECT, "web", "main", one("a")).await;

    let resolved = planner
        .resolve_plan(PlanRef::Active {
            project: PROJECT.into(),
        })
        .await
        .expect("resolve active");

    assert_eq!(resolved, only);
}

#[tokio::test]
async fn active_ignores_an_archived_line() {
    let db = TempDb::new();
    let planner = db.planner();
    let _archived = sync(&planner, PROJECT, "old", "main", one("a")).await;
    let live = sync(&planner, PROJECT, "web", "main", one("b")).await;
    planner
        .archive(PROJECT, "old", None, true, false)
        .await
        .expect("archive old");

    let resolved = planner
        .resolve_plan(PlanRef::Active {
            project: PROJECT.into(),
        })
        .await
        .expect("resolve active");

    assert_eq!(resolved, live);
}

#[tokio::test]
async fn active_ignores_a_complete_line() {
    let db = TempDb::new();
    let planner = db.planner();
    let done = sync(&planner, PROJECT, "done", "main", one("a")).await;
    complete_only(&planner, &done).await;
    let live = sync(&planner, PROJECT, "web", "main", one("b")).await;

    let resolved = planner
        .resolve_plan(PlanRef::Active {
            project: PROJECT.into(),
        })
        .await
        .expect("resolve active");

    assert_eq!(resolved, live);
}

#[tokio::test]
async fn active_with_no_candidates_is_no_active_plan() {
    let db = TempDb::new();
    let planner = db.planner();

    let err = planner
        .resolve_plan(PlanRef::Active {
            project: PROJECT.into(),
        })
        .await
        .expect_err("no live line must fail");

    assert_eq!(
        err.to_string(),
        "NO_ACTIVE_PLAN: project 'proj' has no active plan line (an unarchived line whose \
         selected variant is unarchived and not complete)"
    );
}

#[tokio::test]
async fn active_with_two_candidates_is_ambiguous_and_names_both_lines() {
    let db = TempDb::new();
    let planner = db.planner();
    let _beta = sync(&planner, PROJECT, "beta", "main", one("a")).await;
    let _alpha = sync(&planner, PROJECT, "alpha", "main", one("b")).await;

    let err = planner
        .resolve_plan(PlanRef::Active {
            project: PROJECT.into(),
        })
        .await
        .expect_err("two live lines must be ambiguous");

    assert_eq!(
        err.to_string(),
        "AMBIGUOUS_ACTIVE_PLAN: project 'proj' has 2 active plan lines: alpha, beta; pass plan or plan_id"
    );
}

#[tokio::test]
async fn active_is_scoped_to_its_project() {
    let db = TempDb::new();
    let planner = db.planner();
    let _other = sync(&planner, OTHER_PROJECT, "web", "main", one("a")).await;

    let err = planner
        .resolve_plan(PlanRef::Active {
            project: PROJECT.into(),
        })
        .await
        .expect_err("another project's line must not count");

    assert_eq!(
        err.to_string(),
        "NO_ACTIVE_PLAN: project 'proj' has no active plan line (an unarchived line whose \
         selected variant is unarchived and not complete)"
    );
}

// ---------------------------------------------------------------------
// list_active_plans
// ---------------------------------------------------------------------

#[tokio::test]
async fn list_active_plans_omits_archived_and_complete_lines() {
    let db = TempDb::new();
    let planner = db.planner();
    let _live = sync(&planner, PROJECT, "live", "main", one("a")).await;
    let _archived = sync(&planner, PROJECT, "archived", "main", one("b")).await;
    let done = sync(&planner, PROJECT, "done", "main", one("a")).await;
    complete_only(&planner, &done).await;
    planner
        .archive(PROJECT, "archived", None, true, false)
        .await
        .expect("archive");

    assert_eq!(
        line_names(&planner, PROJECT).await,
        vec!["live".to_string()]
    );
}
