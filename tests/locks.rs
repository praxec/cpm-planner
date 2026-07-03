//! Lock-lifecycle tests for `BasicCpmPlanner`.
//!
//! Includes the SPEC-mandated concurrent-acquire race test and the TTL
//! expiry test (both audited via a capturing `MemoryAuditSink`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use cpm_planner::BasicCpmPlanner;
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::plan::{CallerId, Deliverable, DeliverableStatus, PlanGraph};
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

/// Mutable, thread-safe `now` source. Tests advance it by calling `set`.
#[derive(Clone)]
struct TestClock {
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl TestClock {
    fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Arc::new(Mutex::new(start)),
        }
    }

    fn set(&self, when: DateTime<Utc>) {
        *self.now.lock().expect("test clock not poisoned") = when;
    }

    fn read(&self) -> DateTime<Utc> {
        *self.now.lock().expect("test clock not poisoned")
    }
}

#[tokio::test]
async fn ttl_expiry_test() {
    let audit = Arc::new(MemoryAuditSink::new());
    let clock = TestClock::new(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
    let clock_arc = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        audit.clone(),
        Duration::from_secs(60),
        Arc::new(move || clock_arc.read()),
    );

    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();
    assert_eq!(cohort.rows[0].deliverable.id, "a");

    // Advance time past TTL. Next acquire_cohort should reap the expired
    // lock and re-offer `a` to a different caller.
    clock.set(Utc.with_ymd_and_hms(2026, 1, 1, 0, 5, 0).unwrap());

    let cohort2 = planner
        .acquire_cohort(&plan_id, &caller("c2"), 1)
        .await
        .unwrap();
    assert_eq!(cohort2.rows.len(), 1);
    assert_eq!(cohort2.rows[0].deliverable.id, "a");
    assert_eq!(cohort2.rows[0].lock.caller_id, caller("c2"));

    // Audit log carries an expiry event for the original lock.
    let events = audit.snapshot();
    let expiry = events
        .iter()
        .find(|e| e.event_type == "plan.lock.expired")
        .expect("plan.lock.expired emitted");
    assert_eq!(expiry.payload["deliverable_id"], "a");
    assert_eq!(expiry.payload["last_caller_id"], "c1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_acquire_race_test() {
    // Repeat the test ten times to weed out any latent nondeterminism in
    // the locking implementation. Each iteration uses a fresh planner.
    for iteration in 0..10 {
        let planner = Arc::new(BasicCpmPlanner::new());
        // BasicCpmPlanner enforces graph-level file disjointness at
        // submit time, so we can't encode overlap statically. The race
        // we DO want to exercise is the runtime lock-map check: with 6
        // independent deliverables and 4 callers each requesting 2,
        // total demand (8) exceeds supply (6). The planner MUST
        // serialise via the top-level mutex such that no deliverable
        // (and therefore no owned file) is ever returned in two
        // separate cohorts.
        let graph = PlanGraph {
            deliverables: (0..6)
                .map(|i| deliverable(&format!("d{i}"), &[&format!("src/d{i}.rs")], &[], Some(1.0)))
                .collect(),
            max_chained_dispatch: None,
        };
        let plan_id = planner.submit_plan(graph).await.unwrap();

        let mut handles = Vec::new();
        for c in 0..4 {
            let planner_c = planner.clone();
            let plan_id_c = plan_id.clone();
            handles.push(tokio::spawn(async move {
                planner_c
                    .acquire_cohort(&plan_id_c, &caller(&format!("c{c}")), 2)
                    .await
                    .expect("acquire_cohort should not error")
            }));
        }

        let mut all_files: Vec<PathBuf> = Vec::new();
        let mut all_ids: Vec<String> = Vec::new();
        let mut total_returned = 0usize;
        for h in handles {
            let cohort = h.await.unwrap();
            total_returned += cohort.rows.len();
            for row in &cohort.rows {
                let d = &row.deliverable;
                all_ids.push(d.id.clone());
                for f in &d.owned_files {
                    all_files.push(f.clone());
                }
                // F5 INTERFACE_GAP-001: row pairing is type-enforced;
                // this assertion still documents the operator-facing
                // expectation.
                assert_eq!(
                    row.lock.deliverable_id, d.id,
                    "iter {iteration}: row.lock did not match row.deliverable"
                );
            }
        }

        // (a) no file appears in two callers' returned cohorts
        let mut seen = std::collections::HashSet::new();
        for f in &all_files {
            assert!(
                seen.insert(f.clone()),
                "iter {iteration}: file {f:?} appeared in two cohorts"
            );
        }

        // (b) no deliverable appears in two cohorts
        let mut seen_ids = std::collections::HashSet::new();
        for id in &all_ids {
            assert!(
                seen_ids.insert(id.clone()),
                "iter {iteration}: deliverable {id} appeared in two cohorts"
            );
        }

        // (c) total deliverables ≤ 6 (the graph size).
        assert!(
            total_returned <= 6,
            "iter {iteration}: over-allocation {total_returned} > 6"
        );
    }
}

#[tokio::test]
async fn audit_emission_on_lock_lifecycle() {
    let audit = Arc::new(MemoryAuditSink::new());
    let planner = BasicCpmPlanner::with_audit(audit.clone());
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();

    // Acquire -> acquired event.
    planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();

    // Complete -> released event.
    planner
        .mark_status(&plan_id, "a", &caller("c1"), DeliverableStatus::Complete)
        .await
        .unwrap();

    let types: Vec<String> = audit
        .snapshot()
        .iter()
        .map(|e| e.event_type.clone())
        .collect();
    assert!(types.contains(&"plan.lock.acquired".to_string()));
    assert!(types.contains(&"plan.lock.released".to_string()));

    // Released event should carry reason="completed".
    let released = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.lock.released")
        .expect("released event present");
    assert_eq!(released.payload["reason"], "completed");
}

#[tokio::test]
async fn failed_status_emits_released_with_reason_failed() {
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
        .mark_status(
            &plan_id,
            "a",
            &caller("c1"),
            DeliverableStatus::Failed {
                reason: "compilation broke".to_string(),
            },
        )
        .await
        .unwrap();

    let released = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.lock.released")
        .expect("released event present");
    assert_eq!(released.payload["reason"], "failed");
}

#[tokio::test]
async fn force_release_audit_includes_reason() {
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

    let reason = "operator escape: caller wedged";
    planner.force_release(&plan_id, "a", reason).await.unwrap();

    let evt = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.lock.force_released")
        .expect("force_released event present");
    assert_eq!(evt.payload["reason"], reason);
    assert_eq!(evt.payload["deliverable_id"], "a");
    assert_eq!(evt.payload["last_caller_id"], "c1");
}
