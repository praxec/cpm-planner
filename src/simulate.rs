//! Read-only plan simulation: lint, CPM, optional resource leveling and Monte
//! Carlo, and the scorecard, for a graph that is never persisted.

use serde::{Deserialize, Serialize};

use crate::lint::{LintReport, Severity, lint};
use crate::metrics::{Scorecard, scorecard};
use crate::monte_carlo::{MonteCarloRequest, monte_carlo_with_cpm};
use crate::plan::{MilestoneRow, PlanGraph, PlannerError, ScheduleRow};
use crate::resource_schedule::{ResourceSchedule, ScheduleRequest, resource_schedule_with_cpm};
use crate::schedule::{compute_cpm, milestone_rows, schedule_rows};

/// What to compute beyond the always-on lint, CPM and scorecard.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimulateRequest {
    /// Resource capacities; when present the plan is leveled.
    #[serde(default)]
    pub schedule: Option<ScheduleRequest>,
    /// Monte Carlo parameters; when present the scorecard carries a summary.
    #[serde(default)]
    pub monte_carlo: Option<MonteCarloRequest>,
}

/// Everything `plan.simulate` reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulationResult {
    pub lint: LintReport,
    pub critical_path: Vec<String>,
    pub critical_path_hours: f32,
    /// Same rows as `plan.status`, including the synthetic endpoints.
    pub schedule: Vec<ScheduleRow>,
    pub milestones: Vec<MilestoneRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_schedule: Option<ResourceSchedule>,
    /// `monte_carlo` is filled when requested.
    pub scorecard: Scorecard,
}

/// Simulate `graph` without persisting anything.
///
/// A lint `Error` finding (including a cycle) is [`PlannerError::InvalidGraph`]
/// listing each code and its ids; anything else submit would reject is
/// [`PlannerError::InvalidGraph`] with submit's message. CPM runs once and
/// is shared by leveling and Monte Carlo.
pub fn simulate(
    graph: &PlanGraph,
    req: &SimulateRequest,
) -> Result<SimulationResult, PlannerError> {
    let lint_report = lint(graph);
    let errors: Vec<String> = lint_report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .map(|f| format!("{} [{}]", f.code, f.ids.join(", ")))
        .collect();
    if !errors.is_empty() {
        return Err(PlannerError::InvalidGraph {
            reason: format!("lint errors: {}", errors.join("; ")),
        });
    }
    // Lint's message wins when it already found errors; validation catches
    // anything lint does not check.
    crate::planner::validate_graph(graph)?;
    let cpm = compute_cpm(graph)?;
    let leveled = req
        .schedule
        .as_ref()
        .map(|s| resource_schedule_with_cpm(graph, s, &cpm))
        .transpose()?;
    let summary = req
        .monte_carlo
        .as_ref()
        .map(|m| monte_carlo_with_cpm(graph, m, &cpm))
        .transpose()?;
    let mut card = scorecard(graph, &cpm, leveled.as_ref(), &lint_report);
    card.monte_carlo = summary;
    Ok(SimulationResult {
        lint: lint_report,
        critical_path: cpm.critical_path.clone(),
        critical_path_hours: cpm.critical_path_duration,
        schedule: schedule_rows(graph, &cpm),
        milestones: milestone_rows(graph, &cpm, |_| false),
        resource_schedule: leveled,
        scorecard: card,
    })
}
