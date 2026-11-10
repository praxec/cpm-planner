//! One-call plan overview: a single snapshot at one `as_of` that joins what
//! `plan.status`, `plan.get` and `plan.ev` return today.
//!
//! The overview answers "where is this plan?" without the caller joining
//! three responses by hand: every listed deliverable carries its
//! human-readable title, status and owner; the critical path and milestones
//! say what they feed; the ready queue says how to start each item; and each
//! blocked item names the incomplete prerequisite chain and the kind of
//! action (dependency, manual, external sign-off or gate) that unblocks it.
//!
//! It is read-only and deterministic: no AI calls, no network, no writes.
//! The JSON shape is versioned by [`OVERVIEW_SCHEMA`]; [`render_markdown`]
//! renders the same value as compact Markdown. Every list is bounded
//! ([`OverviewOptions::limit`]) and reports how many entries it omitted, so a
//! plan with hundreds of deliverables stays small.
//!
//! The graph helpers on [`PlanIndex`] ([`PlanIndex::blocker_chain`],
//! [`PlanIndex::next_milestone`], [`PlanIndex::unlocks`], ...) are public so
//! `plan.explain` can reuse them. `docs/overview.md` specifies both tools.
//!
//! # Consistency
//!
//! [`read_snapshot`] reads the head revision and graph, `status`, `ev`, and
//! the head revision again. When the revision moved, the deliverable ids
//! differ, or an EV row's status disagrees with `status`, a write landed
//! between the reads, and it reads again (at most [`MAX_READ_ATTEMPTS`]
//! times). Lease renewals between the reads are harmless: leases are read
//! once, from `status`.

use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::earned_value::{EvReport, EvRowStatus, PlanBaseline, Undefined};
use crate::evidence::{DeliverableEvidenceStatus, Identity, StoredEvidence, Verdict};
use crate::plan::{
    Deliverable, DeliverableStatus, FINISH_ID, LockInfo, MilestoneRow, OwnedFile, PlanGraph,
    PlanId, PlanRef, PlanStatus, PlannerError, START_ID, ScheduleRow,
};
use crate::planner::{MAX_ATTEMPTS, MAX_LAPSES};
use crate::ports::Planner;

/// Schema identifier of the overview JSON; bumped only on a breaking change.
pub const OVERVIEW_SCHEMA: &str = "cpm-planner/overview/v1";

/// Default list length at [`Detail::Brief`].
pub const BRIEF_LIMIT: usize = 10;

/// Default list length at [`Detail::Full`].
pub const FULL_LIMIT: usize = 100;

/// Largest accepted [`OverviewOptions::limit`].
pub const MAX_LIMIT: usize = 1000;

/// Root blockers listed per blocked item at [`Detail::Brief`]; full detail
/// uses the list limit.
pub const BRIEF_ROOT_BLOCKERS: usize = 3;

/// Milestones listed in an entry's `feeds_milestones` at [`Detail::Brief`].
pub const BRIEF_FEEDS: usize = 3;

/// How many times [`read_snapshot`] re-reads when a write lands between its
/// reads.
pub const MAX_READ_ATTEMPTS: usize = 3;

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// How much the overview says per entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detail {
    /// Headline numbers and short lists ([`BRIEF_LIMIT`] per list); blocked
    /// items show their direct prerequisites and up to
    /// [`BRIEF_ROOT_BLOCKERS`] root blockers.
    #[default]
    Brief,
    /// Longer lists ([`FULL_LIMIT`]), the driving blocker chain, schedule
    /// numbers and owned files.
    Full,
}

/// Narrows every deliverable list of the overview. Counts and EV always
/// describe the whole plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverviewFilter {
    /// Keep entries whose `metadata.owner` equals this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Keep entries that feed this milestone (are it or its ancestors).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<String>,
}

impl OverviewFilter {
    fn is_empty(&self) -> bool {
        self.owner.is_none() && self.milestone.is_none()
    }
}

/// Shape options shared by [`overview`] and [`build_overview`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverviewOptions {
    #[serde(default)]
    pub detail: Detail,
    /// Entries per list; `None` uses the detail's default. `1..=MAX_LIMIT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "OverviewFilter::is_empty")]
    pub filter: OverviewFilter,
    /// The plan-wide target identity evidence is judged against
    /// (`docs/evidence.md`, target resolution step 1). Without it a gate is
    /// judged against its candidate's newest identity, and other records
    /// against no target (their identity fields read as unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_target: Option<Identity>,
}

impl OverviewOptions {
    /// Check the limit (`INVALID_VALUE`) and the evidence target
    /// (`INVALID_EVIDENCE`).
    pub fn validate(&self) -> Result<(), PlannerError> {
        self.effective_limit()?;
        if let Some(target) = &self.evidence_target {
            target.validate()?;
        }
        Ok(())
    }

    /// The effective list length, or `INVALID_VALUE` for 0 or above
    /// [`MAX_LIMIT`].
    pub fn effective_limit(&self) -> Result<usize, PlannerError> {
        match self.limit {
            None => Ok(match self.detail {
                Detail::Brief => BRIEF_LIMIT,
                Detail::Full => FULL_LIMIT,
            }),
            Some(n) if (1..=MAX_LIMIT).contains(&n) => Ok(n),
            Some(n) => Err(PlannerError::InvalidValue {
                reason: format!("limit {n} is out of range; use 1..={MAX_LIMIT}"),
            }),
        }
    }
}

/// Request for [`overview`].
#[derive(Debug, Clone)]
pub struct OverviewRequest {
    /// The plan selector, resolved with [`Planner::resolve_plan`].
    pub plan: PlanRef,
    /// The snapshot instant: EV's PV status date and the instant leases are
    /// judged live or expired against. Defaults to the wall clock.
    pub as_of: Option<DateTime<Utc>>,
    pub options: OverviewOptions,
}

impl OverviewRequest {
    pub fn new(plan: PlanRef) -> Self {
        Self {
            plan,
            as_of: None,
            options: OverviewOptions::default(),
        }
    }

    pub fn with_as_of(mut self, as_of: DateTime<Utc>) -> Self {
        self.as_of = Some(as_of);
        self
    }

    pub fn with_detail(mut self, detail: Detail) -> Self {
        self.options.detail = detail;
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.options.limit = Some(limit);
        self
    }

    pub fn with_filter(mut self, filter: OverviewFilter) -> Self {
        self.options.filter = filter;
        self
    }

    /// Judge evidence against `target` (the `evidence_target` argument;
    /// parse wire JSON with [`Identity::from_json`]).
    pub fn with_evidence_target(mut self, target: Identity) -> Self {
        self.options.evidence_target = Some(target);
        self
    }
}

// ---------------------------------------------------------------------------
// Snapshot read
// ---------------------------------------------------------------------------

/// The three existing reads of one plan, taken together at one `as_of`.
#[derive(Debug, Clone)]
pub struct PlanSnapshot {
    pub plan_id: PlanId,
    pub as_of: DateTime<Utc>,
    /// Head revision (1 for a plan never revised).
    pub revision: u32,
    /// The stored head graph (what `plan.get` returns).
    pub graph: PlanGraph,
    /// What `plan.status` returns.
    pub status: PlanStatus,
    /// What `plan.ev` returns at `as_of`; `None` before the first baseline.
    pub ev: Option<EvReport>,
    /// Stored evidence entries per deliverable (only those with any), in
    /// append order ([`Planner::stored_evidence`]).
    pub evidence: HashMap<String, Vec<StoredEvidence>>,
}

