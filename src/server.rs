// SPEC §33 PA4 — MCP server façade for `BasicCpmPlanner`.
//
// Production-code lint surface (consistent with the rest of the workspace —
// `#![cfg_attr(not(test), warn(clippy::unwrap_used))]` is declared at the
// crate root in `lib.rs`).

//! MCP tool surface for the open-source CPM planner.
//!
//! [`PlanServer`] wraps an `Arc<BasicCpmPlanner>` and exposes the
//! [`Planner`] trait methods plus the read-only analysis and portfolio tools
//! as MCP tools so any MCP-speaking agent
//! (Claude Code, Cursor, custom orchestrator, or the §33 LLM executor)
//! can drive the planner over the standard MCP protocol.
//!
//! # Tool surface
//!
//! | Tool name                | Trait method                  |
//! |--------------------------|-------------------------------|
//! | `plan.submit`            | [`Planner::submit_plan`] / [`Planner::sync_plan`] |
//! | `plan.acquire_cohort`    | [`Planner::acquire_cohort`]   |
//! | `plan.heartbeat`         | [`Planner::heartbeat`]        |
//! | `plan.mark_status`       | [`Planner::mark_status`]      |
//! | `plan.status`            | [`Planner::status`]           |
//! | `plan.get`               | [`Planner::get_plan`]         |
//! | `plan.force_release`     | [`Planner::force_release`]    |
//! | `plan.accept`            | [`Planner::accept`]           |
//! | `plan.lint`              | [`crate::lint::lint`]         |
//! | `plan.schedule`          | [`crate::resource_schedule::resource_schedule`] |
//! | `plan.simulate`          | [`crate::simulate::simulate`] |
//! | `plan.sync`              | [`Planner::sync_plan`]        |
//! | `plan.list`              | [`Planner::list_plans`]       |
//! | `plan.export`            | [`Planner::export_plan`]      |
//! | `plan.revise`            | [`Planner::revise_plan`]      |
//! | `plan.fork`              | [`Planner::fork_plan`]        |
//! | `plan.select`            | [`Planner::select_variant`]   |
//! | `plan.archive`           | [`Planner::archive`]          |
//! | `plan.compare`           | [`Planner::compare_plans`]    |
//! | `plan.baseline`          | [`Planner::baseline`]         |
//! | `plan.ev`                | [`Planner::ev`]               |
//! | `plan.snapshot`          | [`Planner::snapshot`]         |
//!
//! # Error mapping
//!
//! [`PlannerError`] variants are surfaced as MCP `internal_error`
//! responses whose `message` is the variant's `Display` output. The
//! variant prefixes (`LOCK_HELD:`, `LOCK_NOT_HELD:`, `LOCK_EXPIRED:`,
//! `OVERLAP_DETECTED:`, `MISSING_PREREQUISITE:`, `PLAN_NOT_FOUND:`,
//! `DELIVERABLE_NOT_FOUND:`, `LAPSE_LIMIT:`, `PREREQUISITES_INCOMPLETE:`, `INVALID_GRAPH:`,
//! `INVALID_CAPACITIES:`, `VARIANT_NOT_SELECTED:`, `ARCHIVE_REFUSED:`,
//! `INVALID_PATH:`, `INVALID_ACTUALS:`, `NOT_BASELINED:`, `BACKEND_ERROR:`) are
//! stable machine-parseable signals — see `core::plan` for the contract.
//! Malformed arguments yield `invalid_params` with the serde error.
//!
//! # Testing pattern
//!
//! [`PlanServer::dispatch_call`] is the transport-free entry point used
//! by integration tests, mirroring the pattern in
//! `praxec-mcp-server`. The `ServerHandler::call_tool` impl is a
//! thin wrapper that wraps the result in `CallToolResult::structured`.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use crate::compare::{CompareRequest, CompareWeights};
use crate::earned_value::{BaselineRequest, Calendar, SnapshotFormat, SnapshotRequest};
use crate::edits::GraphEdit;
use crate::monte_carlo::MonteCarloRequest;
use crate::plan::{
    AcceptRequest, AcquireRequest, CallerId, Cohort, ComparePlansRequest, DeliverableStatus,
    ForceReleaseRequest, ForkRequest, HeartbeatRequest, MarkStatusRequest, PlanDefinition,
    PlanGraph, PlanId, PlanStatus, PlannerError, ReviseRequest, SyncRequest,
};
use crate::ports::Planner;
use crate::project::ProjectRoot;
use crate::resource_schedule::{ScheduleRequest, resource_schedule};
use crate::simulate::SimulateRequest;
use chrono::{DateTime, Utc};
use rmcp::ErrorData as McpError;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::transport::stdio;
use rmcp::{ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::BasicCpmPlanner;

/// SPEC §33 PA4 — tool names. Dot notation `plan.<verb>` matches the
/// convention used elsewhere in the workspace (`praxec.query`,
/// `praxec.command`).
pub const TOOL_SUBMIT: &str = "plan.submit";
pub const TOOL_ACQUIRE_COHORT: &str = "plan.acquire_cohort";
pub const TOOL_HEARTBEAT: &str = "plan.heartbeat";
pub const TOOL_MARK_STATUS: &str = "plan.mark_status";
pub const TOOL_STATUS: &str = "plan.status";
pub const TOOL_GET: &str = "plan.get";
pub const TOOL_FORCE_RELEASE: &str = "plan.force_release";
pub const TOOL_ACCEPT: &str = "plan.accept";
pub const TOOL_LINT: &str = "plan.lint";
pub const TOOL_SCHEDULE: &str = "plan.schedule";
pub const TOOL_SIMULATE: &str = "plan.simulate";
pub const TOOL_SYNC: &str = "plan.sync";
pub const TOOL_LIST: &str = "plan.list";
pub const TOOL_EXPORT: &str = "plan.export";
pub const TOOL_REVISE: &str = "plan.revise";
pub const TOOL_FORK: &str = "plan.fork";
pub const TOOL_SELECT: &str = "plan.select";
pub const TOOL_ARCHIVE: &str = "plan.archive";
pub const TOOL_COMPARE: &str = "plan.compare";
pub const TOOL_BASELINE: &str = "plan.baseline";
pub const TOOL_EV: &str = "plan.ev";
pub const TOOL_SNAPSHOT: &str = "plan.snapshot";

/// All twenty-two MCP tool names exposed by [`PlanServer`], in declaration order.
pub const PLAN_TOOL_NAMES: &[&str] = &[
    TOOL_SUBMIT,
    TOOL_ACQUIRE_COHORT,
    TOOL_HEARTBEAT,
    TOOL_MARK_STATUS,
    TOOL_STATUS,
    TOOL_GET,
    TOOL_FORCE_RELEASE,
    TOOL_ACCEPT,
    TOOL_LINT,
    TOOL_SCHEDULE,
    TOOL_SIMULATE,
    TOOL_SYNC,
    TOOL_LIST,
    TOOL_EXPORT,
    TOOL_REVISE,
    TOOL_FORK,
    TOOL_SELECT,
    TOOL_ARCHIVE,
    TOOL_COMPARE,
    TOOL_BASELINE,
    TOOL_EV,
    TOOL_SNAPSHOT,
];

// ---------------------------------------------------------------------------
// Per-tool argument structs
// ---------------------------------------------------------------------------

// `deny_unknown_fields` on every wire-arg struct: unknown keys are a caller
// bug, not something to ignore. Fail-fast surfaces typos/drift at the
// `parse_args` boundary instead of silently dropping them.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitArgs {
    graph: PlanGraph,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    variant: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquireCohortArgs {
    plan_id: String,
    caller_id: String,
    max_count: usize,
    #[serde(default)]
    ids: Option<Vec<String>>,
    #[serde(default)]
    filter: Option<AcquireFilter>,
    #[serde(default)]
    ttl_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquireFilter {
    #[serde(default)]
    metadata: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptArgs {
    plan_id: String,
    deliverable_id: String,
    accepted_by: String,
    evidence: String,
    #[serde(default)]
    override_lock: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeartbeatArgs {
    plan_id: String,
    deliverable_id: String,
    caller_id: String,
    #[serde(default)]
    ttl_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkStatusArgs {
    plan_id: String,
    deliverable_id: String,
    caller_id: String,
    status: DeliverableStatus,
    /// Any JSON number, so negative, fractional or out-of-range values get
    /// `INVALID_ACTUALS`, not a parse error.
    #[serde(default)]
    earned_pct: Option<f64>,
    #[serde(default)]
    actual_effort_hours: Option<f64>,
    #[serde(default)]
    evidence: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    plan_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetArgs {
    plan_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForceReleaseArgs {
    plan_id: String,
    deliverable_id: String,
    reason: String,
    #[serde(default)]
    reset_counters: bool,
}

/// Shared by `plan.lint`: exactly one of an inline `graph`, a stored
/// `plan_id`, or a plan-file `path` must be supplied.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphOrPlanIdArgs {
    #[serde(default)]
    graph: Option<PlanGraph>,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// `plan.schedule`: the graph/plan selector plus the leveling inputs
/// (`capacities` required; `resource_key` and `project_buffer_pct` defaulted
/// exactly as [`ScheduleRequest`] is).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleToolArgs {
    #[serde(default)]
    graph: Option<PlanGraph>,
    #[serde(default)]
    plan_id: Option<String>,
    capacities: std::collections::BTreeMap<String, u32>,
    #[serde(default = "crate::resource_schedule::default_resource_key")]
    resource_key: String,
    #[serde(default = "crate::resource_schedule::default_buffer_pct")]
    project_buffer_pct: f32,
}

/// `plan.simulate`: the graph/plan selector plus optional leveling and
/// Monte Carlo requests.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SimulateToolArgs {
    #[serde(default)]
    graph: Option<PlanGraph>,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    schedule: Option<ScheduleRequest>,
    #[serde(default)]
    monte_carlo: Option<MonteCarloRequest>,
}

// ── Portfolio tool args ─────────────────────────────────────────────────────

/// `plan.sync`: register or update one variant from a plan file (`path`) or
/// an inline `graph`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    graph: Option<PlanGraph>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    force: bool,
}

/// `plan.list`: every plan line of `project` (default: discovered root).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    include_archived: bool,
}

/// `plan.export`: write a plan's head graph to its variant file (or `path`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportArgs {
    plan_id: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    force: bool,
}

/// `plan.revise`: replace a plan's graph in place, carrying progress over.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviseArgs {
    plan_id: String,
    graph: PlanGraph,
    #[serde(default)]
    force: bool,
}

/// `plan.fork`: copy a named variant's head graph, apply `edits`, register
/// the result as a new draft variant.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkArgs {
    plan_id: String,
    variant: String,
    #[serde(default)]
    edits: Vec<GraphEdit>,
}

