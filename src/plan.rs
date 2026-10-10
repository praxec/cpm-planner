//! SPEC §33 PA1 — `Planner` data model.
//!
//! This module defines the *types* the [`Planner`](crate::ports::Planner) trait
//! (see [`crate::ports::Planner`]) carries across the IP boundary. The trait
//! itself lives in `ports.rs` next to the other runtime ports; everything an
//! implementer needs to construct, mutate, or report on a plan is here.
//!
//! This crate ships a textbook critical-path-method implementation of the
//! contract. The `Planner` trait is the seam: the runtime always holds an
//! `Arc<dyn Planner>`, so operators can register whichever implementation
//! suits their deployment.
//!
//! # Wire format
//!
//! Every data type derives `Serialize` + `Deserialize` because the MCP server
//! layer (PA4) serialises plans, cohorts, and status snapshots into JSON-RPC
//! responses. The unit test at the bottom of this file exercises a one-
//! deliverable round-trip as a smoke test for the wire shape.
//!
//! # Locking semantics summary
//!
//! - A `PlanGraph` is submitted once; the implementation hashes
//!   `(graph, caller)` and returns an existing [`PlanId`] on resubmit. Calls
//!   are idempotent.
//! - [`crate::ports::Planner::acquire_cohort`] returns a [`Cohort`]: a batch
//!   of deliverables whose prerequisites are all [`DeliverableStatus::Complete`]
//!   and whose owned-file claims do not conflict with each other or with any
//!   currently held lock (exclusive conflicts with anything; append/append
//!   may share). The batch is locked atomically (PA3 guarantees this).
//! - [`crate::ports::Planner::mark_status`] with `Complete` or `Failed`
//!   releases the lock. A caller-id mismatch on the held lock yields
//!   [`PlannerError::LockNotHeld`].
//! - [`crate::ports::Planner::heartbeat`] refreshes the TTL; the TTL itself is
//!   an implementation parameter (PA3 sets the open-source default at 5 min).
//! - [`crate::ports::Planner::force_release`] is the operator escape hatch.
//!   Implementations MUST emit an audit event carrying the supplied `reason`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Opaque plan identifier returned by [`crate::ports::Planner::submit_plan`].
///
/// The string form is implementation-defined (UUID, deterministic content
/// hash, ULID, etc.). Callers treat the value as opaque and round-trip it
/// without inspecting the contents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanId(pub String);

impl PlanId {
    /// Borrow the underlying string for logging or hashing.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque per-orchestrator identity. The Planner uses this to verify that
/// the caller releasing a lock is the same caller who acquired it.
///
/// The string form is implementation-defined; it must be stable for the
/// lifetime of a single orchestrator session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallerId(pub String);

impl CallerId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CallerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Full plan submitted to [`crate::ports::Planner::submit_plan`].
///
/// A plan is a DAG of [`Deliverable`]s connected by the
/// `prerequisites` field. The Planner is responsible for detecting cycles,
/// missing prerequisite references, and duplicate ids; on any structural
/// problem it returns [`PlannerError::InvalidGraph`] with a precise
/// `reason`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanGraph {
    /// Every deliverable in the plan. Order is irrelevant; the Planner
    /// derives execution order from the `prerequisites` edges.
    pub deliverables: Vec<Deliverable>,

    /// Optional global guardrail: maximum number of deliverables that may
    /// be dispatched in one chained sequence before the orchestrator must
    /// pause for explicit re-prompt. `None` means no limit. Carried at the
    /// graph level rather than per-deliverable because it reflects an
    /// operator policy, not a property of any single task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chained_dispatch: Option<u32>,
}

/// What a prerequisite edge hands over: a finished artifact or an interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteKind {
    Artifact,
    Interface,
}

/// A prerequisite edge. Wire: a bare id string, or an object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Prerequisite {
    Id(String),
    Edge {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consumes: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<PrerequisiteKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lag_hours: Option<f32>,
    },
}

impl Prerequisite {
    /// Id of the deliverable this edge points at.
    pub fn id(&self) -> &str {
        match self {
            Self::Id(id) | Self::Edge { id, .. } => id,
        }
    }

    /// What the dependent consumes from the prerequisite, if stated.
    pub fn consumes(&self) -> Option<&str> {
        match self {
            Self::Id(_) => None,
            Self::Edge { consumes, .. } => consumes.as_deref(),
        }
    }

    /// Edge kind, if stated.
    pub fn kind(&self) -> Option<PrerequisiteKind> {
        match self {
            Self::Id(_) => None,
            Self::Edge { kind, .. } => *kind,
        }
    }

    /// Hours between the prerequisite finishing and the dependent starting;
    /// `0.0` when absent.
    pub fn lag_hours(&self) -> f32 {
        match self {
            Self::Id(_) => 0.0,
            Self::Edge { lag_hours, .. } => lag_hours.unwrap_or(0.0),
        }
    }
}

impl From<&str> for Prerequisite {
    fn from(id: &str) -> Self {
        Self::Id(id.to_string())
    }
}

impl From<String> for Prerequisite {
    fn from(id: String) -> Self {
        Self::Id(id)
    }
}

/// How a deliverable claims a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileMode {
    /// Sole writer; conflicts with every other claim on the path.
    #[default]
    Exclusive,
    /// Append-only; may be co-leased with other append claims.
    Append,
}

/// One entry of `owned_files`: a bare path (exclusive) or `{path, mode}`.
/// A bare path round-trips as a plain string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged, from = "OwnedFileWire")]
pub enum OwnedFile {
    Path(PathBuf),
    Claim {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<FileMode>,
    },
}

/// Strict wire form of [`OwnedFile`]: the object form rejects unknown keys.
#[derive(Deserialize)]
#[serde(untagged)]
enum OwnedFileWire {
    Path(PathBuf),
    Claim(OwnedClaimWire),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedClaimWire {
    path: PathBuf,
    #[serde(default)]
    mode: Option<FileMode>,
}

impl From<OwnedFileWire> for OwnedFile {
    fn from(w: OwnedFileWire) -> Self {
        match w {
            OwnedFileWire::Path(p) => Self::Path(p),
            OwnedFileWire::Claim(c) => Self::Claim {
                path: c.path,
                mode: c.mode,
            },
        }
    }
}

impl OwnedFile {
    pub fn path(&self) -> &Path {
        match self {
            Self::Path(p) | Self::Claim { path: p, .. } => p,
        }
    }

