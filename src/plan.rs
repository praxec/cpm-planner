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

// The graph model, ids, statuses, limits and errors live in
// `cpm_planner_core::plan`; this module re-exports them at their old
// paths and adds the execution request and response types.
pub use cpm_planner_core::plan::*;

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::earned_value::{BaselineTaken, PlanBaseline};

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
    /// Offer this `metadata.thread`'s ready deliverables first (a thread
    /// token, see [`crate::affinity`]). Without it, the threads this caller
    /// last advanced come first.
    pub thread: Option<String>,
    /// How long this caller defers to another caller's thread: a ready
    /// deliverable whose thread another caller advanced less than this long
    /// ago is held back (`AFFINITY_HOLD`). `None` is
    /// [`crate::affinity::DEFAULT_AFFINITY_SECONDS`] (no hold); clamped to
    /// the planner's maximum lease TTL.
    pub affinity_seconds: Option<u64>,
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
            thread: None,
            affinity_seconds: None,
        }
    }

    /// Offer this thread's ready deliverables first.
    pub fn with_thread(mut self, thread: impl Into<String>) -> Self {
        self.thread = Some(thread.into());
        self
    }

    /// Defer this many seconds to a caller that advanced a thread.
    pub fn with_affinity_seconds(mut self, seconds: u64) -> Self {
        self.affinity_seconds = Some(seconds);
        self
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
    /// One evidence entry (a note or a [`crate::evidence::EvidenceRecord`]),
    /// appended to the deliverable's evidence list.
    pub evidence: Option<crate::evidence::Evidence>,
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

    /// Append one evidence entry: a note (`&str`/`String`) or a record.
    pub fn with_evidence(mut self, evidence: impl Into<crate::evidence::Evidence>) -> Self {
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
    /// A note or a [`crate::evidence::EvidenceRecord`]; also appended to the
    /// deliverable's evidence list.
    pub evidence: crate::evidence::Evidence,
    /// Take over a live lease held by someone else. Defaults to `false`.
    pub override_lock: bool,
}

impl AcceptRequest {
    pub fn new(
        plan_id: PlanId,
        deliverable_id: impl Into<String>,
        accepted_by: impl Into<String>,
        evidence: impl Into<crate::evidence::Evidence>,
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
    /// A baseline taken automatically because the newly selected variant had
    /// progress but no baseline (auto mode); absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_taken: Option<BaselineTaken>,
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
    /// A baseline taken automatically because this acquire started
    /// execution on a plan that had none (auto mode); absent otherwise.
    pub baseline_taken: Option<BaselineTaken>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baseline_taken: Option<BaselineTaken>,
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
            baseline_taken: cohort.baseline_taken,
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
            baseline_taken: flat.baseline_taken,
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
    /// Effective archived flag of this variant (its line is archived or the
    /// variant itself is); `None` for an unnamed plan. Archived variants
    /// stay readable through `status` and `ev`.
    #[serde(default)]
    pub archived: Option<bool>,
    /// Only when [`PlanStatus::archived`] is `Some(true)`: the name of the
    /// project's single active plan line, so a caller holding a stale id
    /// learns where the live work is. `None` when not archived, for an
    /// unnamed plan, or when the project has zero or several active lines.
    #[serde(default)]
    pub active_plan: Option<String>,
    /// `Some(true)` when the variant's plan file no longer matches what was
    /// last synced (by content hash), `Some(false)` when it matches. `None`
    /// (unknown) when the variant has no source file, the planner has no
    /// project root for the variant's project, or the file is unreadable.
    #[serde(default)]
    pub definition_drift: Option<bool>,
    /// The latest earned-value baseline of the plan, or `null` before the
    /// first baseline.
    #[serde(default)]
    pub baseline: Option<PlanBaseline>,
    /// One row per `metadata.thread`, sorted by token: holder, affine
    /// caller, next ready deliverable, completed and total. Omitted from the
    /// wire for a plan without threads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<crate::affinity::ThreadStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
            baseline_taken: None,
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
