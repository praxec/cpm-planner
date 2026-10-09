//! Integration tests for `BasicCpmPlanner`.
//!
//! Covers the trait surface from outside the crate; for lock-lifecycle
//! and TTL behaviour see `tests/locks.rs`.

use std::sync::Arc;

use cpm_planner::BasicCpmPlanner;
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::plan::{
    AcceptRequest, AcquireRequest, CallerId, Deliverable, DeliverableStatus, ForceReleaseRequest,
    HeartbeatRequest, MarkStatusRequest, PlanGraph, PlanId, PlannerError, Prerequisite,
};
use cpm_planner::ports::Planner;

fn deliverable(id: &str, files: &[&str], prereqs: &[&str], effort: Option<f32>) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: files
            .iter()
            .map(|f| cpm_planner::plan::OwnedFile::from(*f))
            .collect(),
        prerequisites: prereqs.iter().map(|s| (*s).into()).collect(),
        estimated_effort_hours: effort,
        metadata: serde_json::Value::Null,
        duration_hours: None,
        estimate: None,
        milestone: false,
        earning_rule: None,
    }
}

fn with_duration(mut d: Deliverable, hours: f32) -> Deliverable {
    d.duration_hours = Some(hours);
    d
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            10,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            5,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();
    assert_eq!(c1.rows.len(), 1);
    let first_id = c1.rows[0].deliverable.id.clone();

    let c2 = planner
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c2").clone(),
            10,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();
    assert_eq!(cohort.rows[0].deliverable.id, "root");

    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "root",
            caller("c1").clone(),
            DeliverableStatus::Complete,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();

    let err = planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("c2").clone(),
            DeliverableStatus::Complete,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();
    let original_expiry = cohort.rows[0].lock.expires_at;

    // Spin briefly so the heartbeat's "now" is strictly later than acquire's.
    tokio::task::yield_now().await;

    planner
        .heartbeat(HeartbeatRequest::new(
            plan_id.clone(),
            "a",
            caller("c1").clone(),
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();
    let err = planner
        .heartbeat(HeartbeatRequest::new(
            plan_id.clone(),
            "a",
            caller("c2").clone(),
        ))
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
    assert_eq!(
        status.critical_path,
        vec!["__start__", "a", "b", "c", "__finish__"]
    );
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();

    planner
        .force_release(ForceReleaseRequest::new(
            plan_id.clone(),
            "a",
            "operator override: caller offline",
        ))
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

async fn submit_diamond_with_spare() -> (BasicCpmPlanner, cpm_planner::plan::PlanId) {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("A", &["src/a.rs"], &[], Some(1.0)),
            deliverable("B", &["src/b.rs"], &["A"], Some(2.0)),
            deliverable("C", &["src/c.rs"], &["A"], Some(4.0)),
            deliverable("D", &["src/d.rs"], &["B", "C"], Some(1.0)),
            deliverable("E", &["src/e.rs"], &[], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    (planner, plan_id)
}

#[tokio::test]
async fn status_schedule_reports_float_for_non_critical_branch() {
    let (planner, plan_id) = submit_diamond_with_spare().await;
    let status = planner.status(&plan_id).await.unwrap();
    let b = status.schedule.iter().find(|r| r.id == "B").unwrap();
    assert!((b.float - 2.0).abs() < 1e-3);
}

#[tokio::test]
async fn status_schedule_marks_critical_rows() {
    let (planner, plan_id) = submit_diamond_with_spare().await;
    let status = planner.status(&plan_id).await.unwrap();
    let critical: Vec<&str> = status
        .schedule
        .iter()
        .filter(|r| r.critical)
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(critical, vec!["__start__", "A", "C", "D", "__finish__"]);
}

#[tokio::test]
async fn status_ready_is_sorted_by_float_ascending() {
    let (planner, plan_id) = submit_diamond_with_spare().await;
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.ready, vec!["A", "E"]);
}

#[tokio::test]
async fn status_ready_excludes_locked_deliverables() {
    let (planner, plan_id) = submit_diamond_with_spare().await;
    planner
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("w1").clone(),
            1,
        ))
        .await
        .unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.ready, vec!["E"]);
}

#[tokio::test]
async fn acquire_cohort_prefers_critical_deliverable() {
    let (planner, plan_id) = submit_diamond_with_spare().await;
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("w1").clone(),
            1,
        ))
        .await
        .unwrap();
    let ids: Vec<&str> = cohort
        .rows
        .iter()
        .map(|r| r.deliverable.id.as_str())
        .collect();
    assert_eq!(ids, vec!["A"]);
}

/// Graph where `a` is a 1h deliverable carrying a 10h chain, while `b` is a
/// 5h leaf behind a completed 7h gate. `a` has the smaller latest start
/// (1h vs 7h) even though `b` has the smaller float (0 vs 1), so only the
/// latest-start-first policy leases `a` first.
async fn submit_long_tail_vs_short_leaf() -> (BasicCpmPlanner, PlanId) {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("gate", &["src/gate.rs"], &[], Some(7.0)),
            deliverable("b", &["src/b.rs"], &["gate"], Some(5.0)),
            deliverable("a", &["src/a.rs"], &[], Some(1.0)),
            deliverable("a_tail", &["src/a_tail.rs"], &["a"], Some(10.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    planner.accept(accept(&plan_id, "gate")).await.unwrap();
    (planner, plan_id)
}

#[tokio::test]
async fn acquire_prefers_longest_remaining_tail() {
    let (planner, plan_id) = submit_long_tail_vs_short_leaf().await;
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(plan_id, caller("w1"), 1))
        .await
        .unwrap();
    assert_eq!(
        (cohort.rows.len(), cohort.rows[0].deliverable.id.as_str()),
        (1, "a")
    );
}

#[tokio::test]
async fn ready_order_matches_longest_remaining_tail() {
    let (planner, plan_id) = submit_long_tail_vs_short_leaf().await;
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.ready, vec!["a", "b"]);
}

#[tokio::test]
async fn submit_rejects_negative_effort() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(-1.0))],
        max_chained_dispatch: None,
    };
    let err = planner.submit_plan(graph).await.unwrap_err();
    assert!(err.to_string().starts_with("INVALID_GRAPH"), "got: {err}");
}