    pub fn mode(&self) -> FileMode {
        match self {
            Self::Path(_) => FileMode::Exclusive,
            Self::Claim { mode, .. } => mode.unwrap_or_default(),
        }
    }
}

impl From<&str> for OwnedFile {
    fn from(s: &str) -> Self {
        Self::Path(PathBuf::from(s))
    }
}

impl From<PathBuf> for OwnedFile {
    fn from(p: PathBuf) -> Self {
        Self::Path(p)
    }
}

/// Optional three-point effort estimate for a [`Deliverable`].
///
/// The three points must satisfy `0 <= optimistic <= likely <= pessimistic`
/// and all be finite and at most [`MAX_HOURS`]; [`crate::planner`] rejects
/// violations as [`PlannerError::InvalidGraph`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Estimate {
    /// Best-case effort in hours.
    pub optimistic: f32,
    /// Most-likely effort in hours; used as the scheduled length when no
    /// explicit effort or duration is set.
    pub likely: f32,
    /// Worst-case effort in hours.
    pub pessimistic: f32,
}

/// How partial progress on a deliverable converts to earned percent for
/// earned value ([`Deliverable::earning_rule`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EarningRule {
    /// 100% only when Complete.
    #[default]
    ZeroHundred,
    /// 50% once started (or any percent reported), 100% when Complete.
    FiftyFifty,
    /// The reported percent, 100% when Complete.
    Weighted,
}

/// A single unit of work scheduled by the Planner.
///
/// `owned_files` is the load-bearing field for concurrent dispatch: the
/// Planner guarantees that two deliverables with conflicting `owned_files`
/// claims (exclusive conflicts with anything; append/append may share) will
/// never be returned in the same [`Cohort`] and will never both hold active
/// locks. This is the only mechanism the Planner uses to prevent
/// write-write conflicts; implementations of [`crate::ports::Planner`]
/// must therefore reject any plan that contains a deliverable whose
/// `owned_files` are not specified up front.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deliverable {
    /// Unique identifier within the plan. The Planner rejects duplicate
    /// ids at submit time with [`PlannerError::InvalidGraph`].
    pub id: String,

    /// Exact file paths the implementer is going to write while completing
    /// this deliverable. The absence of conflicting claims across deliverables
    /// is the lock-contention invariant; see [`crate::ports::Planner::acquire_cohort`] semantics.
    pub owned_files: Vec<OwnedFile>,

    /// Ids of other deliverables in the same plan that must reach
    /// [`DeliverableStatus::Complete`] before this one becomes eligible
    /// for acquisition.
    pub prerequisites: Vec<Prerequisite>,

    /// Estimated wall-clock effort, used by critical-path math in
    /// [`PlanStatus::critical_path`]. `None` means the planner derives an
    /// estimate with `EffortEstimator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_effort_hours: Option<f32>,

    /// Calendar time the deliverable occupies on the schedule, in hours.
    /// When set it replaces the effort estimate as the scheduled length;
    /// effort stays the cost basis. Must be finite and >= 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_hours: Option<f32>,

    /// Optional three-point effort estimate. Scheduled-length precedence is
    /// `duration_hours` > `estimated_effort_hours` > `estimate.likely` >
    /// `0` for a milestone > the estimator's default. So only when neither
    /// `duration_hours` nor `estimated_effort_hours` is set is `likely` the
    /// scheduled length and the basis for cost and DRAG, and only then does
    /// Monte Carlo sample the estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<Estimate>,

    /// Free-form metadata. Conventionally carries model hints, human
    /// descriptions, links to specs, etc. The Planner does not interpret
    /// this field.
    #[serde(default)]
    pub metadata: serde_json::Value,

    /// True for a milestone: a zero-effort marker whose schedule and
    /// critical path `plan.status` reports in `milestones`. A deliverable
    /// with `metadata.milestone == true` is treated the same way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub milestone: bool,

    /// How earned value credits partial progress. `None` means
    /// [`EarningRule::ZeroHundred`]; an explicit `zero_hundred` deserializes
    /// as `None` and hashes like it. Part of the plan's identity (hashed).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_earning_rule"
    )]
    pub earning_rule: Option<EarningRule>,
}

/// The default rule is normalised to `None`, so `"zero_hundred"` and an
/// absent rule describe the same deliverable.
fn deserialize_earning_rule<'de, D>(d: D) -> Result<Option<EarningRule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<EarningRule>::deserialize(d)?.filter(|r| *r != EarningRule::ZeroHundred))
}

impl Deliverable {
    /// Whether this deliverable is a milestone, via the `milestone` field or
    /// the legacy `metadata.milestone == true` convention.
    pub fn is_milestone(&self) -> bool {
        self.milestone
            || self
                .metadata
                .get("milestone")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
    }
}

/// Lifecycle state of a single [`Deliverable`].
///
/// Transitions are driven by [`crate::ports::Planner::mark_status`] and by
/// the Planner's own scheduling logic (e.g. `Pending` -> `Ready` when the
/// last prerequisite completes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeliverableStatus {
    /// At least one prerequisite is not yet complete.
    Pending,
    /// All prerequisites are complete; not yet acquired.
    Ready,
    /// A caller holds an active lock and is working on this deliverable.
    InProgress,
    /// The caller marked this deliverable complete. The lock has been
    /// released.
    Complete,
    /// The caller marked this deliverable failed. The lock has been
    /// released. The reason is preserved for audit and for human / planner
    /// retry decisions.
    Failed {
        /// Human-readable failure reason supplied by the caller.
        reason: String,
    },
}

/// Request bundle for [`crate::ports::Planner::acquire_cohort`].
///
/// Public fields mirror the historical positional arguments so callers can
/// construct it directly or via [`AcquireRequest::new`].
#[derive(Debug, Clone)]
pub struct AcquireRequest {
    pub plan_id: PlanId,
    pub caller_id: CallerId,
    pub max_count: usize,
    /// When set, only these deliverables are considered; each one that is
    /// not leased is reported in `Cohort.blocked` with a reason code.
    pub ids: Option<Vec<String>>,
    /// When set, only deliverables whose `metadata` has every `(key, value)`
    /// pair (JSON equality) are considered. Non-matches are silently skipped.
    pub metadata_filter: Option<serde_json::Map<String, serde_json::Value>>,
    /// Requested lease TTL for this call. `None` uses the planner
    /// default; the planner clamps the value to its configured maximum.
    pub ttl: Option<Duration>,
}

