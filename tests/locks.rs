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
use cpm_planner::plan::{
    AcceptRequest, AcquireRequest, CallerId, Deliverable, DeliverableStatus, ForceReleaseRequest,
    HeartbeatRequest, MarkStatusRequest, PlanGraph, PlannerError,
};
use cpm_planner::ports::Planner;
use cpm_planner::{BasicCpmPlanner, MAX_ATTEMPTS, MAX_LAPSES};

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
        milestone: false,
    }
}

fn appending(id: &str, path: &str) -> Deliverable {
    let mut d = deliverable(id, &[], &[], Some(1.0));
    d.owned_files = vec![cpm_planner::plan::OwnedFile::Claim {
        path: PathBuf::from(path),
        mode: Some(cpm_planner::plan::FileMode::Append),
    }];
    d
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();
    assert_eq!(cohort.rows[0].deliverable.id, "a");

    // Advance time past TTL. Next acquire_cohort should reap the expired
    // lock and re-offer `a` to a different caller.
    clock.set(Utc.with_ymd_and_hms(2026, 1, 1, 0, 5, 0).unwrap());

    let cohort2 = planner
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c2").clone(),
            1,
        ))
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

#[tokio::test]
async fn acquire_with_ttl_sets_lock_expiry() {
    let audit = Arc::new(MemoryAuditSink::new());
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::new(t0);
    let clock_arc = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        audit,
        Duration::from_secs(5 * 60),
        Arc::new(move || clock_arc.read()),
    );
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let cohort = planner
        .acquire_cohort(
            AcquireRequest::new(plan_id, caller("c1"), 1)
                .with_ttl(Duration::from_secs(2 * 60 * 60)),
        )
        .await
        .unwrap();
    assert_eq!(
        cohort.rows[0].lock.expires_at,
        t0 + chrono::Duration::hours(2)
    );
}

#[tokio::test]
async fn acquire_ttl_above_max_is_clamped() {
    let audit = Arc::new(MemoryAuditSink::new());
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::new(t0);
    let clock_arc = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        audit,
        Duration::from_secs(5 * 60),
        Arc::new(move || clock_arc.read()),
    )
    .with_max_ttl(Duration::from_secs(60 * 60));
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    let cohort = planner
        .acquire_cohort(
            AcquireRequest::new(plan_id, caller("c1"), 1)
                .with_ttl(Duration::from_secs(5 * 60 * 60)),
        )
        .await
        .unwrap();
    assert_eq!(
        cohort.rows[0].lock.expires_at,
        t0 + chrono::Duration::hours(1)
    );
}

#[tokio::test]
async fn heartbeat_with_ttl_extends_to_requested_duration() {
    let audit = Arc::new(MemoryAuditSink::new());
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::new(t0);
    let clock_arc = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        audit,
        Duration::from_secs(5 * 60),
        Arc::new(move || clock_arc.read()),
    );
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("c1"), 1))
        .await
        .unwrap();
    planner
        .heartbeat(
            HeartbeatRequest::new(plan_id.clone(), "a", caller("c1"))
                .with_ttl(Duration::from_secs(3 * 60 * 60)),
        )
        .await
        .unwrap();
    let lock = planner
        .status(&plan_id)
        .await
        .unwrap()
        .locks_held
        .into_iter()
        .find(|l| l.deliverable_id == "a")
        .expect("lock held");
    assert_eq!(lock.expires_at, t0 + chrono::Duration::hours(3));
}

