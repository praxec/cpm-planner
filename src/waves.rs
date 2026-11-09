//! Delivery waves: which deliverables each expensive gate proves.
//!
//! A deliverable with `metadata.kind == "gate"` is a fixed-cost delivery
//! operation: a full regression run, a CI pipeline, a promotion, a release.
//! Everything else here is computed from the graph, so the same plan always
//! classifies the same way and no caller has to infer "is this milestone
//! heavy?" from prose.
//!
//! - Gates are visited in topological order, ties in declaration order.
//! - A gate's **wave** claims every ancestor that is ordinary work and that no
//!   earlier gate claimed. Milestones, gates and `metadata.kind == "manual"`
//!   deliverables are never work.
//! - A gate that claims no new work (a parallel check, or the next hop of a
//!   promotion chain) joins the latest wave among its ancestors, so a
//!   regression, its pull request and its release count as one wave. With no
//!   ancestor wave it opens an empty wave.
//! - A milestone is a `wave` milestone when a gate reaches it with no other
//!   milestone in between (or it is itself a gate), else a `checkpoint`.
//!
//! The findings all concern plans that declare gates, except
//! `UNDECLARED_GATE`, which exists to catch the plan that hides heavy work
//! inside an ordinary deliverable.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::estimator::EffortEstimator;
use crate::graph::{prerequisite_ids, reachability};
use crate::lint::{LintFinding, Severity};
use crate::plan::{Deliverable, PlanGraph};
use crate::schedule::scheduled_length;

/// `metadata.kind` of a fixed-cost delivery operation.
pub const GATE_KIND: &str = "gate";

/// `metadata` key that states why a wave cannot merge into the next one.
pub const BOUNDARY_REASON_KEY: &str = "boundary_reason";

/// `metadata` key: `false` declares that scanned text naming a heavy operation
/// does not mean this deliverable runs one.
pub const HEAVY_KEY: &str = "heavy";

/// With several waves, a wave proving fewer work deliverables than this is
/// `WAVE_TOO_SMALL`.
pub const MIN_WAVE_DELIVERABLES: usize = 3;

/// With several waves, gate time at or above this share of all scheduled time
/// is `GATE_TIME_DOMINATES`.
pub const GATE_DOMINANCE_FRACTION: f32 = 0.5;

/// Phrases naming a heavy operation, matched on whole words after lowercasing
/// and replacing every non-alphanumeric character with a space. "regression
/// test" is deliberately absent: adding one is ordinary implementation work.
const HEAVY_PHRASES: [&str; 17] = [
    "all tests",
    "ci cycle",
    "ci pipeline",
    "ci run",
    "cross platform matrix",
    "entire test suite",
    "full build",
    "full regression",
    "full suite",
    "full test suite",
    "merged pr",
    "open pr",
    "pull request",
    "regression run",
    "regression suite",
    "release build",
    "test workspace",
];

/// How a milestone relates to the expensive operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MilestoneClass {
    /// A gate proves this milestone; the heavy operations happen here.
    Wave,
    /// An acceptance point with no gate of its own; nothing heavy is implied.
    Checkpoint,
}

/// One milestone's computed class.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MilestoneRow {
    pub id: String,
    pub class: MilestoneClass,
    /// The 1-based wave of the gate proving it; `None` for a checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wave: Option<usize>,
}

/// One batch of work and the gates that prove it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wave {
    /// 1-based, in topological order of the wave's first gate.
    pub wave: usize,
    pub gates: Vec<String>,
    /// Work deliverables this wave proves.
    pub deliverables: Vec<String>,
    pub work_hours: f32,
    pub gate_hours: f32,
    /// Scheduled work per scheduled gate hour; `None` with zero gate hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_per_gate_hour: Option<f32>,
    /// Why this wave cannot merge into the next, from its gates' metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_reason: Option<String>,
}

/// The plan's delivery-wave structure, reported by `plan.lint`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaveReport {
    pub gate_count: usize,
    pub wave_count: usize,
    pub work_hours: f32,
    pub gate_hours: f32,
    /// `gate_hours / (work_hours + gate_hours)`; 0 for an empty plan.
    pub gate_fraction: f32,
    pub waves: Vec<Wave>,
    pub milestones: Vec<MilestoneRow>,
    /// Work no gate proves. Empty for a plan without gates, where the
    /// question does not arise.
    pub ungated: Vec<String>,
}

