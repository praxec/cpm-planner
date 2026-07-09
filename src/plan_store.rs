//! SQLite persistence for [`crate::planner::BasicCpmPlanner`].
//!
//! The store is the single source of truth for planner state: submitted
//! plans (graph + cached CPM result), per-deliverable statuses, held
//! cohort locks, and the submit-dedup map. Every planner operation loads
//! the relevant [`PlanState`] from SQLite, runs the in-memory scheduling
//! logic, and writes the result back — all inside ONE
//! `BEGIN IMMEDIATE` transaction.
//!
//! # Cross-process atomicity
//!
//! `TransactionBehavior::Immediate` takes the database write lock at
//! `BEGIN`, so the whole read-modify-write of an `acquire_cohort` (ready
//! check + within-cohort file disjointness + disjoint-from-held-locks +
//! lock insert + status flip to `in_progress`) is serialised across
//! processes. Two concurrent acquirers — even in different OS processes —
//! can never both observe the same "ready and unlocked" deliverable, so
//! double-acquisition of a deliverable (or of file-overlapping
//! deliverables) is structurally impossible.
//!
//! WAL mode keeps concurrent readers cheap; `busy_timeout` makes writers
//! queue behind each other instead of erroring.
//!
//! # Startup quarantine
//!
//! [`SqlitePlanStore::open`] reaps every lock whose TTL has already
//! lapsed: the lock row is deleted and the deliverable's status goes back
//! to `ready` (its prerequisites were complete when it was acquired and
//! TTL expiry does not unwind upstream work — the same rule as
//! [`PlanState::reap_expired`]). A deliverable left `in_progress` with no
//! lock row at all (a crash between partial writes on a pre-WAL database,
//! or manual surgery) is likewise reset to `ready`. Locks that are still
//! within TTL are preserved: another process may legitimately be working
//! under them, and clearing them on an unrelated restart would break the
//! cross-process contract.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::locks::PlanState;
use crate::plan::{CallerId, DeliverableStatus, LockInfo, PlanGraph, PlanId, PlannerError};
use crate::task::CriticalPathResult;

/// Environment variable that overrides the default database path.
/// The special value `:memory:` selects a private in-memory database
/// (useful for tests / ephemeral runs).
pub const DB_PATH_ENV: &str = "CPM_PLANNER_DB";

/// Default on-disk location relative to `$HOME`.
const DEFAULT_DB_RELATIVE: &str = ".local/share/praxec/cpm-planner.db";

/// Map any backend failure into the planner's wire-stable error variant.
fn backend(err: impl Into<anyhow::Error>) -> PlannerError {
    PlannerError::BackendError(err.into())
}

/// Convert a stored microsecond timestamp back into a `DateTime<Utc>`.
fn dt_from_micros(us: i64, column: &str) -> Result<DateTime<Utc>, PlannerError> {
    DateTime::from_timestamp_micros(us)
        .ok_or_else(|| backend(anyhow!("corrupt timestamp in column {column}: {us}")))
}

/// SQLite-backed persistence for the planner.
///
/// One instance per process. The inner `Mutex<Connection>` serialises
/// in-process callers; `BEGIN IMMEDIATE` serialises across processes.
/// The mutex is never held across an `.await`.
pub struct SqlitePlanStore {
    conn: Mutex<Connection>,
}