impl AcquireRequest {
    pub fn new(plan_id: PlanId, caller_id: CallerId, max_count: usize) -> Self {
        Self {
            plan_id,
            caller_id,
            max_count,
            ids: None,
            metadata_filter: None,
            ttl: None,
        }
    }

    /// Restrict the acquire to these deliverable ids.
    pub fn with_ids(mut self, ids: Vec<String>) -> Self {
        self.ids = Some(ids);
        self
    }

    /// Restrict the acquire to deliverables matching these metadata pairs.
    pub fn with_metadata_filter(
        mut self,
        filter: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        self.metadata_filter = Some(filter);
        self
    }

    /// Request a lease TTL for this acquire. `None` (the default) uses
    /// the planner default TTL; the planner clamps the value to its
    /// configured maximum.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }
}

/// Request bundle for [`crate::ports::Planner::mark_status`].
///
/// The optional progress fields feed earned value and are validated as
/// `INVALID_ACTUALS`: `earned_pct` is 0..=100 and only accepted with
/// `InProgress` (with `Complete` it is accepted and ignored),
/// `actual_effort_hours` is finite and in `0..=`[`MAX_HOURS`], and
/// `evidence` is at most [`MAX_EVIDENCE_CHARS`] characters, with at most
/// [`MAX_EVIDENCE_ENTRIES`] kept per deliverable.
#[derive(Debug, Clone)]
pub struct MarkStatusRequest {
    pub plan_id: PlanId,
    pub deliverable_id: String,
    pub caller_id: CallerId,
    pub status: DeliverableStatus,
    /// Reported percent complete; replaces the stored value.
    pub earned_pct: Option<u8>,
    /// Reported total effort so far, in hours; replaces the stored value and
    /// takes precedence over leased hours for actual cost.
    pub actual_effort_hours: Option<f32>,
    /// One evidence note, appended to the deliverable's evidence list.
    pub evidence: Option<String>,
}

impl MarkStatusRequest {
    pub fn new(
        plan_id: PlanId,
        deliverable_id: impl Into<String>,
        caller_id: CallerId,
        status: DeliverableStatus,
    ) -> Self {
        Self {
            plan_id,
            deliverable_id: deliverable_id.into(),
            caller_id,
            status,
            earned_pct: None,
            actual_effort_hours: None,
            evidence: None,
        }
    }

    /// Report the percent complete (0..=100).
    pub fn with_earned_pct(mut self, pct: u8) -> Self {
        self.earned_pct = Some(pct);
        self
    }

    /// Report the total effort spent so far, in hours.
    pub fn with_actual_effort_hours(mut self, hours: f32) -> Self {
        self.actual_effort_hours = Some(hours);
        self
    }

    /// Append one evidence note.
    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence = Some(evidence.into());
        self
    }
}

/// Request bundle for [`crate::ports::Planner::heartbeat`].
#[derive(Debug, Clone)]
pub struct HeartbeatRequest {
    pub plan_id: PlanId,
    pub deliverable_id: String,
    pub caller_id: CallerId,
    /// Requested lease TTL for this heartbeat. `None` uses the planner
    /// default; the planner clamps the value to its configured maximum.
    pub ttl: Option<Duration>,
}

impl HeartbeatRequest {
    pub fn new(plan_id: PlanId, deliverable_id: impl Into<String>, caller_id: CallerId) -> Self {
        Self {
            plan_id,
            deliverable_id: deliverable_id.into(),
            caller_id,
            ttl: None,
        }
    }

    /// Request a lease TTL for this heartbeat. `None` (the default)
    /// uses the planner default TTL; the planner clamps the value to
    /// its configured maximum.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }
}

/// Request bundle for [`crate::ports::Planner::force_release`].
#[derive(Debug, Clone)]
pub struct ForceReleaseRequest {
    pub plan_id: PlanId,
    pub deliverable_id: String,
    pub reason: String,
    /// Also clear the deliverable's lapse and failure counters (revives a
    /// lapse-limited or circuit-broken deliverable). Defaults to `false`.
    pub reset_counters: bool,
}

impl ForceReleaseRequest {
    pub fn new(
        plan_id: PlanId,
        deliverable_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            plan_id,
            deliverable_id: deliverable_id.into(),
            reason: reason.into(),
            reset_counters: false,
        }
    }

    /// Set whether the lapse and failure counters are cleared too.
    pub fn reset_counters(mut self, yes: bool) -> Self {
        self.reset_counters = yes;
        self
    }
}

/// Request bundle for [`crate::ports::Planner::accept`].
#[derive(Debug, Clone)]
pub struct AcceptRequest {
    pub plan_id: PlanId,
    pub deliverable_id: String,
    pub accepted_by: String,
    pub evidence: String,
    /// Take over a live lease held by someone else. Defaults to `false`.
    pub override_lock: bool,
}

impl AcceptRequest {
    pub fn new(
        plan_id: PlanId,
        deliverable_id: impl Into<String>,
        accepted_by: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            plan_id,
            deliverable_id: deliverable_id.into(),
            accepted_by: accepted_by.into(),
            evidence: evidence.into(),
            override_lock: false,
        }
    }

    /// Set whether a live lease held by another caller may be taken over.
    pub fn override_lock(mut self, yes: bool) -> Self {
        self.override_lock = yes;
        self
    }
}

/// Request bundle for [`crate::ports::Planner::sync_plan`]: register or
/// update the variant `variant` of the plan line `(project, name)`.
#[derive(Debug, Clone)]
pub struct SyncRequest {
    pub project: String,
    pub name: String,
    pub variant: String,
    pub graph: PlanGraph,
    /// Where the graph was read from (relative to the project root), if it
    /// came from a plan file. `None` for an inline graph.
    pub source_path: Option<String>,
    /// Hash of the source file's bytes. `None` for an inline graph, which is
    /// then identified by its canonical graph hash.
    pub content_hash: Option<String>,
    /// Passed to the revision when the content changed: release live locks
    /// of removed deliverables instead of refusing. Defaults to `false`.
    pub force: bool,
}

