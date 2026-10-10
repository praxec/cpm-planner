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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use crate::plan::{DeliverableStatus, FileMode, LockInfo, OwnedFile, PlanGraph};
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
    /// ([`crate::planner::MAX_LAPSES`]) stops an infinitely-crashing
    /// environment from causing an unbounded re-lease loop: acquire skips
    /// the deliverable and reports it in `blocked` with code `LAPSE_LIMIT`.
    pub(crate) lapse_counts: HashMap<String, u32>,

    /// Currently held locks keyed by deliverable id. A leased deliverable
    /// is `InProgress` while this map contains it; an `InProgress`
    /// deliverable without an entry is either in [`Self::lockless`] or an
    /// orphan of a lost lease.
    pub(crate) locks: HashMap<String, LockInfo>,

    /// Deliverables marked `InProgress` by a LOCKLESS `mark_status` (owner
    /// or manual work), persisted as `deliverable_statuses.lockless`. Lease
    /// provenance for the startup sweep: an `InProgress` deliverable with
    /// no lock row is an orphan (quarantined) unless it is here. Only
    /// meaningful while the deliverable is `InProgress` and unlocked
    /// ([`Self::is_lockless_in_progress`]); the store persists that
    /// conjunction, so any status change or lease takeover clears it.
    pub(crate) lockless: HashSet<String>,

    /// Inverse index: who currently claims each locked file (one exclusive
    /// holder, or a set of append holders). Maintained in lock-step with
    /// `locks` so overlap checks during `acquire_cohort` are
    /// O(candidate.owned_files.len()).
    pub(crate) file_claims: HashMap<PathBuf, FileClaim>,

    /// CPM result computed at submit time. The critical-path ordering
    /// drives priority in `acquire_cohort`; the duration is surfaced via
    /// `status`.
    pub(crate) cached_result: CriticalPathResult,

    /// Lease hours ended during this transaction, keyed by deliverable id:
    /// a DELTA (empty when loaded), added to `ev_actuals.leased_hours` and
    /// consumed by [`crate::plan_store::save_plan_state`]. Written through
    /// [`PlanState::record_lease_end`]; a plan revision carries it over.
    pub(crate) leased_hours: HashMap<String, f32>,

    /// Progress reported by `mark_status` during this transaction, keyed by
    /// deliverable id; applied to `ev_actuals` by
    /// [`crate::plan_store::save_plan_state`]. Written through
    /// [`PlanState::report_actuals`].
    pub(crate) reported_actuals: HashMap<String, crate::ev_store::ReportedActuals>,

    /// Latest time recorded in `leased_hours` or `reported_actuals`; stored
    /// as the actuals rows' `updated_at_us`.
    pub(crate) actuals_updated_at: Option<DateTime<Utc>>,
}

/// Earned-value deltas taken from a [`PlanState`] by
/// [`PlanState::take_actuals_deltas`].
pub(crate) struct ActualsDeltas {
    pub(crate) leased_hours: HashMap<String, f32>,
    pub(crate) reported: HashMap<String, crate::ev_store::ReportedActuals>,
    /// `None` exactly when both maps are empty.
    pub(crate) at: Option<DateTime<Utc>>,
}

/// Who holds a locked path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileClaim {
    Exclusive(String),
    Append(BTreeSet<String>),
}

/// Two claims on one path conflict unless both are append.
pub(crate) fn modes_conflict(requested: FileMode, held: FileMode) -> bool {
    !(requested == FileMode::Append && held == FileMode::Append)
}

impl FileClaim {
    /// Whether a request for this path in `requested` mode conflicts with
    /// this held claim.
    pub(crate) fn conflicts_with(&self, requested: FileMode) -> bool {
        let held = match self {
            FileClaim::Exclusive(_) => FileMode::Exclusive,
            FileClaim::Append(_) => FileMode::Append,
        };
        modes_conflict(requested, held)
    }
}

/// Record `deliverable_id`'s claims in the index.
pub(crate) fn add_file_claims(
    claims: &mut HashMap<PathBuf, FileClaim>,
    deliverable_id: &str,
    files: &[OwnedFile],
) {
    for f in files {
        match f.mode() {
            FileMode::Exclusive => {
                debug_assert!(
                    !matches!(claims.get(f.path()), Some(FileClaim::Append(_))),
                    "exclusive claim by {deliverable_id} on a path held by append claims"
                );
                claims.insert(
                    f.path().to_path_buf(),
                    FileClaim::Exclusive(deliverable_id.to_string()),
                );
            }
            FileMode::Append => match claims.entry(f.path().to_path_buf()) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    debug_assert!(
                        matches!(e.get(), FileClaim::Append(_)),
                        "append claim by {deliverable_id} on a path held exclusively"
                    );
                    if let FileClaim::Append(set) = e.get_mut() {
                        set.insert(deliverable_id.to_string());
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(FileClaim::Append(BTreeSet::from([
                        deliverable_id.to_string()
                    ])));
                }
            },
        }
    }
}