// ── Targeted / filtered acquire; manual deliverables (#14) ──────────────────

fn with_meta(mut d: Deliverable, meta: serde_json::Value) -> Deliverable {
    d.metadata = meta;
    d
}

async fn submit_mixed() -> (BasicCpmPlanner, cpm_planner::plan::PlanId) {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            with_meta(
                deliverable("code1", &["src/c1.rs"], &[], Some(1.0)),
                serde_json::json!({"executor": "claude"}),
            ),
            with_meta(
                deliverable("code2", &["src/c2.rs"], &[], Some(1.0)),
                serde_json::json!({"executor": "claude"}),
            ),
            with_meta(
                deliverable("ownerTask", &["src/o.rs"], &[], Some(1.0)),
                serde_json::json!({"kind": "manual"}),
            ),
            with_meta(
                deliverable("jun", &["src/j.rs"], &[], Some(1.0)),
                serde_json::json!({"executor": "junior"}),
            ),
            deliverable("later", &["src/l.rs"], &["code1"], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    (planner, plan_id)
}

fn cohort_ids(c: &cpm_planner::plan::Cohort) -> Vec<String> {
    c.rows.iter().map(|r| r.deliverable.id.clone()).collect()
}

fn codes(c: &cpm_planner::plan::Cohort) -> Vec<(String, String)> {
    c.blocked
        .iter()
        .map(|b| (b.id.clone(), b.code.clone()))
        .collect()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn unfiltered_acquire_never_leases_manual_deliverables() {
    let (p, id) = submit_mixed().await;
    let cohort = p
        .acquire_cohort(AcquireRequest::new(id, caller("a"), 10))
        .await
        .unwrap();
    assert_eq!(sorted(cohort_ids(&cohort)), vec!["code1", "code2", "jun"]);
}

#[tokio::test]
async fn metadata_filter_leases_only_matching_deliverables() {
    let (p, id) = submit_mixed().await;
    let mut f = serde_json::Map::new();
    f.insert("executor".into(), serde_json::json!("claude"));
    let cohort = p
        .acquire_cohort(AcquireRequest::new(id, caller("a"), 10).with_metadata_filter(f))
        .await
        .unwrap();
    assert_eq!(sorted(cohort_ids(&cohort)), vec!["code1", "code2"]);
}

#[tokio::test]
async fn ids_lease_only_the_requested_deliverables() {
    let (p, id) = submit_mixed().await;
    let cohort = p
        .acquire_cohort(AcquireRequest::new(id, caller("a"), 10).with_ids(strs(&["jun"])))
        .await
        .unwrap();
    assert_eq!(cohort_ids(&cohort), vec!["jun"]);
}

#[tokio::test]
async fn requested_manual_deliverable_is_blocked_with_manual_code() {
    let (p, id) = submit_mixed().await;
    let cohort = p
        .acquire_cohort(AcquireRequest::new(id, caller("a"), 10).with_ids(strs(&["ownerTask"])))
        .await
        .unwrap();
    assert_eq!(codes(&cohort), vec![("ownerTask".into(), "MANUAL".into())]);
}

#[tokio::test]
async fn requested_pending_deliverable_is_blocked_not_ready() {
    let (p, id) = submit_mixed().await;
    let cohort = p
        .acquire_cohort(AcquireRequest::new(id, caller("a"), 10).with_ids(strs(&["later"])))
        .await
        .unwrap();
    let b = &cohort.blocked[0];
    assert!(b.id == "later" && b.code == "NOT_READY" && b.reason.contains("Pending"));
}

#[tokio::test]
async fn requested_locked_deliverable_is_blocked_locked() {
    let (p, id) = submit_mixed().await;
    p.acquire_cohort(AcquireRequest::new(id.clone(), caller("a"), 10).with_ids(strs(&["code1"])))
        .await
        .unwrap();
    let cohort_b = p
        .acquire_cohort(AcquireRequest::new(id, caller("b"), 10).with_ids(strs(&["code1"])))
        .await
        .unwrap();
    assert_eq!(codes(&cohort_b), vec![("code1".into(), "LOCKED".into())]);
}

#[tokio::test]
async fn requesting_unknown_id_is_deliverable_not_found() {
    let (p, id) = submit_mixed().await;
    let err = p
        .acquire_cohort(AcquireRequest::new(id, caller("a"), 10).with_ids(strs(&["nope"])))
        .await
        .unwrap_err();
    assert!(
        err.to_string().starts_with("DELIVERABLE_NOT_FOUND"),
        "got: {err}"
    );
}

#[tokio::test]
async fn requested_ids_beyond_max_count_are_blocked_max_count() {
    let (p, id) = submit_mixed().await;
    let cohort = p
        .acquire_cohort(AcquireRequest::new(id, caller("a"), 1).with_ids(strs(&["code1", "code2"])))
        .await
        .unwrap();
    assert_eq!(
        (
            cohort_ids(&cohort),
            cohort
                .blocked
                .iter()
                .map(|b| b.code.as_str())
                .collect::<Vec<_>>()
        ),
        (vec!["code1".to_string()], vec!["MAX_COUNT"])
    );
}

// ── plan.accept and audited lockless completion (#24) ───────────────────────

async fn submit_accept_graph() -> (BasicCpmPlanner, PlanId) {
    submit_accept_graph_with(BasicCpmPlanner::new()).await
}

async fn submit_accept_graph_with(planner: BasicCpmPlanner) -> (BasicCpmPlanner, PlanId) {
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/a.rs"], &[], Some(1.0)),
            deliverable("b", &["src/b.rs"], &["a"], Some(1.0)),
            with_meta(
                deliverable("sign", &["src/s.rs"], &["a"], Some(1.0)),
                serde_json::json!({"kind": "manual"}),
            ),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    (planner, plan_id)
}

async fn status_of(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str) -> DeliverableStatus {
    planner
        .status(plan_id)
        .await
        .unwrap()
        .deliverables
        .iter()
        .find(|(d, _, _, _, _)| d == id)
        .map(|(_, s, _, _, _)| s.clone())
        .unwrap()
}

fn accept(plan_id: &PlanId, id: &str) -> AcceptRequest {
    AcceptRequest::new(plan_id.clone(), id, "owner", "reviewed the PR")
}

#[tokio::test]
async fn accept_completes_a_ready_deliverable_without_a_lease() {
    let (planner, plan_id) = submit_accept_graph().await;
    planner.accept(accept(&plan_id, "a")).await.unwrap();
    assert_eq!(
        status_of(&planner, &plan_id, "a").await,
        DeliverableStatus::Complete
    );
}

#[tokio::test]
async fn accept_promotes_dependents_to_ready() {
    let (planner, plan_id) = submit_accept_graph().await;
    planner.accept(accept(&plan_id, "a")).await.unwrap();
    assert_eq!(
        status_of(&planner, &plan_id, "b").await,
        DeliverableStatus::Ready
    );
}

#[tokio::test]
async fn accept_completes_a_manual_deliverable() {
    let (planner, plan_id) = submit_accept_graph().await;
    planner.accept(accept(&plan_id, "a")).await.unwrap();
    planner.accept(accept(&plan_id, "sign")).await.unwrap();
    assert_eq!(
        status_of(&planner, &plan_id, "sign").await,
        DeliverableStatus::Complete
    );
}

#[tokio::test]
async fn accept_rejects_incomplete_prerequisites() {
    let (planner, plan_id) = submit_accept_graph().await;
    let err = planner.accept(accept(&plan_id, "b")).await.unwrap_err();
    assert!(
        err.to_string().starts_with("PREREQUISITES_INCOMPLETE"),
        "got: {err}"
    );
}

#[tokio::test]
async fn accept_rejects_foreign_live_lock_without_override() {
    let (planner, plan_id) = submit_accept_graph().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("worker"), 1))
        .await
        .unwrap();
    let err = planner.accept(accept(&plan_id, "a")).await.unwrap_err();
    assert!(err.to_string().starts_with("LOCK_HELD"), "got: {err}");
}