/// `plan.select`: make a variant's line select it as the only executable one.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectArgs {
    plan_id: String,
    #[serde(default)]
    force: bool,
}

/// `plan.archive`: archive or unarchive a whole line or one variant.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveArgs {
    name: String,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default = "default_archived")]
    archived: bool,
    #[serde(default)]
    force: bool,
}

fn default_archived() -> bool {
    true
}

/// `plan.compare`: compare stored plans by id or every live variant of a line.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompareArgs {
    #[serde(default)]
    plan_ids: Option<Vec<String>>,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    schedule: Option<ScheduleRequest>,
    #[serde(default)]
    monte_carlo: Option<MonteCarloRequest>,
    #[serde(default)]
    weights: CompareWeights,
}

// ── Earned-value tool args ──────────────────────────────────────────────────

/// `plan.baseline`: freeze the plan's schedule and budgets as a baseline.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineArgs {
    plan_id: String,
    #[serde(default)]
    start: Option<DateTime<Utc>>,
    #[serde(default)]
    calendar: Option<Calendar>,
    #[serde(default)]
    reason: Option<String>,
}

/// `plan.ev`: the earned-value report against the latest baseline.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvArgs {
    plan_id: String,
    #[serde(default)]
    as_of: Option<DateTime<Utc>>,
}

/// `plan.snapshot`: append an EV snapshot and render the history.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotArgs {
    plan_id: String,
    #[serde(default)]
    as_of: Option<DateTime<Utc>>,
    #[serde(default)]
    format: SnapshotFormat,
}

// ---------------------------------------------------------------------------
// Per-tool response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SubmitResponse {
    plan_id: String,
}

#[derive(Debug, Serialize)]
struct OkResponse {
    ok: bool,
}

impl OkResponse {
    fn new() -> Self {
        Self { ok: true }
    }
}

#[derive(Debug, Serialize)]
struct ExportResponse {
    path: String,
}

#[derive(Debug, Serialize)]
struct ReviseResponse {
    revision: u32,
    diff: crate::revise::RevisionDiff,
}

// `Cohort` and `PlanStatus` already derive `Serialize` (PA1) — return them
// directly.

// ---------------------------------------------------------------------------
// Tool-list construction
// ---------------------------------------------------------------------------

/// The inline `PlanGraph` JSON Schema shared by `plan.submit` and the
/// read-only analysis tools (`plan.lint`, `plan.schedule`, `plan.simulate`).
fn graph_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "deliverables": {
                "type": "array",
                "maxItems": 5000,
                "items": {
                    "type": "object",
                    "properties": {
                        "id":                    { "type": "string" },
                        "owned_files":           { "type": "array", "description": "Each item is a path string (exclusive) or an object {path, mode?: exclusive|append}. Append claims on the same path may be leased together.", "items": { "oneOf": [
                            { "type": "string" },
                            { "type": "object", "properties": {
                                "path": { "type": "string" },
                                "mode": { "type": "string", "enum": ["exclusive", "append"] }
                            }, "required": ["path"], "additionalProperties": false }
                        ] } },
                        "prerequisites":         { "type": "array", "description": "Each item is a deliverable id string, or an object {id, consumes?, kind?: artifact|interface, lag_hours?}.", "items": { "oneOf": [
                            { "type": "string" },
                            { "type": "object", "properties": {
                                "id": { "type": "string" },
                                "consumes": { "type": "string" },
                                "kind": { "type": "string", "enum": ["artifact", "interface"] },
                                "lag_hours": { "type": "number", "minimum": 0, "maximum": 1000000 }
                            }, "required": ["id"], "additionalProperties": false }
                        ] } },
                        "estimated_effort_hours": { "type": "number", "minimum": 0, "maximum": 1000000, "description": "Effort in hours: the cost basis, and the scheduled length when duration_hours is absent." },
                        "duration_hours":         { "type": "number", "minimum": 0, "maximum": 1000000, "description": "Calendar time on the schedule; replaces effort as the scheduled length. Effort stays the cost basis." },
                        "estimate":               { "type": "object", "description": "Optional three-point effort estimate (0 <= optimistic <= likely <= pessimistic <= 1000000). Scheduled-length precedence: duration_hours > estimated_effort_hours > estimate.likely > 0 for a milestone > estimator default. Monte Carlo samples the estimate only when neither duration_hours nor estimated_effort_hours is set.", "properties": {
                            "optimistic":  { "type": "number", "minimum": 0, "maximum": 1000000 },
                            "likely":      { "type": "number", "minimum": 0, "maximum": 1000000 },
                            "pessimistic": { "type": "number", "minimum": 0, "maximum": 1000000 }
                        }, "required": ["optimistic", "likely", "pessimistic"], "additionalProperties": false },
                        "milestone":              { "type": "boolean", "description": "Acceptance point; reported in plan.status milestones with its own critical path." },
                        "earning_rule":           { "type": "string", "enum": ["zero_hundred", "fifty_fifty", "weighted"], "description": "How earned value credits partial progress: zero_hundred (default; 100% only when complete), fifty_fifty (50% once in progress or any earned_pct is reported), weighted (the reported earned_pct). Part of the plan's identity." },
                        "metadata":              {}
                    },
                    "required": ["id", "owned_files", "prerequisites"]
                }
            },
            "max_chained_dispatch": { "type": ["integer", "null"] }
        },
        "required": ["deliverables"]
    })
}

/// Selector for the read-only analysis tools: exactly one of `graph` or
/// `plan_id` is required by the server-side check, but the JSON Schema stays a
/// plain object so clients that only understand `type`/`properties` can still
/// parse it.
fn graph_selector_schema() -> Value {
    let mut schema = graph_schema();
    if let Some(obj) = schema.as_object_mut() {
        obj.insert(
            "description".to_string(),
            json!(
                "An inline plan graph to analyse. Provide exactly one of graph, plan_id, or path."
            ),
        );
    }
    schema
}

fn plan_id_schema() -> Value {
    json!({
        "type": "string",
        "description": "A stored plan (from plan.submit/plan.sync) to analyse instead of an inline graph. Provide exactly one of graph, plan_id, or path."
    })
}

fn path_schema() -> Value {
    json!({
        "type": "string",
        "description": "A plan file path of the form .cpm-planner/plans/<name>/<variant>.json, relative to the project root. Provide exactly one of graph, plan_id, or path."
    })
}

fn edits_schema() -> Value {
    json!({
        "type": "array",
        "description": "Structured graph edits applied in order. Each edit is an object tagged by `op`: remove_edge {from,to}, add_edge {from,to,consumes?}, set_effort {id,hours}, set_duration {id,hours?}, set_estimate {id,estimate?}, set_metadata {id,key,value}, remove_deliverable {id}, add_deliverable {deliverable}.",
        "items": { "type": "object" }
    })
}

fn weights_schema() -> Value {
    let criterion = || json!({ "type": "number", "minimum": 0 });
    json!({
        "type": "object",
        "description": "Per-criterion weights for the combined score (lower is better). Each must be finite and >= 0.",
        "properties": {
            "makespan": criterion(),
            "p80": criterion(),
            "criticality_risk": criterion(),
            "total_effort": criterion(),
            "peak_load": criterion()
        },
        "additionalProperties": false
    })
}

fn calendar_schema() -> Value {
    json!({
        "type": "object",
        "description": "Working-time calendar: only working hours advance the PV clock. Omitted: wall-clock hours. A workday contributes hours_per_day hours starting at 00:00 local time.",
        "properties": {
            "hours_per_day": { "type": "number", "exclusiveMinimum": 0, "maximum": 24, "description": "Default 8." },
            "workdays": { "type": "array", "minItems": 1, "items": { "type": "string", "enum": ["mon", "tue", "wed", "thu", "fri", "sat", "sun"] }, "description": "Default mon..fri." },
            "utc_offset_minutes": { "type": "integer", "minimum": -1440, "maximum": 1440, "description": "Local offset from UTC; default 0." }
        },
        "additionalProperties": false
    })
}

fn capacities_schema() -> Value {
    json!({
        "type": "object",
        "description": "Units available per resource name. Every resource that carries work (a deliverable with nonzero scheduled length) needs a count of at least 1; a missing or zero entry for such a resource is INVALID_CAPACITIES. A resource with only milestones or zero-length work may be omitted or 0.",
        "additionalProperties": { "type": "integer", "minimum": 0 }
    })
}

