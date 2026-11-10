//! `plan.explain`: why one deliverable is not moving, what completing it
//! changes, and the plan's next work sorted into classes.
//!
//! Built on [`read_snapshot`] and [`PlanIndex`], so its answers agree with
//! [`crate::overview`] at the same `as_of`. It is read-only and
//! deterministic: no AI calls, no network, no writes, and no leases. The
//! `file_conflicting` class reproduces `plan.acquire_cohort`'s greedy fill
//! (priority order, file-disjointness against live leases and earlier picks)
//! without taking a lease; `acquire_cohort` stays the authority.
//!
//! `docs/overview.md` specifies the request and the
//! [`EXPLAIN_SCHEMA`] response.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::overview::{
    BlockerClass, FileConflict, ItemRef, MAX_LIMIT, Named, PlanIndex, PlanSnapshot, ReadyAction,
    Section, read_snapshot,
};
use crate::plan::{
    DeliverableStatus, FINISH_ID, FileMode, LockInfo, PlanId, PlanRef, PlannerError, START_ID,
};
use crate::ports::Planner;

/// Schema identifier of the explain JSON; bumped only on a breaking change.
pub const EXPLAIN_SCHEMA: &str = "cpm-planner/explain/v1";

/// Default list length.
pub const DEFAULT_LIMIT: usize = 10;

/// Hours below this count as zero float or slack (the CPM works in `f32`).
pub const FLOAT_EPSILON: f32 = 1e-3;

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// Request for [`explain`].
#[derive(Debug, Clone)]
pub struct ExplainRequest {
    /// The plan selector, resolved with [`Planner::resolve_plan`].
    pub plan: PlanRef,
    /// The deliverable to explain; `None` returns only `next_work`.
    pub deliverable_id: Option<String>,
    /// The snapshot instant (EV status date, lease expiry). Defaults to the
    /// wall clock.
    pub as_of: Option<DateTime<Utc>>,
    /// Entries per list, `1..=MAX_LIMIT`; `None` is [`DEFAULT_LIMIT`].
    pub limit: Option<usize>,
}

impl ExplainRequest {
    pub fn new(plan: PlanRef) -> Self {
        Self {
            plan,
            deliverable_id: None,
            as_of: None,
            limit: None,
        }
    }

    pub fn with_deliverable(mut self, id: impl Into<String>) -> Self {
        self.deliverable_id = Some(id.into());
        self
    }

    pub fn with_as_of(mut self, as_of: DateTime<Utc>) -> Self {
        self.as_of = Some(as_of);
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

fn effective_limit(limit: Option<usize>) -> Result<usize, PlannerError> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(n) if (1..=MAX_LIMIT).contains(&n) => Ok(n),
        Some(n) => Err(PlannerError::InvalidValue {
            reason: format!("limit {n} is out of range; use 1..={MAX_LIMIT}"),
        }),
    }
}

/// Resolve the selector, read a consistent snapshot and explain it.
/// `DELIVERABLE_NOT_FOUND` for an unknown `deliverable_id`; `INVALID_VALUE`
/// for a bad limit.
pub async fn explain<P: Planner + ?Sized>(
    planner: &P,
    req: ExplainRequest,
) -> Result<Explanation, PlannerError> {
    let limit = effective_limit(req.limit)?;
    let plan_id = planner.resolve_plan(req.plan).await?;
    let as_of = req.as_of.unwrap_or_else(Utc::now);
    let snapshot = read_snapshot(planner, &plan_id, as_of).await?;
    build_explanation(&snapshot, req.deliverable_id.as_deref(), limit)
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// One explanation. Serialises as the stable [`EXPLAIN_SCHEMA`] JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    /// Always [`EXPLAIN_SCHEMA`].
    pub schema: String,
    pub as_of: DateTime<Utc>,
    pub plan: ExplainPlan,
    /// The deliverable asked about; omitted when none was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliverable: Option<DeliverableExplanation>,
    pub next_work: NextWork,
}

/// Plan identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainPlan {
    pub plan_id: PlanId,
    pub name: Option<String>,
    pub variant: Option<String>,
    pub revision: u32,
}