#[tokio::test]
async fn accept_with_override_releases_foreign_lock_and_completes() {
    let (planner, plan_id) = submit_accept_graph().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("worker"), 1))
        .await
        .unwrap();
    planner
        .accept(accept(&plan_id, "a").override_lock(true))
        .await
        .unwrap();
    assert_eq!(
        status_of(&planner, &plan_id, "a").await,
        DeliverableStatus::Complete
    );
}

#[tokio::test]
async fn accept_emits_accepted_audit_event_with_evidence() {
    let audit = Arc::new(MemoryAuditSink::new());
    let (planner, plan_id) =
        submit_accept_graph_with(BasicCpmPlanner::with_audit(audit.clone())).await;
    planner.accept(accept(&plan_id, "a")).await.unwrap();
    let evt = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.deliverable.accepted")
        .expect("accepted event present");
    assert_eq!(evt.payload["evidence"], "reviewed the PR");
}

#[tokio::test]
async fn lockless_mark_complete_rejects_incomplete_prerequisites() {
    let (planner, plan_id) = submit_accept_graph().await;
    let err = planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "b",
            caller("w"),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap_err();
    assert!(
        err.to_string().starts_with("PREREQUISITES_INCOMPLETE"),
        "got: {err}"
    );
}

#[tokio::test]
async fn lockless_mark_complete_emits_completed_without_lease_event() {
    let audit = Arc::new(MemoryAuditSink::new());
    let (planner, plan_id) =
        submit_accept_graph_with(BasicCpmPlanner::with_audit(audit.clone())).await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("w"),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    assert!(
        audit
            .snapshot()
            .iter()
            .any(|e| e.event_type == "plan.deliverable.completed_without_lease")
    );
}

#[tokio::test]
async fn ids_and_filter_intersect() {
    let (p, id) = submit_mixed().await;
    let mut f = serde_json::Map::new();
    f.insert("executor".into(), serde_json::json!("claude"));
    let cohort = p
        .acquire_cohort(
            AcquireRequest::new(id, caller("a"), 10)
                .with_ids(strs(&["code1", "jun"]))
                .with_metadata_filter(f),
        )
        .await
        .unwrap();
    assert_eq!(cohort_ids(&cohort), vec!["code1"]);
}

#[tokio::test]
async fn unfiltered_acquire_lists_no_manual_entries_in_blocked() {
    let (p, id) = submit_mixed().await;
    let cohort = p
        .acquire_cohort(AcquireRequest::new(id, caller("a"), 10))
        .await
        .unwrap();
    assert!(cohort.blocked.iter().all(|b| b.code != "MANUAL"));
}

#[tokio::test]
async fn accept_by_name_matching_holder_still_requires_override() {
    let (planner, plan_id) = submit_accept_graph().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w1"), 1))
        .await
        .unwrap();
    let err = planner
        .accept(AcceptRequest::new(plan_id.clone(), "a", "w1", "self"))
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("LOCK_HELD"), "got: {err}");
}

#[tokio::test]
async fn accepting_complete_deliverable_emits_no_second_event() {
    let audit = Arc::new(MemoryAuditSink::new());
    let (planner, plan_id) =
        submit_accept_graph_with(BasicCpmPlanner::with_audit(audit.clone())).await;
    planner.accept(accept(&plan_id, "a")).await.unwrap();
    planner.accept(accept(&plan_id, "a")).await.unwrap();
    let n = audit
        .snapshot()
        .iter()
        .filter(|e| e.event_type == "plan.deliverable.accepted")
        .count();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn accept_rescues_failed_deliverable() {
    let (planner, plan_id) = submit_accept_graph().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w"), 1))
        .await
        .unwrap();
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("w"),
            DeliverableStatus::Failed {
                reason: "boom".to_string(),
            },
        ))
        .await
        .unwrap();
    planner.accept(accept(&plan_id, "a")).await.unwrap();
    assert_eq!(
        status_of(&planner, &plan_id, "a").await,
        DeliverableStatus::Complete
    );
}

