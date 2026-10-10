//! Plan scorecard: one struct of headline numbers that wires the DRAG, risk
//! and network-health metrics together with the schedule and lint results.

use crate::drag::{diameter, drag};
use crate::estimator::EffortEstimator;
use crate::lint::{LintReport, Severity};
use crate::network_health::{CcFlag, cyclomatic_complexity};
use crate::plan::{Deliverable, FINISH_ID, PlanGraph, START_ID};
use crate::resource_schedule::ResourceSchedule;
use crate::risk::{
    CriticalityBand, RiskBandFlag, ValidatedThresholdSet, band_flag, classify_with,
    criticality_risk,
};
use crate::schedule::{effort_basis, scheduled_length};
use crate::task::{CriticalPathResult, Task};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scorecard {
    pub deliverables: usize,
    pub makespan: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_makespan: Option<f32>,
    pub critical_path_len: usize,
    pub critical_count: usize,
    pub near_critical_count: usize,
    pub total_float: f32,
    pub criticality_risk: f64,
    /// Risk band of `criticality_risk`; `"not_applicable"` when the plan has
    /// zero deliverables (there is nothing to score, and a 0.0 risk would
    /// otherwise read as `over_decompressed`).
    pub risk_band: String,
    pub max_drag: Option<(String, f32)>,
    pub diameter: f32,
    pub cyclomatic_complexity: i64,
    pub complexity_flag: String,
    pub merge_bias_count: usize,
    pub total_effort: f32,
    pub parallelism: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_load: Option<f32>,
    pub lint_errors: usize,
    pub lint_warnings: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monte_carlo: Option<crate::monte_carlo::MonteCarloSummary>,
}

/// Build the scorecard. Synthetic `__start__` / `__finish__` tasks are
/// excluded from every count and metric: the task slice is filtered before it
/// reaches `drag` / `diameter`, so those modules need no change.
#[must_use]
pub fn scorecard(
    graph: &PlanGraph,
    cpm: &CriticalPathResult,
    schedule: Option<&ResourceSchedule>,
    lint: &LintReport,
) -> Scorecard {
    let real: Vec<Task> = cpm
        .tasks
        .iter()
        .filter(|t| !is_synthetic(&t.id))
        .cloned()
        .collect();
    let makespan = cpm.critical_path_duration;

    let near_limit = makespan * NEAR_CRITICAL_FRACTION;
    let near_critical_count = real
        .iter()
        .filter(|t| t.float > 0.0 && t.float < near_limit)
        .count();

    let minutes: Vec<u64> = real.iter().map(|t| to_minutes(t.float)).collect();
    let max_minutes = minutes.iter().copied().max().unwrap_or(0);
    let thresholds = ValidatedThresholdSet::default_thirds();
    let bands: Vec<CriticalityBand> = minutes
        .iter()
        .map(|&m| classify_with(m, max_minutes, &thresholds))
        .collect();
    // An empty plan has nothing at risk; `criticality_risk` alone would
    // score an empty band list as maximal risk.
    let risk = if bands.is_empty() {
        0.0
    } else {
        criticality_risk(&bands)
    };
    // A plan with no deliverables has no float to band: report the band as
    // `not_applicable` rather than the `over_decompressed` a 0.0 risk would
    // otherwise imply.
    let risk_band = if graph.deliverables.is_empty() {
        "not_applicable".to_string()
    } else {
        match band_flag(risk) {
            RiskBandFlag::InTarget => "in_target",
            RiskBandFlag::HighRisk => "high_risk",
            RiskBandFlag::OverDecompressed => "over_decompressed",
        }
        .to_string()
    };

    let max_drag = drag(&real)
        .into_iter()
        .filter(|r| r.drag > 0.0)
        .reduce(|best, r| {
            if r.drag > best.drag || (r.drag == best.drag && r.task_id < best.task_id) {
                r
            } else {
                best
            }
        })
        .map(|r| (r.task_id, r.drag));

    // A prerequisite listed twice is one dependency.
    let num_dependencies: usize = graph.deliverables.iter().map(distinct_prereqs).sum();
    let (cc, cc_flag) = cyclomatic_complexity(num_dependencies, graph.deliverables.len());

    let estimator = EffortEstimator::new();
    let total_effort = graph
        .deliverables
        .iter()
        .map(|d| effort_basis(d, &estimator))
        .sum();
    let total_length: f32 = graph
        .deliverables
        .iter()
        .map(|d| scheduled_length(d, &estimator))
        .sum();

    Scorecard {
        deliverables: graph.deliverables.len(),
        makespan,
        resource_makespan: schedule.map(|s| s.makespan),
        critical_path_len: cpm
            .critical_path
            .iter()
            .filter(|id| !is_synthetic(id))
            .count(),
        critical_count: cpm
            .critical_ids
            .iter()
            .filter(|id| !is_synthetic(id))
            .count(),
        near_critical_count,
        total_float: real.iter().map(|t| t.float).sum(),
        criticality_risk: risk,
        risk_band,
        max_drag,
        diameter: diameter(&real),
        cyclomatic_complexity: cc,
        complexity_flag: match cc_flag {
            CcFlag::InTarget => "in_target",
            CcFlag::Warn => "warn",
            CcFlag::TooComplex => "too_complex",
        }
        .to_string(),
        merge_bias_count: graph
            .deliverables
            .iter()
            .filter(|d| distinct_prereqs(d) >= MERGE_BIAS_PREREQS)
            .count(),
        total_effort,
        parallelism: if makespan > 0.0 {
            total_length / makespan
        } else {
            0.0
        },
        peak_load: schedule.and_then(|s| s.load.iter().map(|l| l.utilisation).reduce(f32::max)),
        lint_errors: lint
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count(),
        lint_warnings: lint
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count(),
        monte_carlo: None,
    }
}

/// Float below this fraction of the makespan counts as near-critical.
const NEAR_CRITICAL_FRACTION: f32 = 0.10;
/// A deliverable with at least this many prerequisites shows merge bias.
const MERGE_BIAS_PREREQS: usize = 3;

fn is_synthetic(id: &str) -> bool {
    id == START_ID || id == FINISH_ID
}

/// Number of distinct prerequisite ids of `d`.
fn distinct_prereqs(d: &Deliverable) -> usize {
    d.prerequisites
        .iter()
        .map(|p| p.id())
        .collect::<HashSet<&str>>()
        .len()
}

/// Hours to whole minutes for the integer risk API; negative or NaN is 0.
///
/// Rounds to the nearest minute, so float below 30 seconds counts as zero
/// float (critical) and float noise from `f32` arithmetic cannot move a
/// deliverable between bands.
fn to_minutes(hours: f32) -> u64 {
    (f64::from(hours) * 60.0).round().max(0.0) as u64
}