#[tokio::test]
async fn lease_with_long_ttl_survives_past_default_ttl() {
    let audit = Arc::new(MemoryAuditSink::new());
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::new(t0);
    let clock_arc = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        audit,
        Duration::from_secs(5 * 60),
        Arc::new(move || clock_arc.read()),
    );
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    planner
        .acquire_cohort(
            AcquireRequest::new(plan_id.clone(), caller("c1"), 1)
                .with_ttl(Duration::from_secs(60 * 60)),
        )
        .await
        .unwrap();
    clock.set(t0 + chrono::Duration::minutes(30));
    let second = planner
        .acquire_cohort(AcquireRequest::new(plan_id, caller("c2"), 1))
        .await
        .unwrap();
    assert!(
        second.rows.is_empty(),
        "long-TTL lease must not be reaped after the default 5-minute TTL"
    );
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
            .acquire_cohort(AcquireRequest::new(
                plan_id.clone(),
                caller(&format!("killed-{lapse}")).clone(),
                1,
            ))
            .await
            .unwrap();
        assert_eq!(cohort.rows.len(), 1, "lease {lapse} must be granted");
        clock.set(t0 + chrono::Duration::minutes(5 * i64::from(lapse)));
    }

    // The next acquire must STILL lease it: lapses are not failed attempts.
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("fresh").clone(),
            1,
        ))
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
        let cohort = planner
            .acquire_cohort(AcquireRequest::new(plan_id.clone(), who.clone(), 1))
            .await
            .unwrap();
        assert_eq!(cohort.rows.len(), 1, "attempt {attempt} must lease");
        assert_eq!(cohort.rows[0].deliverable.id, "poison");
        planner
            .mark_status(MarkStatusRequest::new(
                plan_id.clone(),
                "poison",
                who.clone(),
                DeliverableStatus::Failed {
                    reason: format!("build attempt {attempt} broke"),
                },
            ))
            .await
            .unwrap();
        if attempt < MAX_ATTEMPTS {
            // Orchestrator retry: back into the pool.
            planner
                .mark_status(MarkStatusRequest::new(
                    plan_id.clone(),
                    "poison",
                    who.clone(),
                    DeliverableStatus::Ready,
                ))
                .await
                .unwrap();
        }
    }
    // Final retry attempt puts it back to Ready with the budget spent.
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "poison",
            caller("orchestrator").clone(),
            DeliverableStatus::Ready,
        ))
        .await
        .unwrap();

    // The next acquire must NOT lease it a fourth time: it circuit-breaks
    // to Failed and the cohort comes back empty.
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("fresh").clone(),
            10,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("much-later").clone(),
            10,
        ))
        .await
        .unwrap();
    assert!(again.rows.is_empty(), "failed deliverable re-leased later");
}

/// Planner whose lease on "stuck" has lapsed `MAX_LAPSES` times (the final
/// reap happens on the next acquire). "healthy" is independent and
/// file-disjoint; it is released between rounds without counting a lapse.
async fn planner_with_lapse_limited_stuck() -> (
    BasicCpmPlanner,
    Arc<MemoryAuditSink>,
    TestClock,
    cpm_planner::plan::PlanId,
) {
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
        deliverables: vec![
            deliverable("stuck", &["src/stuck.rs"], &[], Some(1.0)),
            deliverable("healthy", &["src/healthy.rs"], &[], Some(1.0)),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    for lapse in 1..=MAX_LAPSES {
        let cohort = planner
            .acquire_cohort(AcquireRequest::new(
                plan_id.clone(),
                caller(&format!("killed-{lapse}")),
                5,
            ))
            .await
            .unwrap();
        assert!(cohort_ids(&cohort).contains(&"stuck".to_string()));
        planner
            .force_release(ForceReleaseRequest::new(
                plan_id.clone(),
                "healthy",
                "reset",
            ))
            .await
            .unwrap();
        clock.set(t0 + chrono::Duration::minutes(5 * i64::from(lapse)));
    }
    (planner, audit, clock, plan_id)
}

fn cohort_ids(cohort: &cpm_planner::plan::Cohort) -> Vec<String> {
    cohort
        .rows
        .iter()
        .map(|r| r.deliverable.id.clone())
        .collect()
}

fn status_of(status: &cpm_planner::plan::PlanStatus, id: &str) -> DeliverableStatus {
    status
        .deliverables
        .iter()
        .find(|row| row.0 == id)
        .expect("deliverable present in status")
        .1
        .clone()
}

fn lapse_count_of(status: &cpm_planner::plan::PlanStatus, id: &str) -> u32 {
    status
        .deliverables
        .iter()
        .find(|row| row.0 == id)
        .expect("deliverable present in status")
        .4
}

#[tokio::test]
async fn requested_lapse_limited_id_is_blocked_lapse_limit() {
    let (planner, _audit, _clock, plan_id) = planner_with_lapse_limited_stuck().await;
    let cohort = planner
        .acquire_cohort(
            AcquireRequest::new(plan_id, caller("fresh"), 5).with_ids(vec!["stuck".to_string()]),
        )
        .await
        .unwrap();
    assert_eq!(
        cohort
            .blocked
            .iter()
            .map(|b| (b.id.as_str(), b.code.as_str()))
            .collect::<Vec<_>>(),
        vec![("stuck", "LAPSE_LIMIT")]
    );
}

#[tokio::test]
async fn acquire_skips_lapse_limited_deliverable_and_leases_the_rest() {
    let (planner, _audit, _clock, plan_id) = planner_with_lapse_limited_stuck().await;
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(plan_id, caller("fresh"), 5))
        .await
        .unwrap();
    assert_eq!(cohort_ids(&cohort), vec!["healthy"]);
}

