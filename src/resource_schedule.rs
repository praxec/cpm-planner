//! Resource-constrained scheduling: levels a plan against per-resource
//! capacities with event-driven list scheduling, then reports the driving
//! chain (what each link waited on) and project / feeding buffers.

use crate::estimator::EffortEstimator;
use crate::plan::{Deliverable, FINISH_ID, PlanGraph, PlannerError};
use crate::task::CriticalPathResult;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const EPS: f32 = 1e-4;
const UNASSIGNED: &str = "unassigned";

/// Default `resource_key`: deliverables name their resource in
/// `metadata.owner`.
pub(crate) fn default_resource_key() -> String {
    "owner".to_string()
}

/// Default `project_buffer_pct`.
pub(crate) fn default_buffer_pct() -> f32 {
    25.0
}

/// Check `project_buffer_pct` is a finite percentage in `0..=100`.
pub(crate) fn check_buffer_pct(pct: f32) -> Result<(), String> {
    if pct.is_finite() && (0.0..=100.0).contains(&pct) {
        Ok(())
    } else {
        Err(format!(
            "project_buffer_pct must be between 0 and 100; got {pct}"
        ))
    }
}

/// Input to [`resource_schedule`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRequest {
    /// Units available per resource name.
    pub capacities: BTreeMap<String, u32>,
    /// Metadata key naming a deliverable's resource.
    #[serde(default = "default_resource_key")]
    pub resource_key: String,
    /// Buffer size as a percentage of the chain's scheduled length.
    #[serde(default = "default_buffer_pct")]
    pub project_buffer_pct: f32,
}

/// One deliverable's leveled position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledRow {
    pub id: String,
    pub resource: String,
    pub start: f32,
    pub finish: f32,
}

/// Busy time of one resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceLoad {
    pub resource: String,
    pub capacity: u32,
    pub busy_hours: f32,
    pub utilisation: f32,
}

/// What a driving-chain link waited on before starting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    Start,
    Dependency,
    Resource,
}

/// One link of the driving chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChainLink {
    pub id: String,
    pub waited_on: WaitKind,
}

/// Buffer protecting the point where a side chain joins the driving chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedingBuffer {
    pub joins: String,
    pub from_chain: Vec<String>,
    pub buffer_hours: f32,
}

/// Result of [`resource_schedule`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceSchedule {
    pub makespan: f32,
    /// Unconstrained CPM makespan, for comparison.
    pub cpm_makespan: f32,
    /// Graph order, no synthetic rows.
    pub rows: Vec<ScheduledRow>,
    /// Sorted by resource.
    pub load: Vec<ResourceLoad>,
    /// Execution order; the first link waited on `Start`.
    pub driving_chain: Vec<ChainLink>,
    /// Percentage of the summed scheduled lengths on `driving_chain`.
    pub project_buffer_hours: f32,
    pub feeding_buffers: Vec<FeedingBuffer>,
}

struct Node {
    len: f32,
    resource: String,
    /// (predecessor index, lag); a repeated id keeps its largest lag.
    preds: Vec<(usize, f32)>,
}

/// Level `graph` against `req.capacities`.
///
/// The graph is validated exactly as `plan.submit` validates it, so a
/// reserved or duplicate id, a cycle, an unknown prerequisite or an
/// out-of-range hour value is [`PlannerError::InvalidGraph`].
pub fn resource_schedule(
    graph: &PlanGraph,
    req: &ScheduleRequest,
) -> Result<ResourceSchedule, PlannerError> {
    crate::planner::validate_graph(graph)?;
    let cpm = crate::schedule::compute_cpm(graph)?;
    resource_schedule_with_cpm(graph, req, &cpm)
}