fn kind_is(d: &Deliverable, kind: &str) -> bool {
    d.metadata.get("kind").and_then(serde_json::Value::as_str) == Some(kind)
}

/// Whether `d` is a fixed-cost delivery gate.
pub fn is_gate(d: &Deliverable) -> bool {
    kind_is(d, GATE_KIND)
}

fn is_work(d: &Deliverable) -> bool {
    !is_gate(d) && !d.is_milestone() && !kind_is(d, "manual")
}

fn boundary_reason(d: &Deliverable) -> Option<String> {
    d.metadata
        .get(BOUNDARY_REASON_KEY)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string)
}

/// Deliverable indices in topological order, ties in declaration order.
/// `None` for a graph with a cycle.
fn topological(graph: &PlanGraph) -> Option<Vec<usize>> {
    let index: HashMap<&str, usize> = graph
        .deliverables
        .iter()
        .enumerate()
        .map(|(i, d)| (d.id.as_str(), i))
        .collect();
    let n = graph.deliverables.len();
    let mut indegree = vec![0usize; n];
    let mut successors: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, d) in graph.deliverables.iter().enumerate() {
        let distinct: HashSet<&str> = prerequisite_ids(d).collect();
        for p in distinct {
            let &pi = index.get(p)?;
            indegree[i] += 1;
            successors[pi].push(i);
        }
    }
    let mut order = Vec::with_capacity(n);
    let mut frontier: std::collections::BTreeSet<usize> =
        (0..n).filter(|&i| indegree[i] == 0).collect();
    while let Some(next) = frontier.pop_first() {
        order.push(next);
        for &s in &successors[next] {
            indegree[s] -= 1;
            if indegree[s] == 0 {
                frontier.insert(s);
            }
        }
    }
    (order.len() == n).then_some(order)
}

/// Compute the wave structure of a graph with unique, known ids and no cycle.
pub fn analyze(graph: &PlanGraph) -> Option<WaveReport> {
    let order = topological(graph)?;
    let estimator = EffortEstimator::new();
    let length = |d: &Deliverable| scheduled_length(d, &estimator);
    let gates: Vec<&Deliverable> = order
        .iter()
        .map(|&i| &graph.deliverables[i])
        .filter(|d| is_gate(d))
        .collect();

    let mut report = WaveReport {
        gate_count: gates.len(),
        wave_count: 0,
        work_hours: 0.0,
        gate_hours: 0.0,
        gate_fraction: 0.0,
        waves: Vec::new(),
        milestones: Vec::new(),
        ungated: Vec::new(),
    };
    for d in &graph.deliverables {
        if is_gate(d) {
            report.gate_hours += length(d);
        } else if is_work(d) {
            report.work_hours += length(d);
        }
    }
    let total = report.work_hours + report.gate_hours;
    if total > 0.0 {
        report.gate_fraction = report.gate_hours / total;
    }

    if gates.is_empty() {
        report.milestones = graph
            .deliverables
            .iter()
            .filter(|d| d.is_milestone())
            .map(|d| MilestoneRow {
                id: d.id.clone(),
                class: MilestoneClass::Checkpoint,
                wave: None,
            })
            .collect();
        return Some(report);
    }

    let reach = reachability(graph);
    let reaches = |from: &str, to: &str| reach.get(from).is_some_and(|r| r.contains(to));

    // id -> wave index for claimed work and for gates.
    let mut assigned: HashMap<&str, usize> = HashMap::new();
    for gate in &gates {
        let claimed: Vec<&Deliverable> = order
            .iter()
            .map(|&i| &graph.deliverables[i])
            .filter(|d| {
                is_work(d) && !assigned.contains_key(d.id.as_str()) && reaches(&d.id, &gate.id)
            })
            .collect();
        let joined = claimed.is_empty().then(|| {
            graph
                .deliverables
                .iter()
                .filter(|d| reaches(&d.id, &gate.id))
                .filter_map(|d| assigned.get(d.id.as_str()).copied())
                .max()
        });
        let slot = match joined.flatten() {
            Some(existing) => existing,
            None => {
                report.waves.push(Wave {
                    wave: report.waves.len() + 1,
                    gates: Vec::new(),
                    deliverables: Vec::new(),
                    work_hours: 0.0,
                    gate_hours: 0.0,
                    work_per_gate_hour: None,
                    boundary_reason: None,
                });
                report.waves.len() - 1
            }
        };
        let wave = &mut report.waves[slot];
        wave.gates.push(gate.id.clone());
        wave.gate_hours += length(gate);
        if wave.boundary_reason.is_none() {
            wave.boundary_reason = boundary_reason(gate);
        }
        assigned.insert(gate.id.as_str(), slot);
        for d in claimed {
            wave.deliverables.push(d.id.clone());
            wave.work_hours += length(d);
            assigned.insert(d.id.as_str(), slot);
        }
    }
    for wave in &mut report.waves {
        wave.work_per_gate_hour =
            (wave.gate_hours > 0.0).then(|| wave.work_hours / wave.gate_hours);
    }
    report.wave_count = report.waves.len();
    report.ungated = graph
        .deliverables
        .iter()
        .filter(|d| is_work(d) && !assigned.contains_key(d.id.as_str()))
        .map(|d| d.id.clone())
        .collect();

    let milestones: Vec<&Deliverable> = graph
        .deliverables
        .iter()
        .filter(|d| d.is_milestone())
        .collect();
    report.milestones = milestones
        .iter()
        .map(|m| {
            let proving = gates
                .iter()
                .filter(|g| {
                    g.id == m.id
                        || (reaches(&g.id, &m.id)
                            && !milestones.iter().any(|between| {
                                between.id != m.id
                                    && between.id != g.id
                                    && reaches(&g.id, &between.id)
                                    && reaches(&between.id, &m.id)
                            }))
                })
                .filter_map(|g| assigned.get(g.id.as_str()).copied())
                .max();
            MilestoneRow {
                id: m.id.clone(),
                class: if proving.is_some() {
                    MilestoneClass::Wave
                } else {
                    MilestoneClass::Checkpoint
                },
                wave: proving.map(|slot| slot + 1),
            }
        })
        .collect();
    Some(report)
}

