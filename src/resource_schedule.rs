//! Resource-constrained scheduling: levels a plan against per-resource
//! capacities with event-driven list scheduling, then reports the driving
//! chain (what each link waited on) and project / feeding buffers.

use crate::estimator::EffortEstimator;
use crate::plan::{Deliverable, FINISH_ID, PlanGraph, PlannerError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const EPS: f32 = 1e-4;
const UNASSIGNED: &str = "unassigned";

fn default_resource_key() -> String {
    "owner".to_string()
}

fn default_buffer_pct() -> f32 {
    25.0
}

/// Input to [`resource_schedule`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
pub fn resource_schedule(
    graph: &PlanGraph,
    req: &ScheduleRequest,
) -> Result<ResourceSchedule, PlannerError> {
    let cpm = crate::schedule::compute_cpm(graph)?;
    if !req.project_buffer_pct.is_finite()
        || req.project_buffer_pct < 0.0
        || req.project_buffer_pct > 100.0
    {
        return Err(PlannerError::InvalidGraph {
            reason: format!(
                "project_buffer_pct must be between 0 and 100; got {}",
                req.project_buffer_pct
            ),
        });
    }
    let cpm_makespan = cpm.get_task(FINISH_ID).map_or(0.0, |t| t.earliest_finish);

    let nodes = build_nodes(graph, &req.resource_key)?;
    check_capacities(&nodes, &req.capacities)?;
    let n = nodes.len();
    let tail = tails(&nodes);

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
            let capacity = req.capacities.get(resource).copied().unwrap_or(0) as usize;
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
    let feeding_buffers = feeding_buffers(&nodes, &ids, &chain_set, pct);

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

/// Longest remaining tail per node: its length plus the heaviest successor
/// (edge lag + that successor's tail). Memoised over a DFS; acyclic by now.
fn tails(nodes: &[Node]) -> Vec<f32> {
    let mut succs: Vec<Vec<(usize, f32)>> = vec![Vec::new(); nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        for &(p, lag) in &n.preds {
            succs[p].push((i, lag));
        }
    }
    fn go(i: usize, nodes: &[Node], succs: &[Vec<(usize, f32)>], memo: &mut [Option<f32>]) -> f32 {
        if let Some(v) = memo[i] {
            return v;
        }
        let best = succs[i]
            .iter()
            .map(|&(s, lag)| lag + go(s, nodes, succs, memo))
            .fold(0.0_f32, f32::max);
        let v = nodes[i].len + best;
        memo[i] = Some(v);
        v
    }
    let mut memo = vec![None; nodes.len()];
    (0..nodes.len())
        .map(|i| go(i, nodes, &succs, &mut memo))
        .collect()
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
    chain: &BTreeSet<usize>,
    pct: f32,
) -> Vec<FeedingBuffer> {
    // Longest dependency chain ending at each off-chain node (nodes are not
    // topologically ordered, so memoise).
    fn best(
        i: usize,
        nodes: &[Node],
        ids: &[&str],
        chain: &BTreeSet<usize>,
        memo: &mut HashMap<usize, (f32, Vec<usize>)>,
    ) -> (f32, Vec<usize>) {
        if let Some(v) = memo.get(&i) {
            return v.clone();
        }
        let mut top: Option<(f32, Vec<usize>)> = None;
        for &(p, _) in nodes[i].preds.iter().filter(|(p, _)| !chain.contains(p)) {
            let cand = best(p, nodes, ids, chain, memo);
            let take = match &top {
                None => true,
                Some(t) => {
                    cand.0 > t.0 + EPS
                        || ((cand.0 - t.0).abs() <= EPS && ids[cand.1[0]] < ids[t.1[0]])
                }
            };
            if take {
                top = Some(cand);
            }
        }
        let (sum, mut path) = top.unwrap_or((0.0, Vec::new()));
        path.push(i);
        let v = (sum + nodes[i].len, path);
        memo.insert(i, v.clone());
        v
    }

    let mut memo = HashMap::new();
    let mut out = Vec::new();
    for &c in chain {
        for &(p, _) in nodes[c].preds.iter().filter(|(p, _)| !chain.contains(p)) {
            let (sum, path) = best(p, nodes, ids, chain, &mut memo);
            out.push(FeedingBuffer {
                joins: ids[c].to_string(),
                from_chain: path.iter().map(|&i| ids[i].to_string()).collect(),
                buffer_hours: pct * sum,
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
            let capacity = caps.get(resource).copied().unwrap_or(0);
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