#[tokio::test]
async fn acquire_reports_lapse_limited_deliverable_as_blocked() {
    let (planner, _audit, _clock, plan_id) = planner_with_lapse_limited_stuck().await;
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(plan_id, caller("fresh"), 5))
        .await
        .unwrap();
    assert_eq!(
        cohort
            .blocked
            .iter()
            .map(|b| (b.id.as_str(), b.code.as_str()))
            .collect::<Vec<_>>(),
        vec![("stuck", "LAPSE_LIMIT")]
    );
}

#[tokio::test]
async fn blocked_reason_names_the_reset_counters_remediation() {
    let (planner, _audit, _clock, plan_id) = planner_with_lapse_limited_stuck().await;
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(plan_id, caller("fresh"), 5))
        .await
        .unwrap();
    assert!(
        cohort.blocked[0]
            .reason
            .contains("plan.force_release {reset_counters: true}")
    );
}

#[tokio::test]
async fn lapse_limit_error_message_keeps_prefix() {
    let err = PlannerError::LapseLimit {
        deliverable_id: "stuck".to_string(),
        lapse_count: MAX_LAPSES,
        max_lapses: MAX_LAPSES,
    };
    assert!(err.to_string().starts_with("LAPSE_LIMIT:"));
}

#[tokio::test]
async fn force_release_with_reset_counters_makes_deliverable_acquirable_again() {
    let (planner, _audit, _clock, plan_id) = planner_with_lapse_limited_stuck().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("fresh"), 5))
        .await
        .unwrap();
    planner
        .force_release(
            ForceReleaseRequest::new(plan_id.clone(), "stuck", "env fixed").reset_counters(true),
        )
        .await
        .unwrap();
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(plan_id, caller("again"), 5))
        .await
        .unwrap();
    assert!(cohort_ids(&cohort).contains(&"stuck".to_string()));
}

#[tokio::test]
async fn force_release_without_reset_keeps_lapse_count() {
    let (planner, _audit, _clock, plan_id) = planner_with_lapse_limited_stuck().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("fresh"), 5))
        .await
        .unwrap();
    planner
        .force_release(
            ForceReleaseRequest::new(plan_id.clone(), "stuck", "no reset").reset_counters(false),
        )
        .await
        .unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(lapse_count_of(&status, "stuck"), MAX_LAPSES);
}

