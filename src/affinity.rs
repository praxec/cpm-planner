//! Thread affinity: keep a continuity thread with one caller when leasing.
//!
//! A deliverable joins a thread with `metadata.thread`, a short token (see
//! [`thread_token_error`]). The contract is `docs/threads.md`. In short:
//!
//! - when a caller completes a threaded deliverable through
//!   `plan.mark_status`, the planner records a **handoff**: that caller
//!   last advanced the thread, at that time (`thread_handoffs`, schema v7);
//! - `plan.acquire_cohort` then ranks each ready deliverable for the
//!   requesting caller: first the deliverables it is affine to (its explicit
//!   `thread`, or, without one, the threads it last advanced), then
//!   unthreaded or unclaimed ones, then those affine to another caller. The
//!   rank only reorders; it never admits anything the acquire would not;
//! - a requester's `affinity_seconds` is how long it defers to another
//!   caller: a ready deliverable whose thread another caller advanced less
//!   than that many seconds ago is held back (`AFFINITY_HOLD`). The default
//!   is 0, no hold; it is clamped to the planner's maximum lease TTL.
//!
//! Affinity is a preference, never a lock. File conflicts, prerequisites,
//! manual and gate exclusion, the lapse limit and the circuit breaker apply
//! exactly as without it, and a plan without `metadata.thread` leases
//! exactly as before.
//!
//! A handoff is honoured only while it still describes the current graph:
//! its deliverable exists, is `Complete` and still carries the same
//! thread. A revision that removes or re-threads it makes the row inert.
//! Failures, force releases, lapses and `plan.accept` are not completions by
//! an executor and record nothing.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::locks::PlanState;
use crate::plan::{Deliverable, DeliverableStatus, PlanGraph, PlanId, PlannerError};
use crate::plan_store::backend;

/// `metadata` key naming a deliverable's continuity thread.
pub const THREAD_KEY: &str = "thread";

/// Longest thread token, in characters.
pub const MAX_THREAD_CHARS: usize = 64;

/// `affinity_seconds` when a request gives none: no hold.
pub const DEFAULT_AFFINITY_SECONDS: u64 = 0;

/// Why `token` is not a valid thread token, or `None` when it is: 1 to
/// [`MAX_THREAD_CHARS`] ASCII letters, digits, `.`, `_` or `-`, starting
/// with a letter or digit. The one rule for `metadata.thread` values, the
/// `acquire_cohort` `thread` argument and the lint.
pub fn thread_token_problem(token: &str) -> Option<String> {
    let len = token.chars().count();
    if len == 0 || len > MAX_THREAD_CHARS {
        return Some(format!(
            "a thread token is 1 to {MAX_THREAD_CHARS} characters, got {len}"
        ));
    }
    if !token.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Some("a thread token starts with an ASCII letter or digit".to_string());
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Some(
            "a thread token holds only ASCII letters, digits, '.', '_' and '-'".to_string(),
        );
    }
    None
}

/// Why a `metadata.thread` value is invalid, or `None` when it is a valid
/// token. A non-string value is invalid. For the lint: a deliverable with an
/// invalid value is treated as unthreaded by the planner.
pub fn thread_token_error(value: &serde_json::Value) -> Option<String> {
    match value.as_str() {
        Some(token) => thread_token_problem(token),
        None => Some(format!(
            "metadata.thread must be a string token, got {value}"
        )),
    }
}

/// The thread of `d`: its `metadata.thread` when that is a valid token.
pub fn thread_of(d: &Deliverable) -> Option<&str> {
    d.metadata
        .get(THREAD_KEY)?
        .as_str()
        .filter(|t| thread_token_problem(t).is_none())
}

/// True when any deliverable of `graph` carries a valid thread.
pub fn has_threads(graph: &PlanGraph) -> bool {
    graph.deliverables.iter().any(|d| thread_of(d).is_some())
}

// ---------------------------------------------------------------------------
// Handoffs (schema v7 `thread_handoffs`)
// ---------------------------------------------------------------------------

/// The caller that last completed a deliverable of a thread, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub thread: String,
    pub caller_id: String,
    pub deliverable_id: String,
    pub completed_at: DateTime<Utc>,
}

