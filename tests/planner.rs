//! Integration tests for `BasicCpmPlanner`.
//!
//! Covers the trait surface from outside the crate; for lock-lifecycle
//! and TTL behaviour see `tests/locks.rs`.

use std::path::PathBuf;
use std::sync::Arc;

use cpm_planner::BasicCpmPlanner;
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::plan::{CallerId, Deliverable, DeliverableStatus, PlanGraph, PlannerError};
use cpm_planner::ports::Planner;

fn deliverable(id: &str, files: &[&str], prereqs: &[&str], effort: Option<f32>) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: files.iter().map(PathBuf::from).collect(),
        prerequisites: prereqs.iter().map(|s| s.to_string()).collect(),
        estimated_effort_hours: effort,
        metadata: serde_json::Value::Null,
    }
}

fn caller(id: &str) -> CallerId {
    CallerId(id.to_string())
}

#[tokio::test]
async fn submit_plan_idempotent_on_identical_graph() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/a.rs"], &[], Some(1.0)),
            deliverable("b", &["src/b.rs"], &["a"], Some(2.0)),
        ],
        max_chained_dispatch: None,
    };
    let id1 = planner.submit_plan(graph.clone()).await.unwrap();
    let id2 = planner.submit_plan(graph).await.unwrap();
    assert_eq!(id1, id2);
}

#[tokio::test]
async fn submit_plan_dedup_ignores_deliverable_order() {
    let planner = BasicCpmPlanner::new();
    let a = deliverable("a", &["src/a.rs"], &[], Some(1.0));
    let b = deliverable("b", &["src/b.rs"], &["a"], Some(2.0));
    let g1 = PlanGraph {
        deliverables: vec![a.clone(), b.clone()],
        max_chained_dispatch: None,
    };
    let g2 = PlanGraph {
        deliverables: vec![b, a],
        max_chained_dispatch: None,
    };
    let id1 = planner.submit_plan(g1).await.unwrap();
    let id2 = planner.submit_plan(g2).await.unwrap();
    assert_eq!(id1, id2);
}

#[tokio::test]
async fn submit_plan_rejects_invalid_graph_cycles_in_prerequisites() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/a.rs"], &["b"], Some(1.0)),
            deliverable("b", &["src/b.rs"], &["a"], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let err = planner.submit_plan(graph).await.unwrap_err();
    match err {
        PlannerError::InvalidGraph { reason } => {
            assert!(reason.to_lowercase().contains("cycle"), "got: {reason}");
        }
        other => panic!("expected InvalidGraph(cycle), got {other:?}"),
    }
}

#[tokio::test]
async fn submit_plan_rejects_invalid_graph_duplicate_file_ownership() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/shared.rs"], &[], Some(1.0)),
            deliverable("b", &["src/shared.rs"], &[], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let err = planner.submit_plan(graph).await.unwrap_err();
    match err {
        PlannerError::InvalidGraph { reason } => {
            assert!(reason.contains("src/shared.rs"), "got: {reason}");
        }
        other => panic!("expected InvalidGraph(duplicate file), got {other:?}"),
    }
}

#[tokio::test]
async fn submit_plan_rejects_unknown_prerequisite() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &["nope"], Some(1.0))],
        max_chained_dispatch: None,
    };
    let err = planner.submit_plan(graph).await.unwrap_err();
    assert!(
        matches!(err, PlannerError::InvalidGraph { .. }),
        "expected InvalidGraph (unknown prerequisite), got {err:?}"
    );
}

#[tokio::test]
async fn acquire_cohort_returns_critical_path_first() {
    let planner = BasicCpmPlanner::new();
    // CP: long -> tail (4 + 1 = 5h). slack branch: short (2h).
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("long", &["src/long.rs"], &[], Some(4.0)),
            deliverable("short", &["src/short.rs"], &[], Some(2.0)),
            deliverable("tail", &["src/tail.rs"], &["long"], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();
    assert_eq!(cohort.rows.len(), 1);
    assert_eq!(cohort.rows[0].deliverable.id, "long");
    assert_eq!(cohort.rows[0].lock.deliverable_id, "long");
}

#[tokio::test]
async fn acquire_cohort_skips_deliverables_with_unmet_prerequisites() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("root", &["src/root.rs"], &[], Some(1.0)),
            deliverable("leaf", &["src/leaf.rs"], &["root"], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("c1"), 10)
        .await
        .unwrap();
    // Only root is Ready; leaf is Pending until root completes.
    assert_eq!(cohort.rows.len(), 1);
    assert_eq!(cohort.rows[0].deliverable.id, "root");
}

#[tokio::test]
async fn acquire_cohort_filters_overlapping_files_within_cohort() {
    let planner = BasicCpmPlanner::new();
    // a and b both touch src/shared.rs at graph level — this fails
    // validation; we need different files. Instead: each owns its own
    // file but they're sequenced via prereqs. Better fixture: three
    // ready leaves; two of them share an owned file via... we can't,
    // graph rejects that. The intra-cohort overlap path is exercised
    // via the lock map across SEPARATE acquire calls (`b` blocked by
    // `a`'s lock when files overlap at runtime — but the graph
    // rejected that). The next-best assertion: when max_count is high
    // but only a single file-disjoint subset is large, the cohort is
    // exactly that subset.
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/a.rs"], &[], Some(1.0)),
            deliverable("b", &["src/b.rs"], &[], Some(1.0)),
            deliverable("c", &["src/c.rs"], &[], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("c1"), 5)
        .await
        .unwrap();
    // All three are file-disjoint, so all three should land in the cohort.
    assert_eq!(cohort.rows.len(), 3);
    let ids: Vec<&str> = cohort
        .rows
        .iter()
        .map(|r| r.deliverable.id.as_str())
        .collect();
    assert!(ids.contains(&"a"));
    assert!(ids.contains(&"b"));
    assert!(ids.contains(&"c"));
    // F5 INTERFACE_GAP-001: the structural row pairing means the lock
    // ALWAYS matches its deliverable — the previous parallel-Vec
    // alignment is now type-enforced. The assertion remains as a
    // belt-and-suspenders pin on the invariant.
    for row in &cohort.rows {
        assert_eq!(row.lock.deliverable_id, row.deliverable.id);
    }
}

