//! Pure plan revision: derive the runtime state of a revised graph from the
//! state of the plan it replaces, carrying progress over.
//!
//! No store and no I/O: [`plan_revision`] maps `(old state, new graph)` to
//! `(new state, diff)`. The store wiring and audit events live elsewhere.
//!
//! Carry-over rules:
//! - an unchanged deliverable (same canonical definition) keeps its status and
//!   counters;
//! - a changed definition keeps its status unless its prerequisite id set
//!   changed, in which case a `Complete`/`Ready`/`Pending` deliverable is
//!   re-derived (`Ready` if every prerequisite is `Complete`, else `Pending`)
//!   and listed in `reopened`;
//! - dependents of a reopened deliverable are re-derived the same way,
//!   transitively;
//! - a deliverable with an `interface` edge to a changed deliverable whose
//!   `metadata.contract == true` is reopened;
//! - new deliverables start `Ready`/`Pending` by their prerequisites;
//! - removed deliverables are dropped; one holding a live lock is refused with
//!   `LOCK_HELD` unless `force`, in which case its lock is released and its id
//!   listed in `released_locks`;
//! - counters (attempts, failures, lapses) are kept for every surviving
//!   deliverable, including re-derived ones;
//! - `InProgress` (leased) deliverables keep their lease unless removed.
//!   `Failed` deliverables are outside the reopen rule: they stay `Failed`
//!   (re-derivation applies only to `Complete`/`Ready`/`Pending`).

// Store wiring (a later task) is the first non-test caller of `plan_revision`.
#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::graph::prerequisite_ids;
use crate::locks::{FileClaim, PlanState, add_file_claims};
use crate::plan::{Deliverable, DeliverableStatus, PlanGraph, PlannerError, PrerequisiteKind};
use crate::planner::{canonical_deliverable, validate_graph};
use chrono::{DateTime, Utc};

/// What a revision changed. Every vector is sorted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RevisionDiff {
    /// Deliverable ids present only in the new graph.
    pub added: Vec<String>,
    /// Deliverable ids present only in the old graph.
    pub removed: Vec<String>,
    /// Surviving ids whose canonical definition differs.
    pub changed: Vec<String>,
    /// Surviving ids whose status was re-derived from their prerequisites.
    pub reopened: Vec<String>,
    /// Deliverable ids whose live lock was force-released by a removal.
    pub released_locks: Vec<String>,
}

/// Whether a deliverable in `status` is subject to re-derivation.
fn rederivable(status: &DeliverableStatus) -> bool {
    matches!(
        status,
        DeliverableStatus::Complete | DeliverableStatus::Ready | DeliverableStatus::Pending
    )
}