/// Build the `Tool` definitions advertised in `list_tools` (one per
/// [`PLAN_TOOL_NAMES`] entry).
///
/// Each tool carries an inline JSON Schema describing its arguments. The
/// schemas are hand-written rather than derived because the workspace's
/// `schemars` version is pinned at 0.8 (matching `praxec-mcp-server`)
/// and the wire types here (`PlanGraph`, `DeliverableStatus`) live in
/// `praxec-core`, which currently does not derive `JsonSchema`. Adding
/// the derive workspace-wide is out of scope for PA4; the hand-written
/// schemas are explicit and reviewable.
pub fn plan_tool_definitions() -> Vec<Tool> {
    vec![
        Tool::new(
            Cow::Borrowed(TOOL_SUBMIT),
            Cow::Borrowed(
                "Submit a plan graph and receive a plan_id. \
                 Idempotent: identical graphs return the same plan_id. \
                 With `name` (and optional `project`/`variant`, variant \
                 defaults to \"main\"), registers a named variant instead: \
                 project defaults to the discovered project root.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "graph": graph_schema(),
                    "project": { "type": "string", "description": "Project key; required with `name` when no project root is discovered." },
                    "name": { "type": "string", "description": "Plan line name; when present the graph is synced as a named variant." },
                    "variant": { "type": "string", "description": "Variant name of a named plan (default \"main\")." }
                },
                "required": ["graph"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_ACQUIRE_COHORT),
            Cow::Borrowed(
                "Acquire up to max_count ready deliverables with no conflicting file claims \
                 atomically. Returns the cohort plus per-deliverable locks. \
                 A deliverable explicitly marked failed 3 times is \
                 circuit-broken to failed instead of re-leased; leases lost \
                 environmentally (TTL lapse, no terminal mark) never trip \
                 that breaker but are bounded separately (LAPSE_LIMIT at 10). \
                 Optional ttl_seconds sets the lease TTL (clamped to the \
                 server maximum). Optional ids targets specific deliverables \
                 and filter.metadata narrows by metadata equality; \
                 deliverables with metadata.kind = \"manual\" are never \
                 leased. ids that do not match filter.metadata are ignored \
                 (not reported in blocked). The response carries blocked \
                 [{id, code, reason}], blocked_count, and needs_operator \
                 (true when any blocked code is LAPSE_LIMIT: clear with \
                 plan.force_release {reset_counters: true}).",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id":   { "type": "string" },
                    "caller_id": { "type": "string" },
                    "max_count": { "type": "integer", "minimum": 1 },
                    "ids":       { "type": "array", "minItems": 1, "items": { "type": "string" } },
                    "ttl_seconds": { "type": "integer", "minimum": 1 },
                    "filter":    {
                        "type": "object",
                        "properties": { "metadata": { "type": "object" } },
                        "additionalProperties": false
                    }
                },
                "required": ["plan_id", "caller_id", "max_count"]
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_HEARTBEAT),
            Cow::Borrowed(
                "Refresh the TTL on a held lock; LOCK_NOT_HELD or LOCK_EXPIRED on failure. \
                 Optional ttl_seconds sets the new TTL (clamped to the server \
                 maximum); without it the lease is never shortened.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id":        { "type": "string" },
                    "deliverable_id": { "type": "string" },
                    "caller_id":      { "type": "string" },
                    "ttl_seconds":    { "type": "integer", "minimum": 1 }
                },
                "required": ["plan_id", "deliverable_id", "caller_id"]
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_MARK_STATUS),
            Cow::Borrowed(
                "Set a deliverable's status. Complete/Failed releases the lock; \
                 caller_id mismatch yields LOCK_NOT_HELD. Without a lock, \
                 Complete, Ready or InProgress requires all prerequisites \
                 complete (PREREQUISITES_INCOMPLETE) and is audited; other \
                 lockless marks are audited too, and an already-complete deliverable \
                 cannot be changed (LOCK_NOT_HELD). Optional earned-value progress: \
                 earned_pct (0..100, only with in_progress; ignored with complete), \
                 actual_effort_hours (total so far, 0..1000000; replaces leased hours \
                 as actual cost) and evidence (<= 2048 chars, appended to a list of \
                 at most 100); \
                 violations are INVALID_ACTUALS. Every lease that ends adds its hours \
                 to the deliverable's leased hours.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id":        { "type": "string" },
                    "deliverable_id": { "type": "string" },
                    "caller_id":      { "type": "string" },
                    "status": {
                        "type": "object",
                        "description": "Internally-tagged: {\"status\":\"pending|ready|in_progress|complete\"} or {\"status\":\"failed\",\"reason\":\"...\"}"
                    },
                    "earned_pct": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 100,
                        "description": "Percent complete (used by the weighted earning rule). Only with status in_progress; accepted and ignored with complete."
                    },
                    "actual_effort_hours": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 1000000,
                        "description": "Total effort spent so far, in hours; replaces the stored value and takes precedence over leased hours for actual cost."
                    },
                    "evidence": {
                        "type": "string",
                        "maxLength": 2048,
                        "description": "One evidence note, appended to the deliverable's evidence list (at most 100 entries)."
                    }
                },
                "required": ["plan_id", "deliverable_id", "caller_id", "status"]
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_STATUS),
            Cow::Borrowed(
                "Read-only snapshot: per-deliverable [id, status, attempt_count, \
                 failure_count, lapse_count] rows, critical_path (one real chain, always from __start__ to __finish__), \
                 critical_ids, per-deliverable schedule (es/ef/ls/lf/float, hours; \
                 synthetic __start__/__finish__ rows have synthetic=true), \
                 the ready set in cohort priority order, plan_complete, milestones (per milestone: id, critical_path from __start__, hours, complete), and held locks.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" }
                },
                "required": ["plan_id"]
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_GET),
            Cow::Borrowed(
                "Return the stored PlanGraph (deliverables, estimates, files, \
                 metadata) for a plan_id.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" }
                },
                "required": ["plan_id"]
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_FORCE_RELEASE),
            Cow::Borrowed(
                "Operator escape hatch — release a lock regardless of caller. \
                 Emits an audit event carrying `reason`.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id":        { "type": "string" },
                    "deliverable_id": { "type": "string" },
                    "reason":         { "type": "string" },
                    "reset_counters": {
                        "type": "boolean",
                        "description": "Also clear lapse and failure counters (revives a circuit-broken deliverable)."
                    }
                },
                "required": ["plan_id", "deliverable_id", "reason"]
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_ACCEPT),
            Cow::Borrowed(
                "Manager/owner acceptance: mark a deliverable Complete without \
                 holding its lease. Requires all prerequisites Complete and \
                 evidence; audited. A live lease held by someone else is refused \
                 (LOCK_HELD) unless override_lock is true.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id":        { "type": "string" },
                    "deliverable_id": { "type": "string" },
                    "accepted_by":    { "type": "string" },
                    "evidence":       { "type": "string" },
                    "override_lock": {
                        "type": "boolean",
                        "description": "Take over a live lease held by another caller."
                    }
                },
                "required": ["plan_id", "deliverable_id", "accepted_by", "evidence"]
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_LINT),
            Cow::Borrowed(
                "Lint a graph, stored plan, or plan file without creating one: cycles (with the \
                 loop), redundant edges, edges without rationale, interface edges \
                 not targeting a contract, deliverables feeding no milestone, and \
                 unordered file overlaps. Provide exactly one of graph, plan_id, or path.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "graph": graph_selector_schema(),
                    "plan_id": plan_id_schema(),
                    "path": path_schema()
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_SCHEDULE),
            Cow::Borrowed(
                "Level a graph or stored plan against resource capacities \
                 (`metadata.owner` by default): makespan, per-deliverable \
                 start/finish, per-resource load, the driving chain (dependency vs \
                 resource waits), and project/feeding buffers. Provide exactly one \
                 of graph or plan_id. capacities is required: every resource that \
                 carries work needs at least 1 unit (otherwise INVALID_CAPACITIES: \
                 lists the missing resources). plan.schedule rejects what \
                 plan.submit rejects (INVALID_GRAPH:).",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "graph": graph_selector_schema(),
                    "plan_id": plan_id_schema(),
                    "capacities": capacities_schema(),
                    "resource_key": {
                        "type": "string",
                        "description": "Metadata key naming a deliverable's resource (default \"owner\")."
                    },
                    "project_buffer_pct": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 100,
                        "description": "Project buffer as a percentage of the driving chain (default 25)."
                    }
                },
                "required": ["capacities"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_SIMULATE),
            Cow::Borrowed(
                "Read-only what-if for a graph, stored plan, or plan file (nothing is \
                 persisted): lint, critical path, schedule, milestones, optional \
                 resource schedule and Monte Carlo, and the scorecard. Provide exactly \
                 one of graph, plan_id, or path. schedule takes the plan.schedule inputs \
                 (capacities required; INVALID_CAPACITIES: when a working resource \
                 has none). monte_carlo takes iterations (1..50000, default 2000) \
                 and seed (default 0xC0FFEE); iterations × (deliverables + \
                 prerequisite edges) must not exceed 200000000. plan.simulate \
                 rejects what plan.submit rejects; lint errors or an invalid graph \
                 are INVALID_GRAPH:.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "graph": graph_selector_schema(),
                    "plan_id": plan_id_schema(),
                    "path": path_schema(),
                    "schedule": {
                        "type": "object",
                        "description": "Level the plan (same inputs as plan.schedule).",
                        "properties": {
                            "capacities": capacities_schema(),
                            "resource_key": { "type": "string", "description": "Metadata key naming a deliverable's resource (default \"owner\")." },
                            "project_buffer_pct": { "type": "number", "minimum": 0, "maximum": 100, "description": "Project buffer as a percentage of the driving chain (default 25)." }
                        },
                        "required": ["capacities"],
                        "additionalProperties": false
                    },
                    "monte_carlo": {
                        "type": "object",
                        "description": "Seeded Monte Carlo over three-point estimates. iterations × (deliverables + prerequisite edges) must not exceed 200000000.",
                        "properties": {
                            "iterations": { "type": "integer", "minimum": 1, "maximum": 50000, "description": "Simulated schedules (default 2000)." },
                            "seed": { "type": "integer", "minimum": 0, "description": "RNG seed (default 12648430 = 0xC0FFEE); the same seed reproduces the output on the same platform and build." }
                        },
                        "additionalProperties": false
                    }
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_SYNC),
            Cow::Borrowed(
                "Register or update one variant of a named plan line from a plan \
                 file (`path`) or an inline `graph`. A path is read from \
                 .cpm-planner/plans/<name>/<variant>.json and tracked by content hash \
                 for drift detection. An inline graph requires `name`; `variant` \
                 defaults to \"main\". `project` defaults to the discovered project \
                 root. `force` releases live locks of removed deliverables.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "path": path_schema(),
                    "graph": graph_schema(),
                    "project": { "type": "string", "description": "Project key; defaults to the discovered project root." },
                    "name": { "type": "string", "description": "Plan line name (required with an inline graph)." },
                    "variant": { "type": "string", "description": "Variant name (default \"main\" for an inline graph)." },
                    "force": { "type": "boolean", "description": "Release live locks of removed deliverables instead of refusing." }
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_LIST),
            Cow::Borrowed(
                "List every plan line of `project` (default: the discovered project \
                 root), sorted by name with each line's variants sorted by variant. \
                 Archived lines and variants are omitted unless include_archived.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project key; defaults to the discovered project root." },
                    "include_archived": { "type": "boolean", "description": "Include archived lines and variants (default false)." }
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_EXPORT),
            Cow::Borrowed(
                "Write the head graph of `plan_id` under the project root and return its \
                 root-relative path: to `path` when given (confined to \
                 .cpm-planner/plans/<name>/<variant>.json), else to its own variant \
                 file. Refuses (INVALID_PATH) another variant's tracked plan file, or \
                 the variant's own file while it holds local edits never synced, \
                 unless force. The written file is not synced; exporting to the \
                 variant's own tracked file records its hash, so drift is false.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" },
                    "path": path_schema(),
                    "force": { "type": "boolean", "description": "Overwrite another variant's tracked file or unsynced local edits." }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_REVISE),
            Cow::Borrowed(
                "Replace a plan's graph in place, carrying progress over: unchanged \
                 deliverables keep status and counters; changed or reopened ones are \
                 re-derived. Removed deliverables holding live locks are refused \
                 (LOCK_HELD) unless force. Returns the new revision and the diff.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" },
                    "graph": graph_schema(),
                    "force": { "type": "boolean", "description": "Release live locks of removed deliverables instead of refusing." }
                },
                "required": ["plan_id", "graph"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_FORK),
            Cow::Borrowed(
                "Copy the head graph of a named variant, apply `edits` in order, and \
                 register the result as a new draft (not selected) variant of the same \
                 line. Fails if the variant already exists.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string", "description": "The named variant to copy." },
                    "variant": { "type": "string", "description": "Name of the new draft variant." },
                    "edits": edits_schema()
                },
                "required": ["plan_id", "variant"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_SELECT),
            Cow::Borrowed(
                "Make the named variant owning `plan_id` its line's selected (the only \
                 executable) variant. Progress carries over from the previously \
                 selected variant. Live locks on the previous variant refuse selection \
                 with LOCK_HELD unless force releases them (audited).",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" },
                    "force": { "type": "boolean", "description": "Release live locks on the previously selected variant." }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_ARCHIVE),
            Cow::Borrowed(
                "Archive (archived defaults to true) or unarchive a whole plan line \
                 (`variant` omitted) or one variant. Archived variants stay readable \
                 but are hidden from plan.list unless include_archived, and refuse sync, \
                 selection and execution (ARCHIVE_REFUSED).",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Plan line name." },
                    "variant": { "type": "string", "description": "One variant; omitted archives the whole line." },
                    "project": { "type": "string", "description": "Project key; defaults to the discovered project root." },
                    "archived": { "type": "boolean", "description": "False unarchives (default true)." },
                    "force": { "type": "boolean", "description": "Release live locks on the selected variant when archiving a line." }
                },
                "required": ["name"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_COMPARE),
            Cow::Borrowed(
                "Compare stored plans on the scorecard, returning the Pareto front, \
                 weighted rank and a recommended plan. Give exactly one of `plan_ids` \
                 (2 to 16 distinct plans) or `plan` (a line name in `project`: every \
                 non-archived variant, at most 16). Read-only. Weights must be finite \
                 and >= 0. With monte_carlo, iterations × (deliverables + prerequisite \
                 edges) summed over all variants must not exceed 200000000.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_ids": { "type": "array", "minItems": 2, "maxItems": 16, "uniqueItems": true, "items": { "type": "string" } },
                    "plan": { "type": "string", "description": "Plan line name; compares every live variant." },
                    "project": { "type": "string", "description": "Project key of `plan`; defaults to the discovered project root." },
                    "schedule": {
                        "type": "object",
                        "description": "Level every variant (same inputs as plan.schedule).",
                        "properties": {
                            "capacities": capacities_schema(),
                            "resource_key": { "type": "string" },
                            "project_buffer_pct": { "type": "number", "minimum": 0, "maximum": 100 }
                        },
                        "required": ["capacities"],
                        "additionalProperties": false
                    },
                    "monte_carlo": {
                        "type": "object",
                        "description": "Run Monte Carlo on every variant (P80 then comes from it).",
                        "properties": {
                            "iterations": { "type": "integer", "minimum": 1, "maximum": 50000 },
                            "seed": { "type": "integer", "minimum": 0 }
                        },
                        "additionalProperties": false
                    },
                    "weights": weights_schema()
                },
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_BASELINE),
            Cow::Borrowed(
                "Freeze the plan's current CPM schedule (earliest start/finish) and budgets \
                 (effort basis x metadata.cost_rate) as its next numbered baseline for earned \
                 value. The first baseline is number 1; re-baselining needs a non-blank \
                 `reason` (INVALID_GRAPH otherwise) and keeps actuals and snapshots. \
                 Without `calendar`, a re-baseline keeps the previous baseline's calendar \
                 (the first baseline counts wall-clock hours). Execution-side: the plan must be its line's selected, unarchived variant \
                 (VARIANT_NOT_SELECTED / ARCHIVE_REFUSED). Audited as plan.ev.baselined.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" },
                    "start": { "type": "string", "format": "date-time", "description": "RFC 3339 instant the PV clock starts from; defaults to now." },
                    "calendar": calendar_schema(),
                    "reason": { "type": "string", "maxLength": 2048, "description": "Why the plan is re-baselined; required when a baseline exists." }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_EV),
            Cow::Borrowed(
                "Earned-value report against the plan's latest baseline as of `as_of` \
                 (default now). `as_of` is the PV status date; EV and AC reflect progress \
                 and actuals recorded up to the moment the call runs. Returns BAC, PV, EV, \
                 AC, SV, CV, SPI, CPI, EAC, ETC, VAC, TCPI, per-deliverable rows (a \
                 baselined deliverable later removed by plan.revise has status `removed` \
                 and keeps the percent it had earned: 100 if complete), critical float \
                 consumed, SPI_BELOW_0_9 / CPI_BELOW_0_9 alerts from the two latest \
                 stored non-backfilled snapshots (by as_of) of the current baseline, and \
                 deliverables added since the baseline. AC counts every recorded hour of \
                 the plan, including removed and unbaselined deliverables (rate 1 when the \
                 baseline has no row). A \
                 ratio whose denominator is 0 is null and explained in `undefined`. \
                 Read-only; works on any variant. \
                 NOT_BASELINED before plan.baseline.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" },
                    "as_of": { "type": "string", "format": "date-time", "description": "RFC 3339 PV status date; defaults to now. EV and AC are always as recorded when the call runs." }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_SNAPSHOT),
            Cow::Borrowed(
                "Compute the earned-value report (as plan.ev) and append it as a \
                 snapshot. `as_of` is the PV status date; EV and AC are the progress \
                 recorded when the call runs, so a snapshot whose as_of is more than an \
                 hour before its taken_at is marked `backfilled: true` and ignored by \
                 alerts. Returns the snapshot summary (alerts consider this snapshot and \
                 the latest earlier non-backfilled one by as_of of the current baseline) \
                 and an export \
                 of the newest 100 snapshots by as_of, oldest first (an older backfill \
                 is counted but not listed): a list of \
                 summaries (format json, default) or a Markdown table with columns \
                 date, PV, EV, AC, SPI, CPI, EAC (format markdown). \
                 Execution-side like plan.baseline. NOT_BASELINED before plan.baseline.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string" },
                    "as_of": { "type": "string", "format": "date-time", "description": "RFC 3339 PV status date; defaults to now. EV and AC are always as recorded when the call runs." },
                    "format": { "type": "string", "enum": ["json", "markdown"], "description": "Export format (default json)." }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            })),
        ),
    ]
}