fn finding(code: &str, severity: Severity, message: String, ids: Vec<String>) -> LintFinding {
    LintFinding {
        code: code.to_string(),
        severity,
        message,
        ids,
        path: None,
    }
}

/// The text of `d` that could name a heavy operation, normalised to
/// space-separated lowercase words with a space at each end.
fn scanned_text(d: &Deliverable) -> String {
    let mut parts: Vec<String> = vec![d.id.clone()];
    for key in ["artifact", "description", "acceptance", "verify"] {
        match d.metadata.get(key) {
            Some(serde_json::Value::String(text)) => parts.push(text.clone()),
            Some(serde_json::Value::Array(items)) => parts.extend(
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string),
            ),
            _ => {}
        }
    }
    let joined = parts.join(" ").to_lowercase();
    let spaced: String = joined
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    format!(
        " {} ",
        spaced.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

/// The first heavy phrase `d` names, unless it may legitimately name one: a
/// gate, an explicit `metadata.heavy: false`, compiler-generated release
/// nodes (their shape is already governed by `delivery_gates`), or a
/// zero-length milestone, which by definition does no work.
fn heavy_phrase(d: &Deliverable) -> Option<&'static str> {
    let bare_milestone = d.is_milestone()
        && d.duration_hours.is_none()
        && d.estimated_effort_hours.is_none()
        && d.estimate.is_none();
    if is_gate(d)
        || bare_milestone
        || d.metadata.get("release").is_some()
        || d.metadata
            .get(HEAVY_KEY)
            .and_then(serde_json::Value::as_bool)
            == Some(false)
    {
        return None;
    }
    let text = scanned_text(d);
    HEAVY_PHRASES
        .iter()
        .find(|phrase| text.contains(&format!(" {phrase} ")))
        .copied()
}

/// `UNDECLARED_GATE`: heavy work hidden in an ordinary deliverable.
pub(crate) fn undeclared_gate_findings(graph: &PlanGraph) -> Vec<LintFinding> {
    graph
        .deliverables
        .iter()
        .filter_map(|d| {
            heavy_phrase(d).map(|phrase| {
                finding(
                    "UNDECLARED_GATE",
                    Severity::Warning,
                    format!(
                        "'{}' names a heavy operation ('{phrase}') but is not a gate; move it to a \
                         metadata.kind = \"gate\" deliverable at the milestone and keep targeted \
                         checks here, or set metadata.heavy = false if it runs none",
                        d.id
                    ),
                    vec![d.id.clone()],
                )
            })
        })
        .collect()
}

