// T26 — restriction-category lint on production code only.
// `#[cfg(test)]` modules inside production sources DO see this when
// invoked via `cargo build`, but `cargo test` evaluates `not(test)`
// as false (test cfg is on) and silences the warning everywhere —
// which is what we want: tests panic deliberately via unwrap, prod
// code should `.expect("invariant: ...")` or propagate.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! cpm-planner: a Critical Path Method (CPM) planner for agent work, exposed
//! as a standalone MCP server and usable as a plain Rust library.
//!
//! The CPM kernel does the forward pass (earliest start/finish), backward pass
//! (latest start/finish), slack computation, critical-path identification,
//! parallel batch grouping, and bottleneck (ROI) analysis.
//!
//! On top of that kernel, [`BasicCpmPlanner`] implements the lock-aware
//! [`Planner`](ports::Planner) trait: callers submit a [`PlanGraph`](plan::PlanGraph),
//! then acquire / heartbeat / release locks on disjoint cohorts of deliverables
//! so that multiple workers can run in parallel without stepping on each other.
//! The same planner keeps named plan lines and variants (plan-as-code files
//! under `.cpm-planner/plans/`), frozen earned-value baselines and snapshots,
//! and durable state in SQLite ([`SqlitePlanStore`]). [`PlanServer`] surfaces
//! all of it as MCP tools (`plan.submit`, `plan.acquire_cohort`, `plan.ev`, …)
//! over stdio, so any MCP-speaking client — Claude Code, Cursor, a custom
//! orchestrator, or a praxec workflow `connection` — can drive it.
//!
//! # Example
//!
//! Schedule a graph, then lease and complete its first ready cohort:
//!
//! ```
//! use cpm_planner::plan::{AcquireRequest, CallerId, DeliverableStatus, PlanGraph};
//! use cpm_planner::ports::Planner;
//! use cpm_planner::{BasicCpmPlanner, MarkStatusRequest};
//!
//! # fn main() -> anyhow::Result<()> {
//! let graph: PlanGraph = serde_json::from_value(serde_json::json!({
//!     "deliverables": [
//!         {"id": "design", "owned_files": ["docs/design.md"], "prerequisites": [],
//!          "estimated_effort_hours": 4.0},
//!         {"id": "build", "owned_files": ["src/app.rs"], "prerequisites": ["design"],
//!          "estimated_effort_hours": 8.0},
//!         {"id": "docs", "owned_files": ["README.md"], "prerequisites": ["design"],
//!          "estimated_effort_hours": 2.0}
//!     ]
//! }))?;
//!
//! // Pure CPM: `__start__` and `__finish__` are synthetic zero-length endpoints.
//! let cpm = cpm_planner::schedule::compute_cpm(&graph)?;
//! assert_eq!(cpm.critical_path, ["__start__", "design", "build", "__finish__"]);
//! assert_eq!(cpm.critical_path_duration, 12.0);
//!
//! // Lock-aware execution on a private in-memory store.
//! let runtime = tokio::runtime::Runtime::new()?;
//! runtime.block_on(async {
//!     let planner = BasicCpmPlanner::new();
//!     let plan_id = planner.submit_plan(graph).await?;
//!     let caller = CallerId("worker-1".into());
//!
//!     let cohort = planner
//!         .acquire_cohort(AcquireRequest::new(plan_id.clone(), caller.clone(), 4))
//!         .await?;
//!     let leased: Vec<&str> = cohort.rows.iter().map(|r| r.deliverable.id.as_str()).collect();
//!     assert_eq!(leased, ["design"]); // `build` and `docs` wait on it
//!
//!     planner
//!         .mark_status(MarkStatusRequest::new(
//!             plan_id.clone(),
//!             "design",
//!             caller.clone(),
//!             DeliverableStatus::Complete,
//!         ))
//!         .await?;
//!
//!     // Completing `design` releases its lock and readies both dependents.
//!     let next = planner
//!         .acquire_cohort(AcquireRequest::new(plan_id, caller, 4))
//!         .await?;
//!     let mut leased: Vec<&str> = next.rows.iter().map(|r| r.deliverable.id.as_str()).collect();
//!     leased.sort();
//!     assert_eq!(leased, ["build", "docs"]);
//!     Ok::<_, anyhow::Error>(())
//! })?;
//! # Ok(())
//! # }
//! ```
//!
//! # Layout
//!
//! - [`plan`] — the wire/domain model (deliverables, cohorts, locks, errors).
//! - [`ports`] — the [`Planner`](ports::Planner) trait.
//! - [`algorithm`] / [`task`] — the pure CPM kernel and its internal model
//!   (the `Task` types carry ES/EF/LS/LF/slack/batching state the wire model
//!   doesn't need to expose).
//! - [`schedule`] — [`schedule::compute_cpm`], the CPM run shared by submit,
//!   status and the analysis tools.
//! - [`planner`] — [`BasicCpmPlanner`], the lock-aware implementation
//!   (including the earned-value operations).
//! - [`plan_store`] — durable SQLite persistence (plans, statuses, cohort
//!   locks, submit dedup, portfolio lines/variants/revisions, earned value)
//!   and the cross-process atomicity mechanism.
//! - [`project`] — project-root discovery and confined plan-file I/O.
//! - [`edits`], [`revise`] — structured graph edits and progress-preserving
//!   revision.
//! - [`lint`], [`simulate`], [`resource_schedule`], [`monte_carlo`],
//!   [`compare`], [`metrics`], [`drag`](mod@drag), [`risk`], [`network_health`] — the
//!   read-only analysis tools and the plan scorecard.
//! - [`earned_value`] — the pure PV / EV / AC engine.
//! - [`review`], [`llm`] — the optional `plan.review` and its OpenRouter /
//!   Jev judge.
//! - [`server`] — the MCP tool façade.
//! - [`skills`] — the embedded agent skills and `cpm-planner skills install`.
//! - [`audit`] — the lock-lifecycle audit surface.
//!
//! See `docs/architecture.md` in the repository for the request flow, the
//! store schema and the concurrency model.
//!
//! This crate has no dependency on praxec; it is consumed purely over the
//! MCP protocol.