/// Read `status`, the head graph, `ev` at `as_of` and the stored evidence of
/// `plan_id`, re-reading when a write lands in between (see the module
/// docs). Before the first baseline `ev` is `None`; any other EV error is
/// returned.
///
/// Blocking: `status` and `ev` recompute the CPM, so async servers should
/// run this off the runtime's worker threads.
pub async fn read_snapshot<P: Planner + ?Sized>(
    planner: &P,
    plan_id: &PlanId,
    as_of: DateTime<Utc>,
) -> Result<PlanSnapshot, PlannerError> {
    for _ in 0..MAX_READ_ATTEMPTS {
        let (revision, graph) = planner.revision_graph(plan_id, None).await?;
        let evidence = planner.stored_evidence(plan_id).await?;
        let status = planner.status(plan_id).await?;
        let ev = match planner.ev(plan_id, Some(as_of)).await {
            Ok(report) => Some(report),
            Err(PlannerError::NotBaselined { .. }) => None,
            Err(e) => return Err(e),
        };
        let evidence_after = planner.stored_evidence(plan_id).await?;
        let (revision_after, _) = planner.revision_graph(plan_id, None).await?;
        if revision == revision_after
            && evidence == evidence_after
            && reads_agree(&graph, &status, ev.as_ref())
        {
            return Ok(PlanSnapshot {
                plan_id: plan_id.clone(),
                as_of,
                revision,
                graph,
                status,
                ev,
                evidence,
            });
        }
    }
    Err(PlannerError::BackendError(anyhow::anyhow!(
        "plan {plan_id} kept changing while the overview read it ({MAX_READ_ATTEMPTS} attempts); \
         retry"
    )))
}

/// Whether the three reads describe the same plan state: identical ids in
/// graph order, and every live EV row's status equal to `status`'s.
fn reads_agree(graph: &PlanGraph, status: &PlanStatus, ev: Option<&EvReport>) -> bool {
    let same_ids = graph.deliverables.len() == status.deliverables.len()
        && graph
            .deliverables
            .iter()
            .zip(&status.deliverables)
            .all(|(d, row)| d.id == row.0);
    if !same_ids {
        return false;
    }
    let Some(ev) = ev else { return true };
    let by_id: HashMap<&str, &DeliverableStatus> = status
        .deliverables
        .iter()
        .map(|row| (row.0.as_str(), &row.1))
        .collect();
    ev.rows
        .iter()
        .all(|row| match (&row.status, by_id.get(row.id.as_str())) {
            (EvRowStatus::Removed, None) => true,
            (s, Some(live)) => s == *live,
            (_, None) => false,
        })
}

/// Resolve the selector, read a consistent [`PlanSnapshot`] and build the
/// [`Overview`]. `as_of` defaults to the wall clock.
pub async fn overview<P: Planner + ?Sized>(
    planner: &P,
    req: OverviewRequest,
) -> Result<Overview, PlannerError> {
    // Validate before any read so a bad request costs nothing.
    req.options.validate()?;
    let plan_id = planner.resolve_plan(req.plan).await?;
    let as_of = req.as_of.unwrap_or_else(Utc::now);
    let snapshot = read_snapshot(planner, &plan_id, as_of).await?;
    build_overview(&snapshot, &req.options)
}

// ---------------------------------------------------------------------------
// Graph index (shared with plan.explain)
// ---------------------------------------------------------------------------

/// What kind of work a deliverable is, from its metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverableKind {
    /// Ordinary agent work, leased with `plan.acquire_cohort`.
    Work,
    /// A milestone (zero-effort marker).
    Milestone,
    /// `metadata.kind == "manual"`: owner action, never leased; closed with
    /// `plan.accept`.
    Manual,
    /// `metadata.kind == "external"` or `metadata.external == true`: a
    /// sign-off from outside the team. Leased like work (the planner gives
    /// it no special lease semantics); classified so blockers name it.
    External,
    /// `metadata.kind == "gate"`: a fixed-cost delivery gate, never leased;
    /// closed with `plan.accept` and evidence.
    Gate,
}

/// Why a blocked deliverable cannot start, by the kind of its root blocker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerClass {
    /// Ordinary work (or a milestone) still to finish.
    Dependency,
    /// A manual deliverable waiting on owner action (`plan.accept`).
    Manual,
    /// An external sign-off.
    External,
    /// A delivery gate (`plan.accept` with evidence).
    Gate,
}

impl From<DeliverableKind> for BlockerClass {
    fn from(kind: DeliverableKind) -> Self {
        match kind {
            DeliverableKind::Work | DeliverableKind::Milestone => Self::Dependency,
            DeliverableKind::Manual => Self::Manual,
            DeliverableKind::External => Self::External,
            DeliverableKind::Gate => Self::Gate,
        }
    }
}

/// Wire status label of a deliverable (the `status` tag of
/// [`DeliverableStatus`], without the failure reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusLabel {
    Pending,
    Ready,
    InProgress,
    SupersededRevision,
    Complete,
    Failed,
}

impl From<&DeliverableStatus> for StatusLabel {
    fn from(s: &DeliverableStatus) -> Self {
        match s {
            DeliverableStatus::Pending => Self::Pending,
            DeliverableStatus::Ready => Self::Ready,
            DeliverableStatus::InProgress => Self::InProgress,
            DeliverableStatus::SupersededRevision => Self::SupersededRevision,
            DeliverableStatus::Complete => Self::Complete,
            DeliverableStatus::Failed { .. } => Self::Failed,
        }
    }
}

impl StatusLabel {
    /// The wire label, e.g. `"in_progress"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::InProgress => "in_progress",
            Self::SupersededRevision => "superseded_revision",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }
}

/// The incomplete prerequisites standing between a deliverable and a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerChain {
    /// The deliverable asked about.
    pub id: String,
    /// Its direct prerequisites that are not `Complete`, in declaration order.
    pub waiting_on: Vec<String>,
    /// Incomplete ancestors with no incomplete prerequisite of their own:
    /// the work that can move now. The driving root (the end of `chain`)
    /// comes first; the rest follow by earliest finish descending, then id.
    pub roots: Vec<String>,
    /// The driving chain: `id`, then at each step the incomplete
    /// prerequisite with the latest earliest finish, ending at `roots[0]`.
    pub chain: Vec<String>,
    /// Class of the driving root (`roots[0]`).
    pub class: BlockerClass,
}

/// Lookup tables over one snapshot's graph and status, built once and shared
/// by the overview and `plan.explain`.
pub struct PlanIndex<'a> {
    graph: &'a PlanGraph,
    pos: HashMap<&'a str, usize>,
    statuses: HashMap<&'a str, &'a DeliverableStatus>,
    counters: HashMap<&'a str, (u32, u32, u32)>,
    schedule: HashMap<&'a str, &'a ScheduleRow>,
    dependents: HashMap<&'a str, Vec<&'a str>>,
    /// Milestone rows in ascending earliest finish, then id.
    milestones: Vec<&'a MilestoneRow>,
    /// Per milestone id: the milestone and every ancestor.
    milestone_ancestors: HashMap<&'a str, HashSet<&'a str>>,
    /// Evidence status per deliverable, when attached with
    /// [`PlanIndex::with_evidence`].
    evidence: HashMap<String, DeliverableEvidenceStatus>,
}