#[tokio::test]
async fn reset_counters_revives_circuit_broken_deliverable() {
    let planner = BasicCpmPlanner::new();
    let graph = PlanGraph {
        deliverables: vec![deliverable("poison", &["src/poison.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    for attempt in 1..=MAX_ATTEMPTS {
        let who = caller(&format!("builder-{attempt}"));
        planner
            .acquire_cohort(AcquireRequest::new(plan_id.clone(), who.clone(), 1))
            .await
            .unwrap();
        planner
            .mark_status(MarkStatusRequest::new(
                plan_id.clone(),
                "poison",
                who.clone(),
                DeliverableStatus::Failed {
                    reason: "broke".to_string(),
                },
            ))
            .await
            .unwrap();
        planner
            .mark_status(MarkStatusRequest::new(
                plan_id.clone(),
                "poison",
                who,
                DeliverableStatus::Ready,
            ))
            .await
            .unwrap();
    }
    // The next acquire circuit-breaks it to Failed.
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("fresh"), 1))
        .await
        .unwrap();
    planner
        .force_release(
            ForceReleaseRequest::new(plan_id.clone(), "poison", "fixed").reset_counters(true),
        )
        .await
        .unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    assert!(matches!(
        status_of(&status, "poison"),
        DeliverableStatus::Ready
    ));
}

#[tokio::test]
async fn reset_counters_emits_audit_event() {
    let (planner, audit, _clock, plan_id) = planner_with_lapse_limited_stuck().await;
    planner
        .force_release(ForceReleaseRequest::new(plan_id, "stuck", "env fixed").reset_counters(true))
        .await
        .unwrap();
    let events = audit.snapshot();
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "plan.deliverable.counters_reset"
                && e.payload["deliverable_id"] == "stuck")
    );
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("c1").clone(),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();

    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(
        status.deliverables,
        vec![("a".to_string(), DeliverableStatus::Complete, 1, 0, 0)]
    );

    // A further acquire neither re-leases it nor bumps the counter.
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c2").clone(),
            1,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
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
                    .acquire_cohort(AcquireRequest::new(
                        plan_id_c.clone(),
                        caller(&format!("c{c}")).clone(),
                        2,
                    ))
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
                    all_files.push(f.path().to_path_buf());
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();

    // Complete -> released event.
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("c1").clone(),
            DeliverableStatus::Complete,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();

    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("c1").clone(),
            DeliverableStatus::Failed {
                reason: "compilation broke".to_string(),
            },
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("c1").clone(),
            1,
        ))
        .await
        .unwrap();

    let reason = "operator escape: caller wedged";
    planner
        .force_release(ForceReleaseRequest::new(plan_id.clone(), "a", reason))
        .await
        .unwrap();

    let evt = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.lock.force_released")
        .expect("force_released event present");
    assert_eq!(evt.payload["reason"], reason);
    assert_eq!(evt.payload["deliverable_id"], "a");
    assert_eq!(evt.payload["last_caller_id"], "c1");
}

#[tokio::test]
async fn accept_ignores_expired_foreign_lease() {
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::new(t0);
    let clock_arc = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        Arc::new(MemoryAuditSink::new()),
        Duration::from_secs(60),
        Arc::new(move || clock_arc.read()),
    );
    let graph = PlanGraph {
        deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await.unwrap();
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w1"), 1))
        .await
        .unwrap();
    clock.set(t0 + chrono::Duration::minutes(5));
    let result = planner
        .accept(AcceptRequest::new(plan_id, "a", "owner", "ok"))
        .await;
    assert!(result.is_ok(), "got: {result:?}");
}

async fn lock_expiry(
    planner: &BasicCpmPlanner,
    plan_id: &cpm_planner::plan::PlanId,
) -> DateTime<Utc> {
    planner
        .status(plan_id)
        .await
        .unwrap()
        .locks_held
        .into_iter()
        .find(|l| l.deliverable_id == "a")
        .expect("lock held")
        .expires_at
}

async fn long_lease_planner() -> (BasicCpmPlanner, TestClock, cpm_planner::plan::PlanId) {
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let clock = TestClock::new(t0);
    let clock_arc = clock.clone();
    let planner = BasicCpmPlanner::with_parts(
        Arc::new(MemoryAuditSink::new()),
        Duration::from_secs(5 * 60),
        Arc::new(move || clock_arc.read()),
    );
    let plan_id = planner
        .submit_plan(PlanGraph {
            deliverables: vec![deliverable("a", &["src/a.rs"], &[], Some(1.0))],
            max_chained_dispatch: None,
        })
        .await
        .unwrap();
    planner
        .acquire_cohort(
            AcquireRequest::new(plan_id.clone(), caller("c1"), 1)
                .with_ttl(Duration::from_secs(2 * 60 * 60)),
        )
        .await
        .unwrap();
    (planner, clock, plan_id)
}

