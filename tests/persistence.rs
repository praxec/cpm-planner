//! P17 acceptance tests: durable SQLite persistence + cross-process
//! atomicity.
//!
//! Every test drives TWO (or more) independent `SqlitePlanStore`
//! connections against the same on-disk database file — the closest
//! in-test proxy for two OS processes (each connection has its own
//! sqlite handle, page cache, and locking state; nothing is shared in
//! Rust memory). "Reopen" tests drop the first planner entirely before
//! opening the second, simulating a restart.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use cpm_planner::audit::NullAuditSink;
use cpm_planner::plan::{CallerId, Deliverable, DeliverableStatus, PlanGraph, PlannerError};
use cpm_planner::ports::Planner;
use cpm_planner::{BasicCpmPlanner, SqlitePlanStore};

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
            "cpm-planner-persistence-test-{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        Self { path }
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

/// A fresh planner over its own connection to `path` — one simulated
/// process.
fn open_planner(path: &Path) -> BasicCpmPlanner {
    let store = SqlitePlanStore::open(path).expect("open store");
    BasicCpmPlanner::with_store(store, Arc::new(NullAuditSink))
}

/// Same, with an injected clock + TTL for deterministic expiry tests.
fn open_planner_with_clock(path: &Path, ttl: Duration, clock: TestClock) -> BasicCpmPlanner {
    let store = SqlitePlanStore::open(path).expect("open store");
    BasicCpmPlanner::with_store_parts(
        store,
        Arc::new(NullAuditSink),
        ttl,
        Arc::new(move || clock.read()),
    )
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
        *self.now.lock().expect("test clock not poisoned") = when;
    }

    fn read(&self) -> DateTime<Utc> {
        *self.now.lock().expect("test clock not poisoned")
    }
}

fn deliverable(id: &str, files: &[&str], prereqs: &[&str]) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: files.iter().map(PathBuf::from).collect(),
        prerequisites: prereqs.iter().map(|s| s.to_string()).collect(),
        estimated_effort_hours: Some(1.0),
        metadata: serde_json::Value::Null,
    }
}

/// d1 (ready) -> d2 (pending until d1 completes).
fn chain_graph() -> PlanGraph {
    PlanGraph {
        deliverables: vec![
            deliverable("d1", &["src/a.rs"], &[]),
            deliverable("d2", &["src/b.rs"], &["d1"]),
        ],
        max_chained_dispatch: None,
    }
}

fn caller(id: &str) -> CallerId {
    CallerId(id.to_string())
}

// ---------------------------------------------------------------------
// Acceptance: restart survival
// ---------------------------------------------------------------------

#[tokio::test]
async fn plan_and_statuses_survive_reopen() {
    let db = TempDb::new();

    let plan_id = {
        let planner = open_planner(&db.path);
        planner.submit_plan(chain_graph()).await.expect("submit")
        // planner (and its connection) dropped here — "restart".
    };

    let planner2 = open_planner(&db.path);
    let status = planner2
        .status(&plan_id)
        .await
        .expect("plan visible after reopen");
    assert_eq!(status.plan_id, plan_id);
    assert_eq!(
        status.deliverables,
        vec![
            ("d1".to_string(), DeliverableStatus::Ready, 0, 0, 0),
            ("d2".to_string(), DeliverableStatus::Pending, 0, 0, 0),
        ]
    );
    assert!(status.locks_held.is_empty());
    assert_eq!(
        status.critical_path,
        vec!["d1".to_string(), "d2".to_string()]
    );
}

#[tokio::test]
async fn idempotent_resubmit_survives_reopen() {
    let db = TempDb::new();

    let first = {
        let planner = open_planner(&db.path);
        planner.submit_plan(chain_graph()).await.expect("submit")
    };

    let planner2 = open_planner(&db.path);
    let second = planner2
        .submit_plan(chain_graph())
        .await
        .expect("resubmit after reopen");
    assert_eq!(
        first, second,
        "identical graph resubmitted after restart must dedup to the same plan_id"
    );
}

