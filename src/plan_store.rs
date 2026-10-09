//! SQLite persistence for [`crate::planner::BasicCpmPlanner`].
//!
//! The store is the single source of truth for planner state: submitted
//! plans (graph + cached CPM result), per-deliverable statuses, held
//! cohort locks, the submit-dedup map, and (schema v3) the portfolio tables
//! — named plan lines, variants and revisions, queried by
//! `crate::portfolio` — and (schema v4) the earned-value tables, accessed
//! through `crate::ev_store`. Every planner operation loads
//! the relevant [`PlanState`] from SQLite, runs the in-memory scheduling
//! logic, and writes the result back — all inside ONE
//! `BEGIN IMMEDIATE` transaction.
//!
//! # Cross-process atomicity
//!
//! `TransactionBehavior::Immediate` takes the database write lock at
//! `BEGIN`, so the whole read-modify-write of an `acquire_cohort` (ready
//! check + within-cohort file-claim conflicts + conflicts with held locks +
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
//! [`PlanState::reap_expired`]), and the lease's hours up to expiry are
//! added to `ev_actuals.leased_hours`. A deliverable left `in_progress` with no
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
pub(crate) fn backend(err: impl Into<anyhow::Error>) -> PlannerError {
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
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directory for {}", path.display()))?;
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

    fn init(mut conn: Connection) -> anyhow::Result<Self> {
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

        migrate(&mut conn)?;

        let store = Self {
            conn: Mutex::new(conn),
        };
        store
            .quarantine_expired(Utc::now())
            .context("quarantining expired locks at startup")?;
        Ok(store)
    }

    /// Reap every lock whose TTL lapsed before `now`: the deliverable
    /// is re-derived (`ready` if every prerequisite is complete, else
    /// `pending`), its `lapse_count` is incremented (the lease
    /// was lost ENVIRONMENTALLY — no terminal mark was ever recorded —
    /// so it feeds the lapse bound, never the failure circuit-breaker),
    /// and the lock row is deleted. Also resets any orphaned
    /// `in_progress` deliverable that has no lock row, likewise counted
    /// as a lapse. `attempt_count` and `failure_count` are preserved
    /// untouched. Returns the number of expired locks reaped. Public so
    /// operators/tests can force a sweep; `open` runs it automatically.
    pub fn quarantine_expired(&self, now: DateTime<Utc>) -> anyhow::Result<usize> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("planner store mutex poisoned"))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let in_progress = serde_json::to_string(&DeliverableStatus::InProgress)?;
        let now_us = now.timestamp_micros();

        // Targets: deliverables with an expired lock, plus orphaned
        // in_progress ones with no lock at all (cannot be legitimately held
        // by anyone). Losing the lease without a terminal mark is an
        // environmental loss, so each counts as a lapse.
        let expired: Vec<(String, String)> = {
            let mut stmt =
                tx.prepare("SELECT plan_id, deliverable_id FROM locks WHERE expires_at_us < ?1")?;
            stmt.query_map(params![now_us], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        // Each expired lease is credited with its hours up to expiry (the
        // in-memory reaper's rule). Orphans have no lock row, so no hours.
        {
            let mut stmt = tx.prepare(
                "SELECT plan_id, deliverable_id, acquired_at_us, expires_at_us
                 FROM locks WHERE expires_at_us < ?1",
            )?;
            let leases: Vec<(String, String, i64, i64)> = stmt
                .query_map(params![now_us], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<Result<_, _>>()?;
            for (plan_id, deliverable_id, acquired_us, expires_us) in leases {
                let start = dt_from_micros(acquired_us, "locks.acquired_at_us")
                    .map_err(|e| anyhow!("{e}"))?;
                let end = dt_from_micros(expires_us, "locks.expires_at_us")
                    .map_err(|e| anyhow!("{e}"))?;
                let hours = crate::locks::hours_between(start, end);
                crate::ev_store::add_leased_hours(
                    &tx,
                    &PlanId(plan_id),
                    &deliverable_id,
                    hours,
                    end,
                )
                .map_err(|e| anyhow!("{e}"))?;
            }
        }
        let orphans: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT plan_id, deliverable_id FROM deliverable_statuses
                 WHERE status = ?1
                   AND (plan_id, deliverable_id) NOT IN
                       (SELECT plan_id, deliverable_id FROM locks)",
            )?;
            stmt.query_map(params![in_progress], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };

        // Re-derive (Ready if every prerequisite is Complete, else Pending)
        // with the same rule as the in-memory reaper.
        type PlanSnapshot = Option<(PlanGraph, HashMap<String, DeliverableStatus>)>;
        let mut plans: HashMap<String, PlanSnapshot> = HashMap::new();
        for (plan_id, deliverable_id) in expired.iter().chain(orphans.iter()) {
            if !plans.contains_key(plan_id) {
                let graph: Option<PlanGraph> = tx
                    .query_row(
                        "SELECT graph FROM plans WHERE plan_id = ?1",
                        params![plan_id],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .and_then(|g| serde_json::from_str(&g).ok());
                let mut statuses = HashMap::new();
                let mut stmt = tx.prepare(
                    "SELECT deliverable_id, status FROM deliverable_statuses WHERE plan_id = ?1",
                )?;
                for row in stmt.query_map(params![plan_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })? {
                    let (id, st) = row?;
                    if let Ok(st) = serde_json::from_str(&st) {
                        statuses.insert(id, st);
                    }
                }
                plans.insert(plan_id.clone(), graph.map(|g| (g, statuses)));
            }
            // An undecodable stored graph cannot be consulted: fall back to Ready.
            let status = plans
                .get(plan_id)
                .and_then(Option::as_ref)
                .and_then(|(g, st)| {
                    g.deliverables
                        .iter()
                        .find(|d| &d.id == deliverable_id)
                        .map(|d| crate::locks::rederive_status(d, st))
                })
                .unwrap_or(DeliverableStatus::Ready);
            tx.execute(
                "UPDATE deliverable_statuses SET status = ?1, lapse_count = lapse_count + 1
                 WHERE plan_id = ?2 AND deliverable_id = ?3",
                params![serde_json::to_string(&status)?, plan_id, deliverable_id],
            )?;
        }
        let reaped = tx.execute(
            "DELETE FROM locks WHERE expires_at_us < ?1",
            params![now_us],
        )?;
        let orphaned = orphans.len();

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
        insert_plan(&tx, &plan_id, &state, Utc::now())?;
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
    ///
    /// Ungated; production execution paths use
    /// [`Self::mutate_executable_plan`].
    #[cfg(test)]
    pub(crate) fn mutate_plan<R>(
        &self,
        plan_id: &PlanId,
        f: impl FnOnce(&mut PlanState) -> Result<R, PlannerError>,
    ) -> Result<R, PlannerError> {
        self.mutate_plan_inner(plan_id, false, f)
    }

    /// [`Self::mutate_plan`] for an execution operation: inside the same
    /// transaction, first refuse a named variant that is not its line's
    /// selected variant (`VARIANT_NOT_SELECTED`, see
    /// [`crate::portfolio::ensure_executable`]).
    pub(crate) fn mutate_executable_plan<R>(
        &self,
        plan_id: &PlanId,
        f: impl FnOnce(&mut PlanState) -> Result<R, PlannerError>,
    ) -> Result<R, PlannerError> {
        self.mutate_plan_inner(plan_id, true, f)
    }

    fn mutate_plan_inner<R>(
        &self,
        plan_id: &PlanId,
        gated: bool,
        f: impl FnOnce(&mut PlanState) -> Result<R, PlannerError>,
    ) -> Result<R, PlannerError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(backend)?;
        if gated {
            crate::portfolio::ensure_executable(&tx, plan_id)?;
        }
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

    /// [`Self::read_plan`] that also returns, from the same snapshot, the
    /// named variant owning the plan (`None` for an unnamed plan).
    pub(crate) fn read_plan_and_variant<R>(
        &self,
        plan_id: &PlanId,
        f: impl FnOnce(&PlanState) -> R,
    ) -> Result<(R, Option<crate::portfolio::VariantInfo>), PlannerError> {
        self.read_tx(|tx| {
            let state =
                load_plan_state(tx, plan_id)?.ok_or_else(|| PlannerError::PlanNotFound {
                    plan_id: plan_id.0.clone(),
                })?;
            let info = crate::portfolio::variant_info(tx, plan_id)?;
            Ok((f(&state), info))
        })
    }

    /// Run `f` inside ONE `BEGIN IMMEDIATE` transaction and commit on `Ok`.
    /// An `Err` from `f` rolls everything back. For multi-table writes
    /// (the portfolio operations) that do not fit [`Self::mutate_plan`].
    pub(crate) fn write_tx<R>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<R, PlannerError>,
    ) -> Result<R, PlannerError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(backend)?;
        let out = f(&tx)?;
        tx.commit().map_err(backend)?;
        Ok(out)
    }

    /// Run `f` over a consistent read snapshot (deferred transaction).
    pub(crate) fn read_tx<R>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<R, PlannerError>,
    ) -> Result<R, PlannerError> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(backend)?;
        f(&tx)
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, PlannerError> {
        self.conn
            .lock()
            .map_err(|_| backend(anyhow!("planner store mutex poisoned")))
    }
}

