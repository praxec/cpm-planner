// SPEC §33 PA4 — MCP server façade for `BasicCpmPlanner`.
//
// Production-code lint surface (consistent with the rest of the workspace —
// `#![cfg_attr(not(test), warn(clippy::unwrap_used))]` is declared at the
// crate root in `lib.rs`).

//! MCP tool surface for the open-source CPM planner.
//!
//! [`PlanServer`] wraps an `Arc<BasicCpmPlanner>` and exposes the six
//! [`Planner`] trait methods as MCP tools so any MCP-speaking agent
//! (Claude Code, Cursor, custom orchestrator, or the §33 LLM executor)
//! can drive the planner over the standard MCP protocol.
//!
//! # Tool surface
//!
//! | Tool name                | Trait method                  |
//! |--------------------------|-------------------------------|
//! | `plan.submit`            | [`Planner::submit_plan`]      |
//! | `plan.acquire_cohort`    | [`Planner::acquire_cohort`]   |
//! | `plan.heartbeat`         | [`Planner::heartbeat`]        |
//! | `plan.mark_status`       | [`Planner::mark_status`]      |
//! | `plan.status`            | [`Planner::status`]           |
//! | `plan.force_release`     | [`Planner::force_release`]    |
//!
//! # Error mapping
//!
//! [`PlannerError`] variants are surfaced as MCP `internal_error`
//! responses whose `message` is the variant's `Display` output. The
//! variant prefixes (`LOCK_HELD:`, `LOCK_NOT_HELD:`, `LOCK_EXPIRED:`,
//! `OVERLAP_DETECTED:`, `MISSING_PREREQUISITE:`, `PLAN_NOT_FOUND:`,
//! `DELIVERABLE_NOT_FOUND:`, `INVALID_GRAPH:`, `BACKEND_ERROR:`) are
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

use crate::plan::{
    CallerId, Cohort, DeliverableStatus, PlanGraph, PlanId, PlanStatus, PlannerError,
};
use crate::ports::Planner;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::transport::stdio;
use rmcp::ErrorData as McpError;
use rmcp::{ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::BasicCpmPlanner;

/// SPEC §33 PA4 — tool names. Dot notation `plan.<verb>` matches the
/// convention used elsewhere in the workspace (`praxec.query`,
/// `praxec.command`).
pub const TOOL_SUBMIT: &str = "plan.submit";
pub const TOOL_ACQUIRE_COHORT: &str = "plan.acquire_cohort";
pub const TOOL_HEARTBEAT: &str = "plan.heartbeat";
pub const TOOL_MARK_STATUS: &str = "plan.mark_status";
pub const TOOL_STATUS: &str = "plan.status";
pub const TOOL_FORCE_RELEASE: &str = "plan.force_release";

/// All six MCP tool names exposed by [`PlanServer`], in declaration order.
pub const PLAN_TOOL_NAMES: &[&str] = &[
    TOOL_SUBMIT,
    TOOL_ACQUIRE_COHORT,
    TOOL_HEARTBEAT,
    TOOL_MARK_STATUS,
    TOOL_STATUS,
    TOOL_FORCE_RELEASE,
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquireCohortArgs {
    plan_id: String,
    caller_id: String,
    max_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeartbeatArgs {
    plan_id: String,
    deliverable_id: String,
    caller_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkStatusArgs {
    plan_id: String,
    deliverable_id: String,
    caller_id: String,
    status: DeliverableStatus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    plan_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForceReleaseArgs {
    plan_id: String,
    deliverable_id: String,
    reason: String,
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

// `Cohort` and `PlanStatus` already derive `Serialize` (PA1) — return them
// directly.

// ---------------------------------------------------------------------------
// Tool-list construction
// ---------------------------------------------------------------------------

/// Build the six `Tool` definitions advertised in `list_tools`.
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
                 Idempotent: identical graphs return the same plan_id.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "graph": {
                        "type": "object",
                        "properties": {
                            "deliverables": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id":                    { "type": "string" },
                                        "owned_files":           { "type": "array", "items": { "type": "string" } },
                                        "prerequisites":         { "type": "array", "items": { "type": "string" } },
                                        "estimated_effort_hours": { "type": "number" },
                                        "metadata":              {}
                                    },
                                    "required": ["id", "owned_files", "prerequisites"]
                                }
                            },
                            "max_chained_dispatch": { "type": ["integer", "null"] }
                        },
                        "required": ["deliverables"]
                    }
                },
                "required": ["graph"],
                "additionalProperties": false
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_ACQUIRE_COHORT),
            Cow::Borrowed(
                "Acquire up to max_count ready, file-disjoint deliverables \
                 atomically. Returns the cohort plus per-deliverable locks.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id":   { "type": "string" },
                    "caller_id": { "type": "string" },
                    "max_count": { "type": "integer", "minimum": 1 }
                },
                "required": ["plan_id", "caller_id", "max_count"]
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_HEARTBEAT),
            Cow::Borrowed(
                "Refresh the TTL on a held lock; LOCK_NOT_HELD or LOCK_EXPIRED on failure.",
            ),
            schema_object(json!({
                "type": "object",
                "properties": {
                    "plan_id":        { "type": "string" },
                    "deliverable_id": { "type": "string" },
                    "caller_id":      { "type": "string" }
                },
                "required": ["plan_id", "deliverable_id", "caller_id"]
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_MARK_STATUS),
            Cow::Borrowed(
                "Set a deliverable's status. Complete/Failed releases the lock; \
                 caller_id mismatch yields LOCK_NOT_HELD.",
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
                    }
                },
                "required": ["plan_id", "deliverable_id", "caller_id", "status"]
            })),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_STATUS),
            Cow::Borrowed("Read-only snapshot: per-deliverable status, critical path, held locks."),
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
                    "reason":         { "type": "string" }
                },
                "required": ["plan_id", "deliverable_id", "reason"]
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