/// [`resource_schedule`] over an already validated graph and its CPM
/// result, so callers that already ran CPM (`simulate`) do not repeat it.
pub(crate) fn resource_schedule_with_cpm(
    graph: &PlanGraph,
    req: &ScheduleRequest,
    cpm: &CriticalPathResult,
) -> Result<ResourceSchedule, PlannerError> {
    check_buffer_pct(req.project_buffer_pct)
        .map_err(|reason| PlannerError::InvalidGraph { reason })?;
    let cpm_makespan = cpm.get_task(FINISH_ID).map_or(0.0, |t| t.earliest_finish);

    let nodes = build_nodes(graph, &req.resource_key)?;
    check_capacities(&nodes, &req.capacities)?;
    let n = nodes.len();
    let order = topological_order(&nodes)?;
    let tail = tails(&nodes, &order);

    // Allocate units only for resources that carry work, and never more than
    // the number of work-bearing deliverables on that resource: extra units
    // could never be occupied, and sizing by raw capacity can demand huge
    // allocations for large (even u32::MAX) capacities.
    let mut work_per_resource: BTreeMap<&str, usize> = BTreeMap::new();
    for n in nodes.iter().filter(|n| n.len > 0.0) {
        *work_per_resource.entry(n.resource.as_str()).or_insert(0) += 1;
    }
    let mut units: BTreeMap<&str, Vec<f32>> = work_per_resource
        .into_iter()
        .map(|(resource, work)| {
            let capacity = req
                .capacities
                .get(resource)
                .copied()
                .expect("INVARIANT: capacities validated before allocation")
                as usize;
            (resource, vec![0.0; capacity.min(work)])
        })
        .collect();
    let mut start = vec![f32::NAN; n];
    let mut finish = vec![f32::NAN; n];
    let mut done = 0;
    while done < n {
        // Candidate: every predecessor placed; its earliest feasible start.
        let mut best: Option<(usize, f32, usize)> = None; // (idx, start, unit)
        for i in 0..n {
            if !start[i].is_nan() || nodes[i].preds.iter().any(|&(p, _)| start[p].is_nan()) {
                continue;
            }
            let ready = nodes[i]
                .preds
                .iter()
                .map(|&(p, lag)| finish[p] + lag)
                .fold(0.0_f32, f32::max);
            let (s, unit) = if nodes[i].len > 0.0 {
                let pool = &units[nodes[i].resource.as_str()];
                let (u, free) = pool
                    .iter()
                    .enumerate()
                    .fold(
                        (0, f32::INFINITY),
                        |acc, (u, &f)| {
                            if f < acc.1 { (u, f) } else { acc }
                        },
                    );
                (ready.max(free), u)
            } else {
                (ready, 0)
            };
            let better = match best {
                None => true,
                Some((b, bs, _)) => {
                    if (s - bs).abs() > EPS {
                        s < bs
                    } else {
                        let (zi, zb) = (nodes[i].len <= 0.0, nodes[b].len <= 0.0);
                        if zi != zb {
                            zi
                        } else if (tail[i] - tail[b]).abs() > EPS {
                            tail[i] > tail[b]
                        } else {
                            graph.deliverables[i].id < graph.deliverables[b].id
                        }
                    }
                }
            };
            if better {
                best = Some((i, s, unit));
            }
        }
        let Some((i, s, unit)) = best else {
            return Err(PlannerError::InvalidGraph {
                reason: "resource schedule could not place every deliverable".to_string(),
            });
        };
        start[i] = s;
        finish[i] = s + nodes[i].len;
        if nodes[i].len > 0.0
            && let Some(pool) = units.get_mut(nodes[i].resource.as_str())
        {
            pool[unit] = finish[i];
        }
        done += 1;
    }

    let ids: Vec<&str> = graph.deliverables.iter().map(|d| d.id.as_str()).collect();
    let makespan = finish.iter().copied().fold(0.0_f32, f32::max);
    let rows = (0..n)
        .map(|i| ScheduledRow {
            id: ids[i].to_string(),
            resource: nodes[i].resource.clone(),
            start: start[i],
            finish: finish[i],
        })
        .collect();

    let chain = driving_chain(&nodes, &ids, &start, &finish);
    let chain_set: BTreeSet<usize> = chain.iter().map(|&(i, _)| i).collect();
    let pct = req.project_buffer_pct / 100.0;
    let project_buffer_hours = pct * chain.iter().map(|&(i, _)| nodes[i].len).sum::<f32>();
    let feeding_buffers = feeding_buffers(&nodes, &ids, &order, &chain_set, pct);

    Ok(ResourceSchedule {
        makespan,
        cpm_makespan,
        rows,
        load: loads(&nodes, &req.capacities, makespan),
        driving_chain: chain
            .iter()
            .map(|&(i, waited_on)| ChainLink {
                id: ids[i].to_string(),
                waited_on,
            })
            .collect(),
        project_buffer_hours,
        feeding_buffers,
    })
}