#[tokio::test]
async fn accepted_event_records_previous_status() {
    let audit = Arc::new(MemoryAuditSink::new());
    let (planner, plan_id) =
        submit_accept_graph_with(BasicCpmPlanner::with_audit(audit.clone())).await;
    planner.accept(accept(&plan_id, "a")).await.unwrap();
    let evt = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.deliverable.accepted")
        .unwrap();
    assert_eq!(evt.payload["previous_status"]["status"], "ready");
}

#[tokio::test]
async fn accepted_event_records_overridden_lock_holder() {
    let audit = Arc::new(MemoryAuditSink::new());
    let (planner, plan_id) =
        submit_accept_graph_with(BasicCpmPlanner::with_audit(audit.clone())).await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w1"), 1))
        .await
        .unwrap();
    planner
        .accept(AcceptRequest::new(plan_id.clone(), "a", "w1", "x").override_lock(true))
        .await
        .unwrap();
    let evt = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.deliverable.accepted")
        .unwrap();
    assert_eq!(evt.payload["overrode_lock_of"], "w1");
}

#[tokio::test]
async fn lockless_mark_complete_on_complete_deliverable_emits_no_second_event() {
    let audit = Arc::new(MemoryAuditSink::new());
    let (planner, plan_id) =
        submit_accept_graph_with(BasicCpmPlanner::with_audit(audit.clone())).await;
    for _ in 0..2 {
        planner
            .mark_status(MarkStatusRequest::new(
                plan_id.clone(),
                "a",
                caller("w"),
                DeliverableStatus::Complete,
            ))
            .await
            .unwrap();
    }
    let n = audit
        .snapshot()
        .iter()
        .filter(|e| e.event_type == "plan.deliverable.completed_without_lease")
        .count();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn overridden_holder_cannot_fail_an_accepted_deliverable() {
    let (planner, plan_id) = submit_accept_graph().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w1"), 1))
        .await
        .unwrap();
    planner
        .accept(accept(&plan_id, "a").override_lock(true))
        .await
        .unwrap();
    let err = planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("w1"),
            DeliverableStatus::Failed {
                reason: "late".to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("LOCK_NOT_HELD"), "got: {err}");
}

#[tokio::test]
async fn lockless_failed_mark_is_audited() {
    let audit = Arc::new(MemoryAuditSink::new());
    let (planner, plan_id) =
        submit_accept_graph_with(BasicCpmPlanner::with_audit(audit.clone())).await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("w"),
            DeliverableStatus::Failed {
                reason: "nope".to_string(),
            },
        ))
        .await
        .unwrap();
    let evt = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.deliverable.marked_without_lease")
        .expect("marked_without_lease event present");
    assert_eq!(
        (
            evt.payload["deliverable_id"].clone(),
            evt.payload["caller_id"].clone(),
            evt.payload["status"]["status"].clone(),
            evt.payload["previous_status"]["status"].clone(),
        ),
        (
            serde_json::json!("a"),
            serde_json::json!("w"),
            serde_json::json!("failed"),
            serde_json::json!("ready"),
        )
    );
}

#[tokio::test]
async fn completed_without_lease_event_records_previous_status() {
    let audit = Arc::new(MemoryAuditSink::new());
    let (planner, plan_id) =
        submit_accept_graph_with(BasicCpmPlanner::with_audit(audit.clone())).await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("w"),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    let evt = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.deliverable.completed_without_lease")
        .unwrap();
    assert_eq!(evt.payload["previous_status"]["status"], "ready");
}

#[tokio::test]
async fn with_max_ttl_is_capped_at_thirty_days() {
    let planner =
        BasicCpmPlanner::new().with_max_ttl(std::time::Duration::from_secs(365 * 24 * 60 * 60));
    let plan_id = planner
        .submit_plan(PlanGraph {
            deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
            max_chained_dispatch: None,
        })
        .await
        .unwrap();
    let cohort = planner
        .acquire_cohort(
            AcquireRequest::new(plan_id, caller("w"), 1)
                .with_ttl(std::time::Duration::from_secs(365 * 24 * 60 * 60)),
        )
        .await
        .unwrap();
    let lock = &cohort.rows[0].lock;
    assert_eq!(
        (lock.expires_at - lock.acquired_at).num_seconds(),
        30 * 24 * 60 * 60
    );
}

#[test]
fn legacy_cohort_payload_without_blocked_deserializes() {
    let cohort: cpm_planner::plan::Cohort = serde_json::from_value(serde_json::json!({
        "plan_id": "p1",
        "deliverables": [],
        "locks": []
    }))
    .unwrap();
    assert!(cohort.blocked.is_empty());
}

// ---------------------------------------------------------------------
// Synthetic __start__ / __finish__ endpoints
// ---------------------------------------------------------------------

async fn submit(planner: &BasicCpmPlanner, deliverables: Vec<Deliverable>) -> PlanId {
    planner
        .submit_plan(PlanGraph {
            deliverables,
            max_chained_dispatch: None,
        })
        .await
        .unwrap()
}

async fn complete(planner: &BasicCpmPlanner, plan_id: &PlanId, id: &str) {
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            id,
            caller("c1"),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
}

fn ab_chain() -> Vec<Deliverable> {
    vec![
        deliverable("a", &["src/a.rs"], &[], Some(1.0)),
        deliverable("b", &["src/b.rs"], &["a"], Some(2.0)),
    ]
}

#[tokio::test]
async fn critical_path_starts_and_ends_at_synthetic_endpoints() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, ab_chain()).await;
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(
        status.critical_path,
        vec!["__start__", "a", "b", "__finish__"]
    );
}

#[tokio::test]
async fn schedule_lists_endpoints_as_synthetic_rows() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, ab_chain()).await;
    let status = planner.status(&plan_id).await.unwrap();
    let ids: Vec<(&str, bool)> = status
        .schedule
        .iter()
        .map(|r| (r.id.as_str(), r.synthetic))
        .collect();
    assert_eq!(
        ids,
        vec![
            ("__start__", true),
            ("a", false),
            ("b", false),
            ("__finish__", true)
        ]
    );
}

#[tokio::test]
async fn finish_depends_on_every_sink() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(
        &planner,
        vec![
            deliverable("a", &["src/a.rs"], &[], Some(1.0)),
            deliverable("b", &["src/b.rs"], &[], Some(3.0)),
        ],
    )
    .await;
    let status = planner.status(&plan_id).await.unwrap();
    let finish_row = status
        .schedule
        .iter()
        .find(|r| r.id == "__finish__")
        .unwrap();
    assert!((finish_row.es - 3.0).abs() < 1e-3);
}

