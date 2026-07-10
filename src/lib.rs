// T26 — restriction-category lint on production code only.
// `#[cfg(test)]` modules inside production sources DO see this when
// invoked via `cargo build`, but `cargo test` evaluates `not(test)`
// as false (test cfg is on) and silences the warning everywhere —
// which is what we want: tests panic deliberately via unwrap, prod
// code should `.expect("invariant: ...")` or propagate.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! cpm-planner: a textbook Critical Path Method (CPM) planner, exposed as a
//! standalone MCP server.
//!
//! The CPM kernel does the forward pass (earliest start/finish), backward pass
//! (latest start/finish), slack computation, critical-path identification,
//! parallel batch grouping, and bottleneck (ROI) analysis.
//!
//! On top of that kernel, [`BasicCpmPlanner`] implements the lock-aware
//! [`Planner`](ports::Planner) trait: callers submit a [`PlanGraph`](plan::PlanGraph),
//! then acquire / heartbeat / release locks on disjoint cohorts of deliverables
//! so that multiple workers can run in parallel without stepping on each other.
//! [`PlanServer`] surfaces those operations as MCP tools (`plan.submit`,
//! `plan.acquire_cohort`, …) over stdio, so any MCP-speaking client — Claude
//! Code, Cursor, a custom orchestrator, or an praxec workflow `connection`
//! — can drive it.
//!
//! # Layout
//!
//! - [`plan`] — the wire/domain model (deliverables, cohorts, locks, errors).
//! - [`ports`] — the [`Planner`](ports::Planner) trait.
//! - [`algorithm`] / [`task`] — the pure CPM kernel and its internal model
//!   (the `Task` types carry ES/EF/LS/LF/slack/batching state the wire model
//!   doesn't need to expose).
//! - [`planner`] — [`BasicCpmPlanner`], the lock-aware implementation.
//! - [`plan_store`] — durable SQLite persistence (plans, statuses, cohort
//!   locks, submit dedup) and the cross-process atomicity mechanism.
//! - [`server`] — the MCP tool façade.
//! - [`audit`] — the lock-lifecycle audit surface.
//!
//! This crate has no dependency on praxec; it is consumed purely over the
//! MCP protocol.

pub mod algorithm;
pub mod audit;
pub mod drag;
pub mod estimator;
mod locks;
pub mod network_health;
pub mod plan;
pub mod plan_store;
pub mod planner;
pub mod ports;
pub mod risk;
pub mod server;
pub mod task;

pub use algorithm::CpmAlgorithm;
pub use drag::{DragResult, diameter, drag};
pub use estimator::{EffortEstimator, EstimationConfig};
pub use plan_store::{DB_PATH_ENV, SqlitePlanStore};
pub use planner::{BasicCpmPlanner, ClockFn, DEFAULT_EFFORT_HOURS, DEFAULT_TTL, MAX_ATTEMPTS};
pub use server::{
    PLAN_TOOL_NAMES, PlanServer, TOOL_ACQUIRE_COHORT, TOOL_FORCE_RELEASE, TOOL_HEARTBEAT,
    TOOL_MARK_STATUS, TOOL_STATUS, TOOL_SUBMIT, plan_tool_definitions,
};
pub use task::{Bottleneck, CriticalPathResult, Task, TaskBatch, TaskKind, TaskStatus};