fn build_nodes(graph: &PlanGraph, key: &str) -> Result<Vec<Node>, PlannerError> {
    let estimator = EffortEstimator::new();
    let index: HashMap<&str, usize> = graph
        .deliverables
        .iter()
        .enumerate()
        .map(|(i, d)| (d.id.as_str(), i))
        .collect();
    graph
        .deliverables
        .iter()
        .map(|d| {
            let mut preds: BTreeMap<usize, f32> = BTreeMap::new();
            for p in &d.prerequisites {
                let &pi = index
                    .get(p.id())
                    .ok_or_else(|| PlannerError::InvalidGraph {
                        reason: format!("{} lists unknown prerequisite {}", d.id, p.id()),
                    })?;
                let lag = preds.entry(pi).or_insert(0.0);
                *lag = lag.max(p.lag_hours().max(0.0));
            }
            Ok(Node {
                len: crate::schedule::scheduled_length(d, &estimator),
                resource: resource_of(d, key),
                preds: preds.into_iter().collect(),
            })
        })
        .collect()
}

fn resource_of(d: &Deliverable, key: &str) -> String {
    d.metadata
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or(UNASSIGNED)
        .to_string()
}

/// Only work that occupies time needs capacity; milestones are exempt.
fn check_capacities(nodes: &[Node], caps: &BTreeMap<String, u32>) -> Result<(), PlannerError> {
    let missing: BTreeSet<&String> = nodes
        .iter()
        .filter(|n| n.len > 0.0)
        .map(|n| &n.resource)
        .filter(|r| caps.get(*r).copied().unwrap_or(0) == 0)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(PlannerError::InvalidCapacities {
            missing: missing.into_iter().cloned().collect(),
        })
    }
}

/// Kahn order over `preds`; errors on a cycle (unreachable once the graph
/// is validated, but never loop on it).
fn topological_order(nodes: &[Node]) -> Result<Vec<usize>, PlannerError> {
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        for &(p, _) in &n.preds {
            succs[p].push(i);
        }
    }
    let mut indeg: Vec<usize> = nodes.iter().map(|n| n.preds.len()).collect();
    let mut order: Vec<usize> = (0..nodes.len()).filter(|&i| indeg[i] == 0).collect();
    let mut head = 0;
    while head < order.len() {
        let u = order[head];
        head += 1;
        for &v in &succs[u] {
            indeg[v] -= 1;
            if indeg[v] == 0 {
                order.push(v);
            }
        }
    }
    if order.len() == nodes.len() {
        Ok(order)
    } else {
        Err(PlannerError::InvalidGraph {
            reason: "resource schedule found a prerequisite cycle".to_string(),
        })
    }
}

/// Longest remaining tail per node: its length plus the heaviest successor
/// (edge lag + that successor's tail). Filled in reverse topological order,
/// so no recursion.
fn tails(nodes: &[Node], order: &[usize]) -> Vec<f32> {
    let mut succs: Vec<Vec<(usize, f32)>> = vec![Vec::new(); nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        for &(p, lag) in &n.preds {
            succs[p].push((i, lag));
        }
    }
    let mut tail = vec![0.0_f32; nodes.len()];
    for &i in order.iter().rev() {
        let best = succs[i]
            .iter()
            .map(|&(s, lag)| lag + tail[s])
            .fold(0.0_f32, f32::max);
        tail[i] = nodes[i].len + best;
    }
    tail
}