impl SyncRequest {
    pub fn new(
        project: impl Into<String>,
        name: impl Into<String>,
        variant: impl Into<String>,
        graph: PlanGraph,
    ) -> Self {
        Self {
            project: project.into(),
            name: name.into(),
            variant: variant.into(),
            graph,
            source_path: None,
            content_hash: None,
            force: false,
        }
    }

    /// Record the plan file the graph was read from.
    pub fn with_source_path(mut self, path: impl Into<String>) -> Self {
        self.source_path = Some(path.into());
        self
    }

    /// Record the hash of the plan file's bytes.
    pub fn with_content_hash(mut self, hash: impl Into<String>) -> Self {
        self.content_hash = Some(hash.into());
        self
    }

    /// Set whether a changed graph may force-release live locks.
    pub fn force(mut self, yes: bool) -> Self {
        self.force = yes;
        self
    }
}

/// Request bundle for [`crate::ports::Planner::revise_plan`].
#[derive(Debug, Clone)]
pub struct ReviseRequest {
    pub plan_id: PlanId,
    pub graph: PlanGraph,
    /// Release live locks of removed (or claim-conflicting) deliverables
    /// instead of refusing with `LOCK_HELD`. Defaults to `false`.
    pub force: bool,
}

impl ReviseRequest {
    pub fn new(plan_id: PlanId, graph: PlanGraph) -> Self {
        Self {
            plan_id,
            graph,
            force: false,
        }
    }

    /// Set whether live locks may be force-released.
    pub fn force(mut self, yes: bool) -> Self {
        self.force = yes;
        self
    }
}

/// Request bundle for [`crate::ports::Planner::fork_plan`]: copy the head
/// graph of the named variant `plan_id`, apply `edits`, and register the
/// result as the new draft variant `variant` of the same plan line.
#[derive(Debug, Clone)]
pub struct ForkRequest {
    pub plan_id: PlanId,
    pub variant: String,
    /// Applied in order with [`crate::edits::apply_edits`].
    pub edits: Vec<crate::edits::GraphEdit>,
    /// Where to write the new variant's plan file. Falls back to the
    /// planner's own root; with neither, the variant is registered inline.
    pub project_root: Option<crate::project::ProjectRoot>,
}

impl ForkRequest {
    pub fn new(plan_id: PlanId, variant: impl Into<String>) -> Self {
        Self {
            plan_id,
            variant: variant.into(),
            edits: Vec::new(),
            project_root: None,
        }
    }

    /// Set the edits applied to the copied graph.
    pub fn with_edits(mut self, edits: Vec<crate::edits::GraphEdit>) -> Self {
        self.edits = edits;
        self
    }

    /// Write the new variant's file under `root`.
    pub fn with_project_root(mut self, root: crate::project::ProjectRoot) -> Self {
        self.project_root = Some(root);
        self
    }
}

/// Request bundle for [`crate::ports::Planner::compare_plans`]. Exactly one
/// of `plan_ids` (two or more plans, any variants or unnamed plans) or
/// `plan` (`(project, name)`: every non-archived variant of that line) is
/// given.
#[derive(Debug, Clone)]
pub struct ComparePlansRequest {
    pub plan_ids: Option<Vec<PlanId>>,
    pub plan: Option<(String, String)>,
    pub request: crate::compare::CompareRequest,
}

impl ComparePlansRequest {
    /// Compare the given plans, in this order.
    pub fn by_ids(plan_ids: Vec<PlanId>, request: crate::compare::CompareRequest) -> Self {
        Self {
            plan_ids: Some(plan_ids),
            plan: None,
            request,
        }
    }

    /// Compare every non-archived variant of the line `(project, name)`.
    pub fn by_plan(
        project: impl Into<String>,
        name: impl Into<String>,
        request: crate::compare::CompareRequest,
    ) -> Self {
        Self {
            plan_ids: None,
            plan: Some((project.into(), name.into())),
            request,
        }
    }
}

/// One variant of a plan line, as reported by
/// [`crate::ports::Planner::list_plans`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariantSummary {
    pub variant: String,
    pub plan_id: PlanId,
    /// True for the line's one executable variant.
    pub selected: bool,
    pub archived: bool,
    pub head_revision: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// Deliverables `Complete`.
    pub complete: usize,
    /// Deliverables in the head graph.
    pub total: usize,
    /// True when every deliverable is `Complete` (vacuously for none).
    pub plan_complete: bool,
    /// Head graph's CPM makespan in hours.
    pub makespan: f32,
}

/// A named plan line and its variants, sorted by variant name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanLineSummary {
    pub project: String,
    pub name: String,
    pub selected_variant: Option<String>,
    pub archived: bool,
    pub variants: Vec<VariantSummary>,
}

/// Result of [`crate::ports::Planner::sync_plan`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub plan_id: PlanId,
    pub name: String,
    pub variant: String,
    /// The variant's head revision after the sync.
    pub revision: u32,
    /// True when this sync registered the variant.
    pub created: bool,
    /// True when this sync created the variant or a new revision.
    pub changed: bool,
    /// What the revision changed; `None` unless a new revision was made.
    #[serde(default)]
    pub diff: Option<crate::revise::RevisionDiff>,
}

/// Result of [`crate::ports::Planner::select_variant`]. Selection does not
/// change any graph, so this is a dedicated summary rather than a
/// [`crate::revise::RevisionDiff`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectOutcome {
    pub plan_id: PlanId,
    pub project: String,
    pub name: String,
    /// The variant now selected (the one `plan_id` belongs to).
    pub variant: String,
    /// The previously selected variant; `None` when the line had none.
    pub previous: Option<String>,
    /// False when `variant` was already selected (a no-op).
    pub changed: bool,
    /// Sorted ids whose `Complete` status was copied from the previously
    /// selected variant (identical canonical definition in both).
    pub carried: Vec<String>,
    /// Sorted ids of the previous variant's live locks released by a forced
    /// selection.
    pub released_locks: Vec<String>,
}