impl<'a> PlanIndex<'a> {
    /// Index `graph` (the head graph) against `status` (the same plan's
    /// `plan.status`).
    pub fn new(graph: &'a PlanGraph, status: &'a PlanStatus) -> Self {
        let pos = graph
            .deliverables
            .iter()
            .enumerate()
            .map(|(i, d)| (d.id.as_str(), i))
            .collect();
        let statuses = status
            .deliverables
            .iter()
            .map(|row| (row.0.as_str(), &row.1))
            .collect();
        let counters = status
            .deliverables
            .iter()
            .map(|row| (row.0.as_str(), (row.2, row.3, row.4)))
            .collect();
        let schedule = status
            .schedule
            .iter()
            .map(|row| (row.id.as_str(), row))
            .collect();
        let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
        for d in &graph.deliverables {
            for p in &d.prerequisites {
                dependents.entry(p.id()).or_default().push(d.id.as_str());
            }
        }
        let mut milestones: Vec<&MilestoneRow> = status.milestones.iter().collect();
        milestones.sort_by(|a, b| a.hours.total_cmp(&b.hours).then_with(|| a.id.cmp(&b.id)));
        let mut index = Self {
            graph,
            pos,
            statuses,
            counters,
            schedule,
            dependents,
            milestones,
            milestone_ancestors: HashMap::new(),
            evidence: HashMap::new(),
        };
        let ancestors: HashMap<&str, HashSet<&str>> = index
            .milestones
            .iter()
            .map(|m| (m.id.as_str(), index.ancestors_inclusive(&m.id)))
            .collect();
        index.milestone_ancestors = ancestors;
        index
    }

    /// Attach each deliverable's evidence status (from
    /// [`crate::evidence::plan_evidence_status`]); [`PlanIndex::item_ref`]
    /// then carries it.
    pub fn with_evidence(mut self, statuses: Vec<DeliverableEvidenceStatus>) -> Self {
        self.evidence = statuses
            .into_iter()
            .map(|s| (s.deliverable_id.clone(), s))
            .collect();
        self
    }

    /// The attached evidence status of `id`.
    pub fn evidence(&self, id: &str) -> Option<&DeliverableEvidenceStatus> {
        self.evidence.get(id)
    }

    /// The deliverable with `id`, if it is in the graph.
    pub fn deliverable(&self, id: &str) -> Option<&'a Deliverable> {
        self.pos.get(id).map(|&i| &self.graph.deliverables[i])
    }

    /// Live status of `id` (`Pending` when unknown).
    pub fn status(&self, id: &str) -> &'a DeliverableStatus {
        const PENDING: &DeliverableStatus = &DeliverableStatus::Pending;
        self.statuses.get(id).copied().unwrap_or(PENDING)
    }

    /// `(attempt_count, failure_count, lapse_count)` of `id`.
    pub fn counters(&self, id: &str) -> (u32, u32, u32) {
        self.counters.get(id).copied().unwrap_or_default()
    }

    /// The CPM schedule row of `id`.
    pub fn schedule(&self, id: &str) -> Option<&'a ScheduleRow> {
        self.schedule.get(id).copied()
    }

    pub fn is_complete(&self, id: &str) -> bool {
        matches!(self.status(id), DeliverableStatus::Complete)
    }

    /// Human-readable title: `metadata.title`, else `metadata.name`, else
    /// `metadata.description`, else `metadata.artifact`, else the id.
    pub fn title(&self, id: &str) -> String {
        self.deliverable(id)
            .and_then(|d| {
                ["title", "name", "description", "artifact"]
                    .iter()
                    .find_map(|key| meta_str(d, key))
            })
            .map_or_else(|| id.to_string(), str::to_string)
    }

    /// `metadata.owner`, when a string.
    pub fn owner(&self, id: &str) -> Option<String> {
        self.deliverable(id)
            .and_then(|d| meta_str(d, "owner"))
            .map(str::to_string)
    }

    /// `metadata.route`, when a string.
    pub fn route(&self, id: &str) -> Option<String> {
        self.deliverable(id)
            .and_then(|d| meta_str(d, "route"))
            .map(str::to_string)
    }

    /// The kind of `id` (see [`DeliverableKind`]); `Work` when unknown.
    pub fn kind(&self, id: &str) -> DeliverableKind {
        let Some(d) = self.deliverable(id) else {
            return DeliverableKind::Work;
        };
        match meta_str(d, "kind") {
            Some("gate") => DeliverableKind::Gate,
            Some("manual") => DeliverableKind::Manual,
            Some("external") => DeliverableKind::External,
            _ if d
                .metadata
                .get("external")
                .and_then(serde_json::Value::as_bool)
                == Some(true) =>
            {
                DeliverableKind::External
            }
            _ if d.is_milestone() => DeliverableKind::Milestone,
            _ => DeliverableKind::Work,
        }
    }

    /// Whether `plan.acquire_cohort` never leases `id` (manual or gate); such
    /// work is closed with `plan.accept`.
    pub fn is_accept_only(&self, id: &str) -> bool {
        matches!(
            self.kind(id),
            DeliverableKind::Manual | DeliverableKind::Gate
        )
    }

    /// How a ready `id` is started: `Operator` at the failure or lapse cap
    /// (`acquire_cohort` skips it), `Accept` for manual or gate work, else
    /// `Acquire`.
    pub fn ready_action(&self, id: &str) -> ReadyAction {
        let (_, failures, lapses) = self.counters(id);
        if failures >= MAX_ATTEMPTS || lapses >= MAX_LAPSES {
            ReadyAction::Operator
        } else if self.is_accept_only(id) {
            ReadyAction::Accept
        } else {
            ReadyAction::Acquire
        }
    }

    /// The common entry fields of `id`.
    pub fn item_ref(&self, id: &str) -> ItemRef {
        ItemRef {
            id: id.to_string(),
            title: self.title(id),
            status: StatusLabel::from(self.status(id)),
            kind: self.kind(id),
            owner: self.owner(id),
            route: self.route(id),
            evidence: self.evidence(id).cloned(),
        }
    }

    /// `{id, title}` of `id`.
    pub fn named(&self, id: &str) -> Named {
        Named {
            id: id.to_string(),
            title: self.title(id),
        }
    }

    /// Direct prerequisites of `id` that are not `Complete`, in declaration
    /// order.
    pub fn incomplete_prerequisites(&self, id: &str) -> Vec<&'a str> {
        self.deliverable(id)
            .map(|d| {
                d.prerequisites
                    .iter()
                    .map(|p| p.id())
                    .filter(|p| !self.is_complete(p))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Ids that list `id` as a prerequisite, in graph order.
    pub fn dependents(&self, id: &str) -> &[&'a str] {
        self.dependents.get(id).map_or(&[], Vec::as_slice)
    }

    /// Why `id` cannot start yet: its incomplete prerequisites, the root
    /// blockers and the driving chain. `None` when every prerequisite is
    /// complete (or `id` is unknown).
    pub fn blocker_chain(&self, id: &str) -> Option<BlockerChain> {
        let waiting_on = self.incomplete_prerequisites(id);
        if waiting_on.is_empty() {
            return None;
        }
        // Every incomplete ancestor reachable through incomplete edges.
        let mut seen: HashSet<&str> = HashSet::new();
        let mut roots: Vec<&str> = Vec::new();
        let mut stack: Vec<&str> = waiting_on.clone();
        while let Some(n) = stack.pop() {
            if !seen.insert(n) {
                continue;
            }
            let up = self.incomplete_prerequisites(n);
            if up.is_empty() {
                roots.push(n);
            } else {
                stack.extend(up);
            }
        }
        roots.sort_by(|a, b| self.driving_order(a, b));
        let mut chain = vec![id.to_string()];
        let mut cursor = self.driving_pick(&waiting_on);
        // Bounded by the graph's depth; `seen` guards a malformed cycle.
        let mut visited: HashSet<&str> = HashSet::new();
        while let Some(n) = cursor {
            if !visited.insert(n) {
                break;
            }
            chain.push(n.to_string());
            cursor = self.driving_pick(&self.incomplete_prerequisites(n));
        }
        // The driving root leads, so `roots[0]` and `class` describe the end
        // of `chain` rather than merely the latest-finishing root.
        if let Some(driver) = chain.last()
            && let Some(at) = roots.iter().position(|r| r == driver)
        {
            let driver = roots.remove(at);
            roots.insert(0, driver);
        }
        let class = roots
            .first()
            .map_or(BlockerClass::Dependency, |r| self.kind(r).into());
        Some(BlockerChain {
            id: id.to_string(),
            waiting_on: waiting_on.into_iter().map(str::to_string).collect(),
            roots: roots.into_iter().map(str::to_string).collect(),
            chain,
            class,
        })
    }

    /// Latest earliest finish first, then id.
    fn driving_order(&self, a: &str, b: &str) -> std::cmp::Ordering {
        let ef = |id: &str| self.schedule(id).map_or(0.0, |r| r.ef);
        ef(b).total_cmp(&ef(a)).then_with(|| a.cmp(b))
    }

    fn driving_pick(&self, ids: &[&'a str]) -> Option<&'a str> {
        ids.iter().copied().min_by(|a, b| self.driving_order(a, b))
    }

    /// `id` and every deliverable it waits on, directly or transitively.
    fn ancestors_inclusive(&self, id: &'a str) -> HashSet<&'a str> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::from([id]);
        while let Some(n) = queue.pop_front() {
            if seen.insert(n)
                && let Some(d) = self.deliverable(n)
            {
                queue.extend(d.prerequisites.iter().map(|p| p.id()));
            }
        }
        seen
    }

    /// Incomplete milestones that `id` feeds (is, or is an ancestor of), in
    /// ascending earliest finish.
    pub fn downstream_milestones(&self, id: &str) -> Vec<&'a str> {
        self.milestones
            .iter()
            .filter(|m| !m.complete)
            .filter(|m| {
                self.milestone_ancestors
                    .get(m.id.as_str())
                    .is_some_and(|set| set.contains(id))
            })
            .map(|m| m.id.as_str())
            .collect()
    }

    /// The first incomplete milestone `id` feeds: the earliest-finishing one.
    pub fn next_milestone(&self, id: &str) -> Option<&'a str> {
        self.downstream_milestones(id).into_iter().next()
    }

    /// Whether `id` is `milestone` or one of its ancestors.
    pub fn feeds(&self, id: &str, milestone: &str) -> bool {
        self.milestone_ancestors
            .get(milestone)
            .is_some_and(|set| set.contains(id))
    }

    /// Dependents of `id` whose only incomplete prerequisite is `id`: what
    /// completing `id` makes eligible for a lease. Graph order.
    pub fn unlocks(&self, id: &str) -> Vec<&'a str> {
        self.dependents(id)
            .iter()
            .copied()
            .filter(|dep| !self.is_complete(dep))
            .filter(|dep| self.incomplete_prerequisites(dep) == [id])
            .collect()
    }
}