/// Where a deliverable stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverableState {
    Complete,
    /// Held by a lease (`lease` says whose, and whether it expired).
    Leased,
    /// Marked in progress without a lease (owner or manual work).
    InProgress,
    /// Can start now (`action` says how).
    Ready,
    /// Waiting on an incomplete prerequisite (`blocked` says which).
    Blocked,
    /// Marked failed; re-acquirable until the failure cap.
    Failed,
}

/// The lease on an explained deliverable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseView {
    pub caller_id: String,
    pub expires_at: DateTime<Utc>,
    /// `expires_at <= as_of`: the next mutating call reaps it.
    pub expired: bool,
}

/// A root blocker, with how to move it when it is ready.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplainRootBlocker {
    #[serde(flatten)]
    pub item: ItemRef,
    pub class: BlockerClass,
    /// Set when the blocker is in the ready set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ReadyAction>,
}

/// Why a deliverable is blocked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockedView {
    /// Class of the driving root blocker.
    pub class: BlockerClass,
    pub waiting_on: Vec<Named>,
    /// Driving blocker first.
    pub root_blockers: Vec<ExplainRootBlocker>,
    pub root_blockers_omitted: usize,
    /// The driving chain, this id first, ending at the driving root.
    pub chain: Vec<String>,
}

/// A milestone the deliverable feeds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FedMilestone {
    pub id: String,
    pub title: String,
    /// The milestone is on the plan's critical path.
    pub critical: bool,
    /// Hours this deliverable can slip before the milestone's earliest
    /// finish moves (0 when it drives the milestone).
    pub slack_hours: f32,
}

/// The explanation of one deliverable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliverableExplanation {
    #[serde(flatten)]
    pub item: ItemRef,
    /// On the plan's critical path (zero total float).
    pub critical: bool,
    /// Total float in hours.
    pub float: f32,
    pub state: DeliverableState,
    /// For `ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ReadyAction>,
    /// For `blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedView>,
    /// For `leased`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseView>,
    /// For `failed`: the reason given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub attempt_count: u32,
    pub failure_count: u32,
    pub lapse_count: u32,
    /// Dependents whose only incomplete prerequisite is this one: what
    /// becomes ready when it completes. Graph order.
    pub unlocks: Vec<Named>,
    pub unlocks_omitted: usize,
    /// Incomplete milestones it feeds, earliest finish first.
    pub feeds_milestones: Vec<FedMilestone>,
    pub feeds_milestones_omitted: usize,
    /// One sentence on what a slip here does to the finish and to the next
    /// milestone.
    pub critical_path_impact: String,
}

/// A next-work class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextWorkClass {
    /// Ready, but `acquire_cohort` would skip it now: an owned file
    /// conflicts with a live lease or a higher-priority pick.
    FileConflicting,
    /// Ready and on the plan's critical path, or on the driving path of an
    /// incomplete milestone.
    ReadyCritical,
    /// Ready with slack: can run in parallel with critical work.
    ReadySlack,
    /// Blocked; the driving root blocker is manual (owner action).
    BlockedManual,
    /// Blocked on an external sign-off.
    BlockedExternal,
    /// Blocked on a delivery gate.
    BlockedGate,
    /// Blocked on ordinary work.
    BlockedDependency,
}

/// One deliverable in a next-work class.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NextWorkEntry {
    #[serde(flatten)]
    pub item: ItemRef,
    pub float: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_milestone: Option<Named>,
    /// For the ready and file-conflicting classes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ReadyAction>,
    /// For the blocked classes: the driving root blocker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<Named>,
    /// For `file_conflicting`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_conflicts: Vec<FileConflict>,
}

/// Every incomplete, unleased, not-failed deliverable in exactly one class.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NextWork {
    pub ready_critical: Section<NextWorkEntry>,
    pub ready_slack: Section<NextWorkEntry>,
    pub blocked_dependency: Section<NextWorkEntry>,
    pub blocked_manual: Section<NextWorkEntry>,
    pub blocked_external: Section<NextWorkEntry>,
    pub blocked_gate: Section<NextWorkEntry>,
    pub file_conflicting: Section<NextWorkEntry>,
}

// ---------------------------------------------------------------------------
// Cohort simulation (acquire_cohort's fill, read-only)
// ---------------------------------------------------------------------------