/// Schema migrations, applied in order. `PRAGMA user_version` records the
/// last one applied. Each step must be idempotent against databases created
/// before versioning existed (user_version 0 but tables present).
const MIGRATIONS: &[fn(&Connection) -> anyhow::Result<()>] = &[
    migrate_v1_base_schema,  // tables + counter columns (pre-versioning layout)
    migrate_v2_cpm_version,  // plans.cpm_version
    migrate_v3_portfolio,    // plan_lines, variants, revisions
    migrate_v4_earned_value, // baselines, ev_actuals, ev_snapshots
];

/// Runs the whole ladder plus the stale sweep in one immediate transaction,
/// so concurrent openers serialise instead of racing on `ALTER TABLE`.
/// `user_version` is read only after the write lock is held.
fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let supported = MIGRATIONS.len() as i64;
    if current > supported {
        return Err(anyhow!(
            "database schema version {current} is newer than this cpm-planner supports \
             ({supported}); upgrade cpm-planner"
        ));
    }
    for (i, step) in MIGRATIONS.iter().enumerate() {
        let version = i as i64 + 1;
        if version > current {
            step(&tx).with_context(|| format!("applying schema migration v{version}"))?;
            tx.pragma_update(None, "user_version", version)?;
        }
    }
    recompute_stale_results(&tx)?;
    tx.commit()?;
    Ok(())
}