fn meta_str<'d>(d: &'d Deliverable, key: &str) -> Option<&'d str> {
    d.metadata
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Response (JSON v1)
// ---------------------------------------------------------------------------

/// One plan overview. Serialises as the stable [`OVERVIEW_SCHEMA`] JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    /// Always [`OVERVIEW_SCHEMA`].
    pub schema: String,
    /// The single instant every section is computed at.
    pub as_of: DateTime<Utc>,
    pub detail: Detail,
    /// The filter applied to the lists, echoed back; omitted when none.
    #[serde(default, skip_serializing_if = "OverviewFilter::is_empty")]
    pub filter: OverviewFilter,
    pub plan: PlanIdentity,
    pub counts: StatusCounts,
    pub ev: EvSection,
    /// Hours along the critical path (`plan.status` `critical_path_hours`).
    pub critical_path_hours: f32,
    /// The critical path in execution order, synthetic endpoints excluded.
    pub critical_path: Section<PathEntry>,
    pub milestones: Section<MilestoneEntry>,
    /// `plan.status` `ready`, in its order (least float first).
    pub ready: Section<ReadyEntry>,
    /// Incomplete deliverables waiting on an incomplete prerequisite, least
    /// float first.
    pub blocked: Section<BlockedEntry>,
    /// Every lease `plan.status` reports, live or expired at `as_of`.
    pub leases: Section<LeaseEntry>,
    /// Failed, superseded, circuit-broken or lapsing deliverables.
    pub attention: Section<AttentionEntry>,
    /// Deliverables per evidence verdict, over the whole plan.
    pub evidence_counts: EvidenceCounts,
    /// Every deliverable whose evidence verdict is `stale`, complete ones
    /// first, then by id.
    pub stale_evidence: Section<ItemRef>,
}

/// Plan identity, from `plan.status` and the head revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanIdentity {
    pub plan_id: PlanId,
    /// Plan line; `null` for an unnamed plan.
    pub name: Option<String>,
    pub variant: Option<String>,
    pub revision: u32,
    pub selected: Option<bool>,
    pub archived: Option<bool>,
    /// For an archived variant: the project's active line, when one.
    pub active_plan: Option<String>,
    /// `plan.status` `definition_drift`: `null` means unknown.
    pub definition_drift: Option<bool>,
    /// Latest EV baseline; `null` before the first.
    pub baseline: Option<PlanBaseline>,
    pub plan_complete: bool,
}

/// Deliverables per status, over the whole plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCounts {
    pub total: usize,
    pub pending: usize,
    pub ready: usize,
    pub in_progress: usize,
    pub superseded_revision: usize,
    pub complete: usize,
    pub failed: usize,
    pub milestones: usize,
    pub milestones_complete: usize,
    /// Leases held, live or expired.
    pub leases: usize,
}

/// Earned value at `as_of`, or why there is none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EvSection {
    /// No baseline yet: `plan.ev` would return `NOT_BASELINED`.
    NotBaselined { reason: String },
    /// The `plan.ev` headline; per-row figures stay in `plan.ev`.
    Baselined(EvHeadline),
}

/// The headline numbers of `plan.ev` at the overview's `as_of`. A `null`
/// ratio is explained by an `undefined` entry naming it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvHeadline {
    pub baseline_number: u32,
    pub backfilled: bool,
    pub bac: f32,
    pub pv: f32,
    pub ev: f32,
    pub ac: f32,
    pub sv: f32,
    pub cv: f32,
    pub spi: Option<f32>,
    pub cpi: Option<f32>,
    pub eac: Option<f32>,
    pub etc: Option<f32>,
    pub vac: Option<f32>,
    pub tcpi: Option<f32>,
    pub undefined: Vec<Undefined>,
    pub alerts: Vec<String>,
    pub critical_float_consumed_hours: f32,
    /// Live deliverables the baseline does not cover (see `plan.ev`).
    pub excluded_unbaselined: usize,
}