pub mod algorithm;
pub mod audit;
pub mod compare;
pub mod drag;
pub mod earned_value;
pub mod edits;
pub mod estimator;
pub(crate) mod ev_store;
mod graph;
#[cfg(test)]
mod lease_hours_tests;
pub mod lint;
pub mod llm;
mod locks;
#[cfg(test)]
mod mark_actuals_tests;
pub mod metrics;
pub mod monte_carlo;
pub mod network_health;
pub mod plan;
pub mod plan_store;
pub mod planner;
mod portfolio;
pub mod ports;
pub mod project;
pub mod resource_schedule;
pub mod review;
pub mod revise;
pub mod risk;
pub mod schedule;
pub mod server;
pub mod simulate;
pub mod skills;
pub mod task;

pub use algorithm::CpmAlgorithm;
pub use drag::{DragResult, diameter, drag};
pub use estimator::{EffortEstimator, EstimationConfig};
pub use plan::{
    AcceptRequest, AcquireRequest, Estimate, ForceReleaseRequest, HeartbeatRequest,
    MarkStatusRequest,
};
pub use plan_store::{DB_PATH_ENV, SqlitePlanStore};
pub use planner::{
    BasicCpmPlanner, ClockFn, DEFAULT_EFFORT_HOURS, DEFAULT_MAX_TTL, DEFAULT_TTL, MAX_ATTEMPTS,
    MAX_LAPSES,
};
pub use server::{
    PLAN_TOOL_NAMES, PlanServer, TOOL_ACCEPT, TOOL_ACQUIRE_COHORT, TOOL_FORCE_RELEASE,
    TOOL_HEARTBEAT, TOOL_MARK_STATUS, TOOL_REVIEW, TOOL_STATUS, TOOL_SUBMIT, plan_tool_definitions,
};
pub use task::{Bottleneck, CriticalPathResult, Task, TaskBatch, TaskKind, TaskStatus};