/// What `plan.acquire_cohort` would do with every ready candidate at
/// `as_of` if asked for all of them: the ids it would lease, in its priority
/// order, and the conflicts of each one it would skip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CohortPreview {
    /// Leased, in `acquire_cohort` priority order (smallest latest start,
    /// then least float, then id).
    pub leasable: Vec<String>,
    /// Skipped for an owned-file conflict, with each conflicting claim.
    pub conflicting: HashMap<String, Vec<FileConflict>>,
}

/// Preview `acquire_cohort`'s greedy fill over the ready set. Candidates are
/// `plan.status` `ready` minus manual, gate and capped deliverables (which
/// it never leases). Leases live at `as_of` hold their claims; expired ones
/// are reaped first, as `acquire_cohort` does. Exclusive conflicts with any
/// claim; append shares with append.
pub fn preview_cohort(snapshot: &PlanSnapshot, index: &PlanIndex<'_>) -> CohortPreview {
    // Held claims: path -> [(holder, mode)].
    let mut held: HashMap<PathBuf, Vec<(&str, FileMode)>> = HashMap::new();
    for lock in live_leases(&snapshot.status.locks_held, snapshot.as_of) {
        if let Some(d) = index.deliverable(&lock.deliverable_id) {
            for f in &d.owned_files {
                held.entry(f.path().to_path_buf())
                    .or_default()
                    .push((lock.deliverable_id.as_str(), f.mode()));
            }
        }
    }
    let mut candidates: Vec<&str> = snapshot
        .status
        .ready
        .iter()
        .map(String::as_str)
        .filter(|id| index.ready_action(id) == ReadyAction::Acquire)
        .collect();
    candidates.sort_by(|a, b| {
        let key = |id: &str| index.schedule(id).map_or((0.0, 0.0), |r| (r.ls, r.float));
        let (la, fa) = key(a);
        let (lb, fb) = key(b);
        la.total_cmp(&lb)
            .then_with(|| fa.total_cmp(&fb))
            .then_with(|| a.cmp(b))
    });
    let mut picked: HashMap<PathBuf, Vec<(&str, FileMode)>> = HashMap::new();
    let mut preview = CohortPreview::default();
    for id in candidates {
        let Some(d) = index.deliverable(id) else {
            continue;
        };
        let mut found: Vec<FileConflict> = Vec::new();
        for f in &d.owned_files {
            for (claims, leased) in [(&held, true), (&picked, false)] {
                for (other, mode) in claims.get(f.path()).into_iter().flatten() {
                    if crate::locks::modes_conflict(f.mode(), *mode) {
                        found.push(FileConflict {
                            path: f.path().to_string_lossy().into_owned(),
                            with: (*other).to_string(),
                            leased,
                        });
                    }
                }
            }
        }
        if found.is_empty() {
            for f in &d.owned_files {
                picked
                    .entry(f.path().to_path_buf())
                    .or_default()
                    .push((id, f.mode()));
            }
            preview.leasable.push(id.to_string());
        } else {
            found.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.with.cmp(&b.with)));
            found.dedup();
            preview.conflicting.insert(id.to_string(), found);
        }
    }
    preview
}