impl EvHeadline {
    fn from_report(r: &EvReport) -> Self {
        Self {
            baseline_number: r.baseline_number,
            backfilled: r.backfilled,
            bac: r.bac,
            pv: r.pv,
            ev: r.ev,
            ac: r.ac,
            sv: r.sv,
            cv: r.cv,
            spi: r.spi,
            cpi: r.cpi,
            eac: r.eac,
            etc: r.etc,
            vac: r.vac,
            tcpi: r.tcpi,
            undefined: r.undefined.clone(),
            alerts: r.alerts.clone(),
            critical_float_consumed_hours: r.critical_float_consumed_hours,
            excluded_unbaselined: r.excluded_unbaselined.len(),
        }
    }
}

/// A bounded list: at most `limit` entries, with the size before truncation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section<T> {
    /// Entries matching the filter, before truncation.
    pub total: usize,
    /// `total - items.len()`.
    pub omitted: usize,
    pub items: Vec<T>,
}

impl<T> Section<T> {
    /// The first `limit` of `all`, with `total` and `omitted` set.
    pub fn bounded(all: Vec<T>, limit: usize) -> Self {
        let total = all.len();
        let items: Vec<T> = all.into_iter().take(limit).collect();
        Self {
            total,
            omitted: total - items.len(),
            items,
        }
    }
}

/// Compact text of a deliverable's evidence status: the verdict, then the
/// changed fields for `stale` or the unknown fields for `unknown`, e.g.
/// `evidence: stale (dependency_lock, definition)`. `None` for `none`.
pub fn evidence_text(status: &DeliverableEvidenceStatus) -> Option<String> {
    let fields = |names: Vec<&str>| {
        if names.is_empty() {
            String::new()
        } else {
            format!(" ({})", names.join(", "))
        }
    };
    let record = status.record.as_ref();
    let text = match status.verdict {
        Verdict::None => return None,
        Verdict::Current => "current".to_string(),
        Verdict::Unbound => "unbound".to_string(),
        Verdict::Stale => format!(
            "stale{}",
            fields(
                record
                    .map(|r| r.changed.iter().map(|c| c.field.as_str()).collect())
                    .unwrap_or_default()
            )
        ),
        Verdict::Unknown => format!(
            "unknown{}",
            fields(
                record
                    .map(|r| r.unknown.iter().map(|f| f.as_str()).collect())
                    .unwrap_or_default()
            )
        ),
    };
    Some(format!("evidence: {text}"))
}

/// Deliverables per evidence verdict, over the whole plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceCounts {
    pub current: usize,
    pub stale: usize,
    pub unknown: usize,
    pub unbound: usize,
    pub none: usize,
}

impl EvidenceCounts {
    fn of(statuses: &[DeliverableEvidenceStatus]) -> Self {
        let mut c = Self::default();
        for s in statuses {
            match s.verdict {
                Verdict::Current => c.current += 1,
                Verdict::Stale => c.stale += 1,
                Verdict::Unknown => c.unknown += 1,
                Verdict::Unbound => c.unbound += 1,
                Verdict::None => c.none += 1,
            }
        }
        c
    }
}

/// The fields every deliverable entry starts with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemRef {
    pub id: String,
    pub title: String,
    pub status: StatusLabel,
    pub kind: DeliverableKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// The deliverable's evidence status, in the `docs/evidence.md` shape
    /// (verdict `current`, `stale`, `unknown`, `unbound` or `none`, with the
    /// governing record's changed and unknown fields). Always set by the
    /// overview; omitted where evidence is not read (`plan.explain`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<DeliverableEvidenceStatus>,
}

/// A short reference: id and title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Named {
    pub id: String,
    pub title: String,
}

/// One critical-path deliverable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathEntry {
    #[serde(flatten)]
    pub item: ItemRef,
    /// Scheduled length in hours (`ef - es`).
    pub hours: f32,
    /// `hours` less earned progress: 0 when complete; scaled by the EV
    /// row's earned percent when baselined.
    pub remaining_hours: f32,
    /// Incomplete milestones this deliverable feeds, earliest first: its
    /// milestone impact. A slip here moves each of them.
    pub feeds_milestones: Vec<Named>,
    /// Earliest start and finish, hours from plan start (full detail only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub es: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ef: Option<f32>,
}

/// One milestone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MilestoneEntry {
    #[serde(flatten)]
    pub item: ItemRef,
    /// Earliest finish, hours from plan start (`plan.status`).
    pub hours: f32,
    pub complete: bool,
    /// On the plan's critical path.
    pub critical: bool,
    /// Incomplete deliverables on the milestone's own critical path.
    pub remaining_on_path: usize,
    /// The first incomplete deliverable on that path: its next action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_on_path: Option<Named>,
}

/// How to start a ready deliverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyAction {
    /// Lease it with `plan.acquire_cohort`.
    Acquire,
    /// Manual or gate work: do it, then close it with `plan.accept`.
    Accept,
    /// At the failure or lapse cap: an operator resets it with
    /// `plan.force_release` (`reset_counters`).
    Operator,
}

/// A ready deliverable two candidates cannot both hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileConflict {
    pub path: String,
    /// The other ready deliverable or lease holder claiming `path`.
    pub with: String,
    /// True when `with` holds a lease now (this one cannot be acquired until
    /// it ends); false for another ready candidate (not both in one cohort).
    pub leased: bool,
}

/// One ready deliverable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadyEntry {
    #[serde(flatten)]
    pub item: ItemRef,
    pub action: ReadyAction,
    pub float: f32,
    pub critical: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_milestone: Option<Named>,
    /// Owned-file conflicts with leases and other ready work, by
    /// `plan.acquire_cohort`'s rule (exclusive conflicts with anything;
    /// append shares with append).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_conflicts: Vec<FileConflict>,
    /// Owned paths (full detail only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owned_files: Vec<String>,
}

/// A root blocker of a blocked deliverable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RootBlocker {
    #[serde(flatten)]
    pub item: ItemRef,
    pub class: BlockerClass,
}

/// One blocked deliverable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockedEntry {
    #[serde(flatten)]
    pub item: ItemRef,
    /// Class of the driving root blocker.
    pub class: BlockerClass,
    pub float: f32,
    /// Direct incomplete prerequisites.
    pub waiting_on: Vec<Named>,
    /// Work that can move now to unblock it, driving blocker first.
    pub root_blockers: Vec<RootBlocker>,
    /// Root blockers not listed.
    pub root_blockers_omitted: usize,
    /// The driving chain of ids, this one first (full detail only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_milestone: Option<Named>,
}

/// One lease from `plan.status` `locks_held`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseEntry {
    #[serde(flatten)]
    pub item: ItemRef,
    pub caller_id: String,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// `expires_at <= as_of`: the next mutating call reaps it.
    pub expired: bool,
    /// Seconds from `as_of` to `expires_at` (negative once expired).
    pub seconds_remaining: i64,
}

/// Why a deliverable needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AttentionCode {
    /// Marked failed; re-acquirable until the failure cap.
    Failed,
    /// `failure_count` reached `MAX_ATTEMPTS`: circuit-broken.
    FailureLimit,
    /// `lapse_count` reached `MAX_LAPSES`: never leased again until reset.
    LapseLimit,
    /// A forced revision invalidated its lease; needs a fresh one.
    SupersededRevision,
    /// Incomplete and has lost at least one lease to TTL expiry.
    Lapsed,
}

