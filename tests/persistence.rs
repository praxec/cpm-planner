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
use cpm_planner::plan::{
    AcquireRequest, CallerId, Deliverable, DeliverableStatus, ForceReleaseRequest,
    HeartbeatRequest, MarkStatusRequest, PlanGraph, PlannerError,
};
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
        owned_files: files
            .iter()
            .map(|f| cpm_planner::plan::OwnedFile::from(*f))
            .collect(),
        prerequisites: prereqs.iter().map(|s| (*s).into()).collect(),
        estimated_effort_hours: Some(1.0),
        metadata: serde_json::Value::Null,
        duration_hours: None,
        estimate: None,
        milestone: false,
        earning_rule: None,
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
        vec![
            "__start__".to_string(),
            "d1".to_string(),
            "d2".to_string(),
            "__finish__".to_string()
        ]
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("proc-a").clone(),
            10,
        ))
        .await
        .expect("first acquire");
    assert_eq!(cohort_a.rows.len(), 1);
    assert_eq!(cohort_a.rows[0].deliverable.id, "d1");

    // The plan was submitted by A's connection but must be fully visible
    // to B; and d1, locked by A, must NOT be acquirable by B.
    let cohort_b = planner_b
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("proc-b").clone(),
            10,
        ))
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
                .acquire_cohort(AcquireRequest::new(
                    plan_id_c.clone(),
                    caller(&format!("proc-{c}")).clone(),
                    2,
                ))
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
                    seen_files.insert(f.path().to_path_buf()),
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("holder").clone(),
            1,
        ))
        .await
        .expect("acquire d1");

    // A different caller_id (via a different connection) cannot complete it.
    let err = planner_b
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "d1",
            caller("intruder").clone(),
            DeliverableStatus::Complete,
        ))
        .await
        .expect_err("non-holder must be rejected");
    assert!(
        matches!(err, PlannerError::LockNotHeld { .. }),
        "expected LockNotHeld, got {err}"
    );

    // The holder (same caller_id, either connection) releases it; the
    // dependent becomes ready and is acquirable from the other connection.
    planner_b
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "d1",
            caller("holder").clone(),
            DeliverableStatus::Complete,
        ))
        .await
        .expect("holder completes d1");

    let cohort = planner_b
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("proc-b").clone(),
            1,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("stale-holder").clone(),
            1,
        ))
        .await
        .expect("acquire d1");

    // Ten minutes later (well past the 60s TTL) another connection asks.
    let late = TestClock::at(t0 + chrono::Duration::minutes(10));
    let planner_b = open_planner_with_clock(&db.path, Duration::from_secs(60), late);
    let cohort = planner_b
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("reclaimer").clone(),
            1,
        ))
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
            .acquire_cohort(AcquireRequest::new(
                plan_id.clone(),
                caller("crashed-proc").clone(),
                1,
            ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("fresh-proc").clone(),
            1,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("worker").clone(),
            1,
        ))
        .await
        .expect("acquire d1"); // expires t0+60

    // Heartbeat at t0+30 pushes expiry to t0+90 — persisted, not in-memory.
    clock_a.set(t0 + chrono::Duration::seconds(30));
    planner_a
        .heartbeat(HeartbeatRequest::new(
            plan_id.clone(),
            "d1",
            caller("worker").clone(),
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("poacher").clone(),
            1,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("reclaimer").clone(),
            1,
        ))
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
            .acquire_cohort(AcquireRequest::new(
                id.clone(),
                caller(&format!("killed-{lease}")).clone(),
                1,
            ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("fresh").clone(),
            10,
        ))
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
        let cohort = planner
            .acquire_cohort(AcquireRequest::new(id.clone(), who.clone(), 1))
            .await
            .expect("acquire");
        assert_eq!(cohort.rows.len(), 1, "attempt {attempt} must lease d1");
        assert_eq!(cohort.rows[0].deliverable.id, "d1");
        planner
            .mark_status(MarkStatusRequest::new(
                id.clone(),
                "d1",
                who.clone(),
                DeliverableStatus::Failed {
                    reason: format!("build attempt {attempt} broke"),
                },
            ))
            .await
            .expect("mark failed");
        // Orchestrator retry: back into the pool.
        planner
            .mark_status(MarkStatusRequest::new(
                id.clone(),
                "d1",
                who.clone(),
                DeliverableStatus::Ready,
            ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("fresh").clone(),
            10,
        ))
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
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("wedged").clone(),
            1,
        ))
        .await
        .expect("acquire d1");

    planner_b
        .force_release(ForceReleaseRequest::new(
            plan_id.clone(),
            "d1",
            "operator: wedged process",
        ))
        .await
        .expect("force release");

    let cohort = planner_b
        .acquire_cohort(AcquireRequest::new(
            plan_id.clone(),
            caller("fresh").clone(),
            1,
        ))
        .await
        .expect("reacquire");
    assert_eq!(cohort.rows.len(), 1);
    assert_eq!(cohort.rows[0].deliverable.id, "d1");
}