impl SqlitePlanStore {
    /// Open (creating if necessary) the database at `path`. Parent
    /// directories are created. The special path `:memory:` opens a
    /// private in-memory database.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if path.as_os_str() == ":memory:" {
            return Self::open_in_memory();
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating parent directory for {}", path.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite database at {}", path.display()))?;
        Self::init(conn)
    }

    /// Open a private in-memory database. State does NOT survive the
    /// process and is NOT shared with other connections — this is the
    /// test / ephemeral configuration.
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory sqlite database")?;
        Self::init(conn)
    }

    /// Open the database at [`Self::default_db_path`].
    pub fn open_default() -> anyhow::Result<Self> {
        Self::open(&Self::default_db_path()?)
    }

    /// Resolve the database path: `$CPM_PLANNER_DB` if set, else
    /// `~/.local/share/praxec/cpm-planner.db`.
    pub fn default_db_path() -> anyhow::Result<PathBuf> {
        if let Some(overridden) = std::env::var_os(DB_PATH_ENV) {
            return Ok(PathBuf::from(overridden));
        }
        let home = std::env::var_os("HOME").ok_or_else(|| {
            anyhow!("HOME is not set and {DB_PATH_ENV} was not provided; cannot locate database")
        })?;
        Ok(PathBuf::from(home).join(DEFAULT_DB_RELATIVE))
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        // WAL + busy_timeout: concurrent processes queue on the write
        // lock instead of failing; readers never block the writer.
        // (`execute_batch` tolerates pragmas that return a row.)
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )
        .context("applying sqlite pragmas")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS plans (
                 plan_id       TEXT PRIMARY KEY,
                 graph         TEXT NOT NULL,
                 cached_result TEXT NOT NULL,
                 created_at_us INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS deliverable_statuses (
                 plan_id        TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE,
                 deliverable_id TEXT NOT NULL,
                 status         TEXT NOT NULL,
                 PRIMARY KEY (plan_id, deliverable_id)
             );
             CREATE TABLE IF NOT EXISTS locks (
                 plan_id        TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE,
                 deliverable_id TEXT NOT NULL,
                 caller_id      TEXT NOT NULL,
                 acquired_at_us INTEGER NOT NULL,
                 expires_at_us  INTEGER NOT NULL,
                 PRIMARY KEY (plan_id, deliverable_id)
             );
             CREATE TABLE IF NOT EXISTS submit_dedup (
                 graph_hash TEXT PRIMARY KEY,
                 plan_id    TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE
             );",
        )
        .context("creating planner tables")?;

        let store = Self {
            conn: Mutex::new(conn),
        };
        store
            .quarantine_expired(Utc::now())
            .context("quarantining expired locks at startup")?;
        Ok(store)
    }

    /// Reap every lock whose TTL lapsed before `now`: the deliverable
    /// goes back to `ready` and the lock row is deleted. Also resets any
    /// orphaned `in_progress` deliverable that has no lock row. Returns
    /// the number of expired locks reaped. Public so operators/tests can
    /// force a sweep; `open` runs it automatically.
    pub fn quarantine_expired(&self, now: DateTime<Utc>) -> anyhow::Result<usize> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("planner store mutex poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let ready = serde_json::to_string(&DeliverableStatus::Ready)?;
        let in_progress = serde_json::to_string(&DeliverableStatus::InProgress)?;
        let now_us = now.timestamp_micros();

        tx.execute(
            "UPDATE deliverable_statuses SET status = ?1
             WHERE (plan_id, deliverable_id) IN
                   (SELECT plan_id, deliverable_id FROM locks WHERE expires_at_us < ?2)",
            params![ready, now_us],
        )?;
        let reaped = tx.execute(
            "DELETE FROM locks WHERE expires_at_us < ?1",
            params![now_us],
        )?;

        // Orphaned in_progress with no lock at all: cannot be legitimately
        // held by anyone, so it goes back to the pool.
        let orphaned = tx.execute(
            "UPDATE deliverable_statuses SET status = ?1
             WHERE status = ?2
               AND (plan_id, deliverable_id) NOT IN
                   (SELECT plan_id, deliverable_id FROM locks)",
            params![ready, in_progress],
        )?;

        tx.commit()?;
        if reaped > 0 || orphaned > 0 {
            tracing::warn!(
                expired_locks = reaped,
                orphaned_in_progress = orphaned,
                "quarantined stale planner state"
            );
        }
        Ok(reaped)
    }

    // -----------------------------------------------------------------
    // Planner-facing primitives
    // -----------------------------------------------------------------

    /// Idempotent submit: inside ONE immediate transaction, return the
    /// existing `PlanId` for `graph_hash` if present, otherwise run
    /// `build` (pure CPU: CPM + initial statuses) and persist the new
    /// plan + dedup row. The transaction closes the TOCTOU window between
    /// concurrent identical submissions across processes.
    pub(crate) fn submit_or_get(
        &self,
        graph_hash: &str,
        build: impl FnOnce() -> Result<(PlanId, PlanState), PlannerError>,
    ) -> Result<PlanId, PlannerError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(backend)?;

        let existing: Option<String> = tx
            .query_row(
                "SELECT plan_id FROM submit_dedup WHERE graph_hash = ?1",
                params![graph_hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(plan_id) = existing {
            return Ok(PlanId(plan_id));
        }

        let (plan_id, state) = build()?;
        let graph_json = serde_json::to_string(&state.graph).map_err(backend)?;
        let result_json = serde_json::to_string(&state.cached_result).map_err(backend)?;
        tx.execute(
            "INSERT INTO plans (plan_id, graph, cached_result, created_at_us)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                plan_id.0,
                graph_json,
                result_json,
                Utc::now().timestamp_micros()
            ],
        )
        .map_err(backend)?;
        save_plan_state(&tx, &plan_id, &state)?;
        tx.execute(
            "INSERT INTO submit_dedup (graph_hash, plan_id) VALUES (?1, ?2)",
            params![graph_hash, plan_id.0],
        )
        .map_err(backend)?;

        tx.commit().map_err(backend)?;
        Ok(plan_id)
    }

    /// Load the plan, hand a mutable [`PlanState`] to `f`, persist the
    /// mutated statuses + locks, and commit — all inside ONE
    /// `BEGIN IMMEDIATE` transaction. An `Err` from `f` rolls the
    /// transaction back, so failed operations never persist partial
    /// mutations.
    pub(crate) fn mutate_plan<R>(
        &self,
        plan_id: &PlanId,
        f: impl FnOnce(&mut PlanState) -> Result<R, PlannerError>,
    ) -> Result<R, PlannerError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(backend)?;
        let mut state =
            load_plan_state(&tx, plan_id)?.ok_or_else(|| PlannerError::PlanNotFound {
                plan_id: plan_id.0.clone(),
            })?;
        let out = f(&mut state)?;
        save_plan_state(&tx, plan_id, &state)?;
        tx.commit().map_err(backend)?;
        Ok(out)
    }

    /// Read-only snapshot of a plan under a deferred transaction (a
    /// consistent WAL read snapshot that never blocks writers).
    pub(crate) fn read_plan<R>(
        &self,
        plan_id: &PlanId,
        f: impl FnOnce(&PlanState) -> R,
    ) -> Result<R, PlannerError> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(backend)?;
        let state = load_plan_state(&tx, plan_id)?.ok_or_else(|| PlannerError::PlanNotFound {
            plan_id: plan_id.0.clone(),
        })?;
        Ok(f(&state))
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, PlannerError> {
        self.conn
            .lock()
            .map_err(|_| backend(anyhow!("planner store mutex poisoned")))
    }
}