/// One deliverable needing attention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttentionEntry {
    #[serde(flatten)]
    pub item: ItemRef,
    pub code: AttentionCode,
    /// The failure reason, for `FAILED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub attempt_count: u32,
    pub failure_count: u32,
    pub lapse_count: u32,
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// Build the overview of `snapshot` (pure: same input, same output).
/// `INVALID_VALUE` for a bad limit or a milestone filter naming a
/// non-milestone; `DELIVERABLE_NOT_FOUND` when it names no deliverable.
pub fn build_overview(
    snapshot: &PlanSnapshot,
    options: &OverviewOptions,
) -> Result<Overview, PlannerError> {
    options.validate()?;
    let limit = options.effective_limit()?;
    let full = options.detail == Detail::Full;
    let status = &snapshot.status;
    let evidence = crate::evidence::plan_evidence_status(
        &snapshot.graph,
        &snapshot.evidence,
        options.evidence_target.as_ref(),
    );
    let evidence_counts = EvidenceCounts::of(&evidence);
    let index = PlanIndex::new(&snapshot.graph, status).with_evidence(evidence);
    if let Some(m) = &options.filter.milestone {
        match index.deliverable(m) {
            None => {
                return Err(PlannerError::DeliverableNotFound {
                    plan_id: snapshot.plan_id.0.clone(),
                    deliverable_id: m.clone(),
                });
            }
            Some(d) if !d.is_milestone() => {
                return Err(PlannerError::InvalidValue {
                    reason: format!("filter milestone '{m}' is not a milestone"),
                });
            }
            Some(_) => {}
        }
    }
    let keep = |id: &str| {
        options
            .filter
            .owner
            .as_ref()
            .is_none_or(|o| index.owner(id).as_deref() == Some(o.as_str()))
            && options
                .filter
                .milestone
                .as_ref()
                .is_none_or(|m| index.feeds(id, m))
    };
    let item = |id: &str| index.item_ref(id);
    let named = |id: &str| index.named(id);
    let feeds_cap = if full { limit } else { BRIEF_FEEDS };
    let float_of = |id: &str| index.schedule(id).map_or(0.0, |r| r.float);
    let critical_set: HashSet<&str> = status.critical_ids.iter().map(String::as_str).collect();
    let earned: HashMap<&str, f32> = snapshot
        .ev
        .iter()
        .flat_map(|r| r.rows.iter())
        .map(|row| (row.id.as_str(), row.earned_pct))
        .collect();

    // Critical path.
    let critical_path: Vec<PathEntry> = status
        .critical_path
        .iter()
        .map(String::as_str)
        .filter(|id| *id != START_ID && *id != FINISH_ID)
        .filter(|id| keep(id))
        .map(|id| {
            let row = index.schedule(id);
            let hours = row.map_or(0.0, |r| r.ef - r.es);
            let remaining_hours = if index.is_complete(id) {
                0.0
            } else {
                hours * (1.0 - earned.get(id).copied().unwrap_or(0.0) / 100.0)
            };
            PathEntry {
                item: item(id),
                hours,
                remaining_hours,
                feeds_milestones: index
                    .downstream_milestones(id)
                    .into_iter()
                    .take(feeds_cap)
                    .map(named)
                    .collect(),
                es: row.filter(|_| full).map(|r| r.es),
                ef: row.filter(|_| full).map(|r| r.ef),
            }
        })
        .collect();

    // Milestones.
    let milestones: Vec<MilestoneEntry> = status
        .milestones
        .iter()
        .filter(|m| keep(&m.id))
        .map(|m| {
            let incomplete: Vec<&str> = m
                .critical_path
                .iter()
                .map(String::as_str)
                .filter(|id| *id != START_ID && *id != FINISH_ID && !index.is_complete(id))
                .collect();
            MilestoneEntry {
                item: item(&m.id),
                hours: m.hours,
                complete: m.complete,
                critical: critical_set.contains(m.id.as_str()),
                remaining_on_path: incomplete.len(),
                next_on_path: incomplete.first().map(|id| named(id)),
            }
        })
        .collect();

    // Ready queue, with owned-file conflicts.
    let ready_ids: Vec<&str> = status.ready.iter().map(String::as_str).collect();
    let lease_of: HashMap<&str, &LockInfo> = status
        .locks_held
        .iter()
        .map(|l| (l.deliverable_id.as_str(), l))
        .collect();
    let conflicts = file_conflicts(&index, &ready_ids, &lease_of);
    let ready: Vec<ReadyEntry> = ready_ids
        .iter()
        .copied()
        .filter(|id| keep(id))
        .map(|id| {
            let action = index.ready_action(id);
            ReadyEntry {
                item: item(id),
                action,
                float: float_of(id),
                critical: critical_set.contains(id),
                next_milestone: index.next_milestone(id).map(named),
                file_conflicts: conflicts
                    .get(id)
                    .map(|v| v.iter().take(limit).cloned().collect())
                    .unwrap_or_default(),
                owned_files: if full {
                    index
                        .deliverable(id)
                        .map(|d| d.owned_files.iter().map(path_string).collect())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                },
            }
        })
        .collect();

    // Blocked: incomplete, not leased, waiting on an incomplete prerequisite.
    let root_cap = if full { limit } else { BRIEF_ROOT_BLOCKERS };
    let mut blocked_chains: Vec<BlockerChain> = snapshot
        .graph
        .deliverables
        .iter()
        .map(|d| d.id.as_str())
        .filter(|id| !index.is_complete(id) && !lease_of.contains_key(id) && keep(id))
        .filter_map(|id| index.blocker_chain(id))
        .collect();
    blocked_chains.sort_by(|a, b| {
        let key = |id: &str| index.schedule(id).map_or((0.0, 0.0), |r| (r.float, r.ls));
        let (fa, la) = key(&a.id);
        let (fb, lb) = key(&b.id);
        fa.total_cmp(&fb)
            .then_with(|| la.total_cmp(&lb))
            .then_with(|| a.id.cmp(&b.id))
    });
    let blocked_total = blocked_chains.len();
    let blocked_items: Vec<BlockedEntry> = blocked_chains
        .into_iter()
        .take(limit)
        .map(|c| BlockedEntry {
            item: item(&c.id),
            class: c.class,
            float: float_of(&c.id),
            waiting_on: c.waiting_on.iter().map(|id| named(id)).collect(),
            root_blockers: c
                .roots
                .iter()
                .take(root_cap)
                .map(|id| RootBlocker {
                    item: item(id),
                    class: index.kind(id).into(),
                })
                .collect(),
            root_blockers_omitted: c.roots.len().saturating_sub(root_cap),
            chain: if full { c.chain.clone() } else { Vec::new() },
            next_milestone: index.next_milestone(&c.id).map(named),
        })
        .collect();
    let blocked = Section {
        total: blocked_total,
        omitted: blocked_total - blocked_items.len(),
        items: blocked_items,
    };

    // Leases.
    let mut locks: Vec<&LockInfo> = status
        .locks_held
        .iter()
        .filter(|l| keep(&l.deliverable_id))
        .collect();
    locks.sort_by(|a, b| {
        a.expires_at
            .cmp(&b.expires_at)
            .then_with(|| a.deliverable_id.cmp(&b.deliverable_id))
    });
    let leases: Vec<LeaseEntry> = locks
        .into_iter()
        .map(|l| LeaseEntry {
            item: item(&l.deliverable_id),
            caller_id: l.caller_id.0.clone(),
            acquired_at: l.acquired_at,
            expires_at: l.expires_at,
            expired: l.expires_at <= snapshot.as_of,
            seconds_remaining: (l.expires_at - snapshot.as_of).num_seconds(),
        })
        .collect();

    // Attention.
    let attention: Vec<AttentionEntry> = status
        .deliverables
        .iter()
        .filter(|row| keep(&row.0))
        .filter_map(|(id, st, attempts, failures, lapses)| {
            let (code, reason) = match st {
                DeliverableStatus::Complete => return None,
                _ if *failures >= MAX_ATTEMPTS => (AttentionCode::FailureLimit, None),
                _ if *lapses >= MAX_LAPSES => (AttentionCode::LapseLimit, None),
                DeliverableStatus::Failed { reason } => {
                    (AttentionCode::Failed, Some(reason.clone()))
                }
                DeliverableStatus::SupersededRevision => (AttentionCode::SupersededRevision, None),
                _ if *lapses > 0 => (AttentionCode::Lapsed, None),
                _ => return None,
            };
            Some(AttentionEntry {
                item: item(id),
                code,
                reason,
                attempt_count: *attempts,
                failure_count: *failures,
                lapse_count: *lapses,
            })
        })
        .collect();

    // Stale evidence: complete first, then by id (docs/evidence.md).
    let mut stale: Vec<&str> = snapshot
        .graph
        .deliverables
        .iter()
        .map(|d| d.id.as_str())
        .filter(|id| {
            index
                .evidence(id)
                .is_some_and(|e| e.verdict == Verdict::Stale)
                && keep(id)
        })
        .collect();
    stale.sort_by(|a, b| {
        index
            .is_complete(b)
            .cmp(&index.is_complete(a))
            .then_with(|| a.cmp(b))
    });
    let stale_evidence: Vec<ItemRef> = stale.into_iter().map(item).collect();

    Ok(Overview {
        schema: OVERVIEW_SCHEMA.to_string(),
        as_of: snapshot.as_of,
        detail: options.detail,
        filter: options.filter.clone(),
        plan: PlanIdentity {
            plan_id: snapshot.plan_id.clone(),
            name: status.name.clone(),
            variant: status.variant.clone(),
            revision: snapshot.revision,
            selected: status.selected,
            archived: status.archived,
            active_plan: status.active_plan.clone(),
            definition_drift: status.definition_drift,
            baseline: status.baseline.clone(),
            plan_complete: status.plan_complete,
        },
        counts: counts(status),
        ev: match &snapshot.ev {
            Some(r) => EvSection::Baselined(EvHeadline::from_report(r)),
            None => EvSection::NotBaselined {
                reason: "NOT_BASELINED: no baseline yet; take one with plan.baseline".to_string(),
            },
        },
        critical_path_hours: status.critical_path_hours,
        critical_path: Section::bounded(critical_path, limit),
        milestones: Section::bounded(milestones, limit),
        ready: Section::bounded(ready, limit),
        blocked,
        leases: Section::bounded(leases, limit),
        attention: Section::bounded(attention, limit),
        evidence_counts,
        stale_evidence: Section::bounded(stale_evidence, limit),
    })
}

