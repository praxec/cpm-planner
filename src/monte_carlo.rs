//! Seeded Monte Carlo schedule risk over three-point estimates.
//!
//! A deliverable's scheduled length follows the usual precedence:
//! `duration_hours`, else `estimated_effort_hours`, else `estimate.likely`,
//! else `0` for a milestone, else the estimator's kind-aware effort. Monte
//! Carlo samples only the third case: a deliverable whose length comes from
//! its [`Estimate`](crate::plan::Estimate) (neither `duration_hours` nor
//! `estimated_effort_hours` set) and whose `pessimistic > optimistic` has its
//! length drawn from a PERT (modified beta) distribution every iteration.
//! Every other deliverable keeps its deterministic scheduled length; an
//! explicit duration or effort always wins over the estimate.
//!
//! Deliverables are visited in graph order each iteration, so a seed
//! reproduces the output exactly on a given platform and toolchain.
//! Synthetic `__start__` / `__finish__` tasks take part in scheduling but
//! never appear in the output.
//!
//! Work is bounded: `iterations` is 1 to [`MAX_ITERATIONS`], and
//! `iterations × (deliverables + prerequisite edges)` may not exceed
//! [`MAX_WORK`].

use crate::estimator::EffortEstimator;
use crate::plan::{FINISH_ID, PlanGraph, PlannerError};
use crate::schedule::{compute_cpm, deliverable_to_task};
use crate::task::{CriticalPathResult, Task};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, Pert};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Maximum iterations per run.
pub const MAX_ITERATIONS: u32 = 50_000;
/// Maximum `iterations × (deliverables + prerequisite edges)` per run.
pub const MAX_WORK: u64 = 200_000_000;
/// A deliverable counts as critical in a run when its float is below this.
const CRITICAL_EPSILON: f32 = 1e-3;

fn default_iterations() -> u32 {
    2000
}

fn default_seed() -> u64 {
    0xC0FFEE
}

/// Check `iterations` is within `1..=MAX_ITERATIONS`.
pub(crate) fn check_iterations(iterations: u32) -> Result<(), String> {
    if iterations == 0 || iterations > MAX_ITERATIONS {
        Err(format!(
            "iterations must be between 1 and {MAX_ITERATIONS}; got {iterations}"
        ))
    } else {
        Ok(())
    }
}

/// Monte Carlo run parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
///
/// The graph is validated exactly as `plan.submit` validates it. Output is
/// reproducible for a given seed on a given platform and toolchain;
/// bit-identical results across targets are not guaranteed because float
/// transcendental functions can differ.
pub fn monte_carlo(
    graph: &PlanGraph,
    req: &MonteCarloRequest,
) -> Result<MonteCarloSummary, PlannerError> {
    crate::planner::validate_graph(graph)?;
    check_request(graph, req)?;
    let cpm = compute_cpm(graph)?;
    monte_carlo_with_cpm(graph, req, &cpm)
}

/// The per-iteration work of `graph`: deliverables plus prerequisite edges.
/// `iterations × work_size` is what [`MAX_WORK`] bounds.
pub(crate) fn work_size(graph: &PlanGraph) -> usize {
    graph.deliverables.len()
        + graph
            .deliverables
            .iter()
            .map(|d| d.prerequisites.len())
            .sum::<usize>()
}

/// Iteration range and work budget.
fn check_request(graph: &PlanGraph, req: &MonteCarloRequest) -> Result<(), PlannerError> {
    check_iterations(req.iterations).map_err(|reason| PlannerError::InvalidGraph { reason })?;
    let size = work_size(graph);
    if u64::from(req.iterations).saturating_mul(size as u64) > MAX_WORK {
        return Err(PlannerError::InvalidGraph {
            reason: format!(
                "monte carlo budget exceeded ({} iterations × {size} nodes+edges > {MAX_WORK})",
                req.iterations
            ),
        });
    }
    Ok(())
}