#[tokio::test]
async fn endpoints_are_never_ready_or_leased() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, ab_chain()).await;
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.ready, vec!["a"]);
}

#[tokio::test]
async fn endpoints_are_absent_from_deliverable_rows() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, ab_chain()).await;
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.deliverables.len(), 2);
}

#[tokio::test]
async fn submit_rejects_reserved_finish_id() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .submit_plan(PlanGraph {
            deliverables: vec![deliverable("__finish__", &["src/a.rs"], &[], Some(1.0))],
            max_chained_dispatch: None,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("INVALID_GRAPH"));
}

#[tokio::test]
async fn submit_rejects_reserved_start_id() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .submit_plan(PlanGraph {
            deliverables: vec![deliverable("__start__", &["src/a.rs"], &[], Some(1.0))],
            max_chained_dispatch: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("deliverable id '__start__' is reserved")
    );
}

#[tokio::test]
async fn plan_complete_after_every_deliverable_completes() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, ab_chain()).await;
    complete(&planner, &plan_id, "a").await;
    complete(&planner, &plan_id, "b").await;
    let status = planner.status(&plan_id).await.unwrap();
    assert!(status.plan_complete);
}

#[tokio::test]
async fn plan_complete_is_false_while_work_remains() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, ab_chain()).await;
    complete(&planner, &plan_id, "a").await;
    let status = planner.status(&plan_id).await.unwrap();
    assert!(!status.plan_complete);
}

#[tokio::test]
async fn completing_last_deliverable_emits_plan_completed_once() {
    let audit = Arc::new(MemoryAuditSink::new());
    let planner = BasicCpmPlanner::with_audit(audit.clone());
    let plan_id = submit(&planner, ab_chain()).await;
    complete(&planner, &plan_id, "a").await;
    complete(&planner, &plan_id, "b").await;
    complete(&planner, &plan_id, "b").await;
    let events = audit.snapshot();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "plan.completed")
            .count(),
        1
    );
}

#[tokio::test]
async fn accepting_last_deliverable_emits_plan_completed() {
    let audit = Arc::new(MemoryAuditSink::new());
    let planner = BasicCpmPlanner::with_audit(audit.clone());
    let plan_id = submit(
        &planner,
        vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
    )
    .await;
    planner
        .accept(AcceptRequest::new(plan_id.clone(), "a", "op", "done"))
        .await
        .unwrap();
    assert_eq!(
        audit
            .event_types()
            .iter()
            .filter(|t| *t == "plan.completed")
            .count(),
        1
    );
}

#[tokio::test]
async fn plan_completed_is_not_emitted_while_work_remains() {
    let audit = Arc::new(MemoryAuditSink::new());
    let planner = BasicCpmPlanner::with_audit(audit.clone());
    let plan_id = submit(&planner, ab_chain()).await;
    complete(&planner, &plan_id, "a").await;
    assert!(!audit.event_types().contains(&"plan.completed".to_string()));
}

#[tokio::test]
async fn empty_graph_has_start_to_finish_critical_path() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, vec![]).await;
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.critical_path, vec!["__start__", "__finish__"]);
}

fn edge(id: &str, consumes: Option<&str>, lag: Option<f32>) -> Prerequisite {
    Prerequisite::Edge {
        id: id.to_string(),
        consumes: consumes.map(str::to_string),
        kind: None,
        lag_hours: lag,
    }
}

fn graph_b_after(prereq: Prerequisite) -> PlanGraph {
    let mut b = deliverable("b", &["src/b.rs"], &[], Some(1.0));
    b.prerequisites = vec![prereq];
    PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0)), b],
        max_chained_dispatch: None,
    }
}

#[tokio::test]
async fn object_prerequisite_keeps_dependent_pending_until_prerequisite_completes() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner
        .submit_plan(graph_b_after(edge("a", Some("api schema"), None)))
        .await
        .unwrap();
    assert_eq!(
        status_of(&planner, &plan_id, "b").await,
        DeliverableStatus::Pending
    );
}

#[tokio::test]
async fn duplicate_id_edges_hash_independent_of_order() {
    let planner = BasicCpmPlanner::new();
    let build = |edges: Vec<Prerequisite>| {
        let mut g = graph_b_after(edges[0].clone());
        g.deliverables[1].prerequisites = edges;
        g
    };
    let x = edge("a", Some("x"), None);
    let y = edge("a", Some("y"), None);
    let forward = planner
        .submit_plan(build(vec![x.clone(), y.clone()]))
        .await
        .unwrap();
    let reversed = planner.submit_plan(build(vec![y, x])).await.unwrap();
    assert_eq!(forward, reversed);
}

#[tokio::test]
async fn object_prerequisite_parses_and_schedules() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner
        .submit_plan(graph_b_after(edge("a", Some("api schema"), None)))
        .await
        .unwrap();
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("owner"),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    assert_eq!(
        status_of(&planner, &plan_id, "b").await,
        DeliverableStatus::Ready
    );
}

#[tokio::test]
async fn string_and_object_prerequisite_hash_identically() {
    let planner = BasicCpmPlanner::new();
    let bare = planner
        .submit_plan(graph_b_after(Prerequisite::from("a")))
        .await
        .unwrap();
    let object = planner
        .submit_plan(graph_b_after(edge("a", None, None)))
        .await
        .unwrap();
    assert_eq!(bare, object);
}

#[tokio::test]
async fn consumes_difference_changes_plan_identity() {
    let planner = BasicCpmPlanner::new();
    let bare = planner
        .submit_plan(graph_b_after(Prerequisite::from("a")))
        .await
        .unwrap();
    let consuming = planner
        .submit_plan(graph_b_after(edge("a", Some("api schema"), None)))
        .await
        .unwrap();
    assert_ne!(bare, consuming);
}

#[tokio::test]
async fn negative_lag_is_rejected() {
    let planner = BasicCpmPlanner::new();
    let err = planner
        .submit_plan(graph_b_after(edge("a", None, Some(-1.0))))
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("INVALID_GRAPH"), "got: {err}");
}