/// Convert a `serde_json::Value` (always built from an object literal in
/// this file) into the `Arc<JsonObject>` rmcp expects for `input_schema`.
fn schema_object(value: Value) -> Arc<rmcp::model::JsonObject> {
    // Invariant: every caller passes a `json!({ ... })` object literal.
    // `debug_assert!` so dev/test builds crash loudly if a future edit drops
    // a non-object literal here; production retains the no-panic fallback
    // to satisfy `clippy::unwrap_used`.
    debug_assert!(
        value.is_object(),
        "schema_object expects an object literal; got non-object"
    );
    let obj = match value.as_object() {
        Some(o) => o.clone(),
        None => serde_json::Map::new(),
    };
    Arc::new(obj)
}

// ---------------------------------------------------------------------------
// PlanServer
// ---------------------------------------------------------------------------

/// MCP server façade exposing a [`BasicCpmPlanner`] over twenty-two tools.
#[derive(Clone)]
pub struct PlanServer {
    planner: Arc<BasicCpmPlanner>,
    server_name: String,
    server_version: String,
}

impl PlanServer {
    /// Build a server backed by the supplied planner.
    pub fn new(planner: Arc<BasicCpmPlanner>) -> Self {
        Self {
            planner,
            server_name: "cpm-planner".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Override the advertised server identity. Defaults to
    /// `("cpm-planner", CARGO_PKG_VERSION)`.
    pub fn with_identity(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.server_name = name.into();
        self.server_version = version.into();
        self
    }

    /// Borrow the inner planner. Tests use this to set up state directly
    /// (e.g. submit a plan, then drive `acquire_cohort` via MCP).
    pub fn planner(&self) -> &Arc<BasicCpmPlanner> {
        &self.planner
    }

    /// The planner's project root ([`BasicCpmPlanner::project_root`], the
    /// single source of truth), or `INVALID_PATH` when there is none and the
    /// tool needs one. Path-based tools read and write under
    /// `<root>/.cpm-planner/plans/`; the default project key is
    /// `root.project_key()`.
    fn project_root(&self) -> Result<&ProjectRoot, McpError> {
        self.planner.project_root().ok_or_else(|| {
            McpError::internal_error(
                "INVALID_PATH: no project root (set CPM_PROJECT_ROOT or run inside a repo)",
                None,
            )
        })
    }

    /// The default project key: the discovered root's canonical path.
    fn default_project(&self) -> Result<String, McpError> {
        Ok(self.project_root()?.project_key())
    }

    /// Resolve the read-only analysis tools' graph selector: exactly one of
    /// an inline `graph`, a stored `plan_id`, or a plan-file `path`; any
    /// other combination is an invalid-params error.
    async fn resolve_graph(
        &self,
        graph: Option<PlanGraph>,
        plan_id: Option<String>,
        path: Option<String>,
    ) -> Result<PlanGraph, McpError> {
        let supplied = [graph.is_some(), plan_id.is_some(), path.is_some()]
            .iter()
            .filter(|present| **present)
            .count();
        if supplied != 1 {
            return Err(McpError::invalid_params(
                "provide exactly one of graph, plan_id, or path",
                None,
            ));
        }
        match (graph, plan_id, path) {
            (Some(graph), None, None) => Ok(graph),
            (None, Some(plan_id), None) => {
                let definition = self
                    .planner
                    .get_plan(&PlanId(plan_id))
                    .await
                    .map_err(planner_error_to_mcp)?;
                Ok(definition.graph)
            }
            (None, None, Some(path)) => {
                let root = self.project_root()?;
                let file = root
                    .resolve_plan_file(&path)
                    .map_err(planner_error_to_mcp)?;
                let (graph, _) = root.read_graph(&file).map_err(planner_error_to_mcp)?;
                Ok(graph)
            }
            // `supplied == 1` guarantees one of the arms above.
            _ => Err(McpError::invalid_params(
                "provide exactly one of graph, plan_id, or path",
                None,
            )),
        }
    }

    /// Serve the MCP surface over stdio. Blocks until the peer disconnects.
    ///
    /// No SIGINT/drain handling is wired here yet; a future revision can
    /// adopt the cancellation-token pattern from
    /// `crates/praxec/src/main.rs::serve` if graceful shutdown becomes
    /// necessary for operators spawning this binary as a long-running child.
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        let service = self.serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    }

    /// Transport-free dispatch entry point. Tests call this directly to
    /// exercise each tool without spinning up a stdio transport.
    ///
    /// Behaviour matches what `ServerHandler::call_tool` does, minus the
    /// `CallToolResult` wrapping.
    pub async fn dispatch_call(&self, request: CallToolRequestParams) -> Result<Value, McpError> {
        let args: Value = request
            .arguments
            .as_ref()
            .map(|m| Value::Object(m.clone()))
            .unwrap_or_else(|| json!({}));

        match request.name.as_ref() {
            TOOL_SUBMIT => self.handle_submit(args).await,
            TOOL_ACQUIRE_COHORT => self.handle_acquire_cohort(args).await,
            TOOL_HEARTBEAT => self.handle_heartbeat(args).await,
            TOOL_MARK_STATUS => self.handle_mark_status(args).await,
            TOOL_STATUS => self.handle_status(args).await,
            TOOL_GET => self.handle_get(args).await,
            TOOL_FORCE_RELEASE => self.handle_force_release(args).await,
            TOOL_ACCEPT => self.handle_accept(args).await,
            TOOL_LINT => self.handle_lint(args).await,
            TOOL_SCHEDULE => self.handle_schedule(args).await,
            TOOL_SIMULATE => self.handle_simulate(args).await,
            TOOL_SYNC => self.handle_sync(args).await,
            TOOL_LIST => self.handle_list(args).await,
            TOOL_EXPORT => self.handle_export(args).await,
            TOOL_REVISE => self.handle_revise(args).await,
            TOOL_FORK => self.handle_fork(args).await,
            TOOL_SELECT => self.handle_select(args).await,
            TOOL_ARCHIVE => self.handle_archive(args).await,
            TOOL_COMPARE => self.handle_compare(args).await,
            TOOL_BASELINE => self.handle_baseline(args).await,
            TOOL_EV => self.handle_ev(args).await,
            TOOL_SNAPSHOT => self.handle_snapshot(args).await,
            other => Err(McpError::invalid_params(
                format!(
                    "Unknown tool '{other}'. Available: {}.",
                    PLAN_TOOL_NAMES.join(", ")
                ),
                None,
            )),
        }
    }

    // -------------------------------------------------------------------
    // Per-tool handlers
    // -------------------------------------------------------------------

    async fn handle_submit(&self, args: Value) -> Result<Value, McpError> {
        let parsed: SubmitArgs = parse_args(args)?;
        match parsed.name {
            None => {
                let plan_id = self
                    .planner
                    .submit_plan(parsed.graph)
                    .await
                    .map_err(planner_error_to_mcp)?;
                to_value(&SubmitResponse { plan_id: plan_id.0 })
            }
            Some(name) => {
                let project = match parsed.project {
                    Some(project) => project,
                    None => self.default_project()?,
                };
                let variant = parsed.variant.unwrap_or_else(|| "main".to_string());
                let outcome = self
                    .planner
                    .sync_plan(SyncRequest::new(project, name, variant, parsed.graph))
                    .await
                    .map_err(planner_error_to_mcp)?;
                to_value(&outcome)
            }
        }
    }

    async fn handle_acquire_cohort(&self, args: Value) -> Result<Value, McpError> {
        let parsed: AcquireCohortArgs = parse_args(args)?;
        if parsed.ids.as_ref().is_some_and(Vec::is_empty) {
            return Err(McpError::invalid_params(
                "ids must be non-empty when provided",
                None,
            ));
        }
        let mut request = AcquireRequest::new(
            PlanId(parsed.plan_id),
            CallerId(parsed.caller_id),
            parsed.max_count,
        );
        if let Some(ids) = parsed.ids {
            request = request.with_ids(ids);
        }
        if let Some(filter) = parsed.filter.and_then(|f| f.metadata) {
            request = request.with_metadata_filter(filter);
        }
        if let Some(ttl) = ttl_from_seconds(parsed.ttl_seconds)? {
            request = request.with_ttl(ttl);
        }
        let cohort: Cohort = self
            .planner
            .acquire_cohort(request)
            .await
            .map_err(planner_error_to_mcp)?;
        // A SCALAR termination signal for declarative cohort drivers: a
        // state-machine guard-expr (e.g. praxec's) can't test array
        // emptiness and fails-fast on a missing path, so a loop that calls
        // acquire_cohort repeatedly until the plan is drained guards on
        // `exhausted == true` rather than inspecting `rows`. True when this
        // acquisition returned no rows (nothing ready / all complete).
        let exhausted = cohort.rows.is_empty();
        let blocked_count = cohort.blocked.len();
        let needs_operator = cohort.blocked.iter().any(|b| b.code == "LAPSE_LIMIT");
        let mut value = to_value(&cohort)?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("exhausted".to_string(), Value::Bool(exhausted));
            obj.insert("blocked_count".to_string(), json!(blocked_count));
            obj.insert("needs_operator".to_string(), Value::Bool(needs_operator));
        }
        Ok(value)
    }