/// Load the full [`PlanState`] for `plan_id`, or `None` if the plan does
/// not exist. The `file -> deliverable` inverse index is rebuilt from the
/// persisted locks + graph (it is derived state; persisting it separately
/// could only ever drift).
fn load_plan_state(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
) -> Result<Option<PlanState>, PlannerError> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT graph, cached_result FROM plans WHERE plan_id = ?1",
            params![plan_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(backend)?;
    let Some((graph_json, result_json)) = row else {
        return Ok(None);
    };
    let graph: PlanGraph = serde_json::from_str(&graph_json).map_err(backend)?;
    let cached_result: CriticalPathResult = serde_json::from_str(&result_json).map_err(backend)?;

    let mut statuses: HashMap<String, DeliverableStatus> = HashMap::new();
    {
        let mut stmt = tx
            .prepare("SELECT deliverable_id, status FROM deliverable_statuses WHERE plan_id = ?1")
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![plan_id.0], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(backend)?;
        for row in rows {
            let (id, status_json) = row.map_err(backend)?;
            let status: DeliverableStatus = serde_json::from_str(&status_json).map_err(backend)?;
            statuses.insert(id, status);
        }
    }

    let mut locks: HashMap<String, LockInfo> = HashMap::new();
    {
        let mut stmt = tx
            .prepare(
                "SELECT deliverable_id, caller_id, acquired_at_us, expires_at_us
                 FROM locks WHERE plan_id = ?1",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![plan_id.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(backend)?;
        for row in rows {
            let (deliverable_id, caller_id, acquired_us, expires_us) = row.map_err(backend)?;
            locks.insert(
                deliverable_id.clone(),
                LockInfo {
                    plan_id: plan_id.clone(),
                    deliverable_id,
                    caller_id: CallerId(caller_id),
                    acquired_at: dt_from_micros(acquired_us, "locks.acquired_at_us")?,
                    expires_at: dt_from_micros(expires_us, "locks.expires_at_us")?,
                },
            );
        }
    }

    // Rebuild the inverse file index from held locks + graph ownership.
    let mut file_to_deliverable: HashMap<PathBuf, String> = HashMap::new();
    for deliverable_id in locks.keys() {
        if let Some(d) = graph.deliverables.iter().find(|d| &d.id == deliverable_id) {
            for f in &d.owned_files {
                file_to_deliverable.insert(f.clone(), deliverable_id.clone());
            }
        }
    }

    Ok(Some(PlanState {
        graph,
        statuses,
        locks,
        file_to_deliverable,
        cached_result,
    }))
}

/// Persist the mutable parts of a [`PlanState`] (statuses + locks). The
/// graph and cached CPM result are immutable after submit and are written
/// once by [`SqlitePlanStore::submit_or_get`].
fn save_plan_state(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
    state: &PlanState,
) -> Result<(), PlannerError> {
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO deliverable_statuses (plan_id, deliverable_id, status)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(plan_id, deliverable_id) DO UPDATE SET status = excluded.status",
            )
            .map_err(backend)?;
        for (deliverable_id, status) in &state.statuses {
            let status_json = serde_json::to_string(status).map_err(backend)?;
            stmt.execute(params![plan_id.0, deliverable_id, status_json])
                .map_err(backend)?;
        }
    }

    tx.execute("DELETE FROM locks WHERE plan_id = ?1", params![plan_id.0])
        .map_err(backend)?;
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO locks
                     (plan_id, deliverable_id, caller_id, acquired_at_us, expires_at_us)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(backend)?;
        for (deliverable_id, lock) in &state.locks {
            stmt.execute(params![
                plan_id.0,
                deliverable_id,
                lock.caller_id.0,
                lock.acquired_at.timestamp_micros(),
                lock.expires_at.timestamp_micros(),
            ])
            .map_err(backend)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Deliverable;
    use crate::task::CriticalPathResult;

    fn plan_state_with_one_ready() -> PlanState {
        let graph = PlanGraph {
            deliverables: vec![Deliverable {
                id: "d1".to_string(),
                owned_files: vec![PathBuf::from("src/a.rs")],
                prerequisites: vec![],
                estimated_effort_hours: Some(1.0),
                metadata: serde_json::Value::Null,
            }],
            max_chained_dispatch: None,
        };
        let mut statuses = HashMap::new();
        statuses.insert("d1".to_string(), DeliverableStatus::Ready);
        PlanState::new(graph, statuses, CriticalPathResult::default())
    }

    #[test]
    fn submit_or_get_is_idempotent_within_one_store() {
        let store = SqlitePlanStore::open_in_memory().unwrap();
        let first = store
            .submit_or_get("hash-1", || {
                Ok((PlanId("plan_a".into()), plan_state_with_one_ready()))
            })
            .unwrap();
        let second = store
            .submit_or_get("hash-1", || {
                panic!("build must not run on a dedup hit");
            })
            .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn mutate_plan_persists_statuses_and_locks() {
        let store = SqlitePlanStore::open_in_memory().unwrap();
        let plan_id = store
            .submit_or_get("hash-1", || {
                Ok((PlanId("plan_a".into()), plan_state_with_one_ready()))
            })
            .unwrap();

        let now = Utc::now();
        store
            .mutate_plan(&plan_id, |state| {
                state
                    .statuses
                    .insert("d1".to_string(), DeliverableStatus::InProgress);
                state.locks.insert(
                    "d1".to_string(),
                    LockInfo {
                        plan_id: plan_id.clone(),
                        deliverable_id: "d1".to_string(),
                        caller_id: CallerId("c1".to_string()),
                        acquired_at: now,
                        expires_at: now + chrono::Duration::seconds(60),
                    },
                );
                Ok(())
            })
            .unwrap();

        store
            .read_plan(&plan_id, |state| {
                assert_eq!(
                    state.statuses.get("d1"),
                    Some(&DeliverableStatus::InProgress)
                );
                let lock = state.locks.get("d1").expect("lock persisted");
                assert_eq!(lock.caller_id, CallerId("c1".to_string()));
                // Inverse file index rebuilt from locks + graph.
                assert_eq!(
                    state.file_to_deliverable.get(&PathBuf::from("src/a.rs")),
                    Some(&"d1".to_string())
                );
            })
            .unwrap();
    }

    #[test]
    fn mutate_plan_error_rolls_back() {
        let store = SqlitePlanStore::open_in_memory().unwrap();
        let plan_id = store
            .submit_or_get("hash-1", || {
                Ok((PlanId("plan_a".into()), plan_state_with_one_ready()))
            })
            .unwrap();

        let err = store.mutate_plan(&plan_id, |state| {
            state
                .statuses
                .insert("d1".to_string(), DeliverableStatus::Complete);
            Err::<(), _>(PlannerError::LockNotHeld {
                caller_id: "c1".to_string(),
                deliverable_id: "d1".to_string(),
            })
        });
        assert!(matches!(err, Err(PlannerError::LockNotHeld { .. })));

        store
            .read_plan(&plan_id, |state| {
                assert_eq!(state.statuses.get("d1"), Some(&DeliverableStatus::Ready));
            })
            .unwrap();
    }

    #[test]
    fn unknown_plan_is_plan_not_found() {
        let store = SqlitePlanStore::open_in_memory().unwrap();
        let missing = PlanId("plan_missing".into());
        let err = store.read_plan(&missing, |_| ());
        assert!(matches!(err, Err(PlannerError::PlanNotFound { .. })));
    }
}
