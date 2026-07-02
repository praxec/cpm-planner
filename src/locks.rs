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
            locks: HashMap::new(),
            file_to_deliverable: HashMap::new(),
            cached_result,
        }
    }

    /// Drop every lock whose `expires_at` is strictly before `now`. Returns
    /// the reaped lock infos so the caller can emit
    /// `plan.lock.expired` audit events outside the mutex.
    ///
    /// Reverting status: an expired deliverable goes back to `Ready`.
    /// (Prereqs were Complete when it was acquired and remain Complete
    /// now — TTL expiry does not unwind upstream work.)
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
                self.statuses.insert(id, DeliverableStatus::Ready);
                reaped.push(info);
            }
        }
        reaped
    }
}