fn counts(status: &PlanStatus) -> StatusCounts {
    let mut c = StatusCounts {
        total: status.deliverables.len(),
        milestones: status.milestones.len(),
        milestones_complete: status.milestones.iter().filter(|m| m.complete).count(),
        leases: status.locks_held.len(),
        ..StatusCounts::default()
    };
    for row in &status.deliverables {
        match StatusLabel::from(&row.1) {
            StatusLabel::Pending => c.pending += 1,
            StatusLabel::Ready => c.ready += 1,
            StatusLabel::InProgress => c.in_progress += 1,
            StatusLabel::SupersededRevision => c.superseded_revision += 1,
            StatusLabel::Complete => c.complete += 1,
            StatusLabel::Failed => c.failed += 1,
        }
    }
    c
}

fn path_string(f: &OwnedFile) -> String {
    f.path().to_string_lossy().into_owned()
}

/// Per ready id: its owned-file conflicts with held leases and with the
/// other ready candidates, by path then other id.
fn file_conflicts(
    index: &PlanIndex<'_>,
    ready: &[&str],
    leases: &HashMap<&str, &LockInfo>,
) -> HashMap<String, Vec<FileConflict>> {
    // path -> [(holder id, mode, leased)]
    let mut claims: HashMap<String, Vec<(&str, crate::plan::FileMode, bool)>> = HashMap::new();
    let holders = leases
        .keys()
        .map(|id| (*id, true))
        .chain(ready.iter().map(|id| (*id, false)));
    for (id, leased) in holders {
        if let Some(d) = index.deliverable(id) {
            for f in &d.owned_files {
                claims
                    .entry(path_string(f))
                    .or_default()
                    .push((id, f.mode(), leased));
            }
        }
    }
    let mut out: HashMap<String, Vec<FileConflict>> = HashMap::new();
    for &id in ready {
        let Some(d) = index.deliverable(id) else {
            continue;
        };
        let mut found: Vec<FileConflict> = Vec::new();
        for f in &d.owned_files {
            let path = path_string(f);
            for (other, mode, leased) in claims.get(&path).into_iter().flatten() {
                if *other != id && crate::locks::modes_conflict(f.mode(), *mode) {
                    found.push(FileConflict {
                        path: path.clone(),
                        with: (*other).to_string(),
                        leased: *leased,
                    });
                }
            }
        }
        found.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.with.cmp(&b.with)));
        found.dedup();
        if !found.is_empty() {
            out.insert(id.to_string(), found);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn fmt_opt(v: Option<f32>) -> String {
    v.map_or_else(|| "n/a".to_string(), |x| format!("{x:.2}"))
}

/// Markdown table cell: pipes escaped, newlines flattened.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Title and id, then the evidence text when there is any evidence.
fn label(item: &ItemRef) -> String {
    let name = if item.title == item.id {
        format!("`{}`", item.id)
    } else {
        format!("{} (`{}`)", cell(&item.title), item.id)
    };
    match item.evidence.as_ref().and_then(evidence_text) {
        Some(text) => format!("{name} · {text}"),
        None => name,
    }
}

fn named_label(n: &Named) -> String {
    if n.title == n.id {
        format!("`{}`", n.id)
    } else {
        format!("{} (`{}`)", cell(&n.title), n.id)
    }
}

fn omitted_line<T>(out: &mut String, s: &Section<T>) {
    if s.omitted > 0 {
        out.push_str(&format!(
            "\n_{} more not shown; raise `limit` or filter._\n",
            s.omitted
        ));
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

/// Render `o` as compact Markdown: a header line, counts, EV, then one short
/// table per non-empty section. Deterministic for a given overview.
pub fn render_markdown(o: &Overview) -> String {
    let mut out = String::new();
    let p = &o.plan;
    let name = match (&p.name, &p.variant) {
        (Some(n), Some(v)) => format!("{n}/{v}"),
        _ => p.plan_id.0.clone(),
    };
    out.push_str(&format!("# Plan overview: {}\n\n", cell(&name)));
    let drift = match p.definition_drift {
        Some(true) => "drifted",
        Some(false) => "in sync",
        None => "unknown",
    };
    let baseline = p
        .baseline
        .as_ref()
        .map_or_else(|| "none".to_string(), |b| format!("#{}", b.baseline_number));
    out.push_str(&format!(
        "`{}` rev {} · as of {} · definition {} · baseline {}{}\n\n",
        p.plan_id.0,
        p.revision,
        o.as_of.to_rfc3339(),
        drift,
        baseline,
        if p.archived == Some(true) {
            " · archived"
        } else {
            ""
        },
    ));
    let c = &o.counts;
    out.push_str(&format!(
        "{}/{} complete · {} ready · {} in progress · {} pending · {} failed · \
         milestones {}/{} · critical path {:.1}h\n",
        c.complete,
        c.total,
        c.ready,
        c.in_progress,
        c.pending,
        c.failed,
        c.milestones_complete,
        c.milestones,
        o.critical_path_hours
    ));
    let e = &o.evidence_counts;
    if e.none < c.total {
        out.push_str(&format!(
            "evidence: {} current · {} stale · {} unknown · {} unbound · {} none\n",
            e.current, e.stale, e.unknown, e.unbound, e.none
        ));
    }
    if !o.filter.is_empty() {
        let mut parts = Vec::new();
        if let Some(owner) = &o.filter.owner {
            parts.push(format!("owner `{owner}`"));
        }
        if let Some(m) = &o.filter.milestone {
            parts.push(format!("feeds `{m}`"));
        }
        out.push_str(&format!("\nLists filtered to {}.\n", parts.join(", ")));
    }

    out.push_str("\n## Earned value\n\n");
    match &o.ev {
        EvSection::NotBaselined { reason } => out.push_str(&format!("{reason}\n")),
        EvSection::Baselined(e) => {
            out.push_str(&format!(
                "Baseline #{}: BAC {:.1} · PV {:.1} · EV {:.1} · AC {:.1} · SPI {} · CPI {} · \
                 EAC {} · ETC {}\n",
                e.baseline_number,
                e.bac,
                e.pv,
                e.ev,
                e.ac,
                fmt_opt(e.spi),
                fmt_opt(e.cpi),
                fmt_opt(e.eac),
                fmt_opt(e.etc)
            ));
            for u in &e.undefined {
                out.push_str(&format!("- {} undefined: {}\n", u.field, cell(&u.reason)));
            }
            for a in &e.alerts {
                out.push_str(&format!("- alert {a}\n"));
            }
        }
    }

    if !o.critical_path.items.is_empty() {
        out.push_str("\n## Critical path\n\n| Deliverable | Status | Owner | Left (h) | Feeds |\n|---|---|---|---|---|\n");
        for e in &o.critical_path.items {
            let feeds: Vec<String> = e
                .feeds_milestones
                .iter()
                .map(|m| format!("`{}`", m.id))
                .collect();
            out.push_str(&format!(
                "| {} | {} | {} | {:.1} | {} |\n",
                label(&e.item),
                e.item.status.as_str(),
                cell(e.item.owner.as_deref().unwrap_or("-")),
                e.remaining_hours,
                if feeds.is_empty() {
                    "-".to_string()
                } else {
                    feeds.join(", ")
                }
            ));
        }
        omitted_line(&mut out, &o.critical_path);
    }

    if !o.milestones.items.is_empty() {
        out.push_str("\n## Milestones\n\n| Milestone | Status | At (h) | Left on path | Next |\n|---|---|---|---|---|\n");
        for m in &o.milestones.items {
            out.push_str(&format!(
                "| {}{} | {} | {:.1} | {} | {} |\n",
                label(&m.item),
                if m.critical { " ★" } else { "" },
                m.item.status.as_str(),
                m.hours,
                m.remaining_on_path,
                m.next_on_path
                    .as_ref()
                    .map_or_else(|| "-".to_string(), named_label)
            ));
        }
        omitted_line(&mut out, &o.milestones);
    }

    if !o.ready.items.is_empty() {
        out.push_str("\n## Ready\n\n| Deliverable | Do | Float (h) | Owner | Next milestone | Conflicts |\n|---|---|---|---|---|---|\n");
        for r in &o.ready.items {
            let action = match r.action {
                ReadyAction::Acquire => "acquire",
                ReadyAction::Accept => "accept",
                ReadyAction::Operator => "operator reset",
            };
            let conflicts: Vec<String> = r
                .file_conflicts
                .iter()
                .map(|f| format!("`{}`{}", f.with, if f.leased { " (leased)" } else { "" }))
                .collect();
            out.push_str(&format!(
                "| {}{} | {} | {:.1} | {} | {} | {} |\n",
                label(&r.item),
                if r.critical { " ★" } else { "" },
                action,
                r.float,
                cell(r.item.owner.as_deref().unwrap_or("-")),
                r.next_milestone
                    .as_ref()
                    .map_or_else(|| "-".to_string(), |m| format!("`{}`", m.id)),
                if conflicts.is_empty() {
                    "-".to_string()
                } else {
                    conflicts.join(", ")
                }
            ));
        }
        omitted_line(&mut out, &o.ready);
    }

    if !o.blocked.items.is_empty() {
        out.push_str("\n## Blocked\n\n| Deliverable | Blocked by | Class | Waiting on |\n|---|---|---|---|\n");
        for b in &o.blocked.items {
            let mut roots: Vec<String> = b
                .root_blockers
                .iter()
                .map(|r| format!("`{}` ({})", r.item.id, r.item.status.as_str()))
                .collect();
            if b.root_blockers_omitted > 0 {
                roots.push(format!("+{}", b.root_blockers_omitted));
            }
            let waiting: Vec<String> = b.waiting_on.iter().map(|w| format!("`{}`", w.id)).collect();
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                label(&b.item),
                roots.join(", "),
                class_str(b.class),
                waiting.join(", ")
            ));
        }
        omitted_line(&mut out, &o.blocked);
    }

    if !o.leases.items.is_empty() {
        out.push_str(
            "\n## Leases\n\n| Deliverable | Holder | Expires | State |\n|---|---|---|---|\n",
        );
        for l in &o.leases.items {
            out.push_str(&format!(
                "| {} | `{}` | {} | {} |\n",
                label(&l.item),
                cell(&l.caller_id),
                l.expires_at.to_rfc3339(),
                if l.expired {
                    "expired".to_string()
                } else {
                    format!("{}m left", l.seconds_remaining / 60)
                }
            ));
        }
        omitted_line(&mut out, &o.leases);
    }

    if !o.attention.items.is_empty() {
        out.push_str("\n## Attention\n\n");
        for a in &o.attention.items {
            let code = serde_json::to_value(a.code)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {} {}: failures {}, lapses {}{}\n",
                code,
                label(&a.item),
                a.failure_count,
                a.lapse_count,
                a.reason
                    .as_deref()
                    .map_or_else(String::new, |r| format!(" ({})", cell(r)))
            ));
        }
        omitted_line(&mut out, &o.attention);
    }

    if !o.stale_evidence.items.is_empty() {
        out.push_str("\n## Stale evidence\n\n");
        for item in &o.stale_evidence.items {
            out.push_str(&format!("- {} ({})\n", label(item), item.status.as_str()));
        }
        omitted_line(&mut out, &o.stale_evidence);
    }
    out
}