fn migrate_v1_base_schema(conn: &Connection) -> anyhow::Result<()> {
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
             attempt_count  INTEGER NOT NULL DEFAULT 0,
             failure_count  INTEGER NOT NULL DEFAULT 0,
             lapse_count    INTEGER NOT NULL DEFAULT 0,
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

    migrate_counter_columns(conn)
}

fn migrate_v2_cpm_version(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(plans)")
        .context("probing plans columns")?;
    let has_column = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .context("reading plans column names")?
        .collect::<Result<Vec<_>, _>>()
        .context("reading plans column name")?
        .iter()
        .any(|c| c == "cpm_version");
    if !has_column {
        conn.execute_batch("ALTER TABLE plans ADD COLUMN cpm_version INTEGER NOT NULL DEFAULT 0")
            .context("adding plans.cpm_version column")?;
    }
    Ok(())
}

/// Portfolio tables: named plan lines, their variants (each one plan row),
/// and every variant's revision history. Legacy plans have no rows here
/// until their first revision (`revisions` only). `variants.content_hash`
/// is the plan file's content hash when `source_path` is set (file-backed
/// sync) and the canonical graph hash otherwise (inline sync, revise); drift
/// detection compares against a file hash only when `source_path` is set.
fn migrate_v3_portfolio(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plan_lines (
             project          TEXT,
             name             TEXT,
             selected_variant TEXT,
             archived         INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (project, name)
         );
         CREATE TABLE IF NOT EXISTS variants (
             project       TEXT,
             name          TEXT,
             variant       TEXT,
             plan_id       TEXT NOT NULL UNIQUE REFERENCES plans(plan_id) ON DELETE CASCADE,
             source_path   TEXT,
             content_hash  TEXT,
             head_revision INTEGER NOT NULL,
             archived      INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (project, name, variant)
         );
         CREATE TABLE IF NOT EXISTS revisions (
             plan_id       TEXT REFERENCES plans(plan_id) ON DELETE CASCADE,
             revision      INTEGER,
             graph         TEXT NOT NULL,
             content_hash  TEXT,
             created_at_us INTEGER NOT NULL,
             PRIMARY KEY (plan_id, revision)
         );",
    )
    .context("creating portfolio tables")
}