fn driving_chain(
    nodes: &[Node],
    ids: &[&str],
    start: &[f32],
    finish: &[f32],
) -> Vec<(usize, WaitKind)> {
    let Some(mut cur) = (0..nodes.len()).min_by(|&a, &b| {
        finish[b]
            .total_cmp(&finish[a])
            .then_with(|| ids[a].cmp(ids[b]))
    }) else {
        return Vec::new();
    };
    let mut rev = Vec::new();
    loop {
        let s = start[cur];
        if s <= EPS {
            rev.push((cur, WaitKind::Start));
            break;
        }
        let dep = nodes[cur]
            .preds
            .iter()
            .filter(|&&(p, lag)| (finish[p] + lag - s).abs() <= EPS)
            .map(|&(p, _)| p)
            .min_by(|&a, &b| ids[a].cmp(ids[b]));
        if let Some(p) = dep {
            rev.push((cur, WaitKind::Dependency));
            cur = p;
            continue;
        }
        let res = (0..nodes.len())
            .filter(|&j| {
                j != cur
                    && nodes[j].len > 0.0
                    && nodes[j].resource == nodes[cur].resource
                    && (finish[j] - s).abs() <= EPS
            })
            .min_by(|&a, &b| ids[a].cmp(ids[b]));
        if let Some(j) = res {
            rev.push((cur, WaitKind::Resource));
            cur = j;
            continue;
        }
        rev.push((cur, WaitKind::Start));
        break;
    }
    rev.reverse();
    rev
}

fn feeding_buffers(
    nodes: &[Node],
    ids: &[&str],
    order: &[usize],
    chain: &BTreeSet<usize>,
    pct: f32,
) -> Vec<FeedingBuffer> {
    // Heaviest off-chain dependency chain ending at each node, filled in
    // topological order: (summed length, best off-chain predecessor, first
    // node of that chain). Ties go to the chain whose first id is smaller.
    let mut sum = vec![0.0_f32; nodes.len()];
    let mut via: Vec<Option<usize>> = vec![None; nodes.len()];
    let mut head: Vec<usize> = (0..nodes.len()).collect();
    for &i in order {
        let mut top: Option<usize> = None;
        for &(p, _) in nodes[i].preds.iter().filter(|(p, _)| !chain.contains(p)) {
            let take = match top {
                None => true,
                Some(t) => {
                    sum[p] > sum[t] + EPS
                        || ((sum[p] - sum[t]).abs() <= EPS && ids[head[p]] < ids[head[t]])
                }
            };
            if take {
                top = Some(p);
            }
        }
        sum[i] = top.map_or(0.0, |t| sum[t]) + nodes[i].len;
        via[i] = top;
        if let Some(t) = top {
            head[i] = head[t];
        }
    }
    let path_to = |end: usize| -> Vec<usize> {
        let mut path = vec![end];
        let mut cur = end;
        while let Some(p) = via[cur] {
            path.push(p);
            cur = p;
        }
        path.reverse();
        path
    };

    let mut out = Vec::new();
    for &c in chain {
        for &(p, _) in nodes[c].preds.iter().filter(|(p, _)| !chain.contains(p)) {
            out.push(FeedingBuffer {
                joins: ids[c].to_string(),
                from_chain: path_to(p).iter().map(|&i| ids[i].to_string()).collect(),
                buffer_hours: pct * sum[p],
            });
        }
    }
    out.sort_by(|a, b| {
        a.joins
            .cmp(&b.joins)
            .then_with(|| a.from_chain.first().cmp(&b.from_chain.first()))
    });
    out
}

fn loads(nodes: &[Node], caps: &BTreeMap<String, u32>, makespan: f32) -> Vec<ResourceLoad> {
    let mut busy: BTreeMap<&str, f32> = BTreeMap::new();
    for n in nodes.iter().filter(|n| n.len > 0.0) {
        *busy.entry(n.resource.as_str()).or_insert(0.0) += n.len;
    }
    busy.into_iter()
        .map(|(resource, busy_hours)| {
            let capacity = caps
                .get(resource)
                .copied()
                .expect("INVARIANT: capacities validated before allocation");
            let available = capacity as f32 * makespan;
            ResourceLoad {
                resource: resource.to_string(),
                capacity,
                busy_hours,
                utilisation: if available > 0.0 {
                    busy_hours / available
                } else {
                    0.0
                },
            }
        })
        .collect()
}