/// Every stored handoff of `plan_id`, keyed by thread, unvalidated.
pub(crate) fn load_handoffs(
    conn: &Connection,
    plan_id: &PlanId,
) -> Result<HashMap<String, Handoff>, PlannerError> {
    let mut stmt = conn
        .prepare(
            "SELECT thread, caller_id, deliverable_id, completed_at_us
             FROM thread_handoffs WHERE plan_id = ?1",
        )
        .map_err(backend)?;
    let rows = stmt
        .query_map(params![plan_id.0], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .map_err(backend)?;
    let mut out = HashMap::new();
    for row in rows {
        let (thread, caller_id, deliverable_id, at_us) = row.map_err(backend)?;
        let completed_at = DateTime::from_timestamp_micros(at_us).ok_or_else(|| {
            backend(anyhow::anyhow!(
                "corrupt timestamp in column thread_handoffs.completed_at_us: {at_us}"
            ))
        })?;
        out.insert(
            thread.clone(),
            Handoff {
                thread,
                caller_id,
                deliverable_id,
                completed_at,
            },
        );
    }
    Ok(out)
}

/// Keep only the handoffs that still describe `state`: the deliverable
/// exists, is `Complete`, and carries the same thread.
pub(crate) fn current_handoffs(
    state: &PlanState,
    handoffs: HashMap<String, Handoff>,
) -> HashMap<String, Handoff> {
    handoffs
        .into_iter()
        .filter(|(thread, h)| {
            state
                .graph
                .deliverables
                .iter()
                .find(|d| d.id == h.deliverable_id)
                .is_some_and(|d| thread_of(d) == Some(thread.as_str()))
                && matches!(
                    state.statuses.get(&h.deliverable_id),
                    Some(DeliverableStatus::Complete)
                )
        })
        .collect()
}

/// Record that `caller_id` completed `deliverable_id` of `thread` at `at`,
/// replacing the thread's previous handoff.
pub(crate) fn record_handoff(
    conn: &Connection,
    plan_id: &PlanId,
    thread: &str,
    caller_id: &str,
    deliverable_id: &str,
    at: DateTime<Utc>,
) -> Result<(), PlannerError> {
    conn.execute(
        "INSERT INTO thread_handoffs
             (plan_id, thread, caller_id, deliverable_id, completed_at_us)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (plan_id, thread) DO UPDATE
             SET caller_id = excluded.caller_id,
                 deliverable_id = excluded.deliverable_id,
                 completed_at_us = excluded.completed_at_us",
        params![
            plan_id.0,
            thread,
            caller_id,
            deliverable_id,
            at.timestamp_micros()
        ],
    )
    .map_err(backend)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Acquire ranking
// ---------------------------------------------------------------------------

/// How a ready deliverable ranks for one requester.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Rank {
    /// The requester's explicit thread, or a thread it last advanced.
    Affine,
    /// Unthreaded, or a thread nobody is affine to.
    Neutral,
    /// A thread another caller last advanced.
    Others,
}

/// The affinity view of one acquire: who is affine to which thread, the
/// requester, its explicit thread and its deference window.
pub(crate) struct AffinityView {
    handoffs: HashMap<String, Handoff>,
    caller: String,
    thread: Option<String>,
    window: chrono::Duration,
    now: DateTime<Utc>,
}

/// A ready deliverable held back for another caller.
pub(crate) struct Hold {
    pub(crate) caller: String,
    pub(crate) remaining_seconds: i64,
}

impl AffinityView {
    pub(crate) fn new(
        handoffs: HashMap<String, Handoff>,
        caller: &str,
        thread: Option<&str>,
        window: std::time::Duration,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            handoffs,
            caller: caller.to_string(),
            thread: thread.map(str::to_string),
            window: chrono::Duration::from_std(window).unwrap_or(chrono::Duration::MAX),
            now,
        }
    }

    /// The rank of `d` for the requester.
    pub(crate) fn rank(&self, d: &Deliverable) -> Rank {
        let Some(thread) = thread_of(d) else {
            return Rank::Neutral;
        };
        if self.thread.as_deref() == Some(thread) {
            return Rank::Affine;
        }
        match self.handoffs.get(thread) {
            Some(h) if h.caller_id == self.caller && self.thread.is_none() => Rank::Affine,
            Some(h) if h.caller_id != self.caller => Rank::Others,
            _ => Rank::Neutral,
        }
    }

    /// `Some` when `d` is held back for another caller: its thread was
    /// advanced by someone else less than the window ago.
    pub(crate) fn hold(&self, d: &Deliverable) -> Option<Hold> {
        let h = self.handoffs.get(thread_of(d)?)?;
        if h.caller_id == self.caller {
            return None;
        }
        let until = h.completed_at.checked_add_signed(self.window)?;
        (self.now < until).then(|| Hold {
            caller: h.caller_id.clone(),
            remaining_seconds: (until - self.now).num_seconds().max(1),
        })
    }
}

/// Validate an acquire's `thread` argument (`INVALID_VALUE`).
pub(crate) fn validate_thread_argument(thread: Option<&str>) -> Result<(), PlannerError> {
    match thread.and_then(thread_token_problem) {
        Some(problem) => Err(PlannerError::InvalidValue {
            reason: format!("thread: {problem}"),
        }),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// One thread in `plan.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadStatus {
    pub thread: String,
    /// The caller holding a live lease on one of the thread's deliverables
    /// (the smallest caller id when several do), else `None`.
    pub holder: Option<String>,
    /// The caller that last completed one of its deliverables, if that
    /// handoff still describes the graph.
    pub affine_caller: Option<String>,
    /// The thread's first ready, unleased deliverable in graph order.
    pub next_ready: Option<String>,
    pub completed: usize,
    pub total: usize,
}

/// The threads of `state`, sorted by thread token. Empty for an unthreaded
/// plan. `handoffs` should already be [`current_handoffs`].
pub(crate) fn thread_statuses(
    state: &PlanState,
    handoffs: &HashMap<String, Handoff>,
) -> Vec<ThreadStatus> {
    let mut by_thread: BTreeMap<&str, ThreadStatus> = BTreeMap::new();
    for d in &state.graph.deliverables {
        let Some(thread) = thread_of(d) else {
            continue;
        };
        let entry = by_thread.entry(thread).or_insert_with(|| ThreadStatus {
            thread: thread.to_string(),
            holder: None,
            affine_caller: handoffs.get(thread).map(|h| h.caller_id.clone()),
            next_ready: None,
            completed: 0,
            total: 0,
        });
        entry.total += 1;
        let status = state.statuses.get(&d.id);
        if matches!(status, Some(DeliverableStatus::Complete)) {
            entry.completed += 1;
        }
        if let Some(lock) = state.locks.get(&d.id) {
            let caller = &lock.caller_id.0;
            if entry.holder.as_ref().is_none_or(|h| caller < h) {
                entry.holder = Some(caller.clone());
            }
        } else if entry.next_ready.is_none() && matches!(status, Some(DeliverableStatus::Ready)) {
            entry.next_ready = Some(d.id.clone());
        }
    }
    by_thread.into_values().collect()
}
