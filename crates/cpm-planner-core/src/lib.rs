// Restriction-category lint on production code only (see cpm-planner's
// lib.rs): tests unwrap deliberately, production code `.expect`s with an
// invariant or propagates.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! cpm-planner-core: the pure Critical Path Method (CPM) kernel of
//! [cpm-planner](https://docs.rs/cpm-planner).
//!
//! It holds the plan graph model and the computations over it: the forward
//! pass (earliest start and finish), the backward pass (latest start and
//! finish), slack, the critical path, parallel batches and bottlenecks,
//! effort estimation, working calendars, resource levelling, Monte Carlo
//! simulation, DRAG and the earned-value engine. It does no I/O and has no
//! SQLite, async runtime, MCP or LLM dependency, so it fits any dependency
//! graph, including one that links a different SQLite.
//!
//! The `cpm-planner` crate depends on this one and re-exports every module
//! at its old path (`cpm_planner::plan`, `cpm_planner::schedule`, …), so the
//! types are the same in both crates.
//!
//! # Example
//!
//! ```
//! use cpm_planner_core::plan::PlanGraph;
//! use cpm_planner_core::schedule::compute_cpm;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let graph: PlanGraph = serde_json::from_value(serde_json::json!({
//!     "deliverables": [
//!         {"id": "design", "owned_files": ["docs/design.md"], "prerequisites": [],
//!          "duration_hours": 4.0},
//!         {"id": "build", "owned_files": ["src/app.rs"], "prerequisites": ["design"],
//!          "duration_hours": 8.0},
//!         {"id": "docs", "owned_files": ["README.md"], "prerequisites": ["design"],
//!          "duration_hours": 2.0}
//!     ]
//! }))?;
//!
//! // `__start__` and `__finish__` are synthetic zero-length endpoints.
//! let cpm = compute_cpm(&graph)?;
//! assert_eq!(cpm.critical_path, ["__start__", "design", "build", "__finish__"]);
//! assert_eq!(cpm.critical_path_duration, 12.0);
//! let build = cpm.tasks.iter().find(|t| t.id == "build").expect("build is scheduled");
//! assert_eq!(build.earliest_start, 4.0);
//! # Ok(())
//! # }
//! ```
//!
//! # Layout
//!
//! - [`plan`]: the graph model (deliverables, prerequisites, owned files,
//!   statuses), ids, limits and [`PlannerError`].
//! - [`validate`]: the structural checks every graph passes before
//!   scheduling.
//! - [`algorithm`] / [`task`]: the CPM passes and their task model.
//! - [`schedule`]: [`schedule::compute_cpm`], the CPM run over a
//!   [`PlanGraph`].
//! - [`estimator`]: three-point and default effort estimation.
//! - [`calendar`]: working calendars and their arithmetic.
//! - [`resource_schedule`]: capacity-levelled scheduling.
//! - [`monte_carlo`]: schedule-risk simulation.
//! - [`drag`](mod@drag), [`risk`], [`network_health`]: analysis metrics.
//! - [`earned_value`]: the PV / EV / AC engine and baseline types.

pub mod algorithm;
pub mod calendar;
pub mod drag;
pub mod earned_value;
pub mod estimator;
/// Graph helpers shared with `cpm-planner` (prerequisite ids, reachability,
/// value checks). Not a stable API.
#[doc(hidden)]
pub mod graph;
pub mod monte_carlo;
pub mod network_health;
pub mod plan;
pub mod resource_schedule;
pub mod risk;
pub mod schedule;
pub mod task;
pub mod validate;

pub use algorithm::CpmAlgorithm;
pub use drag::{DragResult, diameter, drag};
pub use estimator::{EffortEstimator, EstimationConfig};
pub use plan::{Estimate, PlanGraph, PlannerError};
pub use schedule::compute_cpm;
pub use task::{Bottleneck, CriticalPathResult, Task, TaskBatch, TaskKind, TaskStatus};