/// Earned-value tables (see [`crate::ev_store`]): numbered frozen
/// baselines, per-deliverable actuals (reported percent, actual hours,
/// accumulated lease hours, evidence list) and EV snapshots. Rows of a
/// deliverable later removed from the graph are kept: its baselined budget
/// and actual cost still count.
fn migrate_v4_earned_value(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS baselines (
             plan_id       TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE,
             number        INTEGER NOT NULL,
             start_us      INTEGER NOT NULL,
             calendar      TEXT,
             rows          TEXT NOT NULL,
             bac           REAL NOT NULL,
             reason        TEXT,
             created_at_us INTEGER NOT NULL,
             PRIMARY KEY (plan_id, number)
         );
         CREATE TABLE IF NOT EXISTS ev_actuals (
             plan_id        TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE,
             deliverable_id TEXT NOT NULL,
             earned_pct     INTEGER,
             actual_hours   REAL,
             leased_hours   REAL NOT NULL DEFAULT 0,
             evidence       TEXT NOT NULL DEFAULT '[]',
             updated_at_us  INTEGER,
             PRIMARY KEY (plan_id, deliverable_id)
         );
         CREATE TABLE IF NOT EXISTS ev_snapshots (
             plan_id     TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE,
             taken_at_us INTEGER NOT NULL,
             as_of_us    INTEGER NOT NULL,
             summary     TEXT NOT NULL,
             PRIMARY KEY (plan_id, taken_at_us)
         );",
    )
    .context("creating earned-value tables")
}

/// Recompute `cached_result` for every plan stored by an older CPM kernel.
/// A graph that no longer computes is left untouched and logged.
fn recompute_stale_results(conn: &Connection) -> anyhow::Result<()> {
    let stale: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT plan_id, graph FROM plans WHERE cpm_version < ?1")?;
        stmt.query_map([crate::algorithm::CPM_VERSION], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?
    };
    for (plan_id, graph_json) in stale {
        let graph: PlanGraph = match serde_json::from_str(&graph_json) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(%plan_id, error = %e, "could not decode stored graph; leaving cached result as is");
                continue;
            }
        };
        if graph
            .deliverables
            .iter()
            .any(|d| d.id == crate::plan::START_ID || d.id == crate::plan::FINISH_ID)
        {
            tracing::warn!(%plan_id, "stored graph uses a reserved endpoint id; leaving cached result as is");
            continue;
        }
        match crate::schedule::compute_cpm(&graph) {
            Ok(result) => {
                conn.execute(
                    "UPDATE plans SET cached_result = ?1, cpm_version = ?2 WHERE plan_id = ?3",
                    params![
                        serde_json::to_string(&result)?,
                        crate::algorithm::CPM_VERSION,
                        plan_id
                    ],
                )?;
            }
            Err(e) => tracing::warn!(%plan_id, error = %e, "could not recompute stored CPM result"),
        }
    }
    Ok(())
}

/// Migration: add the per-deliverable counter columns to databases
/// created before they existed — `attempt_count` (pre-circuit-breaker
/// databases), `failure_count` + `lapse_count` (databases from before
/// environmental lapses were split off failed attempts). Guarded by a
/// `PRAGMA table_info` probe so reopening an already-migrated (or
/// freshly created) database is a no-op — the live database is never
/// dropped or recreated. Every added column defaults to 0: on a legacy
/// database the acquire-derived `attempt_count` history is preserved as
/// telemetry but treated as NEITHER failures NOR lapses, so an existing
/// deliverable is exactly as far from both breakers as a fresh one.
fn migrate_counter_columns(conn: &Connection) -> anyhow::Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(deliverable_statuses)")
        .context("probing deliverable_statuses columns")?;
    let mut existing: Vec<String> = Vec::new();
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .context("reading deliverable_statuses column names")?;
    for name in names {
        existing.push(name.context("reading deliverable_statuses column name")?);
    }
    for column in ["attempt_count", "failure_count", "lapse_count"] {
        if !existing.iter().any(|c| c == column) {
            conn.execute_batch(&format!(
                "ALTER TABLE deliverable_statuses
                 ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"
            ))
            .with_context(|| format!("adding deliverable_statuses.{column} column"))?;
        }
    }
    Ok(())
}