/// Snapshot of a held lock. The Planner records one [`LockInfo`] per
/// acquired deliverable and surfaces them in `Cohort::locks` and
/// [`PlanStatus::locks_held`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub plan_id: PlanId,
    pub deliverable_id: String,
    pub caller_id: CallerId,
    pub acquired_at: DateTime<Utc>,
    /// TTL deadline. After this instant, the lock is treated as expired
    /// and the Planner is free to release it on any subsequent operation.
    pub expires_at: DateTime<Utc>,
}

/// One row in a [`Cohort`]: a deliverable held under a single lock.
///
/// Pairing the deliverable with its lock structurally makes the
/// invariant "the i-th deliverable is held under the i-th lock"
/// unrepresentable as broken at the type level. Pre-F5 the same
/// invariant lived in a doc comment + a runtime test assertion; a
/// future refactor that pushed to one parallel Vec but not the other
/// would silently violate it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CohortRow {
    pub deliverable: Deliverable,
    pub lock: LockInfo,
}

/// Result of a successful [`crate::ports::Planner::acquire_cohort`]
/// call.
///
/// SPEC §33 audit fixup (F5 INTERFACE_GAP-001) — the previous shape
/// was two parallel vectors (`deliverables: Vec<Deliverable>` +
/// `locks: Vec<LockInfo>`) with the index-pairing invariant carried
/// only in docs. F5 tightens to `rows: Vec<CohortRow>` so the
/// invariant is type-enforced.
///
/// The wire shape is preserved: `#[serde(into = "FlatCohort", try_from =
/// "FlatCohort")]` projects to/from the historical two-array JSON so
/// MCP clients see no breaking change. Deserialization is fallible
/// (CMP-032): mismatched `deliverables`/`locks` lengths are a corrupt
/// payload and are rejected rather than silently truncated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(into = "FlatCohort", try_from = "FlatCohort")]
pub struct Cohort {
    pub plan_id: PlanId,
    pub rows: Vec<CohortRow>,
    /// Deliverables the acquire considered but did not lease, and why.
    pub blocked: Vec<BlockedDeliverable>,
    /// Paths in this cohort claimed in append mode by two or more
    /// deliverables (in the cohort or against held locks).
    pub shared_paths: Vec<PathBuf>,
}

/// A deliverable the acquire considered but did not lease, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedDeliverable {
    pub id: String,
    /// Stable code; the authoritative list is "MANUAL", "NOT_READY",
    /// "LOCKED", "LAPSE_LIMIT", "FILE_CONFLICT" and "MAX_COUNT" (see the
    /// server `instructions()` for when each applies).
    pub code: String,
    pub reason: String,
}

/// Error returned when a `FlatCohort` wire payload cannot be decoded into a
/// [`Cohort`] — currently only the deliverables/locks length mismatch
/// (CMP-032). Carries both lengths for triage and implements `Display` so it
/// satisfies serde's `try_from` error bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CohortDecodeError {
    pub deliverables: usize,
    pub locks: usize,
}

impl std::fmt::Display for CohortDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "COHORT_LENGTH_MISMATCH: deliverables ({}) and locks ({}) arrays \
             must be the same length; each deliverable is held under exactly one lock",
            self.deliverables, self.locks
        )
    }
}

impl std::error::Error for CohortDecodeError {}

/// Wire-shape adapter for [`Cohort`] — keeps the historical
/// `{plan_id, deliverables, locks}` JSON layout so the F5 in-Rust
/// API tightening does NOT break MCP clients. NEVER referenced
/// directly; only via the `From`/`Into` plumbing.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlatCohort {
    plan_id: PlanId,
    deliverables: Vec<Deliverable>,
    locks: Vec<LockInfo>,
    #[serde(default)]
    blocked: Vec<BlockedDeliverable>,
    #[serde(default)]
    shared_paths: Vec<PathBuf>,
}

impl From<Cohort> for FlatCohort {
    fn from(cohort: Cohort) -> Self {
        let mut deliverables = Vec::with_capacity(cohort.rows.len());
        let mut locks = Vec::with_capacity(cohort.rows.len());
        for row in cohort.rows {
            deliverables.push(row.deliverable);
            locks.push(row.lock);
        }
        FlatCohort {
            plan_id: cohort.plan_id,
            deliverables,
            locks,
            blocked: cohort.blocked,
            shared_paths: cohort.shared_paths,
        }
    }
}

impl TryFrom<FlatCohort> for Cohort {
    type Error = CohortDecodeError;

    fn try_from(flat: FlatCohort) -> Result<Self, Self::Error> {
        // CMP-032 — mismatched lengths mean an unpaired deliverable or lock.
        // Previously this truncated to the shorter array, silently dropping
        // lock-grant entries (or deliverables). That hides a real corruption,
        // so we now reject the payload outright.
        if flat.deliverables.len() != flat.locks.len() {
            return Err(CohortDecodeError {
                deliverables: flat.deliverables.len(),
                locks: flat.locks.len(),
            });
        }
        let rows = flat
            .deliverables
            .into_iter()
            .zip(flat.locks)
            .map(|(deliverable, lock)| CohortRow { deliverable, lock })
            .collect();
        Ok(Cohort {
            plan_id: flat.plan_id,
            rows,
            blocked: flat.blocked,
            shared_paths: flat.shared_paths,
        })
    }
}