    async fn handle_heartbeat(&self, args: Value) -> Result<Value, McpError> {
        let parsed: HeartbeatArgs = parse_args(args)?;
        let mut request = HeartbeatRequest::new(
            PlanId(parsed.plan_id),
            parsed.deliverable_id,
            CallerId(parsed.caller_id),
        );
        if let Some(ttl) = ttl_from_seconds(parsed.ttl_seconds)? {
            request = request.with_ttl(ttl);
        }
        self.planner
            .heartbeat(request)
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&OkResponse::new())
    }

    async fn handle_mark_status(&self, args: Value) -> Result<Value, McpError> {
        let parsed: MarkStatusArgs = parse_args(args)?;
        crate::planner::validate_actuals(
            &parsed.status,
            parsed.earned_pct,
            parsed.actual_effort_hours,
            parsed.evidence.as_deref(),
        )
        .map_err(planner_error_to_mcp)?;
        let mut request = MarkStatusRequest::new(
            PlanId(parsed.plan_id),
            parsed.deliverable_id,
            CallerId(parsed.caller_id),
            parsed.status,
        );
        // Validated above: the percent is an integer 0..=100 and the hours
        // fit an f32.
        if let Some(pct) = parsed.earned_pct {
            request = request.with_earned_pct(pct as u8);
        }
        if let Some(hours) = parsed.actual_effort_hours {
            request = request.with_actual_effort_hours(hours as f32);
        }
        if let Some(evidence) = parsed.evidence {
            request = request.with_evidence(evidence);
        }
        self.planner
            .mark_status(request)
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&OkResponse::new())
    }

    async fn handle_status(&self, args: Value) -> Result<Value, McpError> {
        let parsed: StatusArgs = parse_args(args)?;
        let status: PlanStatus = self
            .planner
            .status(&PlanId(parsed.plan_id))
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&status)
    }

    async fn handle_get(&self, args: Value) -> Result<Value, McpError> {
        let parsed: GetArgs = parse_args(args)?;
        let definition: PlanDefinition = self
            .planner
            .get_plan(&PlanId(parsed.plan_id))
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&definition)
    }

    async fn handle_accept(&self, args: Value) -> Result<Value, McpError> {
        let parsed: AcceptArgs = parse_args(args)?;
        self.planner
            .accept(
                AcceptRequest::new(
                    PlanId(parsed.plan_id),
                    parsed.deliverable_id,
                    parsed.accepted_by,
                    parsed.evidence,
                )
                .override_lock(parsed.override_lock),
            )
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&OkResponse::new())
    }

    async fn handle_force_release(&self, args: Value) -> Result<Value, McpError> {
        let parsed: ForceReleaseArgs = parse_args(args)?;
        self.planner
            .force_release(
                ForceReleaseRequest::new(
                    PlanId(parsed.plan_id),
                    parsed.deliverable_id,
                    parsed.reason,
                )
                .reset_counters(parsed.reset_counters),
            )
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&OkResponse::new())
    }

    async fn handle_lint(&self, args: Value) -> Result<Value, McpError> {
        let parsed: GraphOrPlanIdArgs = parse_args(args)?;
        let graph = self
            .resolve_graph(parsed.graph, parsed.plan_id, parsed.path)
            .await?;
        let report = run_blocking(move || Ok(crate::lint::lint(&graph))).await?;
        to_value(&report)
    }

    async fn handle_schedule(&self, args: Value) -> Result<Value, McpError> {
        let parsed: ScheduleToolArgs = parse_args(args)?;
        let request = ScheduleRequest {
            capacities: parsed.capacities,
            resource_key: parsed.resource_key,
            project_buffer_pct: parsed.project_buffer_pct,
        };
        check_schedule_params(&request)?;
        let graph = self
            .resolve_graph(parsed.graph, parsed.plan_id, None)
            .await?;
        let schedule = run_blocking(move || resource_schedule(&graph, &request)).await?;
        to_value(&schedule)
    }

    async fn handle_simulate(&self, args: Value) -> Result<Value, McpError> {
        let parsed: SimulateToolArgs = parse_args(args)?;
        let request = SimulateRequest {
            schedule: parsed.schedule,
            monte_carlo: parsed.monte_carlo,
        };
        if let Some(schedule) = &request.schedule {
            check_schedule_params(schedule)?;
        }
        if let Some(mc) = &request.monte_carlo {
            crate::monte_carlo::check_iterations(mc.iterations)
                .map_err(|reason| McpError::invalid_params(reason, None))?;
        }
        let graph = self
            .resolve_graph(parsed.graph, parsed.plan_id, parsed.path)
            .await?;
        let result = run_blocking(move || crate::simulate::simulate(&graph, &request)).await?;
        to_value(&result)
    }

    // ── Portfolio handlers ─────────────────────────────────────────────────

    async fn handle_sync(&self, args: Value) -> Result<Value, McpError> {
        let parsed: SyncArgs = parse_args(args)?;
        let (graph, source) = match (parsed.path, parsed.graph) {
            (Some(_), Some(_)) => {
                return Err(McpError::invalid_params(
                    "provide exactly one of path or graph",
                    None,
                ));
            }
            (Some(path), None) => {
                let root = self.project_root()?;
                let file = root
                    .resolve_plan_file(&path)
                    .map_err(planner_error_to_mcp)?;
                let (graph, hash) = root.read_graph(&file).map_err(planner_error_to_mcp)?;
                (graph, Some((file, hash)))
            }
            (None, Some(graph)) => (graph, None),
            (None, None) => {
                return Err(McpError::invalid_params(
                    "plan.sync requires exactly one of path or graph",
                    None,
                ));
            }
        };
        let project = match parsed.project {
            Some(project) => project,
            None => self.default_project()?,
        };
        let (name, variant) = match (&source, parsed.name, parsed.variant) {
            (Some((file, _)), name, variant) => {
                // The path names the variant: an explicit name, variant or
                // project may only repeat it.
                let root_key = self.project_root()?.project_key();
                if name.as_ref().is_some_and(|n| *n != file.name)
                    || variant.as_ref().is_some_and(|v| *v != file.variant)
                    || project != root_key
                {
                    return Err(planner_error_to_mcp(PlannerError::InvalidPath {
                        reason: "name/variant/project must match the plan file path".to_string(),
                    }));
                }
                (file.name.clone(), file.variant.clone())
            }
            (None, Some(name), variant) => (name, variant.unwrap_or_else(|| "main".to_string())),
            (None, None, _) => {
                return Err(McpError::invalid_params(
                    "an inline graph requires name",
                    None,
                ));
            }
        };
        let mut request = SyncRequest::new(project, name, variant, graph).force(parsed.force);
        if let Some((file, hash)) = source {
            request = request
                .with_source_path(file.rel_path)
                .with_content_hash(hash);
        }
        let outcome = self
            .planner
            .sync_plan(request)
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&outcome)
    }

    async fn handle_list(&self, args: Value) -> Result<Value, McpError> {
        let parsed: ListArgs = parse_args(args)?;
        let project = match parsed.project {
            Some(project) => project,
            None => self.default_project()?,
        };
        let lines = self
            .planner
            .list_plans(&project, parsed.include_archived)
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&lines)
    }

    async fn handle_export(&self, args: Value) -> Result<Value, McpError> {
        let parsed: ExportArgs = parse_args(args)?;
        let root = self.project_root()?;
        let path = self
            .planner
            .export_plan(
                &PlanId(parsed.plan_id),
                root,
                parsed.path.as_deref(),
                parsed.force,
            )
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&ExportResponse { path })
    }

    async fn handle_revise(&self, args: Value) -> Result<Value, McpError> {
        let parsed: ReviseArgs = parse_args(args)?;
        let (revision, diff) = self
            .planner
            .revise_plan(
                ReviseRequest::new(PlanId(parsed.plan_id), parsed.graph).force(parsed.force),
            )
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&ReviseResponse { revision, diff })
    }

    async fn handle_fork(&self, args: Value) -> Result<Value, McpError> {
        let parsed: ForkArgs = parse_args(args)?;
        let outcome = self
            .planner
            .fork_plan(
                ForkRequest::new(PlanId(parsed.plan_id), parsed.variant).with_edits(parsed.edits),
            )
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&outcome)
    }

    async fn handle_select(&self, args: Value) -> Result<Value, McpError> {
        let parsed: SelectArgs = parse_args(args)?;
        let outcome = self
            .planner
            .select_variant(&PlanId(parsed.plan_id), parsed.force)
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&outcome)
    }

    async fn handle_archive(&self, args: Value) -> Result<Value, McpError> {
        let parsed: ArchiveArgs = parse_args(args)?;
        let project = match parsed.project {
            Some(project) => project,
            None => self.default_project()?,
        };
        self.planner
            .archive(
                &project,
                &parsed.name,
                parsed.variant.as_deref(),
                parsed.archived,
                parsed.force,
            )
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&OkResponse::new())
    }

    async fn handle_compare(&self, args: Value) -> Result<Value, McpError> {
        let parsed: CompareArgs = parse_args(args)?;
        check_compare_weights(&parsed.weights)?;
        if let Some(schedule) = &parsed.schedule {
            check_schedule_params(schedule)?;
        }
        if let Some(mc) = &parsed.monte_carlo {
            crate::monte_carlo::check_iterations(mc.iterations)
                .map_err(|reason| McpError::invalid_params(reason, None))?;
        }
        let request = CompareRequest {
            schedule: parsed.schedule,
            monte_carlo: parsed.monte_carlo,
            weights: parsed.weights,
        };
        let compare = match (parsed.plan_ids, parsed.plan) {
            (Some(ids), None) => {
                let distinct: std::collections::HashSet<&str> =
                    ids.iter().map(String::as_str).collect();
                if distinct.len() != ids.len() {
                    return Err(McpError::invalid_params("plan_ids must be distinct", None));
                }
                ComparePlansRequest::by_ids(ids.into_iter().map(PlanId).collect(), request)
            }
            (None, Some(name)) => {
                let project = match parsed.project {
                    Some(project) => project,
                    None => self.default_project()?,
                };
                ComparePlansRequest::by_plan(project, name, request)
            }
            _ => {
                return Err(McpError::invalid_params(
                    "provide exactly one of plan_ids or plan",
                    None,
                ));
            }
        };
        // Gather inputs from the store, then score off the async runtime.
        let inputs = self
            .planner
            .compare_inputs(&compare)
            .map_err(planner_error_to_mcp)?;
        let request = compare.request;
        let comparison = run_blocking(move || crate::compare::compare(&inputs, &request)).await?;
        to_value(&comparison)
    }

    // ── Earned-value handlers ──────────────────────────────────────────────

    async fn handle_baseline(&self, args: Value) -> Result<Value, McpError> {
        let parsed: BaselineArgs = parse_args(args)?;
        let mut request = BaselineRequest::new(PlanId(parsed.plan_id));
        request.start = parsed.start;
        request.calendar = parsed.calendar;
        request.reason = parsed.reason;
        let outcome = self
            .planner
            .baseline(request)
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&outcome)
    }

    async fn handle_ev(&self, args: Value) -> Result<Value, McpError> {
        let parsed: EvArgs = parse_args(args)?;
        let planner = Arc::clone(&self.planner);
        let plan_id = PlanId(parsed.plan_id);
        let report =
            run_blocking_planner(
                async move { Planner::ev(&*planner, &plan_id, parsed.as_of).await },
            )
            .await?;
        to_value(&report)
    }

    async fn handle_snapshot(&self, args: Value) -> Result<Value, McpError> {
        let parsed: SnapshotArgs = parse_args(args)?;
        let mut request = SnapshotRequest::new(PlanId(parsed.plan_id)).with_format(parsed.format);
        request.as_of = parsed.as_of;
        let planner = Arc::clone(&self.planner);
        let outcome =
            run_blocking_planner(async move { Planner::snapshot(&*planner, request).await })
                .await?;
        to_value(&outcome)
    }
}