// ---------------------------------------------------------------------
// Acceptance: cross-process acquire atomicity
// ---------------------------------------------------------------------

#[tokio::test]
async fn two_connections_cannot_double_acquire_the_same_deliverable() {
    let db = TempDb::new();
    let planner_a = open_planner(&db.path);
    let planner_b = open_planner(&db.path);

    let plan_id = planner_a.submit_plan(chain_graph()).await.expect("submit");

    let cohort_a = planner_a
        .acquire_cohort(&plan_id, &caller("proc-a"), 10)
        .await
        .expect("first acquire");
    assert_eq!(cohort_a.rows.len(), 1);
    assert_eq!(cohort_a.rows[0].deliverable.id, "d1");

    // The plan was submitted by A's connection but must be fully visible
    // to B; and d1, locked by A, must NOT be acquirable by B.
    let cohort_b = planner_b
        .acquire_cohort(&plan_id, &caller("proc-b"), 10)
        .await
        .expect("second acquire (different connection)");
    assert!(
        cohort_b.rows.is_empty(),
        "connection B double-acquired: {:?}",
        cohort_b
            .rows
            .iter()
            .map(|r| r.deliverable.id.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_acquires_across_connections_never_overlap() {
    // 6 independent deliverables, 4 "processes" (independent connections)
    // each demanding 2: total demand 8 > supply 6. The IMMEDIATE
    // transaction must serialise them so no deliverable — and therefore
    // no owned file — is granted twice.
    let db = TempDb::new();
    let graph = PlanGraph {
        deliverables: (0..6)
            .map(|i| deliverable(&format!("d{i}"), &[&format!("src/d{i}.rs")], &[]))
            .collect(),
        max_chained_dispatch: None,
    };

    let submitter = open_planner(&db.path);
    let plan_id = submitter.submit_plan(graph).await.expect("submit");

    let mut handles = Vec::new();
    for c in 0..4 {
        let path = db.path.clone();
        let plan_id_c = plan_id.clone();
        handles.push(tokio::spawn(async move {
            let planner = open_planner(&path);
            planner
                .acquire_cohort(&plan_id_c, &caller(&format!("proc-{c}")), 2)
                .await
                .expect("acquire_cohort should not error")
        }));
    }

    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_files = std::collections::HashSet::new();
    let mut total = 0usize;
    for h in handles {
        let cohort = h.await.expect("task join");
        total += cohort.rows.len();
        for row in &cohort.rows {
            assert!(
                seen_ids.insert(row.deliverable.id.clone()),
                "deliverable {} granted to two connections",
                row.deliverable.id
            );
            for f in &row.deliverable.owned_files {
                assert!(
                    seen_files.insert(f.clone()),
                    "file {f:?} granted to two connections"
                );
            }
        }
    }
    assert!(total <= 6, "over-allocation: {total} > 6");
}

// ---------------------------------------------------------------------
// Acceptance: mark_status holder semantics across connections
// ---------------------------------------------------------------------

#[tokio::test]
async fn nonholder_rejected_and_holder_release_unblocks_dependent() {
    let db = TempDb::new();
    let planner_a = open_planner(&db.path);
    let planner_b = open_planner(&db.path);

    let plan_id = planner_a.submit_plan(chain_graph()).await.expect("submit");
    planner_a
        .acquire_cohort(&plan_id, &caller("holder"), 1)
        .await
        .expect("acquire d1");

    // A different caller_id (via a different connection) cannot complete it.
    let err = planner_b
        .mark_status(
            &plan_id,
            "d1",
            &caller("intruder"),
            DeliverableStatus::Complete,
        )
        .await
        .expect_err("non-holder must be rejected");
    assert!(
        matches!(err, PlannerError::LockNotHeld { .. }),
        "expected LockNotHeld, got {err}"
    );

    // The holder (same caller_id, either connection) releases it; the
    // dependent becomes ready and is acquirable from the other connection.
    planner_b
        .mark_status(
            &plan_id,
            "d1",
            &caller("holder"),
            DeliverableStatus::Complete,
        )
        .await
        .expect("holder completes d1");

    let cohort = planner_b
        .acquire_cohort(&plan_id, &caller("proc-b"), 1)
        .await
        .expect("acquire dependent");
    assert_eq!(cohort.rows.len(), 1);
    assert_eq!(cohort.rows[0].deliverable.id, "d2");
}

// ---------------------------------------------------------------------
// Acceptance: TTL expiry (reclaim + startup quarantine) and heartbeat
// ---------------------------------------------------------------------

#[tokio::test]
async fn expired_lock_is_reclaimable_by_another_connection() {
    let db = TempDb::new();
    // Far-future fake clock: SqlitePlanStore::open runs a startup
    // quarantine against the REAL wall clock, and these locks must not be
    // swept by it — this test exercises the acquire-path reap only.
    let t0 = Utc
        .with_ymd_and_hms(2100, 1, 1, 0, 0, 0)
        .single()
        .expect("valid t0");

    let planner_a = open_planner_with_clock(&db.path, Duration::from_secs(60), TestClock::at(t0));
    let plan_id = planner_a.submit_plan(chain_graph()).await.expect("submit");
    planner_a
        .acquire_cohort(&plan_id, &caller("stale-holder"), 1)
        .await
        .expect("acquire d1");

    // Ten minutes later (well past the 60s TTL) another connection asks.
    let late = TestClock::at(t0 + chrono::Duration::minutes(10));
    let planner_b = open_planner_with_clock(&db.path, Duration::from_secs(60), late);
    let cohort = planner_b
        .acquire_cohort(&plan_id, &caller("reclaimer"), 1)
        .await
        .expect("acquire after expiry");
    assert_eq!(cohort.rows.len(), 1);
    assert_eq!(cohort.rows[0].deliverable.id, "d1");
    assert_eq!(cohort.rows[0].lock.caller_id, caller("reclaimer"));
}

#[tokio::test]
async fn expired_lock_is_quarantined_on_reopen() {
    let db = TempDb::new();
    // A clock far in the past: the persisted lock's TTL has lapsed
    // relative to the real wall clock used by the startup quarantine.
    let ancient = Utc
        .with_ymd_and_hms(2020, 1, 1, 0, 0, 0)
        .single()
        .expect("valid t0");

    let plan_id = {
        let planner =
            open_planner_with_clock(&db.path, Duration::from_secs(60), TestClock::at(ancient));
        let plan_id = planner.submit_plan(chain_graph()).await.expect("submit");
        planner
            .acquire_cohort(&plan_id, &caller("crashed-proc"), 1)
            .await
            .expect("acquire d1");
        plan_id
        // "Crash": planner dropped with the (already-expired) lock held
        // and d1 in_progress.
    };

    // Reopen: SqlitePlanStore::open runs the quarantine sweep.
    let planner2 = open_planner(&db.path);
    let status = planner2
        .status(&plan_id)
        .await
        .expect("status after reopen");
    assert!(
        status.locks_held.is_empty(),
        "expired lock must be cleared at startup"
    );
    assert_eq!(
        status.deliverables[0],
        ("d1".to_string(), DeliverableStatus::Ready, 1, 0, 1),
        "quarantined deliverable goes back to ready with its lease still counted \
         and the environmental loss recorded as a lapse (never a failure)"
    );

    // And it is immediately re-acquirable.
    let cohort = planner2
        .acquire_cohort(&plan_id, &caller("fresh-proc"), 1)
        .await
        .expect("reacquire");
    assert_eq!(cohort.rows.len(), 1);
    assert_eq!(cohort.rows[0].deliverable.id, "d1");
}

#[tokio::test]
async fn heartbeat_ttl_refresh_is_persisted_across_connections() {
    let db = TempDb::new();
    // Far-future fake clock so the startup quarantine (real wall clock)
    // in each SqlitePlanStore::open cannot reap the lock under test.
    let t0 = Utc
        .with_ymd_and_hms(2100, 1, 1, 0, 0, 0)
        .single()
        .expect("valid t0");
    let ttl = Duration::from_secs(60);

    let clock_a = TestClock::at(t0);
    let planner_a = open_planner_with_clock(&db.path, ttl, clock_a.clone());
    let plan_id = planner_a.submit_plan(chain_graph()).await.expect("submit");
    planner_a
        .acquire_cohort(&plan_id, &caller("worker"), 1)
        .await
        .expect("acquire d1"); // expires t0+60

    // Heartbeat at t0+30 pushes expiry to t0+90 — persisted, not in-memory.
    clock_a.set(t0 + chrono::Duration::seconds(30));
    planner_a
        .heartbeat(&plan_id, "d1", &caller("worker"))
        .await
        .expect("heartbeat");

    // At t0+70 (past the ORIGINAL expiry, before the refreshed one) a
    // second connection must NOT be able to reclaim d1.
    let planner_b = open_planner_with_clock(
        &db.path,
        ttl,
        TestClock::at(t0 + chrono::Duration::seconds(70)),
    );
    let cohort = planner_b
        .acquire_cohort(&plan_id, &caller("poacher"), 1)
        .await
        .expect("acquire attempt");
    assert!(
        cohort.rows.is_empty(),
        "heartbeat-refreshed lock was reclaimed before its new expiry"
    );

    // At t0+120 (past the refreshed expiry) it is reclaimable.
    let planner_c = open_planner_with_clock(
        &db.path,
        ttl,
        TestClock::at(t0 + chrono::Duration::seconds(120)),
    );
    let cohort = planner_c
        .acquire_cohort(&plan_id, &caller("reclaimer"), 1)
        .await
        .expect("acquire after refreshed expiry");
    assert_eq!(cohort.rows.len(), 1);
    assert_eq!(cohort.rows[0].deliverable.id, "d1");
}

// ---------------------------------------------------------------------
// Acceptance: circuit-breaker counters are durable — and environmental
// lapses are NOT failed attempts
// ---------------------------------------------------------------------

/// Each "process" (fresh store connection) leases d1 and is killed
/// EXTERNALLY without marking it; the lock lapses via TTL and d1 reverts
/// to Ready. Those lapses are environmental, not implementation
/// failures: the lapse counter must survive every reopen, and the
/// failure circuit-breaker must NOT fire — the next process still gets
/// the lease.
#[tokio::test]
async fn lapse_count_survives_reopen_and_never_trips_the_failure_breaker() {
    let db = TempDb::new();
    let ttl = Duration::from_secs(60);
    // Far-future fake clock: the startup quarantine in each open() runs
    // against the REAL wall clock and must not sweep these locks; expiry
    // is driven by the fake clock on the acquire path.
    let t0 = Utc
        .with_ymd_and_hms(2100, 1, 1, 0, 0, 0)
        .single()
        .expect("valid t0");

    let mut plan_id = None;
    for lease in 1..=cpm_planner::MAX_ATTEMPTS {
        // Each iteration is a fresh "process", opened after the previous
        // process's lock has already lapsed on the fake clock.
        let now = t0 + chrono::Duration::minutes(10 * i64::from(lease));
        let planner = open_planner_with_clock(&db.path, ttl, TestClock::at(now));
        let id = planner.submit_plan(chain_graph()).await.expect("submit");
        let cohort = planner
            .acquire_cohort(&id, &caller(&format!("killed-{lease}")), 1)
            .await
            .expect("acquire");
        assert_eq!(cohort.rows.len(), 1, "lease {lease} must be granted");
        assert_eq!(cohort.rows[0].deliverable.id, "d1");

        let status = planner.status(&id).await.expect("status");
        let (_, _, attempts, failures, lapses) = &status.deliverables[0];
        assert_eq!(*attempts, lease, "attempt_count accumulates durably");
        assert_eq!(*failures, 0, "no driver ever reported failure");
        assert_eq!(*lapses, lease - 1, "each lost lease recorded as a lapse");
        plan_id = Some(id);
        // Planner dropped without mark_status — the external kill.
    }
    let plan_id = plan_id.expect("plan submitted");

    // A fresh process past all TTLs: d1 must STILL be leasable — the
    // lapses burned no circuit-breaker lives.
    let planner = open_planner_with_clock(
        &db.path,
        ttl,
        TestClock::at(t0 + chrono::Duration::hours(10)),
    );
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("fresh"), 10)
        .await
        .expect("acquire after lapses");
    assert_eq!(
        cohort.rows.len(),
        1,
        "healthy deliverable was circuit-broken by environmental lapses"
    );
    assert_eq!(cohort.rows[0].deliverable.id, "d1");
}

/// Each "process" (fresh store connection) leases d1, EXPLICITLY marks
/// it failed, and re-marks it ready for retry. The failure counter must
/// survive every reopen, so after MAX_ATTEMPTS real failed attempts the
/// next process circuit-breaks d1 to Failed instead of leasing it a
/// fourth time.
#[tokio::test]
async fn failure_count_survives_reopen_and_circuit_breaks_across_processes() {
    let db = TempDb::new();

    let mut plan_id = None;
    for attempt in 1..=cpm_planner::MAX_ATTEMPTS {
        // Each iteration is a fresh "process".
        let planner = open_planner(&db.path);
        let id = planner.submit_plan(chain_graph()).await.expect("submit");
        let who = caller(&format!("builder-{attempt}"));
        let cohort = planner.acquire_cohort(&id, &who, 1).await.expect("acquire");
        assert_eq!(cohort.rows.len(), 1, "attempt {attempt} must lease d1");
        assert_eq!(cohort.rows[0].deliverable.id, "d1");
        planner
            .mark_status(
                &id,
                "d1",
                &who,
                DeliverableStatus::Failed {
                    reason: format!("build attempt {attempt} broke"),
                },
            )
            .await
            .expect("mark failed");
        // Orchestrator retry: back into the pool.
        planner
            .mark_status(&id, "d1", &who, DeliverableStatus::Ready)
            .await
            .expect("re-mark ready");

        let status = planner.status(&id).await.expect("status");
        let (_, _, _, failures, lapses) = &status.deliverables[0];
        assert_eq!(*failures, attempt, "failure_count accumulates durably");
        assert_eq!(*lapses, 0, "no lease ever lapsed in this scenario");
        plan_id = Some(id);
    }
    let plan_id = plan_id.expect("plan submitted");

    // A fresh process: d1 must be circuit-broken, not re-leased, and the
    // plan converges (empty cohort).
    let planner = open_planner(&db.path);
    let cohort = planner
        .acquire_cohort(&plan_id, &caller("fresh"), 10)
        .await
        .expect("acquire after circuit-break");
    assert!(
        cohort.rows.is_empty(),
        "poison deliverable re-leased after {} failed attempts",
        cpm_planner::MAX_ATTEMPTS
    );

    let status = planner.status(&plan_id).await.expect("status");
    let (_, d1_status, _, d1_failures, _) = &status.deliverables[0];
    assert!(
        matches!(d1_status, DeliverableStatus::Failed { reason } if reason.contains("circuit-break")),
        "expected circuit-broken Failed, got {d1_status:?}"
    );
    assert_eq!(*d1_failures, cpm_planner::MAX_ATTEMPTS);
    assert!(status.locks_held.is_empty());
    // Dependent of the failed prereq never becomes Ready.
    assert_eq!(status.deliverables[1].1, DeliverableStatus::Pending);
}

#[tokio::test]
async fn force_release_from_another_connection_frees_the_lock() {
    let db = TempDb::new();
    let planner_a = open_planner(&db.path);
    let planner_b = open_planner(&db.path);

    let plan_id = planner_a.submit_plan(chain_graph()).await.expect("submit");
    planner_a
        .acquire_cohort(&plan_id, &caller("wedged"), 1)
        .await
        .expect("acquire d1");

    planner_b
        .force_release(&plan_id, "d1", "operator: wedged process")
        .await
        .expect("force release");

    let cohort = planner_b
        .acquire_cohort(&plan_id, &caller("fresh"), 1)
        .await
        .expect("reacquire");
    assert_eq!(cohort.rows.len(), 1);
    assert_eq!(cohort.rows[0].deliverable.id, "d1");
}