/// Read-only snapshot of a plan's current state, returned by
/// [`crate::ports::Planner::status`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStatus {
    pub plan_id: PlanId,
    /// Per-deliverable `(id, status, attempt_count, failure_count,
    /// lapse_count)`. Order matches insertion order in the originally
    /// submitted [`PlanGraph::deliverables`] so callers can render a
    /// stable table. On the wire each row is a positional array —
    /// trailing elements were appended, so readers indexing `[0..=2]`
    /// keep working (backward-tolerant extension).
    ///
    /// - `attempt_count` — total leases handed out via
    ///   [`crate::ports::Planner::acquire_cohort`] (telemetry).
    /// - `failure_count` — explicit `mark_status(Failed)` outcomes; this
    ///   is the retry pressure against the failure circuit-break cap
    ///   ([`crate::planner::MAX_ATTEMPTS`]).
    /// - `lapse_count` — leases lost environmentally (TTL expiry, no
    ///   terminal mark); pressure against the lapse bound
    ///   ([`crate::planner::MAX_LAPSES`]), never the failure breaker.
    pub deliverables: Vec<(String, DeliverableStatus, u32, u32, u32)>,
    /// Ids on the longest dependency chain, in execution order. Always
    /// begins with `__start__` and ends with `__finish__` (synthetic
    /// endpoints; an empty plan is just those two).
    pub critical_path: Vec<String>,
    /// Scheduled length plus lags along `critical_path` (= `__finish__`
    /// earliest finish).
    pub critical_path_hours: f32,
    /// Every lock currently active across the plan.
    pub locks_held: Vec<LockInfo>,
    /// Every zero-float deliverable (synthetic endpoints excluded), sorted by `(es, id)`.
    #[serde(default)]
    pub critical_ids: Vec<String>,
    /// Per-deliverable CPM schedule, in graph insertion order.
    #[serde(default)]
    pub schedule: Vec<ScheduleRow>,
    /// Deliverables with status `Ready` and no live lock, sorted by
    /// `(latest_start, float, id)` ascending (smallest latest start first,
    /// i.e. longest remaining tail; same order as
    /// [`crate::ports::Planner::acquire_cohort`] via the shared `priority_key`);
    /// membership is a superset: acquire may still skip deliverables at the
    /// failure or lapse cap, manual deliverables, or whose files overlap a
    /// held lock.
    #[serde(default)]
    pub ready: Vec<String>,
    /// True when every deliverable is `Complete` (vacuously true for an
    /// empty plan).
    #[serde(default)]
    pub plan_complete: bool,
    /// One row per milestone deliverable, in graph order.
    #[serde(default)]
    pub milestones: Vec<MilestoneRow>,
    /// Plan line name of a named variant; `None` for an unnamed plan.
    #[serde(default)]
    pub name: Option<String>,
    /// Variant name of a named variant; `None` for an unnamed plan.
    #[serde(default)]
    pub variant: Option<String>,
    /// Whether this variant is its line's selected (executable) variant;
    /// `None` for an unnamed plan.
    #[serde(default)]
    pub selected: Option<bool>,
    /// `Some(true)` when the variant's plan file no longer matches what was
    /// last synced (by content hash), `Some(false)` when it matches. `None`
    /// (unknown) when the variant has no source file, the planner has no
    /// project root for the variant's project, or the file is unreadable.
    #[serde(default)]
    pub definition_drift: Option<bool>,
}

/// A milestone's schedule summary, reported by [`PlanStatus::milestones`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MilestoneRow {
    pub id: String,
    /// Longest chain from `__start__` to this milestone (ends at `id`).
    pub critical_path: Vec<String>,
    /// The milestone's earliest finish, in hours from plan start.
    pub hours: f32,
    /// True once the milestone deliverable is `Complete`.
    pub complete: bool,
}

/// Largest plan accepted by submit and the analysis tools. Bounds the
/// quadratic parts of validation, lint and leveling.
pub const MAX_DELIVERABLES: usize = 5000;

/// Upper bound, in hours, for every effort, duration, lag and estimate
/// value (about 114 years). Keeps schedule arithmetic far from `f32`
/// overflow.
pub const MAX_HOURS: f32 = 1_000_000.0;

/// Longest `evidence` note `mark_status` accepts, in characters.
pub const MAX_EVIDENCE_CHARS: usize = 2048;

/// Most evidence entries one deliverable keeps; appending beyond it is
/// `INVALID_ACTUALS`.
pub const MAX_EVIDENCE_ENTRIES: usize = 100;

/// Reserved id of the synthetic zero-effort source node in every plan's CPM.
pub const START_ID: &str = "__start__";
/// Reserved id of the synthetic zero-effort sink node in every plan's CPM.
pub const FINISH_ID: &str = "__finish__";

/// The stored definition of a plan: the [`PlanGraph`] exactly as submitted,
/// returned by [`crate::ports::Planner::get_plan`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDefinition {
    pub plan_id: PlanId,
    pub graph: PlanGraph,
}

/// One deliverable's CPM schedule, in hours from plan start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRow {
    pub id: String,
    pub es: f32,
    pub ef: f32,
    pub ls: f32,
    pub lf: f32,
    pub float: f32,
    pub critical: bool,
    /// True for the synthetic [`START_ID`] / [`FINISH_ID`] endpoint rows.
    #[serde(default)]
    pub synthetic: bool,
}

/// Errors returned by [`crate::ports::Planner`] methods.
///
/// Every variant carries enough context to log without further lookup. The
/// stable string token at the start of the `#[error(..)]` message doubles
/// as the wire-level error code surfaced by PA4's MCP server, so the
/// variant prefixes (`LOCK_HELD`, `LOCK_NOT_HELD`, etc.) MUST NOT change
/// without bumping the MCP server schema.
#[derive(Debug, Error)]
pub enum PlannerError {
    /// The requested deliverable is already locked by another caller. The
    /// `holder` field surfaces the conflicting caller for human triage.
    #[error("LOCK_HELD: deliverable {deliverable_id} in plan {plan_id} is locked by {holder}")]
    LockHeld {
        plan_id: String,
        deliverable_id: String,
        holder: String,
    },

    /// The caller invoked an operation that requires holding a lock
    /// (`mark_status`, `heartbeat`) but the lock is held by someone else,
    /// or no lock exists at all.
    #[error("LOCK_NOT_HELD: caller {caller_id} does not hold lock on {deliverable_id}")]
    LockNotHeld {
        caller_id: String,
        deliverable_id: String,
    },

    /// The lock the caller is referencing has passed its TTL. The Planner
    /// is free to reclaim the deliverable for another caller.
    #[error("LOCK_EXPIRED: lock on {deliverable_id} expired at {expired_at}")]
    LockExpired {
        deliverable_id: String,
        expired_at: DateTime<Utc>,
    },