// ---------------------------------------------------------------------
// Schema versioning + stale CPM result repair
// ---------------------------------------------------------------------

fn effort_deliverable(id: &str, prereqs: &[&str], hours: f32) -> Deliverable {
    let mut d = deliverable(id, &[], prereqs);
    d.estimated_effort_hours = Some(hours);
    d
}

/// Two independent chains: `P0a(2)->P0b(3)` and `P1a(2)->P1b(3)`.
async fn submit_parallel_chains(path: &Path) -> cpm_planner::plan::PlanId {
    let planner = open_planner(path);
    planner
        .submit_plan(PlanGraph {
            deliverables: vec![
                effort_deliverable("P0a", &[], 2.0),
                effort_deliverable("P0b", &["P0a"], 3.0),
                effort_deliverable("P1a", &[], 2.0),
                effort_deliverable("P1b", &["P1a"], 3.0),
            ],
            max_chained_dispatch: None,
        })
        .await
        .expect("submit")
}

#[tokio::test]
async fn reopening_store_repairs_stale_cached_critical_path() {
    let db = TempDb::new();
    let plan_id = submit_parallel_chains(&db.path).await;
    // Simulate a row written by an older kernel: buggy path + version 0.
    {
        let conn = rusqlite::Connection::open(&db.path).unwrap();
        let mut result: serde_json::Value = serde_json::from_str(
            &conn
                .query_row(
                    "SELECT cached_result FROM plans WHERE plan_id = ?1",
                    [&plan_id.0],
                    |r| r.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        result["critical_path"] = serde_json::json!(["P0a", "P1a", "P0b", "P1b"]);
        conn.execute(
            "UPDATE plans SET cached_result = ?1, cpm_version = 0 WHERE plan_id = ?2",
            rusqlite::params![result.to_string(), plan_id.0],
        )
        .unwrap();
    }
    let planner = open_planner(&db.path);
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(
        status.critical_path,
        vec!["__start__", "P0a", "P0b", "__finish__"]
    );
}

#[test]
fn opened_store_reports_schema_version_4() {
    let db = TempDb::new();
    drop(SqlitePlanStore::open(&db.path).unwrap());
    assert_eq!(user_version(&db.path), 4);
}

fn user_version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

/// A v2 database (no portfolio tables) holding one legacy plan.
async fn v2_database_with_legacy_plan(path: &Path) -> cpm_planner::plan::PlanId {
    let plan_id = submit_parallel_chains(path).await;
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(
            "DROP TABLE revisions;
             DROP TABLE variants;
             DROP TABLE plan_lines;
             PRAGMA user_version = 2;",
        )
        .unwrap();
    plan_id
}

#[tokio::test]
async fn migration_from_v2_database_reaches_v4() {
    let db = TempDb::new();
    v2_database_with_legacy_plan(&db.path).await;
    drop(SqlitePlanStore::open(&db.path).unwrap());
    assert_eq!(user_version(&db.path), 4);
}

#[tokio::test]
async fn legacy_plan_survives_v3_migration() {
    let db = TempDb::new();
    let plan_id = v2_database_with_legacy_plan(&db.path).await;
    let status = open_planner(&db.path).status(&plan_id).await.unwrap();
    assert_eq!(status.deliverables.len(), 4);
}

#[tokio::test]
async fn migrated_v2_database_accepts_named_sync() {
    let db = TempDb::new();
    v2_database_with_legacy_plan(&db.path).await;
    let out = open_planner(&db.path)
        .sync_plan(cpm_planner::plan::SyncRequest::new(
            "proj",
            "web",
            "main",
            chain_graph(),
        ))
        .await;
    assert!(out.is_ok());
}

#[test]
fn newer_schema_rejection_names_supported_version_4() {
    let db = TempDb::new();
    drop(SqlitePlanStore::open(&db.path).unwrap());
    rusqlite::Connection::open(&db.path)
        .unwrap()
        .pragma_update(None, "user_version", 5)
        .unwrap();
    let err = SqlitePlanStore::open(&db.path).err().unwrap();
    assert!(format!("{err:#}").contains("(4)"));
}

#[tokio::test]
async fn newly_submitted_plan_is_stamped_with_current_cpm_version() {
    let db = TempDb::new();
    let plan_id = submit_parallel_chains(&db.path).await;
    let conn = rusqlite::Connection::open(&db.path).unwrap();
    let v: i64 = conn
        .query_row(
            "SELECT cpm_version FROM plans WHERE plan_id = ?1",
            [&plan_id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, cpm_planner::algorithm::CPM_VERSION);
}

#[tokio::test]
async fn store_opens_when_a_stored_graph_is_undecodable() {
    let db = TempDb::new();
    let broken = submit_parallel_chains(&db.path).await;
    let healthy = open_planner(&db.path)
        .submit_plan(chain_graph())
        .await
        .expect("submit healthy");
    {
        let conn = rusqlite::Connection::open(&db.path).unwrap();
        conn.execute(
            "UPDATE plans SET graph = 'not json', cpm_version = 0 WHERE plan_id = ?1",
            [&broken.0],
        )
        .unwrap();
    }
    let planner = open_planner(&db.path);
    let status = planner.status(&healthy).await.unwrap();
    assert_eq!(status.deliverables.len(), 2);
}

#[tokio::test]
async fn pre_versioning_database_is_upgraded_and_repaired() {
    let db = TempDb::new();
    let plan_id = submit_parallel_chains(&db.path).await;
    {
        let conn = rusqlite::Connection::open(&db.path).unwrap();
        let mut result: serde_json::Value = serde_json::from_str(
            &conn
                .query_row("SELECT cached_result FROM plans", [], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap(),
        )
        .unwrap();
        result["critical_path"] = serde_json::json!(["P0a", "P1a", "P0b", "P1b"]);
        conn.execute("UPDATE plans SET cached_result = ?1", [result.to_string()])
            .unwrap();
        // Strip cpm_version and reset to the unversioned layout.
        conn.execute_batch(
            "ALTER TABLE plans DROP COLUMN cpm_version;
             PRAGMA user_version = 0;",
        )
        .unwrap();
    }
    let status = open_planner(&db.path).status(&plan_id).await.unwrap();
    assert_eq!(
        status.critical_path,
        vec!["__start__", "P0a", "P0b", "__finish__"]
    );
}

#[test]
fn newer_schema_version_is_rejected() {
    let db = TempDb::new();
    drop(SqlitePlanStore::open(&db.path).unwrap());
    rusqlite::Connection::open(&db.path)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    assert!(SqlitePlanStore::open(&db.path).is_err());
}

#[tokio::test]
async fn stored_p2_plan_gains_endpoints_after_reopen() {
    let db = TempDb::new();
    let plan_id = submit_parallel_chains(&db.path).await;
    {
        let conn = rusqlite::Connection::open(&db.path).unwrap();
        conn.execute("UPDATE plans SET cpm_version = 1", [])
            .unwrap();
    }
    let status = open_planner(&db.path).status(&plan_id).await.unwrap();
    assert_eq!(
        status.critical_path,
        vec!["__start__", "P0a", "P0b", "__finish__"]
    );
}

#[tokio::test]
async fn append_holders_survive_reopen() {
    let db = TempDb::new();
    let append = |id: &str| {
        let mut d = effort_deliverable(id, &[], 1.0);
        d.owned_files = vec![cpm_planner::plan::OwnedFile::Claim {
            path: PathBuf::from("REGISTRY.md"),
            mode: Some(cpm_planner::plan::FileMode::Append),
        }];
        d
    };
    let mut exclusive = effort_deliverable("c", &["a", "b"], 1.0);
    exclusive.owned_files = vec!["REGISTRY.md".into()];
    let plan_id = {
        let planner = open_planner(&db.path);
        let plan_id = planner
            .submit_plan(PlanGraph {
                deliverables: vec![append("a"), append("b"), exclusive],
                max_chained_dispatch: None,
            })
            .await
            .unwrap();
        let cohort = planner
            .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("c1"), 3))
            .await
            .unwrap();
        assert_eq!(cohort.rows.len(), 2);
        plan_id
    };
    let status = open_planner(&db.path).status(&plan_id).await.unwrap();
    assert_eq!(status.locks_held.len(), 2);
}

#[tokio::test]
async fn stored_graph_with_reserved_endpoint_id_is_left_unrecomputed() {
    let db = TempDb::new();
    let plan_id = submit_parallel_chains(&db.path).await;
    {
        let conn = rusqlite::Connection::open(&db.path).unwrap();
        conn.execute(
            "UPDATE plans SET graph = replace(graph, '\"P0b\"', '\"__finish__\"'), cpm_version = 0",
            [],
        )
        .unwrap();
    }
    drop(open_planner(&db.path));
    let v: i64 = rusqlite::Connection::open(&db.path)
        .unwrap()
        .query_row(
            "SELECT cpm_version FROM plans WHERE plan_id = ?1",
            [&plan_id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, 0);
}

#[tokio::test]
async fn startup_reap_with_incomplete_prerequisites_becomes_pending() {
    let db = TempDb::new();
    let plan_id = {
        let planner = open_planner(&db.path);
        planner.submit_plan(chain_graph()).await.expect("submit")
    };
    {
        // d2 holds an expired lease while its prerequisite d1 is not complete
        // (as after a revision): in_progress + an expired lock row.
        let conn = rusqlite::Connection::open(&db.path).expect("open");
        let in_progress = serde_json::to_string(&DeliverableStatus::InProgress).expect("json");
        conn.execute(
            "UPDATE deliverable_statuses SET status = ?1 WHERE deliverable_id = 'd2'",
            rusqlite::params![in_progress],
        )
        .expect("set status");
        conn.execute(
            "INSERT INTO locks (plan_id, deliverable_id, caller_id, acquired_at_us, expires_at_us)
             VALUES (?1, 'd2', 'w', 0, 1)",
            rusqlite::params![plan_id.0],
        )
        .expect("insert lock");
    }
    let planner = open_planner(&db.path);
    let status = planner.status(&plan_id).await.expect("status");
    assert_eq!(status.deliverables[1].1, DeliverableStatus::Pending);
}

// ---------------------------------------------------------------------
// Lockless in_progress marks across a restart
// ---------------------------------------------------------------------

/// Submit the chain plan and mark d1 `in_progress` with no lease (owner or
/// manual work), then drop the planner: one simulated process.
async fn lockless_in_progress_d1(path: &Path, graph: PlanGraph) -> cpm_planner::plan::PlanId {
    let planner = open_planner(path);
    let plan_id = planner.submit_plan(graph).await.expect("submit");
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "d1",
            caller("owner"),
            DeliverableStatus::InProgress,
        ))
        .await
        .expect("lockless in_progress mark");
    plan_id
}

#[tokio::test]
async fn lockless_in_progress_survives_restart() {
    let db = TempDb::new();
    let plan_id = lockless_in_progress_d1(&db.path, chain_graph()).await;
    let status = open_planner(&db.path)
        .status(&plan_id)
        .await
        .expect("status after reopen");
    assert_eq!(status.deliverables[0].1, DeliverableStatus::InProgress);
}

#[tokio::test]
async fn lockless_in_progress_restart_records_no_lapse() {
    let db = TempDb::new();
    let plan_id = lockless_in_progress_d1(&db.path, chain_graph()).await;
    let status = open_planner(&db.path)
        .status(&plan_id)
        .await
        .expect("status after reopen");
    assert_eq!(status.deliverables[0].4, 0);
}

#[tokio::test]
async fn lockless_earned_pct_survives_restart() {
    let db = TempDb::new();
    let mut graph = chain_graph();
    for d in &mut graph.deliverables {
        d.earning_rule = Some(cpm_planner::earned_value::EarningRule::FiftyFifty);
    }
    let plan_id = lockless_in_progress_d1(&db.path, graph).await;
    open_planner(&db.path)
        .baseline(cpm_planner::earned_value::BaselineRequest::new(
            plan_id.clone(),
        ))
        .await
        .expect("baseline");
    let report = open_planner(&db.path)
        .ev(&plan_id, None)
        .await
        .expect("ev after reopen");
    assert_eq!(report.rows[0].earned_pct, 50.0);
}

#[tokio::test]
async fn lease_held_in_progress_without_lock_is_still_quarantined_on_restart() {
    let db = TempDb::new();
    let plan_id = {
        let planner = open_planner(&db.path);
        let plan_id = planner.submit_plan(chain_graph()).await.expect("submit");
        planner
            .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w"), 1))
            .await
            .expect("acquire d1");
        plan_id
    };
    // The lease row is gone while d1 is still in_progress by lease.
    rusqlite::Connection::open(&db.path)
        .expect("open")
        .execute("DELETE FROM locks", [])
        .expect("delete lock row");
    let status = open_planner(&db.path)
        .status(&plan_id)
        .await
        .expect("status after reopen");
    assert_eq!(
        status.deliverables[0],
        ("d1".to_string(), DeliverableStatus::Ready, 1, 0, 1)
    );
}

#[tokio::test]
async fn lease_after_lockless_hand_back_is_quarantined_normally_after_expiry() {
    let db = TempDb::new();
    let ancient = Utc
        .with_ymd_and_hms(2020, 1, 1, 0, 0, 0)
        .single()
        .expect("valid t0");
    let plan_id = lockless_in_progress_d1(&db.path, chain_graph()).await;
    {
        // The owner hands d1 back, and a worker leases it; the lease has
        // lapsed by the real clock the startup sweep uses.
        let planner =
            open_planner_with_clock(&db.path, Duration::from_secs(60), TestClock::at(ancient));
        planner
            .mark_status(MarkStatusRequest::new(
                plan_id.clone(),
                "d1",
                caller("owner"),
                DeliverableStatus::Ready,
            ))
            .await
            .expect("lockless ready mark");
        planner
            .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller("w"), 1))
            .await
            .expect("acquire d1");
    }
    let status = open_planner(&db.path)
        .status(&plan_id)
        .await
        .expect("status after reopen");
    assert_eq!(
        status.deliverables[0],
        ("d1".to_string(), DeliverableStatus::Ready, 1, 0, 1)
    );
}

#[tokio::test]
async fn revise_keeps_lockless_in_progress_flag() {
    let db = TempDb::new();
    let plan_id = lockless_in_progress_d1(&db.path, chain_graph()).await;
    {
        // Change only d2, so d1 survives with its status carried over.
        let mut graph = chain_graph();
        graph.deliverables[1].estimated_effort_hours = Some(2.0);
        open_planner(&db.path)
            .revise_plan(cpm_planner::plan::ReviseRequest::new(
                plan_id.clone(),
                graph,
            ))
            .await
            .expect("revise");
    }
    let status = open_planner(&db.path)
        .status(&plan_id)
        .await
        .expect("status after reopen");
    assert_eq!(status.deliverables[0].1, DeliverableStatus::InProgress);
}