fn live_leases(locks: &[LockInfo], as_of: DateTime<Utc>) -> impl Iterator<Item = &LockInfo> {
    locks.iter().filter(move |l| l.expires_at > as_of)
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Ids on the driving path of some incomplete milestone.
fn milestone_driving_ids(snapshot: &PlanSnapshot) -> HashSet<&str> {
    snapshot
        .status
        .milestones
        .iter()
        .filter(|m| !m.complete)
        .flat_map(|m| m.critical_path.iter().map(String::as_str))
        .filter(|id| *id != START_ID && *id != FINISH_ID)
        .collect()
}

/// The next-work class of every classified deliverable, in this order:
/// ready ones in `plan.status` `ready` order, then blocked ones by least
/// float, latest start, id. Leased, complete, failed and lockless
/// in-progress deliverables have no class.
pub fn classify_next_work(
    snapshot: &PlanSnapshot,
    index: &PlanIndex<'_>,
    preview: &CohortPreview,
) -> Vec<(String, NextWorkClass)> {
    let critical: HashSet<&str> = snapshot
        .status
        .critical_ids
        .iter()
        .map(String::as_str)
        .collect();
    let driving = milestone_driving_ids(snapshot);
    let mut out: Vec<(String, NextWorkClass)> = snapshot
        .status
        .ready
        .iter()
        .map(|id| {
            let class = if preview.conflicting.contains_key(id) {
                NextWorkClass::FileConflicting
            } else if critical.contains(id.as_str()) || driving.contains(id.as_str()) {
                NextWorkClass::ReadyCritical
            } else {
                NextWorkClass::ReadySlack
            };
            (id.clone(), class)
        })
        .collect();
    let leased: HashSet<&str> = snapshot
        .status
        .locks_held
        .iter()
        .map(|l| l.deliverable_id.as_str())
        .collect();
    let mut blocked: Vec<(&str, BlockerClass)> = snapshot
        .graph
        .deliverables
        .iter()
        .map(|d| d.id.as_str())
        .filter(|id| {
            !leased.contains(id)
                && !matches!(
                    index.status(id),
                    DeliverableStatus::Complete | DeliverableStatus::Failed { .. }
                )
        })
        .filter_map(|id| index.blocker_chain(id).map(|c| (id, c.class)))
        .collect();
    blocked.sort_by(|(a, _), (b, _)| schedule_order(index, a, b));
    out.extend(blocked.into_iter().map(|(id, class)| {
        let class = match class {
            BlockerClass::Manual => NextWorkClass::BlockedManual,
            BlockerClass::External => NextWorkClass::BlockedExternal,
            BlockerClass::Gate => NextWorkClass::BlockedGate,
            BlockerClass::Dependency => NextWorkClass::BlockedDependency,
        };
        (id.to_string(), class)
    }));
    out
}

/// Least float, then latest start, then id.
fn schedule_order(index: &PlanIndex<'_>, a: &str, b: &str) -> std::cmp::Ordering {
    let key = |id: &str| index.schedule(id).map_or((0.0, 0.0), |r| (r.float, r.ls));
    let (fa, la) = key(a);
    let (fb, lb) = key(b);
    fa.total_cmp(&fb)
        .then_with(|| la.total_cmp(&lb))
        .then_with(|| a.cmp(b))
}

// ---------------------------------------------------------------------------
// Milestone slack
// ---------------------------------------------------------------------------

/// Hours `id` can slip before `milestone`'s earliest finish moves: a
/// backward pass from the milestone's earliest finish over its ancestors,
/// with edge lags. `None` when `id` does not feed `milestone`.
pub fn slack_to_milestone(
    snapshot: &PlanSnapshot,
    index: &PlanIndex<'_>,
    id: &str,
    milestone: &str,
) -> Option<f32> {
    if !index.feeds(id, milestone) {
        return None;
    }
    let target = index.schedule(milestone)?;
    // Ancestors of the milestone in topological order (Kahn, ties in graph
    // order), restricted to the ancestor set.
    let members: Vec<&str> = snapshot
        .graph
        .deliverables
        .iter()
        .map(|d| d.id.as_str())
        .filter(|x| index.feeds(x, milestone))
        .collect();
    let member_set: HashSet<&str> = members.iter().copied().collect();
    let mut indegree: HashMap<&str, usize> = members
        .iter()
        .map(|x| {
            let n = index.deliverable(x).map_or(0, |d| {
                d.prerequisites
                    .iter()
                    .filter(|p| member_set.contains(p.id()))
                    .count()
            });
            (*x, n)
        })
        .collect();
    let mut queue: VecDeque<&str> = members
        .iter()
        .copied()
        .filter(|x| indegree.get(x) == Some(&0))
        .collect();
    let mut order: Vec<&str> = Vec::with_capacity(members.len());
    while let Some(x) = queue.pop_front() {
        order.push(x);
        for dep in index.dependents(x) {
            if let Some(n) = indegree.get_mut(dep) {
                *n -= 1;
                if *n == 0 {
                    queue.push_back(dep);
                }
            }
        }
    }
    let mut latest_start: HashMap<&str, f32> = HashMap::new();
    for x in order.into_iter().rev() {
        let row = index.schedule(x)?;
        let duration = row.ef - row.es;
        let latest_finish = if x == milestone {
            target.ef
        } else {
            index
                .dependents(x)
                .iter()
                .filter_map(|s| {
                    let ls = latest_start.get(s)?;
                    let lag = index
                        .deliverable(s)?
                        .prerequisites
                        .iter()
                        .find(|p| p.id() == x)
                        .map_or(0.0, |p| p.lag_hours());
                    Some(ls - lag)
                })
                .min_by(f32::total_cmp)?
        };
        latest_start.insert(x, latest_finish - duration);
    }
    let es = index.schedule(id)?.es;
    let slack = latest_start.get(id)? - es;
    Some(if slack.abs() < FLOAT_EPSILON {
        0.0
    } else {
        slack
    })
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// Build the explanation of `snapshot` (pure: same input, same output).
/// `DELIVERABLE_NOT_FOUND` when `deliverable_id` is not in the graph;
/// `INVALID_VALUE` when `limit` is outside `1..=MAX_LIMIT`.
pub fn build_explanation(
    snapshot: &PlanSnapshot,
    deliverable_id: Option<&str>,
    limit: usize,
) -> Result<Explanation, PlannerError> {
    let limit = effective_limit(Some(limit))?;
    let index = PlanIndex::new(&snapshot.graph, &snapshot.status);
    let preview = preview_cohort(snapshot, &index);
    let deliverable = match deliverable_id {
        None => None,
        Some(id) => {
            if index.deliverable(id).is_none() {
                return Err(PlannerError::DeliverableNotFound {
                    plan_id: snapshot.plan_id.0.clone(),
                    deliverable_id: id.to_string(),
                });
            }
            Some(explain_one(snapshot, &index, id, limit))
        }
    };
    let classes = classify_next_work(snapshot, &index, &preview);
    let entry = |id: &str, class: NextWorkClass| {
        let ready = matches!(
            class,
            NextWorkClass::FileConflicting
                | NextWorkClass::ReadyCritical
                | NextWorkClass::ReadySlack
        );
        NextWorkEntry {
            item: index.item_ref(id),
            float: index.schedule(id).map_or(0.0, |r| r.float),
            next_milestone: index.next_milestone(id).map(|m| index.named(m)),
            action: ready.then(|| index.ready_action(id)),
            blocked_by: if ready {
                None
            } else {
                index
                    .blocker_chain(id)
                    .and_then(|c| c.roots.first().map(|r| index.named(r)))
            },
            file_conflicts: preview
                .conflicting
                .get(id)
                .map(|v| v.iter().take(limit).cloned().collect())
                .unwrap_or_default(),
        }
    };
    let section = |class: NextWorkClass| {
        let ids: Vec<&str> = classes
            .iter()
            .filter(|(_, c)| *c == class)
            .map(|(id, _)| id.as_str())
            .collect();
        let total = ids.len();
        let items: Vec<NextWorkEntry> = ids
            .into_iter()
            .take(limit)
            .map(|id| entry(id, class))
            .collect();
        Section {
            total,
            omitted: total - items.len(),
            items,
        }
    };
    let status = &snapshot.status;
    Ok(Explanation {
        schema: EXPLAIN_SCHEMA.to_string(),
        as_of: snapshot.as_of,
        plan: ExplainPlan {
            plan_id: snapshot.plan_id.clone(),
            name: status.name.clone(),
            variant: status.variant.clone(),
            revision: snapshot.revision,
        },
        deliverable,
        next_work: NextWork {
            ready_critical: section(NextWorkClass::ReadyCritical),
            ready_slack: section(NextWorkClass::ReadySlack),
            blocked_dependency: section(NextWorkClass::BlockedDependency),
            blocked_manual: section(NextWorkClass::BlockedManual),
            blocked_external: section(NextWorkClass::BlockedExternal),
            blocked_gate: section(NextWorkClass::BlockedGate),
            file_conflicting: section(NextWorkClass::FileConflicting),
        },
    })
}

fn explain_one(
    snapshot: &PlanSnapshot,
    index: &PlanIndex<'_>,
    id: &str,
    limit: usize,
) -> DeliverableExplanation {
    let status = &snapshot.status;
    let critical_ids: HashSet<&str> = status.critical_ids.iter().map(String::as_str).collect();
    let critical = critical_ids.contains(id);
    let float = index.schedule(id).map_or(0.0, |r| r.float);
    let lock = status.locks_held.iter().find(|l| l.deliverable_id == id);
    let chain = index.blocker_chain(id);
    let in_ready = status.ready.iter().any(|r| r == id);
    let live = index.status(id);
    let state = match (live, lock) {
        (DeliverableStatus::Complete, _) => DeliverableState::Complete,
        (_, Some(_)) => DeliverableState::Leased,
        (DeliverableStatus::Failed { .. }, _) => DeliverableState::Failed,
        _ if chain.is_some() => DeliverableState::Blocked,
        _ if in_ready => DeliverableState::Ready,
        _ => DeliverableState::InProgress,
    };
    let blocked = match (state, &chain) {
        (DeliverableState::Blocked, Some(c)) => Some(BlockedView {
            class: c.class,
            waiting_on: c.waiting_on.iter().map(|w| index.named(w)).collect(),
            root_blockers: c
                .roots
                .iter()
                .take(limit)
                .map(|r| ExplainRootBlocker {
                    item: index.item_ref(r),
                    class: index.kind(r).into(),
                    action: status
                        .ready
                        .iter()
                        .any(|x| x == r)
                        .then(|| index.ready_action(r)),
                })
                .collect(),
            root_blockers_omitted: c.roots.len().saturating_sub(limit),
            chain: c.chain.clone(),
        }),
        _ => None,
    };
    let unlocks_all = index.unlocks(id);
    let fed: Vec<&str> = index.downstream_milestones(id);
    let feeds_milestones: Vec<FedMilestone> = fed
        .iter()
        .take(limit)
        .map(|m| FedMilestone {
            id: (*m).to_string(),
            title: index.title(m),
            critical: critical_ids.contains(m),
            slack_hours: slack_to_milestone(snapshot, index, id, m).unwrap_or(0.0),
        })
        .collect();
    let (attempts, failures, lapses) = index.counters(id);
    let critical_path_impact = impact_sentence(state, critical, float, feeds_milestones.first());
    DeliverableExplanation {
        item: index.item_ref(id),
        critical,
        float,
        state,
        action: (state == DeliverableState::Ready).then(|| index.ready_action(id)),
        blocked,
        lease: lock.map(|l| LeaseView {
            caller_id: l.caller_id.0.clone(),
            expires_at: l.expires_at,
            expired: l.expires_at <= snapshot.as_of,
        }),
        reason: match live {
            DeliverableStatus::Failed { reason } => Some(reason.clone()),
            _ => None,
        },
        attempt_count: attempts,
        failure_count: failures,
        lapse_count: lapses,
        unlocks: unlocks_all
            .iter()
            .take(limit)
            .map(|u| index.named(u))
            .collect(),
        unlocks_omitted: unlocks_all.len().saturating_sub(limit),
        feeds_milestones_omitted: fed.len().saturating_sub(feeds_milestones.len()),
        feeds_milestones,
        critical_path_impact,
    }
}

fn impact_sentence(
    state: DeliverableState,
    critical: bool,
    float: f32,
    next: Option<&FedMilestone>,
) -> String {
    if state == DeliverableState::Complete {
        return "Complete: no remaining schedule impact.".to_string();
    }
    let finish = if critical || float.abs() < FLOAT_EPSILON {
        "On the critical path: each hour it slips moves the plan finish by an hour".to_string()
    } else {
        format!("{float:.1}h of float before a slip moves the plan finish")
    };
    let milestone = match next {
        None => String::new(),
        Some(m) if m.slack_hours.abs() < FLOAT_EPSILON => {
            format!("; it drives milestone `{}`", m.id)
        }
        Some(m) => format!(
            "; {:.1}h of slack before it moves milestone `{}`",
            m.slack_hours, m.id
        ),
    };
    format!("{finish}{milestone}.")
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn label(id: &str, title: &str) -> String {
    if title == id {
        format!("`{id}`")
    } else {
        format!("{} (`{id}`)", cell(title))
    }
}

fn action_str(a: ReadyAction) -> &'static str {
    match a {
        ReadyAction::Acquire => "acquire",
        ReadyAction::Accept => "accept",
        ReadyAction::Operator => "operator reset",
    }
}

fn class_str(c: BlockerClass) -> &'static str {
    match c {
        BlockerClass::Dependency => "dependency",
        BlockerClass::Manual => "manual",
        BlockerClass::External => "external",
        BlockerClass::Gate => "gate",
    }
}

fn state_str(s: DeliverableState) -> &'static str {
    match s {
        DeliverableState::Complete => "complete",
        DeliverableState::Leased => "leased",
        DeliverableState::InProgress => "in progress",
        DeliverableState::Ready => "ready",
        DeliverableState::Blocked => "blocked",
        DeliverableState::Failed => "failed",
    }
}