/// Load the full [`PlanState`] for `plan_id`, or `None` if the plan does
/// not exist. The `file -> deliverable` inverse index is rebuilt from the
/// persisted locks + graph (it is derived state; persisting it separately
/// could only ever drift).
pub(crate) fn load_plan_state(
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
    let mut attempt_counts: HashMap<String, u32> = HashMap::new();
    let mut failure_counts: HashMap<String, u32> = HashMap::new();
    let mut lapse_counts: HashMap<String, u32> = HashMap::new();
    {
        let mut stmt = tx
            .prepare(
                "SELECT deliverable_id, status, attempt_count, failure_count, lapse_count
                 FROM deliverable_statuses WHERE plan_id = ?1",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![plan_id.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, u32>(4)?,
                ))
            })
            .map_err(backend)?;
        for row in rows {
            let (id, status_json, attempt_count, failure_count, lapse_count) =
                row.map_err(backend)?;
            let status: DeliverableStatus = serde_json::from_str(&status_json).map_err(backend)?;
            statuses.insert(id.clone(), status);
            if attempt_count > 0 {
                attempt_counts.insert(id.clone(), attempt_count);
            }
            if failure_count > 0 {
                failure_counts.insert(id.clone(), failure_count);
            }
            if lapse_count > 0 {
                lapse_counts.insert(id, lapse_count);
            }
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
    let mut file_claims: HashMap<PathBuf, crate::locks::FileClaim> = HashMap::new();
    for deliverable_id in locks.keys() {
        if let Some(d) = graph.deliverables.iter().find(|d| &d.id == deliverable_id) {
            crate::locks::add_file_claims(&mut file_claims, deliverable_id, &d.owned_files);
        }
    }

    Ok(Some(PlanState {
        graph,
        statuses,
        attempt_counts,
        failure_counts,
        lapse_counts,
        locks,
        file_claims,
        cached_result,
        leased_hours: HashMap::new(),
        reported_actuals: HashMap::new(),
        actuals_updated_at: None,
    }))
}

/// Insert the `plans` row for a new plan plus its statuses and locks.
pub(crate) fn insert_plan(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
    state: &PlanState,
    now: DateTime<Utc>,
) -> Result<(), PlannerError> {
    let graph_json = serde_json::to_string(&state.graph).map_err(backend)?;
    let result_json = serde_json::to_string(&state.cached_result).map_err(backend)?;
    tx.execute(
        "INSERT INTO plans (plan_id, graph, cached_result, created_at_us, cpm_version)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            plan_id.0,
            graph_json,
            result_json,
            now.timestamp_micros(),
            crate::algorithm::CPM_VERSION
        ],
    )
    .map_err(backend)?;
    save_plan_state(tx, plan_id, state)
}

/// Replace a plan's whole runtime state after a revision: the `plans` row's
/// graph and cached CPM result, and every status, counter and lock (rows of
/// removed deliverables are deleted).
pub(crate) fn replace_plan_state(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
    state: &PlanState,
) -> Result<(), PlannerError> {
    let graph_json = serde_json::to_string(&state.graph).map_err(backend)?;
    let result_json = serde_json::to_string(&state.cached_result).map_err(backend)?;
    tx.execute(
        "UPDATE plans SET graph = ?1, cached_result = ?2, cpm_version = ?3 WHERE plan_id = ?4",
        params![
            graph_json,
            result_json,
            crate::algorithm::CPM_VERSION,
            plan_id.0
        ],
    )
    .map_err(backend)?;
    tx.execute(
        "DELETE FROM deliverable_statuses WHERE plan_id = ?1",
        params![plan_id.0],
    )
    .map_err(backend)?;
    save_plan_state(tx, plan_id, state)
}