/// [`monte_carlo`] over an already validated graph and its CPM result, so
/// callers that already ran CPM (`simulate`) do not repeat it.
pub(crate) fn monte_carlo_with_cpm(
    graph: &PlanGraph,
    req: &MonteCarloRequest,
    cpm: &CriticalPathResult,
) -> Result<MonteCarloSummary, PlannerError> {
    check_request(graph, req)?;
    let deterministic_makespan = cpm.critical_path_duration;

    let estimator = EffortEstimator::new();
    let mut tasks: Vec<Task> = graph
        .deliverables
        .iter()
        .map(|d| deliverable_to_task(d, &estimator))
        .collect();
    crate::schedule::add_endpoints(graph, &mut tasks);

    // (deliverable index, distribution) for each sampled deliverable: only
    // those whose scheduled length comes from the estimate.
    let mut sampled: Vec<(usize, Pert<f32>)> = Vec::new();
    for (i, d) in graph.deliverables.iter().enumerate() {
        let Some(e) = d.estimate else { continue };
        if d.duration_hours.is_some()
            || d.estimated_effort_hours.is_some()
            || e.pessimistic <= e.optimistic
        {
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
    let mut kernel = Kernel::new(&tasks)?;
    let mut rng = ChaCha8Rng::seed_from_u64(req.seed);
    let mut makespans = Vec::with_capacity(n);
    let mut moments = Moments::new(sampled.len());
    let mut draws = vec![0.0_f32; sampled.len()];
    let mut critical_runs = vec![0u32; graph.deliverables.len()];

    for _ in 0..n {
        for (k, (i, dist)) in sampled.iter().enumerate() {
            let v = dist.sample(&mut rng);
            kernel.len[*i] = v;
            draws[k] = v;
        }
        kernel.run();
        let makespan = kernel.ef[finish_idx];
        makespans.push(makespan);
        moments.add(&draws, makespan);
        for (i, runs) in critical_runs.iter_mut().enumerate() {
            if kernel.float[i] < CRITICAL_EPSILON {
                *runs += 1;
            }
        }
    }

    let mean = (makespans.iter().map(|&m| f64::from(m)).sum::<f64>() / n as f64) as f32;
    let mut sorted = makespans;
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
        .enumerate()
        .map(|(k, (i, _))| Sensitivity {
            id: graph.deliverables[*i].id.clone(),
            correlation: moments.pearson(k),
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

/// Streaming sums for the Pearson correlation of each sampled length with
/// the makespan. Values are shifted by their first observation (a
/// constant series then sums to exactly zero, and the sums stay small), so
/// no per-iteration history is kept.
struct Moments {
    count: f64,
    /// First draw per sampled deliverable, and the first makespan.
    shift_x: Vec<f64>,
    shift_y: f64,
    sx: Vec<f64>,
    sxx: Vec<f64>,
    sxy: Vec<f64>,
    sy: f64,
    syy: f64,
}

impl Moments {
    fn new(k: usize) -> Self {
        Self {
            count: 0.0,
            shift_x: vec![0.0; k],
            shift_y: 0.0,
            sx: vec![0.0; k],
            sxx: vec![0.0; k],
            sxy: vec![0.0; k],
            sy: 0.0,
            syy: 0.0,
        }
    }

    fn add(&mut self, draws: &[f32], makespan: f32) {
        if self.count == 0.0 {
            self.shift_y = f64::from(makespan);
            for (s, &x) in self.shift_x.iter_mut().zip(draws) {
                *s = f64::from(x);
            }
        }
        self.count += 1.0;
        let y = f64::from(makespan) - self.shift_y;
        self.sy += y;
        self.syy += y * y;
        for (k, &x) in draws.iter().enumerate() {
            let x = f64::from(x) - self.shift_x[k];
            self.sx[k] += x;
            self.sxx[k] += x * x;
            self.sxy[k] += x * y;
        }
    }

    /// Pearson correlation for sampled deliverable `k`; 0 when either
    /// series is constant.
    fn pearson(&self, k: usize) -> f32 {
        let n = self.count;
        let cov = n * self.sxy[k] - self.sx[k] * self.sy;
        let vx = n * self.sxx[k] - self.sx[k] * self.sx[k];
        let vy = n * self.syy - self.sy * self.sy;
        if vx <= 0.0 || vy <= 0.0 {
            return 0.0;
        }
        (cov / (vx * vy).sqrt()) as f32
    }
}

/// Index-based CPM kernel built once per run: topological order plus
/// predecessor / successor lists, with reusable per-iteration buffers.
struct Kernel {
    order: Vec<usize>,
    preds: Vec<Vec<(usize, f32)>>,
    succs: Vec<Vec<(usize, f32)>>,
    len: Vec<f32>,
    es: Vec<f32>,
    ef: Vec<f32>,
    ls: Vec<f32>,
    float: Vec<f32>,
}

impl Kernel {
    /// Errors if the task graph has a dependency cycle or unknown id.
    fn new(tasks: &[Task]) -> Result<Self, PlannerError> {
        let n = tasks.len();
        let index: HashMap<&str, usize> = tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id.as_str(), i))
            .collect();
        let mut preds: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
        let mut succs: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
        for (i, t) in tasks.iter().enumerate() {
            let mut seen: Vec<usize> = Vec::new();
            for dep in &t.dependencies {
                let p = *index
                    .get(dep.as_str())
                    .ok_or_else(|| PlannerError::InvalidGraph {
                        reason: format!("unknown prerequisite '{dep}'"),
                    })?;
                if seen.contains(&p) {
                    continue;
                }
                seen.push(p);
                let lag = t.lag_by_dependency.get(dep).copied().unwrap_or(0.0);
                preds[i].push((p, lag));
                succs[p].push((i, lag));
            }
        }
        let mut indeg: Vec<usize> = preds.iter().map(Vec::len).collect();
        let mut order: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
        let mut head = 0;
        while head < order.len() {
            let u = order[head];
            head += 1;
            for &(v, _) in &succs[u] {
                indeg[v] -= 1;
                if indeg[v] == 0 {
                    order.push(v);
                }
            }
        }
        if order.len() != n {
            return Err(PlannerError::InvalidGraph {
                reason: "dependency cycle in Monte Carlo kernel".to_string(),
            });
        }
        Ok(Self {
            order,
            preds,
            succs,
            len: tasks.iter().map(|t| t.effort_hours).collect(),
            es: vec![0.0; n],
            ef: vec![0.0; n],
            ls: vec![0.0; n],
            float: vec![0.0; n],
        })
    }

    /// Forward and backward sweep over `self.len`; fills ES/EF/LS/float and
    /// returns the makespan (largest earliest finish). No allocation.
    fn run(&mut self) -> f32 {
        let mut makespan = 0.0_f32;
        for &i in &self.order {
            let mut es = 0.0_f32;
            for &(p, lag) in &self.preds[i] {
                es = es.max(self.ef[p] + lag);
            }
            self.es[i] = es;
            self.ef[i] = es + self.len[i];
            makespan = makespan.max(self.ef[i]);
        }
        for &i in self.order.iter().rev() {
            let mut lf = makespan;
            let mut first = true;
            for &(s, lag) in &self.succs[i] {
                let v = self.ls[s] - lag;
                if first || v < lf {
                    lf = v;
                    first = false;
                }
            }
            self.ls[i] = lf - self.len[i];
            self.float[i] = self.ls[i] - self.es[i];
        }
        makespan
    }
}

/// Nearest-rank percentile of an ascending, non-empty slice.
fn nearest_rank(sorted: &[f32], q: f64) -> f32 {
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::CpmAlgorithm;
    use rand::RngExt;

    fn random_tasks(rng: &mut ChaCha8Rng) -> Vec<Task> {
        let n = rng.random_range(5..=40usize);
        (0..n)
            .map(|i| {
                let mut t = Task {
                    id: format!("t{i}"),
                    effort_hours: if rng.random_range(0..6) == 0 {
                        0.0
                    } else {
                        rng.random_range(1.0..20.0f32)
                    },
                    ..Task::default()
                };
                for j in 0..i {
                    if rng.random_range(0..5) == 0 {
                        t.dependencies.push(format!("t{j}"));
                        if rng.random_range(0..3) == 0 {
                            t.lag_by_dependency
                                .insert(format!("t{j}"), rng.random_range(0.5..5.0f32));
                        }
                    }
                }
                t
            })
            .collect()
    }

    /// Two-pass Pearson, the reference the streaming sums must match.
    fn two_pass_pearson(x: &[f32], y: &[f32]) -> f32 {
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

    #[test]
    fn streaming_pearson_matches_two_pass() {
        let mut rng = ChaCha8Rng::seed_from_u64(11);
        let x: Vec<f32> = (0..5000).map(|_| rng.random_range(1.0..30.0f32)).collect();
        let y: Vec<f32> = x
            .iter()
            .map(|&v| 100.0 + v * 0.5 + rng.random_range(0.0..10.0f32))
            .collect();
        let mut m = Moments::new(1);
        for (&a, &b) in x.iter().zip(&y) {
            m.add(&[a], b);
        }
        assert!((m.pearson(0) - two_pass_pearson(&x, &y)).abs() < 1e-5);
    }

    #[test]
    fn streaming_pearson_is_zero_for_constant_makespan() {
        let mut m = Moments::new(1);
        for v in [1.0, 5.0, 2.5, 9.0] {
            m.add(&[v], 12.3);
        }
        assert_eq!(m.pearson(0), 0.0);
    }

    #[test]
    fn index_kernel_matches_calculate_on_random_graphs() {
        let mut rng = ChaCha8Rng::seed_from_u64(99);
        let mut mismatches: Vec<String> = Vec::new();
        for g in 0..20 {
            let mut tasks = random_tasks(&mut rng);
            let mut kernel = Kernel::new(&tasks).unwrap();
            let makespan = kernel.run();
            let result = CpmAlgorithm::calculate(&mut tasks);
            if (makespan - result.critical_path_duration).abs() > 1e-3 {
                mismatches.push(format!("graph {g}: makespan"));
            }
            for (i, t) in tasks.iter().enumerate() {
                if (kernel.float[i] - t.float).abs() > 1e-3 {
                    mismatches.push(format!("graph {g}: float of {}", t.id));
                }
            }
        }
        assert_eq!(mismatches, Vec::<String>::new());
    }

    #[test]
    #[ignore = "timing probe: cargo test --release -- --ignored --nocapture"]
    fn timing_1000_deliverables_2000_iterations() {
        let mut rng = ChaCha8Rng::seed_from_u64(5);
        let deliverables: Vec<serde_json::Value> = (0..1000)
            .map(|i| {
                let prereqs: Vec<String> = (0..i)
                    .rev()
                    .take(30)
                    .filter(|_| rng.random_range(0..10) == 0)
                    .map(|j| format!("d{j}"))
                    .collect();
                serde_json::json!({"id": format!("d{i}"), "owned_files": [],
                    "prerequisites": prereqs,
                    "estimate": {"optimistic": 1.0, "likely": 3.0, "pessimistic": 9.0}})
            })
            .collect();
        let graph: PlanGraph =
            serde_json::from_value(serde_json::json!({ "deliverables": deliverables })).unwrap();
        let start = std::time::Instant::now();
        monte_carlo(
            &graph,
            &MonteCarloRequest {
                iterations: 2000,
                seed: 1,
            },
        )
        .unwrap();
        println!("TIMING 1000x2000: {:?}", start.elapsed());
    }
}
