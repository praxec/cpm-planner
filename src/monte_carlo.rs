//! Seeded Monte Carlo schedule risk over three-point estimates.
//!
//! Each deliverable with an [`Estimate`](crate::plan::Estimate) whose
//! `pessimistic > optimistic` has its scheduled length drawn from a PERT
//! (modified beta) distribution every iteration; every other deliverable keeps
//! its deterministic scheduled length. A deliverable with `duration_hours` set
//! is never sampled: the fixed calendar duration wins and its estimate is
//! ignored. Deliverables are visited in graph order each iteration, so a seed
//! reproduces the output exactly. Synthetic `__start__` / `__finish__` tasks
//! take part in scheduling but never appear in the output.

use crate::algorithm::CpmAlgorithm;
use crate::estimator::EffortEstimator;
use crate::plan::{FINISH_ID, PlanGraph, PlannerError};
use crate::schedule::{compute_cpm, deliverable_to_task};
use crate::task::Task;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, Pert};
use serde::{Deserialize, Serialize};

/// Maximum iterations per run.
pub const MAX_ITERATIONS: u32 = 50_000;
/// A deliverable counts as critical in a run when its float is below this.
const CRITICAL_EPSILON: f32 = 1e-3;

fn default_iterations() -> u32 {
    2000
}

fn default_seed() -> u64 {
    0xC0FFEE
}

/// Monte Carlo run parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonteCarloRequest {
    /// Number of simulated schedules, 1 to 50 000 (default 2000).
    #[serde(default = "default_iterations")]
    pub iterations: u32,
    /// RNG seed (default 0xC0FFEE). The same seed gives identical output.
    #[serde(default = "default_seed")]
    pub seed: u64,
}

impl Default for MonteCarloRequest {
    fn default() -> Self {
        Self {
            iterations: default_iterations(),
            seed: default_seed(),
        }
    }
}

/// Share of runs in which a deliverable was critical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CriticalityIndex {
    pub id: String,
    pub index: f32,
}

/// Pearson correlation between a deliverable's sampled length and the makespan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sensitivity {
    pub id: String,
    pub correlation: f32,
}

/// Result of a Monte Carlo run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonteCarloSummary {
    pub iterations: u32,
    pub seed: u64,
    pub p50: f32,
    pub p80: f32,
    pub p95: f32,
    pub mean: f32,
    /// Makespan with every length at its deterministic value.
    pub deterministic_makespan: f32,
    /// Graph order, no synthetic ids.
    pub criticality: Vec<CriticalityIndex>,
    /// Only sampled deliverables; sorted by |correlation| desc, then id.
    pub sensitivity: Vec<Sensitivity>,
}

/// Run a seeded Monte Carlo simulation of `graph`.
pub fn monte_carlo(
    graph: &PlanGraph,
    req: &MonteCarloRequest,
) -> Result<MonteCarloSummary, PlannerError> {
    if req.iterations == 0 || req.iterations > MAX_ITERATIONS {
        return Err(PlannerError::InvalidGraph {
            reason: format!("iterations must be between 1 and {MAX_ITERATIONS}"),
        });
    }
    let deterministic_makespan = compute_cpm(graph)?.critical_path_duration;

    let estimator = EffortEstimator::new();
    let mut tasks: Vec<Task> = graph
        .deliverables
        .iter()
        .map(|d| deliverable_to_task(d, &estimator))
        .collect();
    crate::schedule::add_endpoints(graph, &mut tasks);

    // (deliverable index, distribution) for each sampled deliverable.
    let mut sampled: Vec<(usize, Pert<f32>)> = Vec::new();
    for (i, d) in graph.deliverables.iter().enumerate() {
        let Some(e) = d.estimate else { continue };
        if d.duration_hours.is_some() || e.pessimistic <= e.optimistic {
            continue;
        }
        let dist = Pert::new(e.optimistic, e.pessimistic)
            .with_mode(e.likely)
            .map_err(|err| PlannerError::InvalidGraph {
                reason: format!("deliverable '{}' has an unusable estimate: {err}", d.id),
            })?;
        sampled.push((i, dist));
    }

    let n = req.iterations as usize;
    let finish_idx = tasks
        .iter()
        .position(|t| t.id == FINISH_ID)
        .ok_or_else(|| PlannerError::InvalidGraph {
            reason: "internal error: missing finish endpoint".to_string(),
        })?;
    let mut rng = ChaCha8Rng::seed_from_u64(req.seed);
    let mut makespans = Vec::with_capacity(n);
    let mut lengths: Vec<Vec<f32>> = vec![Vec::with_capacity(n); sampled.len()];
    let mut critical_runs = vec![0u32; graph.deliverables.len()];

    for _ in 0..n {
        for (k, (i, dist)) in sampled.iter().enumerate() {
            let v = dist.sample(&mut rng);
            tasks[*i].effort_hours = v;
            lengths[k].push(v);
        }
        CpmAlgorithm::forward_backward(&mut tasks);
        makespans.push(tasks[finish_idx].earliest_finish);
        for (i, runs) in critical_runs.iter_mut().enumerate() {
            if tasks[i].float < CRITICAL_EPSILON {
                *runs += 1;
            }
        }
    }

    let mean = (makespans.iter().map(|&m| f64::from(m)).sum::<f64>() / n as f64) as f32;
    let mut sorted = makespans.clone();
    sorted.sort_by(f32::total_cmp);

    let criticality = graph
        .deliverables
        .iter()
        .zip(&critical_runs)
        .map(|(d, &runs)| CriticalityIndex {
            id: d.id.clone(),
            index: runs as f32 / n as f32,
        })
        .collect();

    let mut sensitivity: Vec<Sensitivity> = sampled
        .iter()
        .zip(&lengths)
        .map(|((i, _), series)| Sensitivity {
            id: graph.deliverables[*i].id.clone(),
            correlation: pearson(series, &makespans),
        })
        .collect();
    sensitivity.sort_by(|a, b| {
        b.correlation
            .abs()
            .total_cmp(&a.correlation.abs())
            .then_with(|| a.id.cmp(&b.id))
    });

    Ok(MonteCarloSummary {
        iterations: req.iterations,
        seed: req.seed,
        p50: nearest_rank(&sorted, 0.50),
        p80: nearest_rank(&sorted, 0.80),
        p95: nearest_rank(&sorted, 0.95),
        mean,
        deterministic_makespan,
        criticality,
        sensitivity,
    })
}

/// Nearest-rank percentile of an ascending, non-empty slice.
fn nearest_rank(sorted: &[f32], q: f64) -> f32 {
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Pearson correlation; 0 when either series is constant.
fn pearson(x: &[f32], y: &[f32]) -> f32 {
    let n = x.len() as f64;
    let mx = x.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let my = y.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (&a, &b) in x.iter().zip(y) {
        let (da, db) = (f64::from(a) - mx, f64::from(b) - my);
        sxy += da * db;
        sxx += da * da;
        syy += db * db;
    }
    if sxx <= 0.0 || syy <= 0.0 {
        return 0.0;
    }
    (sxy / (sxx * syy).sqrt()) as f32
}