fn is_contract(d: &Deliverable) -> bool {
    d.metadata
        .get("contract")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn prereq_set(d: &Deliverable) -> BTreeSet<&str> {
    prerequisite_ids(d).collect()
}

/// Derive the runtime state of `new` from `old`, carrying progress over per
/// the module rules. Pure: nothing is persisted and no audit is emitted.
///
/// The new graph is validated first (`validate_graph`: duplicate ids, unknown
/// prerequisites, ...), so a bad graph is `INVALID_GRAPH`, never carried over.
/// A lock with `expires_at < now` is not live: it never blocks removal, is not
/// kept, and its deliverable is re-derived from its prerequisites.
///
/// Errors: `LOCK_HELD` when a removed deliverable holds a live lock, or two
/// surviving leases would claim conflicting files, and `force` is false; any error from the CPM recomputation on `new`.
pub(crate) fn plan_revision(
    old: &PlanState,
    new: &PlanGraph,
    force: bool,
    now: DateTime<Utc>,
) -> Result<(PlanState, RevisionDiff), PlannerError> {
    validate_graph(new)?;
    let cached_result = crate::schedule::compute_cpm(new)?;
    let live = |id: &str| old.locks.get(id).filter(|l| l.expires_at >= now);

    let old_by_id: HashMap<&str, &Deliverable> = old
        .graph
        .deliverables
        .iter()
        .map(|d| (d.id.as_str(), d))
        .collect();
    let new_by_id: HashMap<&str, &Deliverable> = new
        .deliverables
        .iter()
        .map(|d| (d.id.as_str(), d))
        .collect();

    let mut diff = RevisionDiff::default();
    for id in old_by_id.keys() {
        if !new_by_id.contains_key(id) {
            diff.removed.push((*id).to_string());
        }
    }
    for d in &new.deliverables {
        match old_by_id.get(d.id.as_str()) {
            None => diff.added.push(d.id.clone()),
            Some(o) if canonical_deliverable(o) != canonical_deliverable(d) => {
                diff.changed.push(d.id.clone());
            }
            Some(_) => {}
        }
    }
    diff.added.sort();
    diff.removed.sort();
    diff.changed.sort();

    // Removal of a leased deliverable: refuse, or release when forced.
    for id in &diff.removed {
        if let Some(lock) = live(id) {
            if !force {
                return Err(PlannerError::LockHeld {
                    plan_id: lock.plan_id.0.clone(),
                    deliverable_id: id.clone(),
                    holder: lock.caller_id.0.clone(),
                });
            }
            diff.released_locks.push(id.clone());
        }
    }

    // Changed deliverables whose contract flag is set in either revision.
    let changed_contracts: HashSet<&str> = diff
        .changed
        .iter()
        .filter(|id| is_contract(new_by_id[id.as_str()]) || is_contract(old_by_id[id.as_str()]))
        .map(String::as_str)
        .collect();

    // Seeds: survivors whose prerequisite id set changed, or that consume an
    // interface of a changed contract deliverable.
    let carried = |id: &str| old.statuses.get(id).filter(|s| rederivable(s));
    let mut rederive: BTreeSet<String> = BTreeSet::new();
    for id in &diff.changed {
        let (o, n) = (old_by_id[id.as_str()], new_by_id[id.as_str()]);
        if carried(id).is_some() && prereq_set(o) != prereq_set(n) {
            rederive.insert(id.clone());
        }
    }
    for d in &new.deliverables {
        let consumes_changed_contract = d.prerequisites.iter().any(|p| {
            p.kind() == Some(PrerequisiteKind::Interface) && changed_contracts.contains(p.id())
        });
        if consumes_changed_contract
            && old_by_id.contains_key(d.id.as_str())
            && carried(&d.id).is_some()
        {
            rederive.insert(d.id.clone());
        }
    }

    // Transitive dependents of everything reopened (leased/failed ones stop
    // the walk: their status is not part of the reopen rule).
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for d in &new.deliverables {
        for p in prereq_set(d) {
            dependents.entry(p).or_default().push(d.id.as_str());
        }
    }
    let mut queue: Vec<String> = rederive.iter().cloned().collect();
    while let Some(id) = queue.pop() {
        for dep in dependents.get(id.as_str()).into_iter().flatten() {
            if old_by_id.contains_key(dep)
                && carried(dep).is_some()
                && rederive.insert((*dep).to_string())
            {
                queue.push((*dep).to_string());
            }
        }
    }
    diff.reopened = rederive.iter().cloned().collect();
    // Survivors whose lease expired are re-derived too, but are not "reopened".
    let expired: HashSet<&str> = old
        .locks
        .keys()
        .map(String::as_str)
        .filter(|id| new_by_id.contains_key(id) && live(id).is_none())
        .collect();

    // Final statuses in prerequisite order (Kahn); re-derived and new
    // deliverables take Ready/Pending from their prerequisites.
    let mut statuses: HashMap<String, DeliverableStatus> =
        HashMap::with_capacity(new.deliverables.len());
    let mut remaining: HashMap<&str, usize> = new
        .deliverables
        .iter()
        .map(|d| (d.id.as_str(), prereq_set(d).len()))
        .collect();
    let mut ready: Vec<&str> = remaining
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect();
    while let Some(id) = ready.pop() {
        let d = new_by_id[id];
        let derive = || {
            if prereq_set(d)
                .iter()
                .all(|p| statuses.get(*p) == Some(&DeliverableStatus::Complete))
            {
                DeliverableStatus::Ready
            } else {
                DeliverableStatus::Pending
            }
        };
        let status = match old.statuses.get(id) {
            Some(s)
                if old_by_id.contains_key(id)
                    && !rederive.contains(id)
                    && !expired.contains(id) =>
            {
                s.clone()
            }
            _ => derive(),
        };
        statuses.insert(id.to_string(), status);
        for dep in dependents.get(id).into_iter().flatten() {
            if let Some(n) = remaining.get_mut(dep) {
                *n -= 1;
                if *n == 0 {
                    ready.push(dep);
                }
            }
        }
    }

    let survives = |id: &String| new_by_id.contains_key(id.as_str());
    let mut state = PlanState::new(new.clone(), statuses, cached_result);
    state.attempt_counts = old
        .attempt_counts
        .iter()
        .filter(|(k, _)| survives(k))
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    state.failure_counts = old
        .failure_counts
        .iter()
        .filter(|(k, _)| survives(k))
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    state.lapse_counts = old
        .lapse_counts
        .iter()
        .filter(|(k, _)| survives(k))
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    state.locks = old
        .locks
        .iter()
        .filter(|(k, l)| survives(k) && l.expires_at >= now)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // Rebuild claims from the NEW definitions in graph order; the earlier
    // claimant wins a conflict.
    for d in &new.deliverables {
        let Some(lock) = state.locks.get(&d.id) else {
            continue;
        };
        let clash = d.owned_files.iter().find_map(|f| {
            state
                .file_claims
                .get(f.path())
                .filter(|c| c.conflicts_with(f.mode()))
        });
        let Some(claim) = clash else {
            add_file_claims(&mut state.file_claims, &d.id, &d.owned_files);
            continue;
        };
        if !force {
            let holder_id = match claim {
                FileClaim::Exclusive(h) => h.clone(),
                FileClaim::Append(set) => set.iter().next().cloned().unwrap_or_default(),
            };
            let holder = state
                .locks
                .get(&holder_id)
                .map_or(holder_id, |l| l.caller_id.0.clone());
            return Err(PlannerError::LockHeld {
                plan_id: lock.plan_id.0.clone(),
                deliverable_id: d.id.clone(),
                holder,
            });
        }
        state.locks.remove(&d.id);
        diff.released_locks.push(d.id.clone());
        let ok = prereq_set(d)
            .iter()
            .all(|p| state.statuses.get(*p) == Some(&DeliverableStatus::Complete));
        let status = if ok {
            DeliverableStatus::Ready
        } else {
            DeliverableStatus::Pending
        };
        state.statuses.insert(d.id.clone(), status);
    }
    diff.released_locks.sort();
    Ok((state, diff))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locks::{FileClaim, add_file_claims};
    use crate::plan::{
        CallerId, Deliverable, DeliverableStatus, LockInfo, OwnedFile, PlanId, Prerequisite,
        PrerequisiteKind,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn dl(id: &str, prereqs: &[&str], effort: f32) -> Deliverable {
        Deliverable {
            id: id.to_string(),
            owned_files: vec![],
            prerequisites: prereqs
                .iter()
                .map(|p| Prerequisite::Id((*p).to_string()))
                .collect(),
            estimated_effort_hours: Some(effort),
            duration_hours: None,
            estimate: None,
            metadata: json!({}),
            milestone: false,
        }
    }

    fn iface(mut d: Deliverable, on: &str) -> Deliverable {
        d.prerequisites.push(Prerequisite::Edge {
            id: on.to_string(),
            consumes: None,
            kind: Some(PrerequisiteKind::Interface),
            lag_hours: None,
        });
        d
    }

    fn graph(ds: Vec<Deliverable>) -> PlanGraph {
        PlanGraph {
            deliverables: ds,
            max_chained_dispatch: None,
        }
    }

    fn state(g: PlanGraph, st: &[(&str, DeliverableStatus)]) -> PlanState {
        let cpm = crate::schedule::compute_cpm(&g).expect("valid graph");
        let statuses = st
            .iter()
            .map(|(i, s)| ((*i).to_string(), s.clone()))
            .collect();
        PlanState::new(g, statuses, cpm)
    }

    fn lock(id: &str) -> LockInfo {
        let now = chrono::Utc::now();
        LockInfo {
            plan_id: PlanId("p".into()),
            deliverable_id: id.to_string(),
            caller_id: CallerId("w1".into()),
            acquired_at: now,
            expires_at: now + chrono::Duration::hours(1),
        }
    }

    fn revise(old: &PlanState, g: PlanGraph) -> (PlanState, RevisionDiff) {
        plan_revision(old, &g, false, Utc::now()).expect("revision succeeds")
    }

    use DeliverableStatus::{Complete, InProgress, Pending, Ready};

    fn ab(a: Deliverable, b: Deliverable) -> PlanState {
        state(graph(vec![a, b]), &[("a", Complete), ("b", Ready)])
    }

    #[test]
    fn unchanged_deliverables_keep_status() {
        let old = ab(dl("a", &[], 1.0), dl("b", &["a"], 1.0));
        let (new, _) = revise(&old, graph(vec![dl("a", &[], 1.0), dl("b", &["a"], 1.0)]));
        assert_eq!(new.statuses["a"], Complete);
    }

    #[test]
    fn unchanged_deliverables_keep_counters() {
        let mut old = ab(dl("a", &[], 1.0), dl("b", &["a"], 1.0));
        old.attempt_counts.insert("a".into(), 3);
        let (new, _) = revise(&old, graph(vec![dl("a", &[], 1.0), dl("b", &["a"], 1.0)]));
        assert_eq!(new.attempt_counts["a"], 3);
    }

    #[test]
    fn added_deliverable_starts_pending_when_prerequisite_incomplete() {
        let old = state(graph(vec![dl("a", &[], 1.0)]), &[("a", Ready)]);
        let (new, _) = revise(&old, graph(vec![dl("a", &[], 1.0), dl("c", &["a"], 1.0)]));
        assert_eq!(new.statuses["c"], Pending);
    }

    #[test]
    fn added_deliverable_starts_ready_when_prerequisites_complete() {
        let old = state(graph(vec![dl("a", &[], 1.0)]), &[("a", Complete)]);
        let (new, _) = revise(&old, graph(vec![dl("a", &[], 1.0), dl("c", &["a"], 1.0)]));
        assert_eq!(new.statuses["c"], Ready);
    }

    #[test]
    fn added_deliverable_is_reported() {
        let old = state(graph(vec![dl("a", &[], 1.0)]), &[("a", Ready)]);
        let (_, diff) = revise(&old, graph(vec![dl("a", &[], 1.0), dl("c", &[], 1.0)]));
        assert_eq!(diff.added, vec!["c".to_string()]);
    }

    #[test]
    fn removed_deliverable_is_dropped() {
        let old = ab(dl("a", &[], 1.0), dl("b", &["a"], 1.0));
        let (new, _) = revise(&old, graph(vec![dl("a", &[], 1.0)]));
        assert!(!new.statuses.contains_key("b"));
    }

    #[test]
    fn removed_deliverable_is_reported() {
        let old = ab(dl("a", &[], 1.0), dl("b", &["a"], 1.0));
        let (_, diff) = revise(&old, graph(vec![dl("a", &[], 1.0)]));
        assert_eq!(diff.removed, vec!["b".to_string()]);
    }

    #[test]
    fn removed_deliverable_counters_are_dropped() {
        let mut old = ab(dl("a", &[], 1.0), dl("b", &["a"], 1.0));
        old.failure_counts.insert("b".into(), 2);
        let (new, _) = revise(&old, graph(vec![dl("a", &[], 1.0)]));
        assert!(!new.failure_counts.contains_key("b"));
    }

    fn leased() -> PlanState {
        let mut old = state(
            graph(vec![dl("a", &[], 1.0), dl("b", &[], 1.0)]),
            &[("a", InProgress), ("b", Ready)],
        );
        old.locks.insert("a".into(), lock("a"));
        old
    }

    #[test]
    fn removing_leased_deliverable_requires_force() {
        let err = plan_revision(
            &leased(),
            &graph(vec![dl("b", &[], 1.0)]),
            false,
            Utc::now(),
        )
        .err()
        .expect("lock held");
        assert!(matches!(err, PlannerError::LockHeld { .. }));
    }

    #[test]
    fn forced_removal_releases_lock() {
        let (_, diff) = plan_revision(&leased(), &graph(vec![dl("b", &[], 1.0)]), true, Utc::now())
            .expect("forced");
        assert_eq!(diff.released_locks, vec!["a".to_string()]);
    }

    #[test]
    fn forced_removal_drops_lock_from_state() {
        let (new, _) = plan_revision(&leased(), &graph(vec![dl("b", &[], 1.0)]), true, Utc::now())
            .expect("forced");
        assert!(new.locks.is_empty());
    }

    #[test]
    fn leased_deliverable_survives_revision() {
        let (new, _) = revise(&leased(), graph(vec![dl("a", &[], 1.0), dl("b", &[], 1.0)]));
        assert!(new.locks.contains_key("a"));
    }

    #[test]
    fn leased_deliverable_stays_in_progress() {
        let (new, _) = revise(&leased(), graph(vec![dl("a", &[], 1.0), dl("b", &[], 1.0)]));
        assert_eq!(new.statuses["a"], InProgress);
    }

    #[test]
    fn leased_deliverable_keeps_status_when_prerequisites_change() {
        let (new, _) = revise(
            &leased(),
            graph(vec![dl("a", &["b"], 1.0), dl("b", &[], 1.0)]),
        );
        assert_eq!(new.statuses["a"], InProgress);
    }

    #[test]
    fn file_claims_are_rebuilt_from_surviving_locks() {
        let mut a = dl("a", &[], 1.0);
        a.owned_files = vec![OwnedFile::Path(PathBuf::from("x.rs"))];
        let mut old = state(graph(vec![a.clone()]), &[("a", InProgress)]);
        old.locks.insert("a".into(), lock("a"));
        let mut expected: HashMap<PathBuf, FileClaim> = HashMap::new();
        add_file_claims(&mut expected, "a", &a.owned_files);
        let (new, _) = revise(&old, graph(vec![a]));
        assert_eq!(new.file_claims, expected);
    }

    #[test]
    fn changed_prerequisites_reopen_complete_deliverable() {
        let old = state(
            graph(vec![
                dl("a", &[], 1.0),
                dl("x", &[], 1.0),
                dl("b", &["a"], 1.0),
            ]),
            &[("a", Complete), ("x", Ready), ("b", Complete)],
        );
        let (new, _) = revise(
            &old,
            graph(vec![
                dl("a", &[], 1.0),
                dl("x", &[], 1.0),
                dl("b", &["a", "x"], 1.0),
            ]),
        );
        assert_eq!(new.statuses["b"], Pending);
    }

    #[test]
    fn changed_prerequisites_are_listed_as_reopened() {
        let old = state(
            graph(vec![
                dl("a", &[], 1.0),
                dl("x", &[], 1.0),
                dl("b", &["a"], 1.0),
            ]),
            &[("a", Complete), ("x", Ready), ("b", Complete)],
        );
        let (_, diff) = revise(
            &old,
            graph(vec![
                dl("a", &[], 1.0),
                dl("x", &[], 1.0),
                dl("b", &["a", "x"], 1.0),
            ]),
        );
        assert_eq!(diff.reopened, vec!["b".to_string()]);
    }

    #[test]
    fn reopened_deliverable_with_complete_prerequisites_becomes_ready() {
        let old = state(
            graph(vec![
                dl("a", &[], 1.0),
                dl("b", &["a"], 1.0),
                dl("c", &[], 1.0),
            ]),
            &[("a", Complete), ("b", Complete), ("c", Complete)],
        );
        let (new, _) = revise(
            &old,
            graph(vec![
                dl("a", &[], 1.0),
                dl("b", &["a", "c"], 1.0),
                dl("c", &[], 1.0),
            ]),
        );
        assert_eq!(new.statuses["b"], Ready);
    }

    #[test]
    fn reopen_propagates_to_dependents() {
        let old = state(
            graph(vec![
                dl("a", &[], 1.0),
                dl("x", &[], 1.0),
                dl("b", &["a"], 1.0),
                dl("c", &["b"], 1.0),
            ]),
            &[
                ("a", Complete),
                ("x", Ready),
                ("b", Complete),
                ("c", Complete),
            ],
        );
        let (new, _) = revise(
            &old,
            graph(vec![
                dl("a", &[], 1.0),
                dl("x", &[], 1.0),
                dl("b", &["a", "x"], 1.0),
                dl("c", &["b"], 1.0),
            ]),
        );
        assert_eq!(new.statuses["c"], Pending);
    }

    #[test]
    fn dependent_of_removed_prerequisite_is_rederived() {
        let old = state(
            graph(vec![
                dl("a", &[], 1.0),
                dl("x", &[], 1.0),
                dl("b", &["a", "x"], 1.0),
            ]),
            &[("a", Complete), ("x", Ready), ("b", Pending)],
        );
        let (new, _) = revise(&old, graph(vec![dl("a", &[], 1.0), dl("b", &["a"], 1.0)]));
        assert_eq!(new.statuses["b"], Ready);
    }

    fn contract_pair(contract: bool, v2: bool) -> (PlanState, PlanGraph) {
        let mut p = dl("p", &[], 1.0);
        p.metadata = json!({ "contract": contract });
        let c = iface(dl("c", &[], 1.0), "p");
        let old = state(
            graph(vec![p.clone(), c.clone()]),
            &[("p", Complete), ("c", Complete)],
        );
        let mut p2 = p;
        if v2 {
            p2.estimated_effort_hours = Some(9.0);
        }
        (old, graph(vec![p2, c]))
    }

    #[test]
    fn interface_consumer_reopens_when_contract_changes() {
        let (old, g) = contract_pair(true, true);
        let (new, _) = revise(&old, g);
        assert_eq!(new.statuses["c"], Ready);
    }

    #[test]
    fn interface_consumer_keeps_status_when_non_contract_changes() {
        let (old, g) = contract_pair(false, true);
        let (new, _) = revise(&old, g);
        assert_eq!(new.statuses["c"], Complete);
    }

    #[test]
    fn interface_consumer_keeps_status_when_contract_unchanged() {
        let (old, g) = contract_pair(true, false);
        let (new, _) = revise(&old, g);
        assert_eq!(new.statuses["c"], Complete);
    }

    #[test]
    fn metadata_only_change_keeps_status() {
        let old = ab(dl("a", &[], 1.0), dl("b", &["a"], 1.0));
        let mut a2 = dl("a", &[], 1.0);
        a2.metadata = json!({ "note": "edited" });
        let (new, _) = revise(&old, graph(vec![a2, dl("b", &["a"], 1.0)]));
        assert_eq!(new.statuses["a"], Complete);
    }

    #[test]
    fn metadata_only_change_is_reported_as_changed() {
        let old = ab(dl("a", &[], 1.0), dl("b", &["a"], 1.0));
        let mut a2 = dl("a", &[], 1.0);
        a2.metadata = json!({ "note": "edited" });
        let (_, diff) = revise(&old, graph(vec![a2, dl("b", &["a"], 1.0)]));
        assert_eq!(diff.changed, vec!["a".to_string()]);
    }

    #[test]
    fn failed_deliverable_stays_failed() {
        let failed = DeliverableStatus::Failed { reason: "x".into() };
        let old = state(
            graph(vec![dl("a", &[], 1.0), dl("b", &[], 1.0)]),
            &[("a", Complete), ("b", failed.clone())],
        );
        let (new, _) = revise(&old, graph(vec![dl("a", &[], 1.0), dl("b", &["a"], 1.0)]));
        assert_eq!(new.statuses["b"], failed);
    }

    #[test]
    fn revision_recomputes_critical_path() {
        let old = state(graph(vec![dl("a", &[], 1.0)]), &[("a", Ready)]);
        let (new, _) = revise(&old, graph(vec![dl("a", &[], 1.0), dl("c", &["a"], 5.0)]));
        assert!(new.cached_result.critical_path.contains(&"c".to_string()));
    }

    #[test]
    fn diff_vectors_are_sorted() {
        let old = state(graph(vec![dl("a", &[], 1.0)]), &[("a", Ready)]);
        let (_, diff) = revise(
            &old,
            graph(vec![
                dl("a", &[], 1.0),
                dl("z", &[], 1.0),
                dl("m", &[], 1.0),
            ]),
        );
        assert_eq!(diff.added, vec!["m".to_string(), "z".to_string()]);
    }

    #[test]
    fn duplicate_id_edges_revise_in_linear_time() {
        let chain = || {
            let mut ds = vec![dl("n0", &[], 1.0)];
            for i in 1..200 {
                let prev = format!("n{}", i - 1);
                ds.push(iface(dl(&format!("n{i}"), &[&prev], 1.0), &prev));
            }
            graph(ds)
        };
        let old = state(chain(), &[("n0", Ready)]);
        let start = std::time::Instant::now();
        let ok = plan_revision(&old, &chain(), false, Utc::now()).is_ok();
        assert!(ok && start.elapsed() < std::time::Duration::from_secs(2));
    }

    fn claiming(mut d: Deliverable, path: &str) -> Deliverable {
        d.owned_files = vec![OwnedFile::Path(PathBuf::from(path))];
        d
    }

    fn two_leases() -> (PlanState, PlanGraph) {
        let mut old = state(
            graph(vec![claiming(dl("x", &[], 1.0), "f.rs"), dl("y", &[], 1.0)]),
            &[("x", InProgress), ("y", InProgress)],
        );
        old.locks.insert("x".into(), lock("x"));
        old.locks.insert("y".into(), lock("y"));
        let g = graph(vec![
            claiming(dl("x", &[], 1.0), "f.rs"),
            claiming(dl("y", &["x"], 1.0), "f.rs"),
        ]);
        (old, g)
    }

    #[test]
    fn conflicting_claims_between_surviving_leases_are_refused() {
        let (old, g) = two_leases();
        let err = plan_revision(&old, &g, false, Utc::now())
            .err()
            .expect("conflict");
        assert!(matches!(err, PlannerError::LockHeld { .. }));
    }

    #[test]
    fn forced_revision_releases_conflicting_lease() {
        let (old, g) = two_leases();
        let (new, diff) = plan_revision(&old, &g, true, Utc::now()).expect("forced");
        assert!(diff.released_locks == vec!["y".to_string()] && new.statuses["y"] == Pending);
    }

    #[test]
    fn forced_revision_keeps_earlier_claimant_claim() {
        let (old, g) = two_leases();
        let (new, _) = plan_revision(&old, &g, true, Utc::now()).expect("forced");
        assert_eq!(
            new.file_claims.get(&PathBuf::from("f.rs")),
            Some(&FileClaim::Exclusive("x".into()))
        );
    }

    #[test]
    fn revision_with_unknown_prerequisite_is_invalid_graph() {
        let old = state(graph(vec![dl("a", &[], 1.0)]), &[("a", Ready)]);
        let err = plan_revision(
            &old,
            &graph(vec![dl("a", &["ghost"], 1.0)]),
            false,
            Utc::now(),
        )
        .err()
        .expect("invalid");
        assert!(matches!(err, PlannerError::InvalidGraph { .. }));
    }

    fn expired_leased() -> PlanState {
        let mut old = leased();
        let l = old.locks.get_mut("a").expect("lock");
        l.expires_at = Utc::now() - chrono::Duration::hours(1);
        old
    }

    #[test]
    fn expired_lease_does_not_block_removal() {
        let res = plan_revision(
            &expired_leased(),
            &graph(vec![dl("b", &[], 1.0)]),
            false,
            Utc::now(),
        );
        assert!(res.is_ok());
    }

    #[test]
    fn expired_lease_is_not_kept_and_is_rederived() {
        let (new, _) = revise(
            &expired_leased(),
            graph(vec![dl("a", &[], 1.0), dl("b", &[], 1.0)]),
        );
        assert!(new.locks.is_empty() && new.statuses["a"] == Ready);
    }

    fn chain_abc(c_pre: &[&str], b_effort: f32, b_pre: &[&str]) -> PlanGraph {
        graph(vec![
            dl("a", &[], 1.0),
            dl("x", &[], 1.0),
            dl("b", b_pre, b_effort),
            dl("c", c_pre, 1.0),
        ])
    }

    #[test]
    fn reopen_walk_stops_at_in_progress() {
        let mut old = state(
            chain_abc(&["b"], 1.0, &["a"]),
            &[
                ("a", Complete),
                ("x", Ready),
                ("b", InProgress),
                ("c", Pending),
            ],
        );
        old.locks.insert("b".into(), lock("b"));
        let (new, _) = revise(&old, chain_abc(&["b", "x"], 2.0, &["a"]));
        assert_eq!(new.statuses["b"], InProgress);
    }

    #[test]
    fn reopen_walk_stops_at_failed() {
        let failed = DeliverableStatus::Failed { reason: "x".into() };
        let old = state(
            chain_abc(&["b"], 1.0, &["a"]),
            &[
                ("a", Complete),
                ("x", Ready),
                ("b", Complete),
                ("c", failed.clone()),
            ],
        );
        let (new, _) = revise(&old, chain_abc(&["b"], 1.0, &["a", "x"]));
        assert_eq!(new.statuses["c"], failed);
    }

    #[test]
    fn old_only_contract_flag_reopens_interface_consumer() {
        let (old, _) = contract_pair(true, false);
        let mut p2 = dl("p", &[], 1.0);
        p2.metadata = json!({ "contract": false });
        let (new, _) = revise(&old, graph(vec![p2, iface(dl("c", &[], 1.0), "p")]));
        assert_eq!(new.statuses["c"], Ready);
    }
}