/// MCP server façade exposing a [`BasicCpmPlanner`] over six tools.
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
            TOOL_FORCE_RELEASE => self.handle_force_release(args).await,
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
        let plan_id = self
            .planner
            .submit_plan(parsed.graph)
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&SubmitResponse { plan_id: plan_id.0 })
    }

    async fn handle_acquire_cohort(&self, args: Value) -> Result<Value, McpError> {
        let parsed: AcquireCohortArgs = parse_args(args)?;
        let cohort: Cohort = self
            .planner
            .acquire_cohort(
                &PlanId(parsed.plan_id),
                &CallerId(parsed.caller_id),
                parsed.max_count,
            )
            .await
            .map_err(planner_error_to_mcp)?;
        // A SCALAR termination signal for declarative cohort drivers: a
        // state-machine guard-expr (e.g. praxec's) can't test array
        // emptiness and fails-fast on a missing path, so a loop that calls
        // acquire_cohort repeatedly until the plan is drained guards on
        // `exhausted == true` rather than inspecting `rows`. True when this
        // acquisition returned no rows (nothing ready / all complete).
        let exhausted = cohort.rows.is_empty();
        let mut value = to_value(&cohort)?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("exhausted".to_string(), Value::Bool(exhausted));
        }
        Ok(value)
    }

    async fn handle_heartbeat(&self, args: Value) -> Result<Value, McpError> {
        let parsed: HeartbeatArgs = parse_args(args)?;
        self.planner
            .heartbeat(
                &PlanId(parsed.plan_id),
                &parsed.deliverable_id,
                &CallerId(parsed.caller_id),
            )
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&OkResponse::new())
    }

    async fn handle_mark_status(&self, args: Value) -> Result<Value, McpError> {
        let parsed: MarkStatusArgs = parse_args(args)?;
        self.planner
            .mark_status(
                &PlanId(parsed.plan_id),
                &parsed.deliverable_id,
                &CallerId(parsed.caller_id),
                parsed.status,
            )
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

    async fn handle_force_release(&self, args: Value) -> Result<Value, McpError> {
        let parsed: ForceReleaseArgs = parse_args(args)?;
        self.planner
            .force_release(
                &PlanId(parsed.plan_id),
                &parsed.deliverable_id,
                &parsed.reason,
            )
            .await
            .map_err(planner_error_to_mcp)?;
        to_value(&OkResponse::new())
    }
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
            "MCP server exposing the open-source Praxec CPM planner via six tools.".to_string(),
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

Tools (six total, all `plan.<verb>`):
  plan.submit          — submit a PlanGraph, get a plan_id (idempotent on identical graphs)
  plan.acquire_cohort  — atomically acquire ready, file-disjoint deliverables
  plan.heartbeat       — refresh a held lock's TTL
  plan.mark_status     — set a deliverable's status (Complete/Failed releases the lock)
  plan.status          — read-only snapshot (statuses, critical path, held locks)
  plan.force_release   — operator escape hatch; emits audit event with `reason`

Errors carry stable prefixes: LOCK_HELD, LOCK_NOT_HELD, LOCK_EXPIRED,
OVERLAP_DETECTED, MISSING_PREREQUISITE, PLAN_NOT_FOUND,
DELIVERABLE_NOT_FOUND, INVALID_GRAPH, BACKEND_ERROR.

DeliverableStatus is internally tagged on `status`:
  {"status":"pending"} | {"status":"ready"} | {"status":"in_progress"} |
  {"status":"complete"} | {"status":"failed","reason":"..."}
"#
}