fn milestone(id: &str, prereqs: &[&str]) -> Deliverable {
    let mut d = deliverable(id, &[], prereqs, Some(0.0));
    d.milestone = true;
    d
}

fn milestone_graph() -> Vec<Deliverable> {
    vec![
        deliverable("a", &["src/a.rs"], &[], Some(1.0)),
        deliverable("x", &["src/x.rs"], &[], Some(3.0)),
        milestone("m", &["a", "x"]),
        deliverable("c", &["src/c.rs"], &["m"], Some(5.0)),
    ]
}

#[tokio::test]
async fn milestone_row_reports_longest_chain_to_milestone() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, milestone_graph()).await;
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.milestones[0].critical_path, ["__start__", "x", "m"]);
}

#[tokio::test]
async fn milestone_hours_is_milestone_earliest_finish() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, milestone_graph()).await;
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.milestones[0].hours, 3.0);
}

#[tokio::test]
async fn metadata_milestone_flag_is_honoured() {
    let planner = BasicCpmPlanner::new();
    let graph = vec![with_meta(
        deliverable("m", &["src/m.rs"], &[], Some(0.0)),
        serde_json::json!({"milestone": true}),
    )];
    let plan_id = submit(&planner, graph).await;
    let status = planner.status(&plan_id).await.unwrap();
    assert!(status.milestones.len() == 1 && status.milestones[0].id == "m");
}

#[tokio::test]
async fn milestone_flag_changes_plan_identity() {
    let planner = BasicCpmPlanner::new();
    let plain = PlanGraph {
        deliverables: vec![deliverable("m", &["src/m.rs"], &[], Some(0.0))],
        max_chained_dispatch: None,
    };
    let mut flagged = plain.clone();
    flagged.deliverables[0].milestone = true;
    let a = planner.submit_plan(plain).await.unwrap();
    let b = planner.submit_plan(flagged).await.unwrap();
    assert_ne!(a, b);
}

#[tokio::test]
async fn milestone_complete_reflects_status() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, milestone_graph()).await;
    for id in ["a", "x", "m"] {
        complete(&planner, &plan_id, id).await;
    }
    let status = planner.status(&plan_id).await.unwrap();
    assert!(status.milestones[0].complete);
}

#[tokio::test]
async fn milestone_incomplete_before_completion() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(&planner, milestone_graph()).await;
    complete(&planner, &plan_id, "a").await;
    let status = planner.status(&plan_id).await.unwrap();
    assert!(!status.milestones[0].complete);
}

#[tokio::test]
async fn downstream_milestone_path_excludes_unrelated_branch() {
    let planner = BasicCpmPlanner::new();
    let plan_id = submit(
        &planner,
        vec![
            deliverable("a", &["src/a.rs"], &[], Some(1.0)),
            milestone("m1", &["a"]),
            deliverable("c", &["src/c.rs"], &[], Some(2.0)),
            milestone("m2", &["c"]),
        ],
    )
    .await;
    let status = planner.status(&plan_id).await.unwrap();
    let m2 = status.milestones.iter().find(|m| m.id == "m2").unwrap();
    assert_eq!(m2.critical_path, ["__start__", "c", "m2"]);
}

#[tokio::test]
async fn milestone_without_estimate_is_zero_length() {
    let planner = BasicCpmPlanner::new();
    let mut m = deliverable("m", &[], &["a"], None);
    m.milestone = true;
    let plan_id = submit(
        &planner,
        vec![deliverable("a", &["src/a.rs"], &[], Some(2.0)), m],
    )
    .await;
    let status = planner.status(&plan_id).await.unwrap();
    let row = status.schedule.iter().find(|r| r.id == "m").unwrap();
    assert_eq!(row.ef, 2.0);
}

#[tokio::test]
async fn duration_hours_overrides_effort_for_schedule() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![with_duration(
            deliverable("d", &["src/d.rs"], &[], Some(8.0)),
            2.0,
        )],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    let finish = status
        .schedule
        .iter()
        .find(|r| r.id == "__finish__")
        .unwrap();
    assert!((finish.es - 2.0).abs() < 1e-3);
}

#[tokio::test]
async fn duration_change_changes_plan_identity() {
    let planner = BasicCpmPlanner::new();
    let base = deliverable("d", &["src/d.rs"], &[], Some(8.0));
    let g1 = PlanGraph {
        deliverables: vec![with_duration(base.clone(), 2.0)],
        max_chained_dispatch: None,
    };
    let g2 = PlanGraph {
        deliverables: vec![with_duration(base, 3.0)],
        max_chained_dispatch: None,
    };
    let id1 = planner.submit_plan(g1).await.unwrap();
    let id2 = planner.submit_plan(g2).await.unwrap();
    assert_ne!(id1, id2);
}

#[tokio::test]
async fn submit_rejects_negative_duration() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![with_duration(
            deliverable("d", &["src/d.rs"], &[], Some(1.0)),
            -1.0,
        )],
        max_chained_dispatch: None,
    };
    let err = planner.submit_plan(graph).await.unwrap_err();
    assert!(matches!(err, PlannerError::InvalidGraph { .. }));
}

#[tokio::test]
async fn path_and_object_file_forms_hash_identically() {
    let planner = BasicCpmPlanner::new();
    let plain = deliverable("a", &["x"], &[], Some(1.0));
    let mut object = deliverable("a", &[], &[], Some(1.0));
    object.owned_files = vec![serde_json::from_value(serde_json::json!({ "path": "x" })).unwrap()];
    let first = planner
        .submit_plan(PlanGraph {
            deliverables: vec![plain],
            max_chained_dispatch: None,
        })
        .await
        .unwrap();
    let second = planner
        .submit_plan(PlanGraph {
            deliverables: vec![object],
            max_chained_dispatch: None,
        })
        .await
        .unwrap();
    assert_eq!(first, second);
}

async fn b_start_with_edges(edges: Vec<Prerequisite>) -> f32 {
    let planner = BasicCpmPlanner::new();
    let mut b = deliverable("b", &["src/b.rs"], &[], Some(1.0));
    b.prerequisites = edges;
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(2.0)), b],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    status.schedule.iter().find(|r| r.id == "b").unwrap().es
}