/// Range checks the analysis tools apply before any work, as
/// `invalid_params`. The library repeats them as `INVALID_GRAPH`.
fn check_schedule_params(request: &ScheduleRequest) -> Result<(), McpError> {
    crate::resource_schedule::check_buffer_pct(request.project_buffer_pct)
        .map_err(|reason| McpError::invalid_params(reason, None))
}

/// `plan.compare` rejects any weight that is not finite and `>= 0`.
fn check_compare_weights(weights: &CompareWeights) -> Result<(), McpError> {
    let criteria = [
        ("makespan", weights.makespan),
        ("p80", weights.p80),
        ("criticality_risk", weights.criticality_risk),
        ("total_effort", weights.total_effort),
        ("peak_load", weights.peak_load),
    ];
    for (name, value) in criteria {
        if !value.is_finite() || value < 0.0 {
            return Err(McpError::invalid_params(
                format!("weights.{name} must be finite and >= 0"),
                None,
            ));
        }
    }
    Ok(())
}

/// Run a [`Planner`] call whose future does synchronous store and CPM work
/// (`plan.ev`, `plan.snapshot`) on a blocking thread, driven to completion
/// there, so it never stalls the runtime's worker threads.
async fn run_blocking_planner<T, Fut>(call: Fut) -> Result<T, McpError>
where
    T: Send + 'static,
    Fut: std::future::Future<Output = Result<T, PlannerError>> + Send + 'static,
{
    run_blocking(move || tokio::runtime::Handle::current().block_on(call)).await
}