/// The findings that follow from a computed wave structure.
pub(crate) fn wave_findings(graph: &PlanGraph, report: &WaveReport) -> Vec<LintFinding> {
    let mut out = Vec::new();
    if report.gate_count == 0 {
        return out;
    }
    let by_id: HashMap<&str, &Deliverable> = graph
        .deliverables
        .iter()
        .map(|d| (d.id.as_str(), d))
        .collect();
    let reach = reachability(graph);

    for d in graph.deliverables.iter().filter(|d| is_gate(d)) {
        if d.duration_hours.is_none() && d.estimated_effort_hours.is_none() && d.estimate.is_none()
        {
            out.push(finding(
                "GATE_NO_DURATION",
                Severity::Warning,
                format!(
                    "gate '{}' declares no observed time; set duration_hours or a three-point \
                     estimate so its cost is on the schedule",
                    d.id
                ),
                vec![d.id.clone()],
            ));
        }
        let feeds_milestone = d.is_milestone()
            || reach.get(&d.id).is_some_and(|r| {
                r.iter()
                    .any(|id| by_id.get(id.as_str()).is_some_and(|m| m.is_milestone()))
            });
        if !feeds_milestone {
            out.push(finding(
                "GATE_WITHOUT_MILESTONE",
                Severity::Warning,
                format!(
                    "gate '{}' proves no milestone; add the acceptance point it exists for",
                    d.id
                ),
                vec![d.id.clone()],
            ));
        }
        // Work that waits on a gate's receipt stops coding for the whole run.
        for dependent in graph
            .deliverables
            .iter()
            .filter(|w| is_work(w) && prerequisite_ids(w).any(|p| p == d.id))
        {
            out.push(finding(
                "WORK_WAITS_ON_GATE",
                Severity::Info,
                format!(
                    "'{}' waits for gate '{}'; consume the code or contract instead unless it \
                     needs the proven or released result",
                    dependent.id, d.id
                ),
                vec![dependent.id.clone(), d.id.clone()],
            ));
        }
    }

    if !report.ungated.is_empty() {
        out.push(finding(
            "UNGATED_WORK",
            Severity::Warning,
            format!(
                "{} work deliverable(s) reach no gate, so nothing proves them; make a gate consume \
                 them or cut them",
                report.ungated.len()
            ),
            report.ungated.clone(),
        ));
    }

    if report.wave_count > 1 {
        let total = report.work_hours + report.gate_hours;
        if total > 0.0 && report.gate_fraction >= GATE_DOMINANCE_FRACTION {
            out.push(finding(
                "GATE_TIME_DOMINATES",
                Severity::Warning,
                format!(
                    "{} wave(s) spend {:.0}% of scheduled time in gates; merge waves so more work \
                     shares each run",
                    report.wave_count,
                    report.gate_fraction * 100.0
                ),
                report.waves.iter().flat_map(|w| w.gates.clone()).collect(),
            ));
        }
        for wave in &report.waves {
            if wave.deliverables.len() < MIN_WAVE_DELIVERABLES {
                out.push(finding(
                    "WAVE_TOO_SMALL",
                    Severity::Warning,
                    format!(
                        "wave {} proves {} work deliverable(s) (minimum {MIN_WAVE_DELIVERABLES} \
                         when a plan has several waves); merge it with a neighbour",
                        wave.wave,
                        wave.deliverables.len()
                    ),
                    wave.gates.clone(),
                ));
            }
        }
        for wave in &report.waves[..report.waves.len() - 1] {
            if wave.boundary_reason.is_none() {
                out.push(finding(
                    "WAVE_BOUNDARY_UNJUSTIFIED",
                    Severity::Warning,
                    format!(
                        "wave {} ends before the plan does; merge it into the next wave, or state \
                         in metadata.{BOUNDARY_REASON_KEY} on its gate why it cannot merge",
                        wave.wave
                    ),
                    wave.gates.clone(),
                ));
            }
        }
    }
    out
}