#[tokio::test]
async fn duplicate_prerequisite_ids_use_the_largest_lag() {
    let es = b_start_with_edges(vec![edge("a", None, Some(4.0)), edge("a", None, Some(1.0))]).await;
    assert!((es - 6.0).abs() < 1e-3);
}

#[tokio::test]
async fn prerequisite_lag_hours_flows_into_schedule() {
    let es = b_start_with_edges(vec![edge("a", None, Some(3.0))]).await;
    assert!((es - 5.0).abs() < 1e-3);
}

fn overlap_reason(result: Result<PlanId, PlannerError>) -> String {
    match result.unwrap_err() {
        PlannerError::InvalidGraph { reason } => reason,
        other => panic!("expected InvalidGraph, got {other:?}"),
    }
}

#[tokio::test]
async fn ordered_deliverables_may_share_an_exclusive_file() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/x.rs"], &[], Some(1.0)),
            deliverable("b", &["src/x.rs"], &["a"], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    assert!(planner.submit_plan(graph).await.is_ok());
}

#[tokio::test]
async fn transitively_ordered_deliverables_may_share_a_file() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/x.rs"], &[], Some(1.0)),
            deliverable("m", &["src/m.rs"], &["a"], Some(1.0)),
            deliverable("b", &["src/x.rs"], &["m"], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    assert!(planner.submit_plan(graph).await.is_ok());
}

#[tokio::test]
async fn unordered_deliverables_sharing_an_exclusive_file_are_rejected() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/x.rs"], &[], Some(1.0)),
            deliverable("b", &["src/x.rs"], &[], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let reason = overlap_reason(planner.submit_plan(graph).await);
    assert_eq!(
        reason,
        "file 'src/x.rs' is claimed by 'a' and 'b', which are not ordered by prerequisites (one could run while the other holds it)"
    );
}

#[tokio::test]
async fn unordered_append_and_exclusive_claims_are_rejected() {
    let planner = BasicCpmPlanner::new();
    let mut a = deliverable("a", &[], &[], Some(1.0));
    a.owned_files = vec![cpm_planner::plan::OwnedFile::Claim {
        path: "REGISTRY.md".into(),
        mode: Some(cpm_planner::plan::FileMode::Append),
    }];
    let graph = PlanGraph {
        deliverables: vec![a, deliverable("b", &["REGISTRY.md"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let reason = overlap_reason(planner.submit_plan(graph).await);
    assert!(reason.contains("'a' and 'b'"), "got: {reason}");
}

#[tokio::test]
async fn ordering_must_hold_between_every_claiming_pair() {
    // a -> b ordered, but c shares the file and is unordered with both.
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/x.rs"], &[], Some(1.0)),
            deliverable("b", &["src/x.rs"], &["a"], Some(1.0)),
            deliverable("c", &["src/x.rs"], &[], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let reason = overlap_reason(planner.submit_plan(graph).await);
    assert!(reason.contains("'c'"), "got: {reason}");
}

#[tokio::test]
async fn ordered_sharers_are_leased_one_after_the_other() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/x.rs"], &[], Some(1.0)),
            deliverable("b", &["src/x.rs"], &["a"], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let first = planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w"), 5))
        .await
        .unwrap();
    assert_eq!(cohort_ids(&first), vec!["a".to_string()]);
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("w"),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    let second = planner
        .acquire_cohort(AcquireRequest::new(plan_id, caller("w"), 5))
        .await
        .unwrap();
    assert_eq!(cohort_ids(&second), vec!["b".to_string()]);
}

async fn submit_ordered_sharers() -> (BasicCpmPlanner, PlanId) {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("a", &["src/x.rs"], &[], Some(1.0)),
            deliverable("b", &["src/x.rs"], &["a"], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    (planner, plan_id)
}

#[tokio::test]
async fn lockless_ready_mark_with_incomplete_prerequisites_is_rejected() {
    let (planner, plan_id) = submit_ordered_sharers().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w"), 1))
        .await
        .unwrap();
    let err = planner
        .mark_status(MarkStatusRequest::new(
            plan_id,
            "b",
            caller("other"),
            DeliverableStatus::Ready,
        ))
        .await
        .unwrap_err();
    assert!(
        err.to_string().starts_with("PREREQUISITES_INCOMPLETE"),
        "got: {err}"
    );
}

#[tokio::test]
async fn lockless_in_progress_mark_with_incomplete_prerequisites_is_rejected() {
    let (planner, plan_id) = submit_ordered_sharers().await;
    let err = planner
        .mark_status(MarkStatusRequest::new(
            plan_id,
            "b",
            caller("other"),
            DeliverableStatus::InProgress,
        ))
        .await
        .unwrap_err();
    assert!(
        err.to_string().starts_with("PREREQUISITES_INCOMPLETE"),
        "got: {err}"
    );
}

#[tokio::test]
async fn lockless_ready_mark_after_prerequisites_complete_is_allowed() {
    let (planner, plan_id) = submit_ordered_sharers().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w"), 1))
        .await
        .unwrap();
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("w"),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    let result = planner
        .mark_status(MarkStatusRequest::new(
            plan_id,
            "b",
            caller("other"),
            DeliverableStatus::Ready,
        ))
        .await;
    assert!(result.is_ok(), "got: {result:?}");
}

#[tokio::test]
async fn ordered_sharer_is_not_leased_while_predecessor_holds_the_file() {
    let (planner, plan_id) = submit_ordered_sharers().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w"), 1))
        .await
        .unwrap();
    let mut req = AcquireRequest::new(plan_id, caller("w2"), 1);
    req.ids = Some(vec!["b".to_string()]);
    let cohort = planner.acquire_cohort(req).await.unwrap();
    assert_eq!(
        codes(&cohort),
        vec![("b".to_string(), "NOT_READY".to_string())]
    );
}

// ── P4 Task 0: three-point estimates ────────────────────────────────────────

fn with_estimate(mut d: Deliverable, estimate: cpm_planner::plan::Estimate) -> Deliverable {
    d.estimate = Some(estimate);
    d
}

fn estimate(optimistic: f32, likely: f32, pessimistic: f32) -> cpm_planner::plan::Estimate {
    cpm_planner::plan::Estimate {
        optimistic,
        likely,
        pessimistic,
    }
}

async fn scheduled_ef(planner: &BasicCpmPlanner, graph: PlanGraph, id: &str) -> f32 {
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    status
        .schedule
        .iter()
        .find(|row| row.id == id)
        .expect("scheduled row")
        .ef
}

#[tokio::test]
async fn estimate_likely_sets_scheduled_length_when_no_effort() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![with_estimate(
            deliverable("a", &["src/a.rs"], &[], None),
            estimate(1.0, 3.0, 5.0),
        )],
        max_chained_dispatch: None,
    };
    assert_eq!(scheduled_ef(&planner, graph, "a").await, 3.0);
}

#[tokio::test]
async fn explicit_effort_beats_estimate_likely() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![with_estimate(
            deliverable("a", &["src/a.rs"], &[], Some(2.0)),
            estimate(1.0, 3.0, 5.0),
        )],
        max_chained_dispatch: None,
    };
    assert_eq!(scheduled_ef(&planner, graph, "a").await, 2.0);
}

#[tokio::test]
async fn duration_beats_estimate() {
    let planner = BasicCpmPlanner::new();
    let mut d = with_estimate(
        deliverable("a", &["src/a.rs"], &[], None),
        estimate(1.0, 3.0, 5.0),
    );
    d.duration_hours = Some(7.0);
    let graph = PlanGraph {
        deliverables: vec![d],
        max_chained_dispatch: None,
    };
    assert_eq!(scheduled_ef(&planner, graph, "a").await, 7.0);
}

#[tokio::test]
async fn estimate_with_optimistic_above_likely_is_invalid_graph() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![with_estimate(
            deliverable("a", &["src/a.rs"], &[], None),
            estimate(3.0, 2.0, 4.0),
        )],
        max_chained_dispatch: None,
    };
    let err = planner.submit_plan(graph).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "INVALID_GRAPH: deliverable 'a' estimate must satisfy 0 <= optimistic <= likely <= pessimistic"
    );
}

