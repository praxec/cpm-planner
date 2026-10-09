//! Compare plan variants on the scorecard: Pareto front and weighted rank.

use crate::lint::{Severity, lint};
use crate::metrics::{Scorecard, scorecard};
use crate::monte_carlo::{MonteCarloRequest, monte_carlo};
use crate::plan::{PlanGraph, PlanId, PlannerError};
use crate::planner::canonical_deliverable;
use crate::resource_schedule::{ScheduleRequest, resource_schedule};
use crate::revise::RevisionDiff;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

fn w1() -> f32 {
    1.0
}
fn w05() -> f32 {
    0.5
}

/// Per-criterion weights for the combined score (lower score is better).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompareWeights {
    #[serde(default = "w1")]
    pub makespan: f32,
    #[serde(default = "w1")]
    pub p80: f32,
    #[serde(default = "w1")]
    pub criticality_risk: f32,
    #[serde(default = "w05")]
    pub total_effort: f32,
    #[serde(default = "w05")]
    pub peak_load: f32,
}

impl Default for CompareWeights {
    fn default() -> Self {
        Self {
            makespan: w1(),
            p80: w1(),
            criticality_risk: w1(),
            total_effort: w05(),
            peak_load: w05(),
        }
    }
}

/// What to compute for every variant.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompareRequest {
    /// Resource-level every variant against these capacities.
    #[serde(default)]
    pub schedule: Option<ScheduleRequest>,
    /// Run Monte Carlo on every variant; P80 then comes from it.
    #[serde(default)]
    pub monte_carlo: Option<MonteCarloRequest>,
    #[serde(default)]
    pub weights: CompareWeights,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariantComparison {
    pub plan_id: PlanId,
    pub variant: String,
    pub scorecard: Scorecard,
    /// Structural difference against the first input variant: `added`,
    /// `removed` and `changed` (canonical definition differs) deliverable ids.
    /// A comparison carries no runtime state, so `reopened` and
    /// `released_locks` are always empty.
    pub diff_vs_first: RevisionDiff,
    pub pareto_optimal: bool,
    pub score: f32,
    pub rank: u32,
    pub rationale: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// In input order; `rank` gives the ordering.
    pub variants: Vec<VariantComparison>,
    pub recommended: PlanId,
}

const CRITERIA: [&str; 5] = [
    "makespan",
    "p80",
    "criticality_risk",
    "total_effort",
    "peak_load",
];

fn criteria(card: &Scorecard) -> [f32; 5] {
    let makespan = card.resource_makespan.unwrap_or(card.makespan);
    let p80 = card.monte_carlo.as_ref().map_or(makespan, |m| m.p80);
    [
        makespan,
        p80,
        card.criticality_risk as f32,
        card.total_effort,
        card.peak_load.unwrap_or(0.0),
    ]
}

fn dominates(a: &[f32; 5], b: &[f32; 5]) -> bool {
    a.iter().zip(b).all(|(x, y)| x <= y) && a.iter().zip(b).any(|(x, y)| x < y)
}

fn diff(first: &PlanGraph, other: &PlanGraph) -> RevisionDiff {
    let canon = |g: &PlanGraph| -> BTreeMap<String, serde_json::Value> {
        g.deliverables
            .iter()
            .map(|d| (d.id.clone(), canonical_deliverable(d)))
            .collect()
    };
    let (a, b) = (canon(first), canon(other));
    RevisionDiff {
        added: b.keys().filter(|k| !a.contains_key(*k)).cloned().collect(),
        removed: a.keys().filter(|k| !b.contains_key(*k)).cloned().collect(),
        changed: a
            .iter()
            .filter(|(k, v)| b.get(*k).is_some_and(|o| o != *v))
            .map(|(k, _)| k.clone())
            .collect(),
        reopened: Vec::new(),
        released_locks: Vec::new(),
    }
}

fn rationale(i: usize, all: &[[f32; 5]]) -> String {
    let (mut best, mut worst) = (Vec::new(), Vec::new());
    for (c, name) in CRITERIA.iter().enumerate() {
        let min = all.iter().map(|v| v[c]).fold(f32::INFINITY, f32::min);
        let max = all.iter().map(|v| v[c]).fold(f32::NEG_INFINITY, f32::max);
        if min < max {
            if all[i][c] <= min {
                best.push(*name);
            }
            if all[i][c] >= max {
                worst.push(*name);
            }
        }
    }
    let list = |v: &[&str]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.join(", ")
        }
    };
    format!("best: {}; worst: {}", list(&best), list(&worst))
}

/// Compare `inputs` (plan id, variant name, graph) on the scorecard.
pub fn compare(
    inputs: &[(PlanId, String, PlanGraph)],
    req: &CompareRequest,
) -> Result<Comparison, PlannerError> {
    if inputs.len() < 2 {
        return Err(PlannerError::InvalidGraph {
            reason: "compare needs at least two variants".to_string(),
        });
    }

    let mut cards: Vec<Scorecard> = Vec::with_capacity(inputs.len());
    for (_, variant, graph) in inputs {
        let report = lint(graph);
        if let Some(f) = report
            .findings
            .iter()
            .find(|f| f.severity == Severity::Error)
        {
            return Err(PlannerError::InvalidGraph {
                reason: format!("variant {variant}: {} {}", f.code, f.message),
            });
        }
        let cpm = crate::schedule::compute_cpm(graph)?;
        let sched = req
            .schedule
            .as_ref()
            .map(|s| resource_schedule(graph, s))
            .transpose()?;
        let mut card = scorecard(graph, &cpm, sched.as_ref(), &report);
        card.monte_carlo = req
            .monte_carlo
            .as_ref()
            .map(|m| monte_carlo(graph, m))
            .transpose()?;
        cards.push(card);
    }

    let values: Vec<[f32; 5]> = cards.iter().map(criteria).collect();
    let w = &req.weights;
    let weights = [
        w.makespan,
        w.p80,
        w.criticality_risk,
        w.total_effort,
        w.peak_load,
    ];
    let mins: Vec<f32> = (0..5)
        .map(|c| values.iter().map(|v| v[c]).fold(f32::INFINITY, f32::min))
        .collect();
    let scores: Vec<f32> = values
        .iter()
        .map(|v| {
            (0..5)
                .filter(|&c| mins[c] > 0.0)
                .map(|c| weights[c] * (v[c] / mins[c]))
                .sum()
        })
        .collect();

    let mut order: Vec<usize> = (0..inputs.len()).collect();
    order.sort_by(|&a, &b| {
        scores[a]
            .total_cmp(&scores[b])
            .then_with(|| inputs[a].0.0.cmp(&inputs[b].0.0))
    });
    let mut ranks = vec![0u32; inputs.len()];
    for (pos, &i) in order.iter().enumerate() {
        ranks[i] = u32::try_from(pos + 1).unwrap_or(u32::MAX);
    }

    let variants = cards
        .into_iter()
        .enumerate()
        .map(|(i, card)| VariantComparison {
            plan_id: inputs[i].0.clone(),
            variant: inputs[i].1.clone(),
            scorecard: card,
            diff_vs_first: diff(&inputs[0].2, &inputs[i].2),
            pareto_optimal: !values.iter().any(|o| dominates(o, &values[i])),
            score: scores[i],
            rank: ranks[i],
            rationale: rationale(i, &values),
        })
        .collect();

    Ok(Comparison {
        variants,
        recommended: inputs[order[0]].0.clone(),
    })
}
