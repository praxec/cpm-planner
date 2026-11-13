//! The plan graph model: deliverables, prerequisites, owned files, statuses,
//! plan and caller ids, limits and [`PlannerError`].
//!
//! These are the types the CPM kernel schedules. `cpm-planner` re-exports
//! this whole module as `cpm_planner::plan` and adds the execution types of
//! its lock-aware `Planner` trait there (`AcquireRequest`, `Cohort`,
//! `PlanStatus`, …), which name planner-side modules this crate does not
//! have. Doc references to `cpm_planner::…` below point at that crate.
//!
//! # Wire format
//!
//! Every data type derives `Serialize` + `Deserialize`; the MCP server in
//! `cpm-planner` sends them as JSON. The unit test at the bottom of this file
//! exercises a one-deliverable round-trip as a smoke test for the wire shape.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Opaque plan identifier returned by `cpm_planner::ports::Planner::submit_plan`.
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

/// How a caller names the plan it wants resolved (see
/// `cpm_planner::ports::Planner::resolve_plan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRef {
    /// The plan with this exact id, returned unchanged without a lookup.
    Id(PlanId),
    /// The selected variant of the plan line `(project, name)`.
    Line { project: String, name: String },
    /// The project's single active plan line: its selected variant's plan.
    Active { project: String },
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

/// Full plan submitted to `cpm_planner::ports::Planner::submit_plan`.
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
/// and all be finite and at most [`MAX_HOURS`]; `cpm_planner::planner` rejects
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
/// never be returned in the same `cpm_planner::plan::Cohort` and will never both hold active
/// locks. This is the only mechanism the Planner uses to prevent
/// write-write conflicts; implementations of `cpm_planner::ports::Planner`
/// must therefore reject any plan that contains a deliverable whose
/// `owned_files` are not specified up front.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deliverable {
    /// Unique identifier within the plan. The Planner rejects duplicate
    /// ids at submit time with [`PlannerError::InvalidGraph`].
    pub id: String,

    /// Exact file paths the implementer is going to write while completing
    /// this deliverable. The absence of conflicting claims across deliverables
    /// is the lock-contention invariant; see `cpm_planner::ports::Planner::acquire_cohort` semantics.
    pub owned_files: Vec<OwnedFile>,

    /// Ids of other deliverables in the same plan that must reach
    /// [`DeliverableStatus::Complete`] before this one becomes eligible
    /// for acquisition.
    pub prerequisites: Vec<Prerequisite>,

    /// Estimated wall-clock effort, used by critical-path math in
    /// `cpm_planner::plan::PlanStatus::critical_path`. `None` means the planner derives an
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
/// Transitions are driven by `cpm_planner::ports::Planner::mark_status` and by
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
    /// A forced plan revision invalidated the active lease because its
    /// prerequisite or revision-barrier contract changed. The prior evidence
    /// remains diagnostic, but only a fresh lease may complete this work.
    SupersededRevision,
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

/// A milestone's schedule summary, reported by `cpm_planner::plan::PlanStatus::milestones`.
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
/// returned by `cpm_planner::ports::Planner::get_plan`.
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

/// Errors returned by `cpm_planner::ports::Planner` methods.
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
    /// `cpm_planner::plan::Cohort::blocked` until an operator intervenes (fix the
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

    /// `cpm_planner::ports::Planner::resolve_plan` with
    /// [`PlanRef::Active`] found no active line: every line
    /// of `project` is archived, complete, or has no live selected variant.
    #[error(
        "NO_ACTIVE_PLAN: project '{project}' has no active plan line (an unarchived line whose \
         selected variant is unarchived and not complete)"
    )]
    NoActivePlan { project: String },

    /// `cpm_planner::ports::Planner::resolve_plan` with
    /// [`PlanRef::Active`] found more than one active line in
    /// `project`; `lines` holds every name, sorted.
    #[error(
        "AMBIGUOUS_ACTIVE_PLAN: project '{project}' has {n} active plan lines: {names}; pass \
         plan or plan_id",
        n = lines.len(),
        names = lines.join(", ")
    )]
    AmbiguousActivePlan { project: String, lines: Vec<String> },

    /// `mark_status` progress fields failed validation: `earned_pct` above
    /// 100 or given with a status other than `in_progress`/`complete`,
    /// `actual_effort_hours` not finite or outside `0..=1000000`, or
    /// `evidence` longer than [`MAX_EVIDENCE_CHARS`] characters or beyond
    /// [`MAX_EVIDENCE_ENTRIES`] entries for the deliverable.
    #[error("INVALID_ACTUALS: {reason}")]
    InvalidActuals { reason: String },

    /// An evidence record or identity is malformed: unknown field or
    /// `schema_version`, unknown scope, a value out of bounds, a required
    /// identity field missing, `party` on the wrong scope, or a server-added
    /// field sent by the caller (see `cpm_planner::evidence`).
    #[error("INVALID_EVIDENCE: {reason}")]
    InvalidEvidence { reason: String },

    /// An evidence record's scope may not close the deliverable, or the
    /// deliverable declares a `metadata.evidence_scope` its kind may not.
    #[error("EVIDENCE_SCOPE_MISMATCH: deliverable '{deliverable_id}': {reason}")]
    EvidenceScopeMismatch {
        deliverable_id: String,
        reason: String,
    },

    /// A closing call on a deliverable with a declared evidence scope (or a
    /// gate whose candidate holds an identity) carried no record of an
    /// allowed scope, and none is stored for the current definition.
    #[error(
        "EVIDENCE_REQUIRED: deliverable '{deliverable_id}' must be closed with a {scope} \
         evidence record"
    )]
    EvidenceRequired {
        deliverable_id: String,
        scope: String,
    },

    /// A gate's closing record is not current against the newest identity
    /// recorded on its `metadata.candidate`.
    #[error(
        "EVIDENCE_STALE: deliverable '{deliverable_id}': its evidence does not match candidate \
         '{candidate}' on: {fields}",
        fields = changed.join(", ")
    )]
    EvidenceStale {
        deliverable_id: String,
        candidate: String,
        changed: Vec<String>,
    },

    /// A request carried a value that is well-typed but out of range for
    /// the plan it targets, e.g. a `plan.baseline` `backfill_from` in the
    /// future or before the plan's creation time.
    #[error("INVALID_VALUE: {reason}")]
    InvalidValue { reason: String },

    /// An earned-value read or snapshot (`plan.ev`, `plan.snapshot`)
    /// targeted a plan that has no baseline yet; take one with
    /// `plan.baseline` first.
    #[error("NOT_BASELINED: plan {plan_id} has no baseline; take one with plan.baseline")]
    NotBaselined { plan_id: String },

    /// The server runs in `refuse` baseline mode and an execution-starting
    /// call targeted a plan with no baseline yet; take one with
    /// `plan.baseline` first.
    #[error(
        "BASELINE_REQUIRED: plan {plan_id} has no earned-value baseline and \
         CPM_BASELINE_MODE=refuse; take one with plan.baseline"
    )]
    BaselineRequired { plan_id: String },

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
}