/// Drop only `deliverable_id`'s claims; an emptied append set removes the path.
pub(crate) fn release_file_claims(
    claims: &mut HashMap<PathBuf, FileClaim>,
    deliverable_id: &str,
    files: &[OwnedFile],
) {
    for f in files {
        let drop_path = match claims.get_mut(f.path()) {
            Some(FileClaim::Exclusive(owner)) => owner == deliverable_id,
            Some(FileClaim::Append(set)) => {
                set.remove(deliverable_id);
                set.is_empty()
            }
            None => false,
        };
        if drop_path {
            claims.remove(f.path());
        }
    }
}

/// Hours from `start` to `end`; 0 when `end` is earlier.
pub(crate) fn hours_between(start: DateTime<Utc>, end: DateTime<Utc>) -> f32 {
    let micros = (end - start).num_microseconds().unwrap_or(i64::MAX);
    (micros.max(0) as f64 / 3_600_000_000.0) as f32
}

/// The single re-derivation rule: `Ready` if every prerequisite of `d` is
/// `Complete` in `statuses`, else `Pending`. Used by the in-memory reaper, the
/// startup reaper in the store, and plan revision.
pub(crate) fn rederive_status(
    d: &crate::plan::Deliverable,
    statuses: &HashMap<String, DeliverableStatus>,
) -> DeliverableStatus {
    if crate::graph::prerequisite_ids(d)
        .all(|p| statuses.get(p) == Some(&DeliverableStatus::Complete))
    {
        DeliverableStatus::Ready
    } else {
        DeliverableStatus::Pending
    }
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
            lockless: HashSet::new(),
            file_claims: HashMap::new(),
            cached_result,
            leased_hours: HashMap::new(),
            reported_actuals: HashMap::new(),
            actuals_updated_at: None,
        }
    }

    /// Take (and clear) the earned-value deltas of this transaction: lease
    /// hours ended, progress reported, and the time to stamp them with.
    /// Consuming them is what keeps a second save from counting them twice.
    pub(crate) fn take_actuals_deltas(&mut self) -> ActualsDeltas {
        ActualsDeltas {
            leased_hours: std::mem::take(&mut self.leased_hours),
            reported: std::mem::take(&mut self.reported_actuals),
            at: self.actuals_updated_at.take(),
        }
    }

    fn touch_actuals(&mut self, at: DateTime<Utc>) {
        self.actuals_updated_at = Some(self.actuals_updated_at.map_or(at, |t| t.max(at)));
    }

    /// Queue a `mark_status` progress report for `deliverable_id` at `at`.
    /// An empty report is dropped.
    pub(crate) fn report_actuals(
        &mut self,
        deliverable_id: &str,
        report: crate::ev_store::ReportedActuals,
        at: DateTime<Utc>,
    ) {
        if report == crate::ev_store::ReportedActuals::default() {
            return;
        }
        self.reported_actuals
            .insert(deliverable_id.to_string(), report);
        self.touch_actuals(at);
    }

    /// Credit `lock`'s holder with the hours from acquisition to `end`
    /// (never negative) as leased time for earned value.
    pub(crate) fn record_lease_end(&mut self, lock: &LockInfo, end: DateTime<Utc>) {
        *self
            .leased_hours
            .entry(lock.deliverable_id.clone())
            .or_insert(0.0) += hours_between(lock.acquired_at, end);
        self.touch_actuals(end);
    }

    /// End `deliverable_id`'s lease at `end`: drop the lock and its file
    /// claims and record the leased hours. The status is left to the caller.
    /// Returns the ended lock, or `None` when none was held.
    pub(crate) fn end_lease(
        &mut self,
        deliverable_id: &str,
        end: DateTime<Utc>,
    ) -> Option<LockInfo> {
        let lock = self.locks.remove(deliverable_id)?;
        if let Some(d) = self
            .graph
            .deliverables
            .iter()
            .find(|d| d.id == deliverable_id)
        {
            release_file_claims(&mut self.file_claims, deliverable_id, &d.owned_files);
        }
        self.record_lease_end(&lock, end);
        Some(lock)
    }

    /// Whether `deliverable_id` is `InProgress` from a lockless mark: in
    /// [`Self::lockless`], `InProgress`, and holding no lock. This is the
    /// value persisted as `deliverable_statuses.lockless`.
    pub(crate) fn is_lockless_in_progress(&self, deliverable_id: &str) -> bool {
        self.lockless.contains(deliverable_id)
            && self.statuses.get(deliverable_id) == Some(&DeliverableStatus::InProgress)
            && !self.locks.contains_key(deliverable_id)
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
    ///
    /// Each reaped lease is credited with its hours up to `expires_at`.
    pub(crate) fn reap_expired(&mut self, now: DateTime<Utc>) -> Vec<LockInfo> {
        let expired_ids: Vec<String> = self
            .locks
            .iter()
            .filter(|(_, info)| info.expires_at < now)
            .map(|(id, _)| id.clone())
            .collect();

        let mut reaped = Vec::with_capacity(expired_ids.len());
        for id in expired_ids {
            // The lease ended when it lapsed, not when it is noticed.
            let end = self.locks.get(&id).map_or(now, |l| l.expires_at.min(now));
            if let Some(info) = self.end_lease(&id, end) {
                *self.lapse_counts.entry(id.clone()).or_insert(0) += 1;
                // Re-derive rather than assume Ready: after a revision the
                // lease can sit above a prerequisite that is no longer Complete.
                let status = self
                    .graph
                    .deliverables
                    .iter()
                    .find(|d| d.id == id)
                    .map_or(DeliverableStatus::Ready, |d| {
                        rederive_status(d, &self.statuses)
                    });
                self.statuses.insert(id, status);
                reaped.push(info);
            }
        }
        reaped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releasing_one_append_holder_keeps_the_other_claim() {
        let files = vec![OwnedFile::Claim {
            path: PathBuf::from("R.md"),
            mode: Some(FileMode::Append),
        }];
        let mut claims = HashMap::new();
        add_file_claims(&mut claims, "a", &files);
        add_file_claims(&mut claims, "b", &files);
        release_file_claims(&mut claims, "a", &files);
        assert_eq!(
            claims.get(&PathBuf::from("R.md")),
            Some(&FileClaim::Append(BTreeSet::from(["b".to_string()])))
        );
    }

    #[test]
    fn file_claim_conflicts_unless_both_sides_append() {
        let ex = FileClaim::Exclusive("a".into());
        let ap = FileClaim::Append(BTreeSet::from(["a".to_string()]));
        assert!(ex.conflicts_with(FileMode::Exclusive));
        assert!(ex.conflicts_with(FileMode::Append));
        assert!(ap.conflicts_with(FileMode::Exclusive));
        assert!(!ap.conflicts_with(FileMode::Append));
    }

    #[test]
    fn reaped_lease_with_incomplete_prerequisites_becomes_pending() {
        use crate::plan::{CallerId, Deliverable, PlanId, Prerequisite};
        let dl = |id: &str, pre: &[&str]| Deliverable {
            id: id.to_string(),
            owned_files: vec![],
            prerequisites: pre
                .iter()
                .map(|p| Prerequisite::Id((*p).to_string()))
                .collect(),
            estimated_effort_hours: Some(1.0),
            duration_hours: None,
            estimate: None,
            metadata: serde_json::json!({}),
            milestone: false,
            earning_rule: None,
        };
        let graph = PlanGraph {
            deliverables: vec![dl("a", &[]), dl("b", &["a"])],
            max_chained_dispatch: None,
        };
        let cpm = crate::schedule::compute_cpm(&graph).expect("valid");
        let statuses = HashMap::from([
            ("a".to_string(), DeliverableStatus::Ready),
            ("b".to_string(), DeliverableStatus::InProgress),
        ]);
        let mut state = PlanState::new(graph, statuses, cpm);
        let now = Utc::now();
        state.locks.insert(
            "b".into(),
            LockInfo {
                plan_id: PlanId("p".into()),
                deliverable_id: "b".into(),
                caller_id: CallerId("w".into()),
                acquired_at: now,
                expires_at: now - chrono::Duration::hours(1),
            },
        );
        state.reap_expired(now);
        assert_eq!(state.statuses["b"], DeliverableStatus::Pending);
    }
}
