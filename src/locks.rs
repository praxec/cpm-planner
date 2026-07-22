//! Lock store for [`BasicCpmPlanner`].
//!
//! Each plan owns one [`PlanState`]: the submitted graph, per-deliverable
//! status, an `id -> LockInfo` map, an inverse `file -> deliverable_id`
//! index for O(file_count) overlap checks, and a cached
//! [`CriticalPathResult`] computed at submit time.
//!
//! The whole `BasicCpmPlanner` keeps a single
//! `tokio::sync::Mutex<HashMap<PlanId, PlanState>>`; holding that mutex
//! for the entirety of an `acquire_cohort` body is the atomicity story
//! — no other concurrent acquirer can see a half-applied lock map.
//!
//! Audit emission deliberately does NOT happen inside the locked region.
//! Lifecycle methods drain pending events into a `Vec<AuditEvent>` while
//! holding the mutex, then flush after dropping it.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::plan::{DeliverableStatus, LockInfo, PlanGraph};
use chrono::{DateTime, Utc};

use crate::task::CriticalPathResult;

/// Per-plan state. One entry per submitted plan.
pub(crate) struct PlanState {
    /// The original graph as submitted. Used for status snapshots, lookup
    /// of `owned_files`, and prerequisite walks.
    pub(crate) graph: PlanGraph,

    /// Per-deliverable lifecycle status keyed by deliverable id.
    pub(crate) statuses: HashMap<String, DeliverableStatus>,

    /// How many times each deliverable has been LEASED to a driver via
    /// `acquire_cohort`, keyed by deliverable id. A missing entry means
    /// zero. Incremented at lease time only — a candidate skipped for a
    /// file conflict does not count. Never reset by lock expiry or
    /// force-release. This is TELEMETRY (total leases handed out); the
    /// failure circuit-breaker consults `failure_counts`, NOT this map,
    /// because a lease lost to the environment (TTL lapse) is not an
    /// implementation attempt.
    pub(crate) attempt_counts: HashMap<String, u32>,

    /// How many times each deliverable was EXPLICITLY marked failed via
    /// `mark_status` (status = `Failed`), keyed by deliverable id. A
    /// missing entry means zero. These are the real implementation
    /// attempts the failure circuit-breaker counts: a driver got the
    /// lease, did the work, and reported failure. Never reset — the
    /// counter is what lets `acquire_cohort` circuit-break a poison
    /// deliverable instead of re-leasing it forever.
    pub(crate) failure_counts: HashMap<String, u32>,

    /// How many times each deliverable's lease LAPSED via TTL with no
    /// terminal mark (driver killed externally, harness timeout, session
    /// restart), keyed by deliverable id. A missing entry means zero.
    /// Environmental losses, not implementation failures — they never
    /// trip the failure circuit-breaker. A separate generous bound
    /// ([`crate::planner::MAX_LAPSES`]) turns an infinitely-crashing
    /// environment into a loud `LAPSE_LIMIT` error instead of an
    /// unbounded re-lease loop.
    pub(crate) lapse_counts: HashMap<String, u32>,

    /// Currently held locks keyed by deliverable id. A deliverable is
    /// `InProgress` iff this map contains it.
    pub(crate) locks: HashMap<String, LockInfo>,

    /// Inverse index: which deliverable currently owns each locked file.
    /// Maintained in lock-step with `locks` so overlap checks during
    /// `acquire_cohort` are O(candidate.owned_files.len()).
    pub(crate) file_to_deliverable: HashMap<PathBuf, String>,

    /// CPM result computed at submit time. The critical-path ordering
    /// drives priority in `acquire_cohort`; the duration is surfaced via
    /// `status`.
    pub(crate) cached_result: CriticalPathResult,
}

impl PlanState {
    pub(crate) fn new(
        graph: PlanGraph,
        statuses: HashMap<String, DeliverableStatus>,
        cached_result: CriticalPathResult,
    ) -> Self {
        Self {
            graph,
            statuses,
            attempt_counts: HashMap::new(),
            failure_counts: HashMap::new(),
            lapse_counts: HashMap::new(),
            locks: HashMap::new(),
            file_to_deliverable: HashMap::new(),
            cached_result,
        }
    }

    /// Lease count for `deliverable_id`. Missing entry = never leased.
    pub(crate) fn attempt_count(&self, deliverable_id: &str) -> u32 {
        self.attempt_counts
            .get(deliverable_id)
            .copied()
            .unwrap_or(0)
    }

    /// Explicit-failure count for `deliverable_id` (marked `Failed` via
    /// `mark_status`). Missing entry = never failed.
    pub(crate) fn failure_count(&self, deliverable_id: &str) -> u32 {
        self.failure_counts
            .get(deliverable_id)
            .copied()
            .unwrap_or(0)
    }

    /// Environmental lapse count for `deliverable_id` (lease expired via
    /// TTL with no terminal mark). Missing entry = never lapsed.
    pub(crate) fn lapse_count(&self, deliverable_id: &str) -> u32 {
        self.lapse_counts.get(deliverable_id).copied().unwrap_or(0)
    }

    /// Drop every lock whose `expires_at` is strictly before `now`. Returns
    /// the reaped lock infos so the caller can emit
    /// `plan.lock.expired` audit events outside the mutex.
    ///
    /// Reverting status: an expired deliverable goes back to `Ready`.
    /// (Prereqs were Complete when it was acquired and remain Complete
    /// now — TTL expiry does not unwind upstream work.)
    ///
    /// Every reaped lock is an ENVIRONMENTAL lapse — the driver never
    /// reported a terminal status — so `lapse_counts` is incremented here
    /// (NOT `failure_counts`: a killed driver is not an implementation
    /// failure). The lapse bound in `acquire_cohort` is what keeps an
    /// infinitely-crashing environment from re-leasing forever.
    pub(crate) fn reap_expired(&mut self, now: DateTime<Utc>) -> Vec<LockInfo> {
        let expired_ids: Vec<String> = self
            .locks
            .iter()
            .filter(|(_, info)| info.expires_at < now)
            .map(|(id, _)| id.clone())
            .collect();

        let mut reaped = Vec::with_capacity(expired_ids.len());
        for id in expired_ids {
            if let Some(info) = self.locks.remove(&id) {
                // Drop this deliverable's owned_files from the inverse index.
                if let Some(deliverable) = self.graph.deliverables.iter().find(|d| d.id == id) {
                    for f in &deliverable.owned_files {
                        self.file_to_deliverable.remove(f);
                    }
                }
                *self.lapse_counts.entry(id.clone()).or_insert(0) += 1;
                self.statuses.insert(id, DeliverableStatus::Ready);
                reaped.push(info);
            }
        }
        reaped
    }
}