/// Render `e` as compact Markdown: the deliverable (when asked), then one
/// line per non-empty next-work class. Deterministic for a given value.
pub fn render_markdown(e: &Explanation) -> String {
    let mut out = String::new();
    let name = match (&e.plan.name, &e.plan.variant) {
        (Some(n), Some(v)) => format!("{n}/{v}"),
        _ => e.plan.plan_id.0.clone(),
    };
    out.push_str(&format!(
        "# Explain: {}\n\n`{}` rev {} · as of {}\n",
        cell(&name),
        e.plan.plan_id.0,
        e.plan.revision,
        e.as_of.to_rfc3339()
    ));
    if let Some(d) = &e.deliverable {
        out.push_str(&format!(
            "\n## {}\n\n{}{}\n",
            label(&d.item.id, &d.item.title),
            state_str(d.state),
            d.action
                .map_or_else(String::new, |a| format!(": {}", action_str(a)))
        ));
        if let Some(l) = &d.lease {
            out.push_str(&format!(
                "Leased by `{}` until {}{}.\n",
                cell(&l.caller_id),
                l.expires_at.to_rfc3339(),
                if l.expired { " (expired)" } else { "" }
            ));
        }
        if let Some(r) = &d.reason {
            out.push_str(&format!("Failed: {}\n", cell(r)));
        }
        if let Some(b) = &d.blocked {
            out.push_str(&format!(
                "Blocked ({}) waiting on {}.\n",
                class_str(b.class),
                b.waiting_on
                    .iter()
                    .map(|w| format!("`{}`", w.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            for r in &b.root_blockers {
                out.push_str(&format!(
                    "- move {} ({}, {}{})\n",
                    label(&r.item.id, &r.item.title),
                    r.item.status.as_str(),
                    class_str(r.class),
                    r.action
                        .map_or_else(String::new, |a| format!(": {}", action_str(a)))
                ));
            }
            if b.root_blockers_omitted > 0 {
                out.push_str(&format!("- +{} more\n", b.root_blockers_omitted));
            }
            out.push_str(&format!("Chain: {}\n", b.chain.join(" ← ")));
        }
        if !d.unlocks.is_empty() {
            out.push_str(&format!(
                "Completing it unlocks {}{}.\n",
                d.unlocks
                    .iter()
                    .map(|u| format!("`{}`", u.id))
                    .collect::<Vec<_>>()
                    .join(", "),
                if d.unlocks_omitted > 0 {
                    format!(" (+{})", d.unlocks_omitted)
                } else {
                    String::new()
                }
            ));
        }
        out.push_str(&format!("{}\n", d.critical_path_impact));
    }
    out.push_str("\n## Next work\n\n");
    let classes: [(&str, &Section<NextWorkEntry>); 7] = [
        ("Ready, critical", &e.next_work.ready_critical),
        ("Ready, with slack", &e.next_work.ready_slack),
        ("File-conflicting", &e.next_work.file_conflicting),
        ("Blocked, manual", &e.next_work.blocked_manual),
        ("Blocked, external", &e.next_work.blocked_external),
        ("Blocked, gate", &e.next_work.blocked_gate),
        ("Blocked, dependency", &e.next_work.blocked_dependency),
    ];
    let mut any = false;
    for (title, s) in classes {
        if s.total == 0 {
            continue;
        }
        any = true;
        let items: Vec<String> = s
            .items
            .iter()
            .map(|n| {
                let extra = match (&n.action, &n.blocked_by) {
                    (Some(a), _) => format!(" {}", action_str(*a)),
                    (None, Some(b)) => format!(" ← `{}`", b.id),
                    (None, None) => String::new(),
                };
                format!("{}{extra}", label(&n.item.id, &n.item.title))
            })
            .collect();
        out.push_str(&format!(
            "- **{title}** ({}): {}{}\n",
            s.total,
            items.join("; "),
            if s.omitted > 0 {
                format!("; +{} more", s.omitted)
            } else {
                String::new()
            }
        ));
    }
    if !any {
        out.push_str("Nothing to start: every deliverable is complete, leased or failed.\n");
    }
    out
}