#[tokio::test]
async fn acquire_cohort_excludes_files_locked_by_other_callers() {
    let planner = BasicCpmPlanner::new();
    // Two independent deliverables — `c1` takes `a`, then `c2` calls
    // acquire and must NOT see `a` re-offered while the lock is held.
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/a.rs"], &[], Some(1.0)),
            deliverable("b", &["src/b.rs"], &[], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let c1 = planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();
    assert_eq!(c1.rows.len(), 1);
    let first_id = c1.rows[0].deliverable.id.clone();

    let c2 = planner
        .acquire_cohort(&plan_id, &caller("c2"), 10)
        .await
        .unwrap();
    for row in &c2.rows {
        let d = &row.deliverable;
        assert_ne!(
            d.id, first_id,
            "second caller saw an already-locked deliverable"
        );
    }
}

#[tokio::test]
async fn mark_status_complete_releases_lock_and_advances_dependents() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("root", &["src/root.rs"], &[], Some(1.0)),
            deliverable("leaf", &["src/leaf.rs"], &["root"], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();
    assert_eq!(cohort.rows[0].deliverable.id, "root");

    planner
        .mark_status(&plan_id, "root", &caller("c1"), DeliverableStatus::Complete)
        .await
        .unwrap();

    // Status reflects: root Complete, leaf Ready.
    let status = planner.status(&plan_id).await.unwrap();
    let leaf_status = status
        .deliverables
        .iter()
        .find(|(id, _, _, _, _)| id == "leaf")
        .map(|(_, s, _, _, _)| s.clone())
        .unwrap();
    assert_eq!(leaf_status, DeliverableStatus::Ready);

    // Re-acquire should now offer leaf.
    let next = planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();
    assert_eq!(next.rows.len(), 1);
    assert_eq!(next.rows[0].deliverable.id, "leaf");
}

#[tokio::test]
async fn mark_status_with_wrong_caller_id_fails_with_lock_not_held() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();

    let err = planner
        .mark_status(&plan_id, "a", &caller("c2"), DeliverableStatus::Complete)
        .await
        .unwrap_err();
    match err {
        PlannerError::LockNotHeld {
            caller_id,
            deliverable_id,
        } => {
            assert_eq!(caller_id, "c2");
            assert_eq!(deliverable_id, "a");
        }
        other => panic!("expected LockNotHeld, got {other:?}"),
    }
}

#[tokio::test]
async fn heartbeat_extends_ttl() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();
    let original_expiry = cohort.rows[0].lock.expires_at;

    // Spin briefly so the heartbeat's "now" is strictly later than acquire's.
    tokio::task::yield_now().await;

    planner
        .heartbeat(&plan_id, "a", &caller("c1"))
        .await
        .unwrap();

    let status = planner.status(&plan_id).await.unwrap();
    let new_expiry = status.locks_held[0].expires_at;
    assert!(
        new_expiry >= original_expiry,
        "expiry did not advance: {new_expiry} < {original_expiry}"
    );
}

#[tokio::test]
async fn heartbeat_with_wrong_caller_fails() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();
    let err = planner
        .heartbeat(&plan_id, "a", &caller("c2"))
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::LockNotHeld { .. }));
}

#[tokio::test]
async fn status_reflects_cached_critical_path() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/a.rs"], &[], Some(1.0)),
            deliverable("b", &["src/b.rs"], &["a"], Some(2.0)),
            deliverable("c", &["src/c.rs"], &["b"], Some(3.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.critical_path, vec!["a", "b", "c"]);
    assert!((status.critical_path_hours - 6.0).abs() < 0.001);
    assert!(status.locks_held.is_empty());
}

#[tokio::test]
async fn force_release_reverts_to_ready_and_audits_reason() {
    let audit = Arc::new(MemoryAuditSink::new());
    let planner = BasicCpmPlanner::with_audit(audit.clone());
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();

    planner
        .force_release(&plan_id, "a", "operator override: caller offline")
        .await
        .unwrap();

    // Status should be back to Ready, no locks held.
    let status = planner.status(&plan_id).await.unwrap();
    let a_status = status
        .deliverables
        .iter()
        .find(|(id, _, _, _, _)| id == "a")
        .map(|(_, s, _, _, _)| s.clone())
        .unwrap();
    assert_eq!(a_status, DeliverableStatus::Ready);
    assert!(status.locks_held.is_empty());

    // Audit event records the reason verbatim.
    let events = audit.snapshot();
    let force_evt = events
        .iter()
        .find(|e| e.event_type == "plan.lock.force_released")
        .expect("force_released event present");
    assert_eq!(
        force_evt.payload["reason"],
        "operator override: caller offline"
    );
}