/// Persist the mutable parts of a [`PlanState`] (statuses, locks, and the
/// lease hours ended and progress reported in this transaction, applied to
/// `ev_actuals`). The
/// graph and cached CPM result are written by [`insert_plan`] and only
/// replaced by a revision ([`replace_plan_state`]).
pub(crate) fn save_plan_state(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
    state: &PlanState,
) -> Result<(), PlannerError> {
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO deliverable_statuses
                     (plan_id, deliverable_id, status, attempt_count, failure_count, lapse_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(plan_id, deliverable_id) DO UPDATE
                     SET status = excluded.status,
                         attempt_count = excluded.attempt_count,
                         failure_count = excluded.failure_count,
                         lapse_count = excluded.lapse_count",
            )
            .map_err(backend)?;
        for (deliverable_id, status) in &state.statuses {
            let status_json = serde_json::to_string(status).map_err(backend)?;
            stmt.execute(params![
                plan_id.0,
                deliverable_id,
                status_json,
                state.attempt_count(deliverable_id),
                state.failure_count(deliverable_id),
                state.lapse_count(deliverable_id)
            ])
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

    // Lease hours ended and progress reported in this transaction (deltas,
    // see `PlanState::leased_hours` / `reported_actuals`).
    if let Some(at) = state.actuals_updated_at {
        for (deliverable_id, hours) in &state.leased_hours {
            crate::ev_store::add_leased_hours(tx, plan_id, deliverable_id, *hours, at)?;
        }
        for (deliverable_id, report) in &state.reported_actuals {
            crate::ev_store::record_reported(tx, plan_id, deliverable_id, report, at)?;
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
                owned_files: vec!["src/a.rs".into()],
                prerequisites: vec![],
                estimated_effort_hours: Some(1.0),
                metadata: serde_json::Value::Null,
                duration_hours: None,
                estimate: None,
                milestone: false,
                earning_rule: None,
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
                    state.file_claims.get(&PathBuf::from("src/a.rs")),
                    Some(&crate::locks::FileClaim::Exclusive("d1".to_string()))
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

    #[test]
    fn attempt_counts_round_trip_through_the_store() {
        let store = SqlitePlanStore::open_in_memory().unwrap();
        let plan_id = store
            .submit_or_get("hash-1", || {
                Ok((PlanId("plan_a".into()), plan_state_with_one_ready()))
            })
            .unwrap();

        store
            .mutate_plan(&plan_id, |state| {
                *state.attempt_counts.entry("d1".to_string()).or_insert(0) += 1;
                Ok(())
            })
            .unwrap();

        store
            .read_plan(&plan_id, |state| {
                assert_eq!(state.attempt_count("d1"), 1);
            })
            .unwrap();
    }

    #[test]
    fn failure_and_lapse_counts_round_trip_through_the_store() {
        let store = SqlitePlanStore::open_in_memory().unwrap();
        let plan_id = store
            .submit_or_get("hash-1", || {
                Ok((PlanId("plan_a".into()), plan_state_with_one_ready()))
            })
            .unwrap();

        store
            .mutate_plan(&plan_id, |state| {
                *state.failure_counts.entry("d1".to_string()).or_insert(0) += 1;
                *state.lapse_counts.entry("d1".to_string()).or_insert(0) += 2;
                Ok(())
            })
            .unwrap();

        store
            .read_plan(&plan_id, |state| {
                assert_eq!(state.failure_count("d1"), 1);
                assert_eq!(state.lapse_count("d1"), 2);
            })
            .unwrap();
    }

    /// Unique on-disk temp db removed (with WAL sidecars) on drop, so a
    /// passing OR failing run leaves no litter behind.
    struct TempDbFile {
        path: PathBuf,
    }

    impl TempDbFile {
        fn new(tag: &str) -> Self {
            Self {
                path: std::env::temp_dir().join(format!(
                    "cpm-planner-store-test-{tag}-{}.db",
                    uuid::Uuid::new_v4().simple()
                )),
            }
        }
    }

    impl Drop for TempDbFile {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.path.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(p));
            }
        }
    }

    /// A v4 database stripped back to the v3 layout (earned-value tables
    /// dropped, `user_version` 3) holding one plan.
    fn v3_database(tag: &str) -> TempDbFile {
        let db = TempDbFile::new(tag);
        let store = SqlitePlanStore::open(&db.path).unwrap();
        store
            .submit_or_get("hash-1", || {
                Ok((PlanId("plan_v3".into()), plan_state_with_one_ready()))
            })
            .unwrap();
        drop(store);
        let conn = Connection::open(&db.path).unwrap();
        conn.execute_batch(
            "DROP TABLE baselines; DROP TABLE ev_actuals; DROP TABLE ev_snapshots;
             PRAGMA user_version = 3;",
        )
        .unwrap();
        db
    }

    #[test]
    fn migrating_v3_to_v4_keeps_plans() {
        let db = v3_database("v3-keeps");
        let store = SqlitePlanStore::open(&db.path).unwrap();
        let status = store
            .read_plan(&PlanId("plan_v3".into()), |s| s.statuses.get("d1").cloned())
            .unwrap();
        assert_eq!(status, Some(DeliverableStatus::Ready));
    }

    #[test]
    fn migrating_v3_to_v4_creates_the_earned_value_tables() {
        let db = v3_database("v3-tables");
        drop(SqlitePlanStore::open(&db.path).unwrap());
        let conn = Connection::open(&db.path).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'
                 AND name IN ('baselines', 'ev_actuals', 'ev_snapshots')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 3);
    }

    /// Migration safety: a database created BEFORE the circuit-breaker
    /// (no `attempt_count` column) must open without error, keep its
    /// existing rows (defaulting attempt_count to 0), accept writes to
    /// the migrated column, and tolerate a second open (the guarded
    /// ALTER must be a no-op, not a duplicate-column error).
    #[test]
    fn opening_a_pre_attempt_count_database_migrates_in_place() {
        let db = TempDbFile::new("migration");

        // Hand-build the legacy schema + one live plan row, exactly as a
        // pre-circuit-breaker binary would have left it.
        {
            let conn = Connection::open(&db.path).unwrap();
            conn.execute_batch(
                "CREATE TABLE plans (
                     plan_id       TEXT PRIMARY KEY,
                     graph         TEXT NOT NULL,
                     cached_result TEXT NOT NULL,
                     created_at_us INTEGER NOT NULL
                 );
                 CREATE TABLE deliverable_statuses (
                     plan_id        TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE,
                     deliverable_id TEXT NOT NULL,
                     status         TEXT NOT NULL,
                     PRIMARY KEY (plan_id, deliverable_id)
                 );
                 CREATE TABLE locks (
                     plan_id        TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE,
                     deliverable_id TEXT NOT NULL,
                     caller_id      TEXT NOT NULL,
                     acquired_at_us INTEGER NOT NULL,
                     expires_at_us  INTEGER NOT NULL,
                     PRIMARY KEY (plan_id, deliverable_id)
                 );
                 CREATE TABLE submit_dedup (
                     graph_hash TEXT PRIMARY KEY,
                     plan_id    TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE
                 );",
            )
            .unwrap();

            let legacy = plan_state_with_one_ready();
            conn.execute(
                "INSERT INTO plans (plan_id, graph, cached_result, created_at_us)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    "plan_legacy",
                    serde_json::to_string(&legacy.graph).unwrap(),
                    serde_json::to_string(&legacy.cached_result).unwrap(),
                    0i64
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO deliverable_statuses (plan_id, deliverable_id, status)
                 VALUES (?1, ?2, ?3)",
                params![
                    "plan_legacy",
                    "d1",
                    serde_json::to_string(&DeliverableStatus::Ready).unwrap()
                ],
            )
            .unwrap();
        }

        let plan_id = PlanId("plan_legacy".into());

        // Open migrates in place: legacy row readable, attempt_count = 0.
        let store = SqlitePlanStore::open(&db.path).unwrap();
        store
            .read_plan(&plan_id, |state| {
                assert_eq!(state.statuses.get("d1"), Some(&DeliverableStatus::Ready));
                assert_eq!(state.attempt_count("d1"), 0);
            })
            .unwrap();

        // The migrated column accepts and persists writes.
        store
            .mutate_plan(&plan_id, |state| {
                state.attempt_counts.insert("d1".to_string(), 2);
                Ok(())
            })
            .unwrap();
        drop(store);

        // Reopen: the guarded ALTER is a no-op, data intact.
        let store2 = SqlitePlanStore::open(&db.path).unwrap();
        store2
            .read_plan(&plan_id, |state| {
                assert_eq!(state.attempt_count("d1"), 2);
            })
            .unwrap();
    }

    /// Migration safety for the LIVE production schema: a database from
    /// the lease-counting era (`attempt_count` present, no
    /// `failure_count`/`lapse_count`) must open without error, and a
    /// deliverable with acquire-derived attempt history — e.g.
    /// attempt_count=2 accrued purely from environmental lapses — must be
    /// exactly as far from BOTH breakers as a fresh one: legacy attempts
    /// are telemetry, treated as neither failures nor lapses. A second
    /// open must be a no-op (guarded ALTER, no duplicate-column error).
    #[test]
    fn opening_a_pre_lapse_count_database_treats_legacy_attempts_as_neither_failures_nor_lapses() {
        let db = TempDbFile::new("lapse-migration");

        // Hand-build the lease-counting-era schema + one in-flight plan
        // row, exactly as the pre-fix binary would have left it after two
        // environmental lease losses.
        {
            let conn = Connection::open(&db.path).unwrap();
            conn.execute_batch(
                "CREATE TABLE plans (
                     plan_id       TEXT PRIMARY KEY,
                     graph         TEXT NOT NULL,
                     cached_result TEXT NOT NULL,
                     created_at_us INTEGER NOT NULL
                 );
                 CREATE TABLE deliverable_statuses (
                     plan_id        TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE,
                     deliverable_id TEXT NOT NULL,
                     status         TEXT NOT NULL,
                     attempt_count  INTEGER NOT NULL DEFAULT 0,
                     PRIMARY KEY (plan_id, deliverable_id)
                 );
                 CREATE TABLE locks (
                     plan_id        TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE,
                     deliverable_id TEXT NOT NULL,
                     caller_id      TEXT NOT NULL,
                     acquired_at_us INTEGER NOT NULL,
                     expires_at_us  INTEGER NOT NULL,
                     PRIMARY KEY (plan_id, deliverable_id)
                 );
                 CREATE TABLE submit_dedup (
                     graph_hash TEXT PRIMARY KEY,
                     plan_id    TEXT NOT NULL REFERENCES plans(plan_id) ON DELETE CASCADE
                 );",
            )
            .unwrap();

            let legacy = plan_state_with_one_ready();
            conn.execute(
                "INSERT INTO plans (plan_id, graph, cached_result, created_at_us)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    "plan_live",
                    serde_json::to_string(&legacy.graph).unwrap(),
                    serde_json::to_string(&legacy.cached_result).unwrap(),
                    0i64
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO deliverable_statuses (plan_id, deliverable_id, status, attempt_count)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    "plan_live",
                    "d1",
                    serde_json::to_string(&DeliverableStatus::Ready).unwrap(),
                    2u32
                ],
            )
            .unwrap();
        }

        let plan_id = PlanId("plan_live".into());

        // Open migrates in place: legacy lease history preserved as
        // telemetry, both breaker counters start at zero.
        let store = SqlitePlanStore::open(&db.path).unwrap();
        store
            .read_plan(&plan_id, |state| {
                assert_eq!(state.statuses.get("d1"), Some(&DeliverableStatus::Ready));
                assert_eq!(state.attempt_count("d1"), 2, "legacy history preserved");
                assert_eq!(
                    state.failure_count("d1"),
                    0,
                    "legacy leases are not failures"
                );
                assert_eq!(state.lapse_count("d1"), 0, "legacy leases are not lapses");
            })
            .unwrap();

        // The migrated columns accept and persist writes.
        store
            .mutate_plan(&plan_id, |state| {
                state.failure_counts.insert("d1".to_string(), 1);
                state.lapse_counts.insert("d1".to_string(), 3);
                Ok(())
            })
            .unwrap();
        drop(store);

        // Reopen: the guarded ALTER is a no-op, data intact.
        let store2 = SqlitePlanStore::open(&db.path).unwrap();
        store2
            .read_plan(&plan_id, |state| {
                assert_eq!(state.attempt_count("d1"), 2);
                assert_eq!(state.failure_count("d1"), 1);
                assert_eq!(state.lapse_count("d1"), 3);
            })
            .unwrap();
    }
}
