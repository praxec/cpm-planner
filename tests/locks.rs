//! Lock-lifecycle tests for `BasicCpmPlanner`.
//!
//! Includes the SPEC-mandated concurrent-acquire race test and the TTL
//! expiry test (both audited via a capturing `MemoryAuditSink`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::plan::{CallerId, Deliverable, DeliverableStatus, PlanGraph, PlannerError};
use cpm_planner::ports::Planner;
use cpm_planner::{BasicCpmPlanner, MAX_ATTEMPTS, MAX_LAPSES};

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

/// Defect fix: a lease lost to the ENVIRONMENT (driver killed externally,
/// lock lapses via TTL, no terminal mark) is NOT an implementation
/// attempt. MAX_ATTEMPTS environmental lapses must not trip the failure
/// circuit-breaker — the deliverable stays leasable.
#[tokio::test]
async fn environmental_lapses_do_not_trip_the_failure_circuit_breaker() {
    let audit = Arc::new(MemoryAuditSink::new());
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::new(t0);
    let clock_arc = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        audit.clone(),
        Duration::from_secs(60),
        Arc::new(move || clock_arc.read()),
    );

    let graph = PlanGraph {
        deliverables: vec![deliverable("healthy", &["src/healthy.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();

    // MAX_ATTEMPTS leases, each lost environmentally: no mark_status, the
    // lock is left to lapse via TTL, and the acquire-path reap reverts the
    // deliverable to Ready.
    for lapse in 1..=MAX_ATTEMPTS {
        let cohort = planner
            .acquire_cohort(&plan_id, &caller(&format!("killed-{lapse}")), 1)
            .await
            .unwrap();
        assert_eq!(cohort.rows.len(), 1, "lease {lapse} must be granted");
        clock.set(t0 + chrono::Duration::minutes(5 * i64::from(lapse)));
    }

    // The next acquire must STILL lease it: lapses are not failed attempts.
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("fresh"), 1)
        .await
        .unwrap();
    assert_eq!(
        cohort.rows.len(),
        1,
        "a healthy deliverable was circuit-broken by environmental lapses"
    );
    assert_eq!(cohort.rows[0].deliverable.id, "healthy");

    // Counters tell the two stories apart: every lease counted, every
    // lapse counted, zero implementation failures.
    let status = planner.status(&plan_id).await.unwrap();
    let (_, d_status, attempts, failures, lapses) = &status.deliverables[0];
    assert_eq!(*d_status, DeliverableStatus::InProgress);
    assert_eq!(*attempts, MAX_ATTEMPTS + 1, "all leases counted");
    assert_eq!(*failures, 0, "a lapse is not an implementation failure");
    assert_eq!(*lapses, MAX_ATTEMPTS, "every TTL lapse counted");
}

/// Circuit-breaker: a deliverable EXPLICITLY marked failed (plan.mark_status
/// status=failed) on every attempt is auto-failed after MAX_ATTEMPTS real
/// failed attempts instead of being re-leased forever.
#[tokio::test]
async fn poison_deliverable_circuit_breaks_after_max_failed_attempts() {
    let audit = Arc::new(MemoryAuditSink::new());
    let planner = BasicCpmPlanner::with_audit(audit.clone());

    let graph = PlanGraph {
        deliverables: vec![
            deliverable("poison", &["src/poison.rs"], &[], Some(1.0)),
            deliverable("dependent", &["src/dep.rs"], &["poison"], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();

    // MAX_ATTEMPTS real attempts: each lease ends with the driver
    // EXPLICITLY reporting failure, and the orchestrator re-marking the
    // deliverable ready for another try.
    for attempt in 1..=MAX_ATTEMPTS {
        let who = caller(&format!("builder-{attempt}"));
        let cohort = planner.acquire_cohort(&plan_id, &who, 1).await.unwrap();
        assert_eq!(cohort.rows.len(), 1, "attempt {attempt} must lease");
        assert_eq!(cohort.rows[0].deliverable.id, "poison");
        planner
            .mark_status(
                &plan_id,
                "poison",
                &who,
                DeliverableStatus::Failed {
                    reason: format!("build attempt {attempt} broke"),
                },
            )
            .await
            .unwrap();
        if attempt < MAX_ATTEMPTS {
            // Orchestrator retry: back into the pool.
            planner
                .mark_status(&plan_id, "poison", &who, DeliverableStatus::Ready)
                .await
                .unwrap();
        }
    }
    // Final retry attempt puts it back to Ready with the budget spent.
    planner
        .mark_status(
            &plan_id,
            "poison",
            &caller("orchestrator"),
            DeliverableStatus::Ready,
        )
        .await
        .unwrap();

    // The next acquire must NOT lease it a fourth time: it circuit-breaks
    // to Failed and the cohort comes back empty.
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("fresh"), 10)
        .await
        .unwrap();
    assert!(
        cohort.rows.is_empty(),
        "circuit-broken deliverable was re-leased: {:?}",
        cohort
            .rows
            .iter()
            .map(|r| r.deliverable.id.clone())
            .collect::<Vec<_>>()
    );

    let status = planner.status(&plan_id).await.unwrap();
    let (_, poison_status, poison_attempts, poison_failures, poison_lapses) = status
        .deliverables
        .iter()
        .find(|(id, _, _, _, _)| id == "poison")
        .expect("poison entry present");
    match poison_status {
        DeliverableStatus::Failed { reason } => assert_eq!(
            reason,
            &format!("circuit-break: exceeded {MAX_ATTEMPTS} failed attempts"),
            "auto-fail reason must carry the circuit-break marker"
        ),
        other => panic!("expected Failed after circuit-break, got {other:?}"),
    }
    assert_eq!(*poison_attempts, MAX_ATTEMPTS);
    assert_eq!(*poison_failures, MAX_ATTEMPTS);
    assert_eq!(*poison_lapses, 0, "no lease ever lapsed in this scenario");
    assert!(
        status.locks_held.is_empty(),
        "auto-failed deliverable must hold no lock"
    );

    // The dependent of the failed prereq stays Pending — never Ready,
    // never leased. That's correct: a failed prerequisite means it can't
    // run; the plan converges instead of blocking on it.
    let (_, dep_status, dep_attempts, _, _) = status
        .deliverables
        .iter()
        .find(|(id, _, _, _, _)| id == "dependent")
        .expect("dependent entry present");
    assert_eq!(*dep_status, DeliverableStatus::Pending);
    assert_eq!(*dep_attempts, 0);

    // Audit trail carries the circuit-break event.
    let evt = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.deliverable.circuit_broken")
        .expect("circuit_broken event present");
    assert_eq!(evt.payload["deliverable_id"], "poison");
    assert_eq!(evt.payload["failure_count"], MAX_ATTEMPTS);
    assert_eq!(evt.payload["max_attempts"], MAX_ATTEMPTS);

    // And it is never handed out again on later acquires either.
    let again = planner
        .acquire_cohort(&plan_id, &caller("much-later"), 10)
        .await
        .unwrap();
    assert!(again.rows.is_empty(), "failed deliverable re-leased later");
}

/// Runaway protection: environmental lapses never auto-fail a
/// deliverable, but an infinitely-crashing environment must not spin
/// forever either. At MAX_LAPSES lapses, acquire_cohort refuses to
/// re-lease and surfaces the stable LAPSE_LIMIT error naming the
/// environmental (not implementation) cause.
#[tokio::test]
async fn lapse_limit_stops_releasing_with_stable_prefix_error() {
    let audit = Arc::new(MemoryAuditSink::new());
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::new(t0);
    let clock_arc = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        audit.clone(),
        Duration::from_secs(60),
        Arc::new(move || clock_arc.read()),
    );

    let graph = PlanGraph {
        deliverables: vec![deliverable("doomed-env", &["src/d.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();

    // MAX_LAPSES leases, every one lost to the environment.
    for lapse in 1..=MAX_LAPSES {
        let cohort = planner
            .acquire_cohort(&plan_id, &caller(&format!("killed-{lapse}")), 1)
            .await
            .unwrap();
        assert_eq!(cohort.rows.len(), 1, "lease {lapse} must be granted");
        clock.set(t0 + chrono::Duration::minutes(5 * i64::from(lapse)));
    }

    // The next acquire must fail loudly instead of leasing an 11th time.
    let err = planner
        .acquire_cohort(&plan_id, &caller("fresh"), 1)
        .await
        .expect_err("lapse limit must stop the re-lease loop");
    assert!(
        matches!(&err, PlannerError::LapseLimit { deliverable_id, lapse_count, max_lapses }
            if deliverable_id == "doomed-env"
                && *lapse_count == MAX_LAPSES
                && *max_lapses == MAX_LAPSES),
        "expected LapseLimit, got {err}"
    );
    let msg = err.to_string();
    assert!(
        msg.starts_with("LAPSE_LIMIT:"),
        "stable prefix missing: {msg}"
    );
    assert!(
        msg.contains("environmental"),
        "message must name the environmental cause: {msg}"
    );

    // The deliverable is NOT auto-failed — it is healthy as far as anyone
    // knows — and its failure budget is untouched. The Err rolled back the
    // in-flight reap, so the durable lapse count stays at the limit.
    let status = planner.status(&plan_id).await.unwrap();
    let (_, d_status, _, failures, lapses) = &status.deliverables[0];
    assert!(
        !matches!(d_status, DeliverableStatus::Failed { .. }),
        "lapse limit must not auto-fail; got {d_status:?}"
    );
    assert_eq!(*failures, 0);
    assert_eq!(*lapses, MAX_LAPSES - 1, "final reap rolled back with Err");

    // Every subsequent acquire keeps failing the same way (stable, loud).
    let err2 = planner
        .acquire_cohort(&plan_id, &caller("again"), 1)
        .await
        .expect_err("still lapse-limited");
    assert!(err2.to_string().starts_with("LAPSE_LIMIT:"));

    // Operator escape hatch: clear the (long-expired) lease, then
    // explicitly mark the deliverable terminal to unblock the plan
    // (here: give up on it).
    planner
        .force_release(
            &plan_id,
            "doomed-env",
            "operator: environment unrecoverable",
        )
        .await
        .unwrap();
    planner
        .mark_status(
            &plan_id,
            "doomed-env",
            &caller("operator"),
            DeliverableStatus::Failed {
                reason: "operator: environment unrecoverable".to_string(),
            },
        )
        .await
        .unwrap();
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("after-triage"), 1)
        .await
        .expect("plan proceeds after operator triage");
    assert!(cohort.rows.is_empty(), "nothing else to lease");
}

/// A deliverable that completes normally on its first lease is untouched
/// by the circuit-breaker: its attempt_count stops at 1.
#[tokio::test]
async fn completed_deliverable_attempt_count_stops_at_one() {
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
    planner
        .mark_status(&plan_id, "a", &caller("c1"), DeliverableStatus::Complete)
        .await
        .unwrap();

    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(
        status.deliverables,
        vec![("a".to_string(), DeliverableStatus::Complete, 1, 0, 0)]
    );

    // A further acquire neither re-leases it nor bumps the counter.
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("c2"), 1)
        .await
        .unwrap();
    assert!(cohort.rows.is_empty());
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.deliverables[0].2, 1);
}

/// attempt_count increments on LEASE only: a Ready candidate left behind
/// because the cohort was already full is not charged an attempt.
#[tokio::test]
async fn unleased_candidate_is_not_charged_an_attempt() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("first", &["src/first.rs"], &[], Some(4.0)),
            deliverable("second", &["src/second.rs"], &[], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();

    // max_count = 1: exactly one of the two Ready candidates is leased.
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("c1"), 1)
        .await
        .unwrap();
    assert_eq!(cohort.rows.len(), 1);
    let leased = cohort.rows[0].deliverable.id.clone();

    let status = planner.status(&plan_id).await.unwrap();
    for (id, deliverable_status, attempts, _, _) in &status.deliverables {
        if *id == leased {
            assert_eq!(*attempts, 1, "leased deliverable counts one attempt");
        } else {
            assert_eq!(*attempts, 0, "unleased candidate must not be charged");
            assert_eq!(*deliverable_status, DeliverableStatus::Ready);
        }
    }
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