/// Run CPU-bound analysis off the async runtime's worker threads.
async fn run_blocking<T, F>(work: F) -> Result<T, McpError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PlannerError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| McpError::internal_error(format!("analysis task failed: {e}"), None))?
        .map_err(planner_error_to_mcp)
}

// ---------------------------------------------------------------------------
// ServerHandler impl
// ---------------------------------------------------------------------------

impl ServerHandler for PlanServer {
    fn get_info(&self) -> ServerInfo {
        let mut server_info =
            Implementation::new(self.server_name.clone(), self.server_version.clone());
        server_info.title = Some("cpm-planner".to_string());
        server_info.description = Some(
            "MCP server exposing the open-source Praxec CPM planner via twenty-two tools."
                .to_string(),
        );

        let mut info = InitializeResult::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info.instructions = Some(instructions().to_string());
        info
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(plan_tool_definitions()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch_call(request)
            .await
            .map(CallToolResult::structured)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        plan_tool_definitions().into_iter().find(|t| t.name == name)
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        tracing::info!("cpm-planner client initialized");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse tool arguments, mapping serde failures to `invalid_params`.
fn parse_args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, McpError> {
    serde_json::from_value(args)
        .map_err(|e| McpError::invalid_params(format!("invalid arguments: {e}"), None))
}

/// Convert an optional wire `ttl_seconds` into a lease TTL.
///
/// `None` means "use the planner default". `Some(0)` is rejected here, at
/// the server layer, as invalid params; positive values pass through and
/// the planner clamps them to its configured maximum.
fn ttl_from_seconds(secs: Option<u64>) -> Result<Option<Duration>, McpError> {
    match secs {
        None => Ok(None),
        Some(0) => Err(McpError::invalid_params(
            "ttl_seconds must be >= 1 when provided",
            None,
        )),
        Some(s) => Ok(Some(Duration::from_secs(s))),
    }
}

/// Serialise a response into a JSON `Value`, mapping serde failures to
/// `internal_error`. Each response type is a small struct or a wire type
/// that already derives `Serialize`; this fallible boundary exists so the
/// crate-level `clippy::unwrap_used` lint stays clean.
fn to_value<T: Serialize>(value: &T) -> Result<Value, McpError> {
    serde_json::to_value(value)
        .map_err(|e| McpError::internal_error(format!("response serialisation failed: {e}"), None))
}

/// Map a [`PlannerError`] into an MCP `internal_error` whose message is
/// the variant's `Display` output. The error message starts with the
/// stable code prefix (e.g. `LOCK_HELD:`, `LOCK_NOT_HELD:`,
/// `INVALID_GRAPH:`) so clients can pattern-match on the prefix to drive
/// retry / triage logic without relying on free-form text.
///
/// Per SPEC §33 PA4 FMECA F2: operators need structured error codes, not
/// generic strings.
fn planner_error_to_mcp(err: PlannerError) -> McpError {
    McpError::internal_error(err.to_string(), None)
}

/// `instructions()` is surfaced via `InitializeResult.instructions` so a
/// connecting agent gets a one-shot orientation to the tool surface.
fn instructions() -> &'static str {
    r#"This is the cpm-planner MCP server — the open-source CPM planner.

Tools (twenty-two total, all `plan.<verb>`):
  plan.submit          — submit a PlanGraph, get a plan_id (idempotent on identical graphs)
                        a prerequisite is an id string or {id, consumes?, kind?: artifact|interface, lag_hours?}; a deliverable's duration_hours (calendar time; when absent the default is the effort estimate, explicit or estimator-derived) and lag_hours (minimum wait after a prerequisite finishes) drive the schedule
                        a milestone (milestone: true) is zero-length unless you give it an estimate or duration; it is still an ordinary deliverable someone must complete (accept or mark Complete), and it is not leased if metadata.kind=manual
                        an optional earning_rule (zero_hundred default: 100% only when complete; fifty_fifty: 50% once in progress or any earned_pct is reported; weighted: the reported earned_pct) sets how earned value credits progress; it is part of the plan's identity
                        an optional estimate {optimistic, likely, pessimistic} (0 <= optimistic <= likely <= pessimistic) is a three-point effort estimate; scheduled length precedence is duration_hours > estimated_effort_hours > estimate.likely > 0 for a milestone > estimator default, and Monte Carlo samples the estimate only when neither duration_hours nor estimated_effort_hours is set
                        limits: at most 5000 deliverables; every hour value (effort, duration, lag, estimate) must be finite and between 0 and 1000000 (INVALID_GRAPH)
  plan.acquire_cohort  — atomically acquire ready deliverables with no conflicting file claims (an owned_files entry may be {path, mode: "append"}: append claims on one path may be co-leased and are listed in the response's shared_paths; exclusive claims never overlap anything at once; plan.submit accepts a shared file only when the claimants are ordered by prerequisites, or all claims are append)
  plan.heartbeat       — refresh a held lock's TTL
  plan.mark_status     — set a deliverable's status (Complete/Failed releases the lock); lockless Complete/Ready/InProgress requires complete prerequisites (PREREQUISITES_INCOMPLETE) and is audited
                        optional earned-value progress: earned_pct (integer 0..100, only with in_progress; accepted and ignored with complete), actual_effort_hours (total so far, finite 0..1000000; replaces leased hours as actual cost), evidence (<= 2048 chars, appended to the deliverable's list, at most 100 entries); violations are INVALID_ACTUALS. plan.select copies the actuals of every deliverable whose Complete status it carries (leased hours add up; the carried deliverable's reported earned_pct and hours win; the newest 100 evidence entries are kept). Every lease that ends (complete, failed, force_release, expiry, accept override, forced revise/select/archive) adds its hours (up to expiry for a lapsed lease) to the deliverable's leased hours
  plan.status          — read-only snapshot ([id, status, attempt_count, failure_count, lapse_count] rows, critical_path (one real chain, always __start__ to __finish__), critical_ids, per-deliverable schedule (es/ef/ls/lf/float, hours; synthetic __start__/__finish__ endpoint rows have synthetic=true and critical=true; critical_ids lists only real deliverables), the ready set in acquire/ready priority order (smallest latest start first, i.e. longest remaining tail, then least float, then id), plan_complete, milestones (one row per `milestone: true` deliverable, or metadata.milestone == true: id, critical_path from __start__ to it, hours = its earliest finish, complete), held locks; for a named variant also name, variant, selected and definition_drift). __start__ and __finish__ are reserved deliverable ids (INVALID_GRAPH)
                        definition_drift: null when unknown (no root for the variant's project, no tracked file, file missing/unreadable/over 8 MiB); false when the file matches the last synced/exported bytes or its graph equals the head graph (re-formatting is not drift); true when the file's graph differs from the head or does not parse — so after an inline revise it is true until plan.export or plan.sync {path}
  plan.get             — return the submitted PlanGraph (deliverables, estimates, files, metadata) for a plan_id
  plan.force_release   — operator escape hatch; emits audit event with `reason`; optional reset_counters:true also clears lapse/failure counters and revives a circuit-broken deliverable
  plan.accept          — a manager/owner marks a deliverable Complete without a lease (audited; evidence required; override_lock to take over a live lease)
  plan.lint            — static checks without submitting: cycles (with the loop), redundant edges, edges without rationale, interface edges not targeting a contract, deliverables feeding no milestone, unordered file overlaps
  plan.schedule        — level a graph against resource capacities (`metadata.owner` by default): makespan, per-deliverable start/finish, per-resource load, driving chain (dependency vs resource waits), project and feeding buffers; capacities is required and every resource carrying work needs >= 1 unit (INVALID_CAPACITIES: lists the missing ones); project_buffer_pct 0..100 (default 25)
  plan.simulate        — read-only what-if: lint, critical path, schedule, milestones, optional resource schedule (schedule: same inputs as plan.schedule) and Monte Carlo (monte_carlo: iterations 1..50000, default 2000; seed, default 0xC0FFEE, reproducible on the same platform and build; iterations × (deliverables + prerequisite edges) must not exceed 200000000), and the scorecard; persists nothing
  plan.sync            — register or update one variant of a named plan line from a plan file (path: .cpm-planner/plans/<name>/<variant>.json, tracked by content hash for drift; any name/variant/project given must match the path) or an inline graph (requires name; variant defaults to "main"); project defaults to the discovered root; force releases live locks of removed deliverables
  plan.list            — list every plan line of project (default: discovered root), variants sorted; archived omitted unless include_archived
  plan.export          — write the head graph of a plan_id to its variant file (or a confined path) and return the root-relative path; refuses another variant's tracked file or unsynced local edits unless force; the file is not synced (exporting to the variant's own tracked file records its hash)
  plan.revise          — replace a plan's graph in place, carrying progress over; returns the new revision and diff; force releases live locks of removed deliverables
  plan.fork            — copy a named variant's head graph, apply ordered edits, register it as a new draft (not selected) variant; an archived line is refused (ARCHIVE_REFUSED) before any file is written
  plan.select          — make a variant its line's selected (only executable) variant, carrying progress over (Complete statuses of identically defined deliverables, with their earned-value actuals); force releases locks on the previous variant
  plan.archive         — archive (archived defaults true) or unarchive a whole line or one variant; archived variants stay readable but refuse sync/select/execute
  plan.compare         — compare stored plans (plan_ids: 2..16 distinct ids, or plan line name in project, default the discovered root, for every live variant, at most 16) on the scorecard: Pareto front, weighted rank, recommended; weights must be finite and >= 0; the Monte Carlo budget (200000000) is shared across variants
  plan.baseline        — freeze the plan's CPM schedule (es/ef) and budgets (effort basis x metadata.cost_rate, default 1) as its next numbered earned-value baseline; optional start (RFC 3339, default now) and calendar {hours_per_day (0 < h <= 24, default 8), workdays (default mon..fri), utc_offset_minutes (default 0)} (omitted: wall-clock hours on the first baseline, the previous baseline's calendar on a re-baseline); re-baselining needs a non-blank reason (<= 2048 chars, INVALID_GRAPH otherwise) and keeps actuals and snapshots; baselines, actuals and snapshots belong to one variant, so a newly selected variant takes its own baseline 1 with no reason needed; selected, unarchived variant only; audited as plan.ev.baselined
  plan.ev              — earned-value report against the latest baseline as of as_of (RFC 3339, default now; as_of is the PV status date, while EV and AC reflect progress and actuals recorded up to the moment the call runs): bac, pv, ev, ac, sv, cv, spi, cpi, eac, etc, vac, tcpi, per-deliverable rows, critical_float_consumed_hours, alerts (SPI_BELOW_0_9 / CPI_BELOW_0_9 when below 0.9 on the two latest stored non-backfilled snapshots by as_of of the current baseline; the current reading is not one of them), excluded_unbaselined; AC sums every recorded hour of the plan (removed and unbaselined deliverables included, at rate 1 when the baseline has no row); a baselined deliverable removed by plan.revise reports status "removed" and keeps its earned percent at removal (100 if complete), and re-adding it restarts its earned percent (hours keep accumulating); a ratio with a zero denominator is null and explained in `undefined` (never NaN); read-only on any variant; NOT_BASELINED before plan.baseline
  plan.snapshot        — compute the plan.ev report and append it as a snapshot (as_of is the PV status date; EV and AC are as recorded when the call runs, so a snapshot whose as_of is more than an hour before taken_at is marked backfilled: true, raises no alerts and is skipped by later alerts); returns summary (undefined explains each null ratio; alerts consider only readings up to its own position: this snapshot and the latest earlier one by as_of of the current baseline, so a backfill never takes alerts from newer readings), snapshot_count and export of the newest 100 snapshots by as_of (ties by taken_at), oldest first, so a backfilled as_of lands in date order; a backfill older than those 100 is stored and counted but not listed: a list of summaries (format "json", default) or a Markdown table with columns date, PV, EV, AC, SPI, CPI, EAC (format "markdown"); selected, unarchived variant only; NOT_BASELINED before plan.baseline
  plan.submit with a `name` (optional `project`/`variant`, variant defaults to "main") registers a named variant instead of an unnamed plan
  plan.lint, plan.simulate take exactly one of an inline graph, a stored plan_id, or a plan-file path; plan.schedule takes graph or plan_id; plan.schedule and plan.simulate reject what plan.submit rejects, and plan.lint reports it as findings

Plan-as-code workflow: author the graph as
.cpm-planner/plans/<name>/<variant>.json, check it with plan.lint {path},
then register it with plan.sync {path} and execute its plan_id. Design many
variants and pick one: plan.fork creates a draft variant from edits,
plan.compare scores every live variant, and plan.select makes exactly one
executable. Never keep untracked scratch graphs — the files under
.cpm-planner/plans/ are the source of truth for definitions.

Leases default to 5 minutes. Pass ttl_seconds (≤ server max, default 8h)
on acquire/heartbeat for long-running work, and heartbeat at least every
ttl/3.

Errors carry stable prefixes: LOCK_HELD, LOCK_NOT_HELD, LOCK_EXPIRED,
OVERLAP_DETECTED, MISSING_PREREQUISITE, PLAN_NOT_FOUND,
DELIVERABLE_NOT_FOUND, LAPSE_LIMIT, PREREQUISITES_INCOMPLETE, INVALID_GRAPH,
INVALID_CAPACITIES, VARIANT_NOT_SELECTED, ARCHIVE_REFUSED, INVALID_PATH,
INVALID_ACTUALS, NOT_BASELINED, BACKEND_ERROR.

DeliverableStatus is internally tagged on `status`:
  {"status":"pending"} | {"status":"ready"} | {"status":"in_progress"} |
  {"status":"complete"} | {"status":"failed","reason":"..."}

Circuit-breaker (visible in plan.status): every lease increments
attempt_count (telemetry). An EXPLICIT mark_status failed increments
failure_count — a ready deliverable with 3 failed attempts is auto-failed
by the next acquire_cohort (reason "circuit-break: exceeded 3 failed
attempts") instead of being re-leased forever. A lease that lapses via
TTL with no terminal mark (driver killed/timed out) increments
lapse_count instead: environmental losses never trip the failure breaker,
but at 10 lapses acquire_cohort stops re-leasing that deliverable: it is
skipped (the rest of the plan stays leasable) and reported in the acquire
response's `blocked` list as {id, code:"LAPSE_LIMIT", reason}. Fix the
environment, then clear it with plan.force_release {reset_counters: true}.
The response also carries scalar `blocked_count` and `needs_operator`.
exhausted:true with needs_operator:true means the plan is stalled on
lapse-limited deliverables (clear with plan.force_release {reset_counters:
true}), not drained.

Targeted acquire: plan.acquire_cohort accepts optional `ids` (only those
deliverables are considered; unknown id -> DELIVERABLE_NOT_FOUND) and
`filter: {"metadata": {key: value}}` (only deliverables whose metadata
equals every pair; others are silently skipped). ids that do not match
filter.metadata are ignored (not reported in blocked). Deliverables with
metadata.kind = "manual" are never leased. With `ids`, each requested id
that is not leased appears in `blocked` with code MANUAL, NOT_READY,
LOCKED, LAPSE_LIMIT, FILE_CONFLICT or MAX_COUNT.
"#
}