#[tokio::test]
async fn heartbeat_without_ttl_never_shortens_long_lease() {
    let (planner, clock, plan_id) = long_lease_planner().await;
    clock.set(Utc.with_ymd_and_hms(2026, 1, 1, 0, 10, 0).unwrap());
    planner
        .heartbeat(HeartbeatRequest::new(plan_id.clone(), "a", caller("c1")))
        .await
        .unwrap();
    assert_eq!(
        lock_expiry(&planner, &plan_id).await,
        Utc.with_ymd_and_hms(2026, 1, 1, 2, 0, 0).unwrap()
    );
}

#[tokio::test]
async fn heartbeat_with_explicit_shorter_ttl_shortens_lease() {
    let (planner, clock, plan_id) = long_lease_planner().await;
    clock.set(Utc.with_ymd_and_hms(2026, 1, 1, 0, 10, 0).unwrap());
    planner
        .heartbeat(
            HeartbeatRequest::new(plan_id.clone(), "a", caller("c1"))
                .with_ttl(Duration::from_secs(60)),
        )
        .await
        .unwrap();
    assert_eq!(
        lock_expiry(&planner, &plan_id).await,
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 11, 0).unwrap()
    );
}

#[tokio::test]
async fn lapse_limited_deliverable_is_not_auto_failed() {
    let (planner, _audit, _clock, plan_id) = planner_with_lapse_limited_stuck().await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("fresh"), 5))
        .await
        .unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status_of(&status, "stuck"), DeliverableStatus::Ready);
}

#[tokio::test]
async fn reset_counters_event_is_attributed_to_operator() {
    let (planner, audit, _clock, plan_id) = planner_with_lapse_limited_stuck().await;
    planner
        .force_release(ForceReleaseRequest::new(plan_id, "stuck", "env fixed").reset_counters(true))
        .await
        .unwrap();
    let evt = audit
        .snapshot()
        .into_iter()
        .find(|e| e.event_type == "plan.deliverable.counters_reset")
        .expect("counters_reset event");
    assert_eq!(evt.actor.as_deref(), Some("operator"));
}

#[tokio::test]
async fn append_claims_on_same_path_are_coleased() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner
        .submit_plan(PlanGraph {
            deliverables: vec![appending("a", "REGISTRY.md"), appending("b", "REGISTRY.md")],
            max_chained_dispatch: None,
        })
        .await
        .unwrap();
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(plan_id, caller("c1"), 2))
        .await
        .unwrap();
    assert_eq!(cohort.rows.len(), 2);
}

#[tokio::test]
async fn cohort_reports_shared_append_paths() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner
        .submit_plan(PlanGraph {
            deliverables: vec![appending("a", "REGISTRY.md"), appending("b", "REGISTRY.md")],
            max_chained_dispatch: None,
        })
        .await
        .unwrap();
    let cohort = planner
        .acquire_cohort(AcquireRequest::new(plan_id, caller("c1"), 2))
        .await
        .unwrap();
    assert_eq!(cohort.shared_paths, vec![PathBuf::from("REGISTRY.md")]);
}

#[tokio::test]
async fn unordered_exclusive_and_append_claims_are_rejected_at_submit() {
    let planner = BasicCpmPlanner::new();
    let exclusive = deliverable("c", &["REGISTRY.md"], &[], Some(1.0));
    let err = planner
        .submit_plan(PlanGraph {
            deliverables: vec![
                appending("a", "REGISTRY.md"),
                appending("b", "REGISTRY.md"),
                exclusive,
            ],
            max_chained_dispatch: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, PlannerError::InvalidGraph { .. }), "{err:?}");
}

#[tokio::test]
async fn releasing_one_append_holder_keeps_the_other_claim() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner
        .submit_plan(PlanGraph {
            deliverables: vec![appending("a", "REGISTRY.md"), appending("b", "REGISTRY.md")],
            max_chained_dispatch: None,
        })
        .await
        .unwrap();
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("c1"), 2))
        .await
        .unwrap();
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "a",
            caller("c1"),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.locks_held.len(), 1);
}