    /// `acquire_cohort` discovered that the candidate deliverable's
    /// `owned_files` overlap with files held by an existing lock. Surfaced
    /// as a distinct variant (rather than `LOCK_HELD`) because the
    /// conflict is at the *file* level, not the deliverable level.
    #[error(
        "OVERLAP_DETECTED: deliverable {deliverable_id} owns files {files:?} that overlap with \
         currently locked files"
    )]
    OverlapDetected {
        deliverable_id: String,
        files: Vec<PathBuf>,
    },

    /// `acquire_cohort` cannot include the candidate because at least one
    /// of its prerequisites is not yet [`DeliverableStatus::Complete`].
    /// This is not a hard error for the whole call (other cohort members
    /// may still be returned); it surfaces when a caller explicitly
    /// requests an ineligible deliverable.
    #[error(
        "MISSING_PREREQUISITE: deliverable {deliverable_id} requires {prereq} which is not \
         Complete"
    )]
    MissingPrerequisite {
        deliverable_id: String,
        prereq: String,
    },

    /// The plan id supplied to a lookup or mutation does not correspond to
    /// any submitted plan.
    #[error("PLAN_NOT_FOUND: {plan_id}")]
    PlanNotFound { plan_id: String },

    /// The deliverable id supplied to a lookup or mutation does not
    /// correspond to any deliverable in the named plan.
    #[error("DELIVERABLE_NOT_FOUND: {deliverable_id} in plan {plan_id}")]
    DeliverableNotFound {
        plan_id: String,
        deliverable_id: String,
    },

    /// A deliverable's lease has lapsed via TTL (no terminal mark — the
    /// driving process was killed or timed out) more times than the
    /// runaway bound allows. These are ENVIRONMENTAL losses, not
    /// implementation failures, so the deliverable is NOT auto-failed;
    /// instead `acquire_cohort` skips it and reports it in
    /// [`Cohort::blocked`] until an operator intervenes (fix the
    /// environment, then `force_release` with `reset_counters`). No longer
    /// returned by `acquire_cohort`; kept for wire compatibility.
    #[error(
        "LAPSE_LIMIT: deliverable {deliverable_id} lost {lapse_count} leases to environmental \
         lapses (TTL expiry with no terminal mark — killed/timed-out drivers, NOT implementation \
         failures; bound {max_lapses}); fix the environment, then mark_status the deliverable to \
         proceed; clear it with plan.force_release {{reset_counters: true}}"
    )]
    LapseLimit {
        deliverable_id: String,
        lapse_count: u32,
        max_lapses: u32,
    },

    /// A deliverable cannot be completed without a lease (or accepted)
    /// while some of its prerequisites are not yet `Complete`.
    #[error(
        "PREREQUISITES_INCOMPLETE: {deliverable_id} in plan {plan_id} has incomplete \
         prerequisites [{}]",
        missing.join(", ")
    )]
    PrerequisitesIncomplete {
        plan_id: String,
        deliverable_id: String,
        missing: Vec<String>,
    },

    /// The submitted graph fails a structural invariant: duplicate ids,
    /// unknown prerequisite reference, cycle, empty `owned_files`, etc.
    /// The `reason` is the precise failure message.
    #[error("INVALID_GRAPH: {reason}")]
    InvalidGraph { reason: String },

    /// A project/plan-file path failed validation: not a valid slug, not of the
    /// form `.cpm-planner/plans/<name>/<variant>.json`, absolute, contains `..`,
    /// or resolves (e.g. via a symlink) outside the plans directory.
    #[error("INVALID_PATH: {reason}")]
    InvalidPath { reason: String },

    /// `plan.schedule` was given no usable capacity (missing or zero) for
    /// one or more resources that scheduled work needs. `missing` is sorted.
    #[error("INVALID_CAPACITIES: no capacity for resources [{}]", missing.join(", "))]
    InvalidCapacities { missing: Vec<String> },

    /// An execution operation (`acquire_cohort`, `heartbeat`, `mark_status`,
    /// `accept`, `force_release`) targeted a named plan variant that is not
    /// its line's selected variant. Read and analysis tools are never gated.
    #[error(
        "VARIANT_NOT_SELECTED: plan {plan_id} is variant '{variant}' of '{name}'; selected is \
         '{selected}'"
    )]
    VariantNotSelected {
        plan_id: String,
        name: String,
        variant: String,
        selected: String,
    },

    /// An operation was refused because of archiving: archiving the selected
    /// variant alone, syncing into / selecting an archived variant or line,
    /// unarchiving a variant of an archived line, or an execution operation
    /// on any variant of an archived line.
    #[error("ARCHIVE_REFUSED: {reason}")]
    ArchiveRefused { reason: String },

    /// `mark_status` progress fields failed validation: `earned_pct` above
    /// 100 or given with a status other than `in_progress`/`complete`,
    /// `actual_effort_hours` not finite or outside `0..=1000000`, or
    /// `evidence` longer than [`MAX_EVIDENCE_CHARS`] characters or beyond
    /// [`MAX_EVIDENCE_ENTRIES`] entries for the deliverable.
    #[error("INVALID_ACTUALS: {reason}")]
    InvalidActuals { reason: String },

    /// An earned-value read or snapshot (`plan.ev`, `plan.snapshot`)
    /// targeted a plan that has no baseline yet; take one with
    /// `plan.baseline` first.
    #[error("NOT_BASELINED: plan {plan_id} has no baseline; take one with plan.baseline")]
    NotBaselined { plan_id: String },

    /// Catch-all for backend failures (DB unavailable, serialization
    /// errors against the persistence layer, etc.). Wraps the underlying
    /// `anyhow::Error` so the caller can introspect via `source()`.
    #[error("BACKEND_ERROR: {0}")]
    BackendError(#[source] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire-shape smoke test: constructing a `PlanGraph` with one
    /// deliverable and round-tripping it through `serde_json` must
    /// preserve every field. The MCP server in PA4 depends on this
    /// invariant.
    #[test]
    fn plan_graph_serde_roundtrip() -> Result<(), serde_json::Error> {
        let graph = PlanGraph {
            deliverables: vec![Deliverable {
                id: "d1".to_string(),
                owned_files: vec!["src/foo.rs".into(), "src/bar.rs".into()],
                prerequisites: vec!["d0".into()],
                estimated_effort_hours: Some(1.5),
                metadata: serde_json::json!({"description": "smoke test"}),
                duration_hours: None,
                estimate: None,
                milestone: false,
                earning_rule: None,
            }],
            max_chained_dispatch: Some(8),
        };

        let json = serde_json::to_string(&graph)?;
        let back: PlanGraph = serde_json::from_str(&json)?;

        assert_eq!(back.deliverables.len(), 1);
        let d = &back.deliverables[0];
        assert_eq!(d.id, "d1");
        assert_eq!(
            d.owned_files,
            vec![OwnedFile::from("src/foo.rs"), OwnedFile::from("src/bar.rs")]
        );
        assert_eq!(d.prerequisites, vec![Prerequisite::from("d0")]);
        assert_eq!(d.estimated_effort_hours, Some(1.5));
        assert_eq!(d.metadata, serde_json::json!({"description": "smoke test"}));
        assert_eq!(back.max_chained_dispatch, Some(8));
        Ok(())
    }

    #[test]
    fn explicit_default_earning_rule_deserializes_as_absent() {
        let d: Deliverable = serde_json::from_value(serde_json::json!({
            "id": "a",
            "owned_files": [],
            "prerequisites": [],
            "earning_rule": "zero_hundred"
        }))
        .unwrap();
        assert_eq!(d.earning_rule, None);
    }

    /// `DeliverableStatus` uses an internally-tagged enum representation
    /// so the wire form matches what PA4's MCP server publishes. Pin the
    /// shape with an explicit JSON check so an accidental derive change
    /// fails loudly.
    #[test]
    fn deliverable_status_failed_carries_reason() -> Result<(), serde_json::Error> {
        let status = DeliverableStatus::Failed {
            reason: "tests failed".to_string(),
        };
        let json = serde_json::to_value(&status)?;
        assert_eq!(json["status"], "failed");
        assert_eq!(json["reason"], "tests failed");
        let back: DeliverableStatus = serde_json::from_value(json)?;
        assert_eq!(back, status);
        Ok(())
    }

    /// SPEC §33 audit fixup (F5 INTERFACE_GAP-001) — Cohort tightened
    /// from parallel vectors to `Vec<CohortRow>`, but the JSON wire
    /// shape MUST stay as `{plan_id, deliverables, locks}` so MCP
    /// clients see no breaking change. Pin both directions of the
    /// `serde(into/from)` adapter.
    #[test]
    fn cohort_wire_shape_preserves_two_array_layout() -> Result<(), serde_json::Error> {
        let plan_id = PlanId("plan_x".to_string());
        let now = chrono::Utc::now();
        let cohort = Cohort {
            plan_id: plan_id.clone(),
            blocked: vec![],
            shared_paths: vec![],
            rows: vec![
                CohortRow {
                    deliverable: Deliverable {
                        id: "d1".to_string(),
                        owned_files: vec!["a.rs".into()],
                        prerequisites: vec![],
                        estimated_effort_hours: Some(1.0),
                        metadata: serde_json::Value::Null,
                        duration_hours: None,
                        estimate: None,
                        milestone: false,
                        earning_rule: None,
                    },
                    lock: LockInfo {
                        plan_id: plan_id.clone(),
                        deliverable_id: "d1".to_string(),
                        caller_id: CallerId("c1".to_string()),
                        acquired_at: now,
                        expires_at: now + chrono::Duration::seconds(60),
                    },
                },
                CohortRow {
                    deliverable: Deliverable {
                        id: "d2".to_string(),
                        owned_files: vec!["b.rs".into()],
                        prerequisites: vec![],
                        estimated_effort_hours: Some(2.0),
                        metadata: serde_json::Value::Null,
                        duration_hours: None,
                        estimate: None,
                        milestone: false,
                        earning_rule: None,
                    },
                    lock: LockInfo {
                        plan_id: plan_id.clone(),
                        deliverable_id: "d2".to_string(),
                        caller_id: CallerId("c1".to_string()),
                        acquired_at: now,
                        expires_at: now + chrono::Duration::seconds(60),
                    },
                },
            ],
        };
        let json = serde_json::to_value(&cohort)?;
        // Wire shape: top-level keys are plan_id + deliverables + locks
        // (NOT `rows`). MCP clients depending on the historical shape
        // continue to see it.
        assert!(json.get("deliverables").is_some());
        assert!(json.get("locks").is_some());
        assert!(json.get("rows").is_none());
        let deliverables = json["deliverables"].as_array().unwrap();
        let locks = json["locks"].as_array().unwrap();
        assert_eq!(deliverables.len(), 2);
        assert_eq!(locks.len(), 2);
        // Position-aligned pairing on the wire.
        assert_eq!(deliverables[0]["id"], "d1");
        assert_eq!(locks[0]["deliverable_id"], "d1");

        // Round-trip back into the in-Rust row form.
        let back: Cohort = serde_json::from_value(json)?;
        assert_eq!(back.rows.len(), 2);
        assert_eq!(back.rows[0].deliverable.id, "d1");
        assert_eq!(back.rows[0].lock.deliverable_id, "d1");
        assert_eq!(back.rows[1].deliverable.id, "d2");
        assert_eq!(back.rows[1].lock.deliverable_id, "d2");
        Ok(())
    }

    /// CMP-032 — a wire payload whose `deliverables` and `locks` arrays have
    /// mismatched lengths is corrupt (an unpaired lock-grant or deliverable).
    /// Deserialization MUST error rather than silently truncate to the shorter
    /// array and drop the unpaired entry.
    #[test]
    fn cohort_rejects_mismatched_deliverables_and_locks_lengths() {
        let now = chrono::Utc::now();
        // Two deliverables but only one lock — the historical truncating impl
        // would have dropped d2 silently.
        let wire = serde_json::json!({
            "plan_id": "plan_x",
            "deliverables": [
                {
                    "id": "d1",
                    "owned_files": ["a.rs"],
                    "prerequisites": [],
                    "estimated_effort_hours": 1.0,
                    "metadata": null
                },
                {
                    "id": "d2",
                    "owned_files": ["b.rs"],
                    "prerequisites": [],
                    "estimated_effort_hours": 2.0,
                    "metadata": null
                }
            ],
            "locks": [
                {
                    "plan_id": "plan_x",
                    "deliverable_id": "d1",
                    "caller_id": "c1",
                    "acquired_at": now,
                    "expires_at": now + chrono::Duration::seconds(60)
                }
            ]
        });

        let err = serde_json::from_value::<Cohort>(wire).unwrap_err();
        assert!(
            err.to_string().contains("COHORT_LENGTH_MISMATCH"),
            "expected COHORT_LENGTH_MISMATCH, got: {err}"
        );
    }
}