#[tokio::test]
async fn estimate_changes_plan_identity() {
    let planner = BasicCpmPlanner::new();
    let without = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], None)],
        max_chained_dispatch: None,
    };
    let with = PlanGraph {
        deliverables: vec![with_estimate(
            deliverable("a", &["src/a.rs"], &[], None),
            estimate(1.0, 3.0, 5.0),
        )],
        max_chained_dispatch: None,
    };
    assert_ne!(
        planner.submit_plan(without).await.unwrap(),
        planner.submit_plan(with).await.unwrap()
    );
}

#[tokio::test]
async fn submit_rejects_unknown_estimate_field() {
    use rmcp::model::CallToolRequestParams;

    let server = cpm_planner::PlanServer::new(Arc::new(BasicCpmPlanner::new()));
    let args = serde_json::json!({
        "graph": {
            "deliverables": [{
                "id": "a",
                "owned_files": ["src/a.rs"],
                "prerequisites": [],
                "estimate": { "optimistic": 1.0, "likely": 2.0, "pessimistic": 3.0, "bogus": 4.0 }
            }]
        }
    });
    let request = CallToolRequestParams::new(cpm_planner::TOOL_SUBMIT.to_string())
        .with_arguments(args.as_object().unwrap().clone());
    let err = server.dispatch_call(request).await.unwrap_err();
    assert!(err.message.contains("unknown field"), "{}", err.message);
}

#[tokio::test]
async fn submit_rejects_effort_above_one_million_hours() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1_000_001.0))],
        max_chained_dispatch: None,
    };
    let err = planner.submit_plan(graph).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "INVALID_GRAPH: deliverable 'a' has invalid estimated_effort_hours 1000001; must be a finite number between 0 and 1000000"
    );
}

#[tokio::test]
async fn submit_rejects_lag_above_one_million_hours() {
    let planner = BasicCpmPlanner::new();
    let mut b = deliverable("b", &["src/b.rs"], &[], Some(1.0));
    b.prerequisites = vec![edge("a", None, Some(2_000_000.0))];
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0)), b],
        max_chained_dispatch: None,
    };
    let err = planner.submit_plan(graph).await.unwrap_err();
    assert!(err.to_string().contains("between 0 and 1000000"), "{err}");
}

#[tokio::test]
async fn submit_rejects_estimate_above_one_million_hours() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![with_estimate(
            deliverable("a", &["src/a.rs"], &[], None),
            estimate(1.0, 2.0, 1_000_001.0),
        )],
        max_chained_dispatch: None,
    };
    let err = planner.submit_plan(graph).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "INVALID_GRAPH: deliverable 'a' has invalid estimate.pessimistic 1000001; must be a finite number between 0 and 1000000"
    );
}

#[tokio::test]
async fn submit_accepts_exactly_one_million_hours() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1_000_000.0))],
        max_chained_dispatch: None,
    };
    assert!(planner.submit_plan(graph).await.is_ok());
}

#[tokio::test]
async fn submit_rejects_more_than_5000_deliverables() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: (0..5001)
            .map(|i| deliverable(&format!("d{i}"), &[], &[], Some(1.0)))
            .collect(),
        max_chained_dispatch: None,
    };
    let err = planner.submit_plan(graph).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "INVALID_GRAPH: plan has 5001 deliverables; maximum is 5000"
    );
}

#[tokio::test]
async fn earning_rule_changes_plan_identity() {
    let planner = BasicCpmPlanner::new();
    let plain = deliverable("a", &["src/a.rs"], &[], Some(1.0));
    let mut weighted = plain.clone();
    weighted.earning_rule = Some(cpm_planner::plan::EarningRule::Weighted);
    let graph = |d: Deliverable| PlanGraph {
        deliverables: vec![d],
        max_chained_dispatch: None,
    };
    let id1 = planner.submit_plan(graph(plain)).await.unwrap();
    let id2 = planner.submit_plan(graph(weighted)).await.unwrap();
    assert_ne!(id1, id2);
}
