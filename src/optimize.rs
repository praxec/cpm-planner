//! Offline, deterministic plan optimization: proposes `plan.fork` edit
//! lists, never applies them.
//!
//! [`optimize`] reads a [`PlanGraph`] and looks for five shapes of waste:
//!
//! | kind | signal | edits |
//! |---|---|---|
//! | `merge` | two work deliverables with the same `metadata.owner` claim the same file exclusively | replace both with one deliverable that keeps both prerequisites, files, effort and acceptance criteria |
//! | `false_dependency` | an edge whose `consumes` is missing, low-information or a resource constraint | remove the edge |
//! | `parallel_cohort` | three or more work deliverables joined only by such edges | remove all of them, so the members can run together |
//! | `gate_consolidation` | two consecutive delivery waves with one gate each and no stated boundary reason | drop the first gate and let the second prove both waves |
//! | `gate_bypass` | work waits on a gate but its `consumes` names neither the gate's artifact nor a proven result | depend on the gate's prerequisites instead |
//!
//! Every candidate is then checked the same way:
//!
//! 1. [`apply_edits`] to a clone (the input graph is never touched);
//! 2. ordering into milestones, gates, manual and external deliverables is
//!    kept: when an edit would let one of them stop waiting for work it
//!    waited for, a compensating `add_edge` is appended to the edit list;
//! 3. acceptance criteria, gates, manual/external approvals, interface
//!    contracts, owned files and milestones are checked against the original;
//! 4. [`simulate`] runs lint, CPM, the optional leveling and the scorecard;
//!    a lint error, or a warning the original does not have, rejects it;
//! 5. the candidate must not lengthen the schedule or add effort, and must
//!    improve something.
//!
//! A candidate that fails any step lands in [`OptimizeReport::rejected`] with
//! every reason. Output is a pure function of the graph and the request:
//! candidates are generated and ranked in sorted id order, and every number is
//! rounded to [`HOURS_SCALE`]. No model, provider or network is involved.
//! `plan.acquire_cohort` stays the authority for leases and file conflicts; a
//! `parallel_cohort` proposal only says the members could be leased together.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::edits::{GraphEdit, apply_edits};
use crate::estimator::EffortEstimator;
use crate::lint::{LintReport, Severity};
use crate::plan::{
    Deliverable, Estimate, FileMode, OwnedFile, PlanGraph, PlannerError, Prerequisite,
    PrerequisiteKind,
};
use crate::resource_schedule::ScheduleRequest;
use crate::schedule::{effort_basis, scheduled_length};
use crate::simulate::{SimulateRequest, SimulationResult, simulate};
use crate::waves;

/// Most candidates evaluated in one run; the rest are counted in
/// [`OptimizeReport::truncated`].
pub const MAX_CANDIDATES: usize = 256;

/// Default cap on [`OptimizeReport::proposals`].
pub const DEFAULT_MAX_PROPOSALS: usize = 32;

/// A same-directory or same-file group larger than this yields no merge
/// candidates (it is shared infrastructure, not micro-deliverables).
pub const MAX_MERGE_GROUP: usize = 32;

/// Score weight of one saved effort hour (a saved makespan hour weighs 1).
pub const EFFORT_WEIGHT: f32 = 0.5;

/// Score weight of one deliverable fewer.
pub const DELIVERABLE_WEIGHT: f32 = 0.25;

/// Score weight of one lint warning resolved.
pub const WARNING_WEIGHT: f32 = 0.1;

/// Scores and hour deltas are rounded to `1 / HOURS_SCALE`.
pub const HOURS_SCALE: f64 = 100.0;

/// Smallest score that counts as an improvement.
pub const MIN_SCORE: f32 = 0.01;

/// Metadata keys whose text is a deliverable's acceptance criteria.
pub const CRITERIA_KEYS: [&str; 4] = ["artifact", "acceptance", "acceptance_criteria", "verify"];

/// Words in an edge's `consumes` that mean the dependent needs a gate's
/// proven or released result rather than the code behind it. Whole words,
/// lower-cased, after replacing non-alphanumerics with spaces.
pub const GATE_RESULT_WORDS: [&str; 18] = [
    "approval",
    "approved",
    "evidence",
    "green",
    "merged",
    "pass",
    "passed",
    "passing",
    "promoted",
    "proof",
    "proven",
    "published",
    "release",
    "released",
    "report",
    "result",
    "signed",
    "verified",
];

const EPS: f32 = 1e-3;

/// Input to [`optimize`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizeRequest {
    /// Resource capacities; when present every candidate is leveled and the
    /// leveled makespan is the one that must not grow.
    #[serde(default)]
    pub schedule: Option<ScheduleRequest>,
    /// Cap on returned proposals; [`DEFAULT_MAX_PROPOSALS`] when absent.
    #[serde(default)]
    pub max_proposals: Option<usize>,
}

/// What a proposal changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizeKind {
    /// Two same-owner deliverables with overlapping files become one.
    Merge,
    /// An edge with no stated artifact is removed.
    FalseDependency,
    /// Several such edges are removed so their deliverables run together.
    ParallelCohort,
    /// Two consecutive waves share one gate execution.
    GateConsolidation,
    /// Work stops waiting for a gate whose result it does not consume.
    GateBypass,
    /// A caller-supplied edit list checked by [`validate_edits`].
    Edits,
}

impl OptimizeKind {
    /// Wire name, also the prefix of every proposal id of this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::FalseDependency => "false_dependency",
            Self::ParallelCohort => "parallel_cohort",
            Self::GateConsolidation => "gate_consolidation",
            Self::GateBypass => "gate_bypass",
            Self::Edits => "edits",
        }
    }
}

/// Headline numbers of one graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanMetrics {
    pub deliverables: usize,
    /// Unconstrained CPM makespan, hours.
    pub makespan: f32,
    /// Leveled makespan, when the request has capacities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_makespan: Option<f32>,
    /// Hours leveling adds to the CPM makespan (`resource_makespan -
    /// makespan`); 0 without capacities.
    pub resource_wait_hours: f32,
    /// Summed effort basis, hours.
    pub total_effort: f32,
    /// Scheduled hours spent in gates.
    pub gate_hours: f32,
    /// The scorecard's criticality risk.
    pub criticality_risk: f64,
    pub lint_warnings: usize,
}

/// `after - before` for each [`PlanMetrics`] number; negative is better.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDeltas {
    pub deliverables: i64,
    pub makespan_hours: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_makespan_hours: Option<f32>,
    pub resource_wait_hours: f32,
    pub effort_hours: f32,
    pub gate_hours: f32,
    pub criticality_risk: f64,
    pub lint_warnings: i64,
}

/// A validated edit list `plan.fork { edits }` can apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptimizeProposal {
    /// Stable id, e.g. `merge:a+b`, `gate_bypass:regression->docs`.
    pub id: String,
    pub kind: OptimizeKind,
    /// Deliverables involved, sorted.
    pub ids: Vec<String>,
    /// The edits, compensating edges included, in application order.
    pub edits: Vec<GraphEdit>,
    pub deltas: MetricDeltas,
    pub after: PlanMetrics,
    /// `-makespan delta + EFFORT_WEIGHT * effort saved + DELIVERABLE_WEIGHT *
    /// deliverables removed + WARNING_WEIGHT * warnings resolved`, where the
    /// makespan is the leveled one when capacities were given.
    pub score: f32,
    pub rationale: String,
    /// What was checked and kept, in fixed order.
    pub preserved: Vec<String>,
    /// What the proposal costs that the numbers do not show.
    pub trade_offs: Vec<String>,
}

/// A candidate that failed validation, with every reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedCandidate {
    pub id: String,
    pub kind: OptimizeKind,
    pub ids: Vec<String>,
    pub edits: Vec<GraphEdit>,
    pub reasons: Vec<String>,
}

/// Everything [`optimize`] reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptimizeReport {
    pub baseline: PlanMetrics,
    /// Best score first, ties by id.
    pub proposals: Vec<OptimizeProposal>,
    /// In candidate id order.
    pub rejected: Vec<RejectedCandidate>,
    /// Candidates evaluated (accepted plus rejected).
    pub candidates: usize,
    /// Candidates beyond [`MAX_CANDIDATES`] or proposals beyond
    /// `max_proposals` were left out.
    pub truncated: bool,
}

/// Propose edits for `graph` without changing it.
///
/// The graph must simulate cleanly first: a lint error or a submit-time
/// violation is [`PlannerError::InvalidGraph`] exactly as from [`simulate`].
pub fn optimize(graph: &PlanGraph, req: &OptimizeRequest) -> Result<OptimizeReport, PlannerError> {
    let sim_req = SimulateRequest {
        schedule: req.schedule.clone(),
        monte_carlo: None,
    };
    let base_sim = simulate(graph, &sim_req)?;
    let baseline = metrics(graph, &base_sim);
    let ctx = Context::new(graph, &base_sim.lint);

    let mut candidates: Vec<Candidate> = Vec::new();
    candidates.extend(ctx.merge_candidates());
    candidates.extend(ctx.false_dependency_candidates());
    candidates.extend(ctx.cohort_candidates());
    candidates.extend(ctx.gate_consolidation_candidates());
    candidates.extend(ctx.gate_bypass_candidates());
    candidates.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    candidates.dedup_by(|a, b| a.id == b.id);
    let mut truncated = candidates.len() > MAX_CANDIDATES;
    candidates.truncate(MAX_CANDIDATES);

    let mut proposals = Vec::new();
    let mut rejected = Vec::new();
    let count = candidates.len();
    for candidate in candidates {
        match ctx.evaluate(candidate, &sim_req, &base_sim.lint, &baseline) {
            Ok(p) => proposals.push(p),
            Err(r) => rejected.push(r),
        }
    }
    proposals.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    let cap = req.max_proposals.unwrap_or(DEFAULT_MAX_PROPOSALS);
    if proposals.len() > cap {
        truncated = true;
        proposals.truncate(cap);
    }
    rejected.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    Ok(OptimizeReport {
        baseline,
        proposals,
        rejected,
        candidates: count,
        truncated,
    })
}

/// Check a caller-supplied edit list with the same rules as a generated
/// candidate (id `edits`): compensating edges are appended, anything a
/// removed deliverable carried counts as lost, and the list must not lengthen
/// the schedule or add effort and must improve something.
///
/// The outer error is [`PlannerError::InvalidGraph`] when `graph` itself does
/// not simulate; the inner `Err` is the rejection with every reason.
pub fn validate_edits(
    graph: &PlanGraph,
    req: &OptimizeRequest,
    edits: &[GraphEdit],
) -> Result<Result<OptimizeProposal, RejectedCandidate>, PlannerError> {
    let sim_req = SimulateRequest {
        schedule: req.schedule.clone(),
        monte_carlo: None,
    };
    let base_sim = simulate(graph, &sim_req)?;
    let baseline = metrics(graph, &base_sim);
    let ctx = Context::new(graph, &base_sim.lint);
    let mut ids: Vec<String> = edits.iter().flat_map(edit_ids).collect();
    ids.retain(|id| !id.is_empty());
    let mut candidate = Candidate::new(OptimizeKind::Edits, String::new(), ids);
    candidate.id = OptimizeKind::Edits.as_str().to_string();
    candidate.edits = edits.to_vec();
    candidate.rationale = format!("{} caller-supplied edit(s)", edits.len());
    Ok(ctx.evaluate(candidate, &sim_req, &base_sim.lint, &baseline))
}

fn edit_ids(edit: &GraphEdit) -> Vec<String> {
    match edit {
        GraphEdit::RemoveEdge { from, to } | GraphEdit::AddEdge { from, to, .. } => {
            vec![from.clone(), to.clone()]
        }
        GraphEdit::SetEffort { id, .. }
        | GraphEdit::SetDuration { id, .. }
        | GraphEdit::SetEstimate { id, .. }
        | GraphEdit::SetMetadata { id, .. }
        | GraphEdit::RemoveDeliverable { id } => vec![id.clone()],
        GraphEdit::AddDeliverable { deliverable } => vec![deliverable.id.clone()],
    }
}

// ------------------------------------------------------------- candidates

struct Candidate {
    id: String,
    kind: OptimizeKind,
    ids: Vec<String>,
    edits: Vec<GraphEdit>,
    /// Removed deliverable id -> the deliverable that now carries it.
    removed: BTreeMap<String, String>,
    /// Reasons known before any edit is applied.
    blockers: Vec<String>,
    rationale: String,
    trade_offs: Vec<String>,
}

impl Candidate {
    fn new(kind: OptimizeKind, key: String, mut ids: Vec<String>) -> Self {
        ids.sort();
        ids.dedup();
        Self {
            id: format!("{}:{key}", kind.as_str()),
            kind,
            ids,
            edits: Vec::new(),
            removed: BTreeMap::new(),
            blockers: Vec::new(),
            rationale: String::new(),
            trade_offs: Vec::new(),
        }
    }

    fn reject(self, reasons: Vec<String>) -> RejectedCandidate {
        RejectedCandidate {
            id: self.id,
            kind: self.kind,
            ids: self.ids,
            edits: self.edits,
            reasons,
        }
    }
}

/// A cohort's members and the suspect edges joining them.
type Group<'s> = (BTreeSet<&'s str>, Vec<(&'s str, &'s str)>);

struct Context<'a> {
    graph: &'a PlanGraph,
    by_id: HashMap<&'a str, &'a Deliverable>,
    /// Deliverable id -> ids that depend on it directly, sorted.
    dependents: HashMap<&'a str, BTreeSet<&'a str>>,
    /// Original transitive successors.
    reach: HashMap<String, HashSet<String>>,
    /// Original topological position.
    topo: HashMap<&'a str, usize>,
    /// `(code, ids)` of every warning in the original lint.
    suspect_edges: BTreeMap<(String, String), &'static str>,
    estimator: EffortEstimator,
}

fn kind_of(d: &Deliverable) -> Option<&str> {
    d.metadata.get("kind").and_then(Value::as_str)
}

fn is_manual_or_external(d: &Deliverable) -> bool {
    matches!(kind_of(d), Some("manual" | "external"))
        || d.metadata.get("external").and_then(Value::as_bool) == Some(true)
}

fn is_contract(d: &Deliverable) -> bool {
    d.metadata.get("contract").and_then(Value::as_bool) == Some(true)
}

/// Deliverables whose ordering is a commitment: what they wait for must keep
/// reaching them.
fn is_anchor(d: &Deliverable) -> bool {
    d.is_milestone() || waves::is_gate(d) || is_manual_or_external(d)
}

fn is_work(d: &Deliverable) -> bool {
    !is_anchor(d) && !is_contract(d)
}

fn owner(d: &Deliverable) -> Option<&Value> {
    d.metadata.get("owner")
}

fn protected_reason(d: &Deliverable) -> Option<String> {
    if is_manual_or_external(d) {
        Some(format!("'{}' is a manual or external approval", d.id))
    } else if waves::is_gate(d) {
        Some(format!("'{}' is a gate", d.id))
    } else if d.is_milestone() {
        Some(format!("'{}' is a milestone", d.id))
    } else if is_contract(d) {
        Some(format!("'{}' is an interface contract", d.id))
    } else {
        None
    }
}

fn exclusive_overlap(a: &Deliverable, b: &Deliverable) -> Option<String> {
    a.owned_files.iter().find_map(|fa| {
        b.owned_files
            .iter()
            .find(|fb| {
                fa.path() == fb.path()
                    && !(fa.mode() == FileMode::Append && fb.mode() == FileMode::Append)
            })
            .map(|_| fa.path().display().to_string())
    })
}

fn words(text: &str) -> String {
    let spaced: String = text
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect();
    format!(
        " {} ",
        spaced.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

fn edge_consumes(to: &Deliverable, from: &str) -> Option<String> {
    let stated: Vec<&str> = to
        .prerequisites
        .iter()
        .filter(|p| p.id() == from)
        .filter_map(Prerequisite::consumes)
        .collect();
    (!stated.is_empty()).then(|| stated.join("; "))
}

fn criteria(d: &Deliverable) -> Vec<String> {
    let mut out = Vec::new();
    for key in CRITERIA_KEYS {
        match d.metadata.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => out.push(s.clone()),
            Some(Value::Array(items)) => out.extend(
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string),
            ),
            _ => {}
        }
    }
    out
}

/// Combine one criteria key of two deliverables without losing either.
fn combine_criterion(keep: Option<&Value>, other: Option<&Value>) -> Option<Value> {
    match (keep, other) {
        (None, None) => None,
        (Some(v), None) | (None, Some(v)) => Some(v.clone()),
        (Some(Value::String(a)), Some(Value::String(b))) if a == b || a.contains(b.as_str()) => {
            Some(Value::String(a.clone()))
        }
        (Some(Value::String(a)), Some(Value::String(b))) => {
            Some(Value::String(format!("{a}; {b}")))
        }
        (Some(a), Some(b)) => {
            let mut items: Vec<Value> = Vec::new();
            for v in [a, b] {
                match v {
                    Value::Array(xs) => items.extend(xs.iter().cloned()),
                    other => items.push(other.clone()),
                }
            }
            let mut seen = HashSet::new();
            items.retain(|v| seen.insert(v.to_string()));
            Some(Value::Array(items))
        }
    }
}

fn topological(graph: &PlanGraph) -> HashMap<&str, usize> {
    let ids: HashMap<&str, usize> = graph
        .deliverables
        .iter()
        .enumerate()
        .map(|(i, d)| (d.id.as_str(), i))
        .collect();
    let n = graph.deliverables.len();
    let mut indegree = vec![0usize; n];
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, d) in graph.deliverables.iter().enumerate() {
        let distinct: BTreeSet<&str> = d.prerequisites.iter().map(Prerequisite::id).collect();
        for p in distinct {
            if let Some(&pi) = ids.get(p) {
                indegree[i] += 1;
                succ[pi].push(i);
            }
        }
    }
    let mut frontier: BTreeSet<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    let mut out = HashMap::new();
    while let Some(next) = frontier.pop_first() {
        out.insert(graph.deliverables[next].id.as_str(), out.len());
        for &s in &succ[next] {
            indegree[s] -= 1;
            if indegree[s] == 0 {
                frontier.insert(s);
            }
        }
    }
    out
}

/// Transitive prerequisites of `id` in `graph`.
fn ancestors(graph: &PlanGraph, id: &str) -> BTreeSet<String> {
    let prereqs: HashMap<&str, Vec<&str>> = graph
        .deliverables
        .iter()
        .map(|d| {
            (
                d.id.as_str(),
                d.prerequisites.iter().map(Prerequisite::id).collect(),
            )
        })
        .collect();
    let mut seen = BTreeSet::new();
    let mut stack: Vec<&str> = prereqs.get(id).cloned().unwrap_or_default();
    while let Some(n) = stack.pop() {
        if seen.insert(n.to_string())
            && let Some(next) = prereqs.get(n)
        {
            stack.extend(next.iter().copied());
        }
    }
    seen
}

fn round_hours(hours: f32) -> f32 {
    ((f64::from(hours) * HOURS_SCALE).round() / HOURS_SCALE) as f32
}

fn round_risk(risk: f64) -> f64 {
    (risk * 10_000.0).round() / 10_000.0
}

fn fmt_hours(hours: f32) -> String {
    format!("{}", round_hours(hours))
}

fn metrics(graph: &PlanGraph, sim: &SimulationResult) -> PlanMetrics {
    let makespan = round_hours(sim.critical_path_hours);
    let resource_makespan = sim
        .resource_schedule
        .as_ref()
        .map(|r| round_hours(r.makespan));
    PlanMetrics {
        deliverables: graph.deliverables.len(),
        makespan,
        resource_makespan,
        resource_wait_hours: resource_makespan
            .map_or(0.0, |r| round_hours((r - makespan).max(0.0))),
        total_effort: round_hours(sim.scorecard.total_effort),
        gate_hours: round_hours(sim.lint.waves.as_ref().map_or(0.0, |w| w.gate_hours)),
        criticality_risk: round_risk(sim.scorecard.criticality_risk),
        lint_warnings: sim
            .lint
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count(),
    }
}

fn deltas(before: &PlanMetrics, after: &PlanMetrics) -> MetricDeltas {
    MetricDeltas {
        deliverables: after.deliverables as i64 - before.deliverables as i64,
        makespan_hours: round_hours(after.makespan - before.makespan),
        resource_makespan_hours: before
            .resource_makespan
            .zip(after.resource_makespan)
            .map(|(b, a)| round_hours(a - b)),
        resource_wait_hours: round_hours(after.resource_wait_hours - before.resource_wait_hours),
        effort_hours: round_hours(after.total_effort - before.total_effort),
        gate_hours: round_hours(after.gate_hours - before.gate_hours),
        criticality_risk: round_risk(after.criticality_risk - before.criticality_risk),
        lint_warnings: after.lint_warnings as i64 - before.lint_warnings as i64,
    }
}

impl<'a> Context<'a> {
    fn new(graph: &'a PlanGraph, lint: &LintReport) -> Self {
        let by_id = graph
            .deliverables
            .iter()
            .map(|d| (d.id.as_str(), d))
            .collect();
        let mut dependents: HashMap<&str, BTreeSet<&str>> = HashMap::new();
        for d in &graph.deliverables {
            for p in &d.prerequisites {
                dependents.entry(p.id()).or_default().insert(d.id.as_str());
            }
        }
        let mut suspect_edges = BTreeMap::new();
        for f in &lint.findings {
            let signal = match f.code.as_str() {
                "RESOURCE_CONSTRAINT_EDGE" => "its consumes names a resource constraint",
                "NO_RATIONALE" => "it does not say what it consumes",
                "LOW_INFORMATION_CONSUMES" if f.ids.len() == 2 => {
                    "its consumes does not identify an artifact"
                }
                _ => continue,
            };
            // Lint ids are [dependent, prerequisite].
            suspect_edges
                .entry((f.ids[1].clone(), f.ids[0].clone()))
                .or_insert(signal);
        }
        Self {
            graph,
            by_id,
            dependents,
            reach: crate::graph::reachability(graph),
            topo: topological(graph),
            suspect_edges,
            estimator: EffortEstimator::new(),
        }
    }

    fn reaches(&self, from: &str, to: &str) -> bool {
        self.reach.get(from).is_some_and(|r| r.contains(to))
    }

    fn length(&self, d: &Deliverable) -> f32 {
        scheduled_length(d, &self.estimator)
    }

    fn dependents_of(&self, id: &str) -> Vec<&'a str> {
        self.dependents
            .get(id)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    // ---------------------------------------------------------- merge

    fn merge_candidates(&self) -> Vec<Candidate> {
        let mut claimants: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
        for d in &self.graph.deliverables {
            for f in &d.owned_files {
                if f.mode() == FileMode::Exclusive {
                    claimants
                        .entry(f.path().display().to_string())
                        .or_default()
                        .insert(d.id.as_str());
                }
            }
        }
        // Append claims overlap an exclusive claim on the same path.
        for d in &self.graph.deliverables {
            for f in &d.owned_files {
                let path = f.path().display().to_string();
                if f.mode() == FileMode::Append
                    && let Some(set) = claimants.get_mut(&path)
                {
                    set.insert(d.id.as_str());
                }
            }
        }
        let mut pairs: BTreeSet<(&str, &str)> = BTreeSet::new();
        for members in claimants.values() {
            if members.len() > MAX_MERGE_GROUP {
                continue;
            }
            let list: Vec<&str> = members.iter().copied().collect();
            for (i, &a) in list.iter().enumerate() {
                for &b in &list[i + 1..] {
                    let (da, db) = (self.by_id[a], self.by_id[b]);
                    if owner(da).is_some() && owner(da) == owner(db) {
                        pairs.insert((a, b));
                    }
                }
            }
        }
        pairs
            .into_iter()
            .map(|(a, b)| self.merge(self.by_id[a], self.by_id[b]))
            .collect()
    }

    fn merge(&self, a: &'a Deliverable, b: &'a Deliverable) -> Candidate {
        // The upstream one survives; unordered pairs keep the smaller id.
        let (keep, other) = if self.reaches(&b.id, &a.id) {
            (b, a)
        } else {
            (a, b)
        };
        let mut c = Candidate::new(
            OptimizeKind::Merge,
            format!("{}+{}", a.id, b.id),
            vec![a.id.clone(), b.id.clone()],
        );
        let shared = exclusive_overlap(keep, other).unwrap_or_default();
        for d in [keep, other] {
            if let Some(reason) = protected_reason(d) {
                c.blockers
                    .push(format!("{reason}; it keeps its own deliverable"));
            }
        }
        // Edges leaving the pair, by dependent.
        let mut outgoing: BTreeMap<&str, Vec<&Prerequisite>> = BTreeMap::new();
        for src in [keep, other] {
            for dep in self.dependents_of(&src.id) {
                if dep == keep.id || dep == other.id {
                    continue;
                }
                for p in self.by_id[dep]
                    .prerequisites
                    .iter()
                    .filter(|p| p.id() == src.id)
                {
                    if p.kind() == Some(PrerequisiteKind::Interface) {
                        c.blockers.push(format!(
                            "interface edge '{}' -> '{dep}' would lose its kind",
                            src.id
                        ));
                    }
                    if p.lag_hours() != 0.0 {
                        c.blockers.push(format!(
                            "edge '{}' -> '{dep}' has a lag the merge would lose",
                            src.id
                        ));
                    }
                    outgoing.entry(dep).or_default().push(p);
                }
            }
        }
        for (dep, edges) in &outgoing {
            for p in edges {
                c.edits.push(GraphEdit::RemoveEdge {
                    from: p.id().to_string(),
                    to: (*dep).to_string(),
                });
            }
        }
        c.edits.push(GraphEdit::RemoveDeliverable {
            id: other.id.clone(),
        });
        c.edits.push(GraphEdit::RemoveDeliverable {
            id: keep.id.clone(),
        });
        c.edits.push(GraphEdit::AddDeliverable {
            deliverable: self.merged(keep, other),
        });
        for (dep, edges) in &outgoing {
            let mut texts: Vec<&str> = edges.iter().filter_map(|p| p.consumes()).collect();
            texts.dedup();
            c.edits.push(GraphEdit::AddEdge {
                from: keep.id.clone(),
                to: (*dep).to_string(),
                consumes: (!texts.is_empty()).then(|| texts.join("; ")),
            });
        }
        c.removed.insert(other.id.clone(), keep.id.clone());
        c.rationale = format!(
            "'{}' and '{}' share owner {} and file '{shared}'; one deliverable '{}' carries both \
             ({} h)",
            keep.id,
            other.id,
            owner(keep).map(Value::to_string).unwrap_or_default(),
            keep.id,
            fmt_hours(self.length(keep) + self.length(other)),
        );
        c.trade_offs.push(format!(
            "one lease covers both; '{}' can no longer be split across workers",
            keep.id
        ));
        c
    }

    fn merged(&self, keep: &Deliverable, other: &Deliverable) -> Deliverable {
        let mut prerequisites: Vec<Prerequisite> = Vec::new();
        for p in keep.prerequisites.iter().chain(&other.prerequisites) {
            if p.id() != keep.id
                && p.id() != other.id
                && !prerequisites.iter().any(|q| q.id() == p.id())
            {
                prerequisites.push(p.clone());
            }
        }
        let mut owned_files: Vec<OwnedFile> = Vec::new();
        for f in keep.owned_files.iter().chain(&other.owned_files) {
            match owned_files.iter_mut().find(|g| g.path() == f.path()) {
                Some(existing) => {
                    if f.mode() == FileMode::Exclusive {
                        *existing = OwnedFile::Path(f.path().to_path_buf());
                    }
                }
                None => owned_files.push(f.clone()),
            }
        }
        let mut metadata = match &keep.metadata {
            Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };
        if let Value::Object(om) = &other.metadata {
            for (k, v) in om {
                metadata.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        for key in CRITERIA_KEYS {
            if let Some(v) = combine_criterion(keep.metadata.get(key), other.metadata.get(key)) {
                metadata.insert(key.to_string(), v);
            }
        }
        metadata.insert(
            "merged_from".to_string(),
            Value::from(vec![keep.id.clone(), other.id.clone()]),
        );
        let effort =
            round_hours(effort_basis(keep, &self.estimator) + effort_basis(other, &self.estimator));
        let duration = (keep.duration_hours.is_some() || other.duration_hours.is_some())
            .then(|| round_hours(self.length(keep) + self.length(other)));
        let estimate = keep.estimate.zip(other.estimate).map(|(x, y)| Estimate {
            optimistic: x.optimistic + y.optimistic,
            likely: x.likely + y.likely,
            pessimistic: x.pessimistic + y.pessimistic,
        });
        Deliverable {
            id: keep.id.clone(),
            owned_files,
            prerequisites,
            estimated_effort_hours: Some(effort),
            duration_hours: duration,
            estimate,
            metadata: Value::Object(metadata),
            milestone: false,
            earning_rule: keep.earning_rule,
        }
    }

    // ------------------------------------------------ false dependency

    fn edge_blockers(&self, from: &Deliverable, to: &Deliverable) -> Vec<String> {
        let mut out = Vec::new();
        for d in [from, to] {
            if let Some(reason) = protected_reason(d) {
                out.push(format!("{reason}; its edges are commitments"));
            }
        }
        if to
            .prerequisites
            .iter()
            .any(|p| p.id() == from.id && p.kind() == Some(PrerequisiteKind::Interface))
        {
            out.push(format!(
                "'{}' -> '{}' is an interface contract edge",
                from.id, to.id
            ));
        }
        if let Some(path) = exclusive_overlap(from, to) {
            out.push(format!(
                "file collision on '{path}' between '{}' and '{}'; acquire_cohort would never \
                 lease them together",
                from.id, to.id
            ));
        }
        out
    }

    fn false_dependency_candidates(&self) -> Vec<Candidate> {
        self.suspect_edges
            .iter()
            .filter_map(|((from, to), signal)| {
                let (f, t) = (self.by_id.get(from.as_str())?, self.by_id.get(to.as_str())?);
                if waves::is_gate(f) && is_work(t) {
                    return None; // a gate_bypass candidate
                }
                let mut c = Candidate::new(
                    OptimizeKind::FalseDependency,
                    format!("{from}->{to}"),
                    vec![from.clone(), to.clone()],
                );
                c.blockers = self.edge_blockers(f, t);
                c.edits.push(GraphEdit::RemoveEdge {
                    from: from.clone(),
                    to: to.clone(),
                });
                c.rationale = format!("'{to}' waits on '{from}' but {signal}; removing the edge");
                c.trade_offs.push(format!(
                    "confirm '{to}' builds and tests without '{from}' before forking"
                ));
                Some(c)
            })
            .collect()
    }

    fn cohort_candidates(&self) -> Vec<Candidate> {
        // Union-find over suspect edges between work deliverables.
        let mut parent: BTreeMap<&str, &str> = BTreeMap::new();
        fn find<'s>(parent: &mut BTreeMap<&'s str, &'s str>, x: &'s str) -> &'s str {
            let p = *parent.entry(x).or_insert(x);
            if p == x {
                return x;
            }
            let root = find(parent, p);
            parent.insert(x, root);
            root
        }
        let mut edges: Vec<(&str, &str)> = Vec::new();
        for (from, to) in self.suspect_edges.keys() {
            let (Some(f), Some(t)) = (self.by_id.get(from.as_str()), self.by_id.get(to.as_str()))
            else {
                continue;
            };
            if is_anchor(f) || is_anchor(t) {
                continue;
            }
            edges.push((f.id.as_str(), t.id.as_str()));
            let (ra, rb) = (find(&mut parent, &f.id), find(&mut parent, &t.id));
            if ra != rb {
                let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
                parent.insert(hi, lo);
            }
        }
        let mut groups: BTreeMap<&str, Group<'_>> = BTreeMap::new();
        for &(f, t) in &edges {
            let root = find(&mut parent, f);
            let g = groups.entry(root).or_default();
            g.0.insert(f);
            g.0.insert(t);
            g.1.push((f, t));
        }
        groups
            .into_values()
            .filter(|(members, _)| members.len() >= 3)
            .map(|(members, edges)| {
                let ids: Vec<String> = members.iter().map(|s| (*s).to_string()).collect();
                let mut c =
                    Candidate::new(OptimizeKind::ParallelCohort, ids.join(","), ids.clone());
                for &(f, t) in &edges {
                    for reason in self.edge_blockers(self.by_id[f], self.by_id[t]) {
                        if !c.blockers.contains(&reason) {
                            c.blockers.push(reason);
                        }
                    }
                    c.edits.push(GraphEdit::RemoveEdge {
                        from: f.to_string(),
                        to: t.to_string(),
                    });
                }
                let list: Vec<&str> = members.iter().copied().collect();
                for (i, &a) in list.iter().enumerate() {
                    for &b in &list[i + 1..] {
                        if let Some(path) = exclusive_overlap(self.by_id[a], self.by_id[b]) {
                            let reason = format!(
                                "file collision on '{path}' between '{a}' and '{b}'; \
                                 acquire_cohort would never lease them together"
                            );
                            if !c.blockers.contains(&reason) {
                                c.blockers.push(reason);
                            }
                        }
                    }
                }
                c.rationale = format!(
                    "[{}] are chained only by edges that name no artifact; removing {} edge(s) \
                     lets them run as one cohort",
                    ids.join(", "),
                    edges.len()
                );
                c.trade_offs.push(
                    "acquire_cohort still decides which members are leased together".to_string(),
                );
                c
            })
            .collect()
    }

    // ------------------------------------------------------------ gates

    fn gate_consolidation_candidates(&self) -> Vec<Candidate> {
        let Some(report) = waves::analyze(self.graph) else {
            return Vec::new();
        };
        report
            .waves
            .windows(2)
            .filter_map(|pair| {
                let (first, second) = (&pair[0], &pair[1]);
                let g = self.by_id.get(first.gates.first()?.as_str())?;
                let h = self.by_id.get(second.gates.first()?.as_str())?;
                Some(self.consolidate(first, second, g, h))
            })
            .collect()
    }

    fn consolidate(
        &self,
        first: &waves::Wave,
        second: &waves::Wave,
        g: &'a Deliverable,
        h: &'a Deliverable,
    ) -> Candidate {
        let mut c = Candidate::new(
            OptimizeKind::GateConsolidation,
            format!("{}->{}", g.id, h.id),
            vec![g.id.clone(), h.id.clone()],
        );
        if first.gates.len() > 1 {
            c.blockers.push(format!(
                "wave {} has a chain of gates [{}]; consolidate it by hand",
                first.wave,
                first.gates.join(", ")
            ));
        }
        if let Some(reason) = &first.boundary_reason {
            c.blockers.push(format!(
                "wave {} states why it is its own wave: {reason}",
                first.wave
            ));
        }
        if is_manual_or_external(g) {
            c.blockers
                .push(format!("'{}' is a manual or external approval", g.id));
        }
        let command = |d: &Deliverable| d.metadata.get("command").cloned();
        if let (Some(a), Some(b)) = (command(g), command(h))
            && a != b
        {
            c.blockers.push(format!(
                "'{}' and '{}' run different commands, so they are not a repeated gate",
                g.id, h.id
            ));
        }
        for dep in self.dependents_of(&g.id) {
            let d = self.by_id[dep];
            c.edits.push(GraphEdit::RemoveEdge {
                from: g.id.clone(),
                to: dep.to_string(),
            });
            if is_work(d) && dep != h.id {
                self.rewire_through(&mut c, g, d);
            }
        }
        for p in &g.prerequisites {
            if p.id() != h.id
                && !h.prerequisites.iter().any(|q| q.id() == p.id())
                && !c.edits.iter().any(|e| matches!(e, GraphEdit::AddEdge { from, to, .. } if from == p.id() && *to == h.id))
            {
                c.edits.push(GraphEdit::AddEdge {
                    from: p.id().to_string(),
                    to: h.id.clone(),
                    consumes: Some(match p.consumes() {
                        Some(text) => format!("{text} (proven by '{}' with wave {})", h.id, second.wave),
                        None => format!("'{}' output, proven by '{}'", p.id(), h.id),
                    }),
                });
            }
        }
        for key in CRITERIA_KEYS {
            if g.metadata.get(key).is_some()
                && let Some(v) = combine_criterion(h.metadata.get(key), g.metadata.get(key))
                && Some(&v) != h.metadata.get(key)
            {
                c.edits.push(GraphEdit::SetMetadata {
                    id: h.id.clone(),
                    key: key.to_string(),
                    value: v,
                });
            }
        }
        c.edits.push(GraphEdit::SetMetadata {
            id: h.id.clone(),
            key: "consolidates".to_string(),
            value: Value::from(vec![g.id.clone()]),
        });
        c.edits
            .push(GraphEdit::RemoveDeliverable { id: g.id.clone() });
        c.removed.insert(g.id.clone(), h.id.clone());
        c.rationale = format!(
            "waves {} and {} each run a gate ('{}' {} h, '{}' {} h); '{}' proves both and '{}' \
             goes",
            first.wave,
            second.wave,
            g.id,
            fmt_hours(self.length(g)),
            h.id,
            fmt_hours(self.length(h)),
            h.id,
            g.id,
        );
        c.trade_offs.push(format!(
            "a failure in wave {}'s work is found at '{}', later than '{}' would find it",
            first.wave, h.id, g.id
        ));
        c
    }

    /// Make work `d` depend on `gate`'s prerequisites instead of `gate`.
    fn rewire_through(&self, c: &mut Candidate, gate: &Deliverable, d: &Deliverable) {
        let original = edge_consumes(d, &gate.id);
        for p in &gate.prerequisites {
            let from = p.id();
            if from == d.id || d.prerequisites.iter().any(|q| q.id() == from) {
                continue;
            }
            c.edits.push(GraphEdit::AddEdge {
                from: from.to_string(),
                to: d.id.clone(),
                consumes: Some(match &original {
                    Some(text) => format!("{text} from '{from}' (not via gate '{}')", gate.id),
                    None => format!("'{from}' code (not via gate '{}')", gate.id),
                }),
            });
        }
    }

    fn consumes_gate_result(&self, gate: &Deliverable, d: &Deliverable) -> bool {
        let Some(text) = edge_consumes(d, &gate.id) else {
            return false;
        };
        let text = words(&text);
        GATE_RESULT_WORDS
            .iter()
            .any(|w| text.contains(&format!(" {w} ")))
            || text.contains(&words(&gate.id))
            || criteria(gate).iter().any(|a| {
                let a = words(a);
                a.trim().len() > 1 && (text.contains(&a) || a.contains(&text))
            })
    }

    fn gate_bypass_candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for g in self.graph.deliverables.iter().filter(|d| waves::is_gate(d)) {
            for dep in self.dependents_of(&g.id) {
                let d = self.by_id[dep];
                if !is_work(d) || self.consumes_gate_result(g, d) {
                    continue;
                }
                let mut c = Candidate::new(
                    OptimizeKind::GateBypass,
                    format!("{}->{dep}", g.id),
                    vec![g.id.clone(), dep.to_string()],
                );
                c.edits.push(GraphEdit::RemoveEdge {
                    from: g.id.clone(),
                    to: dep.to_string(),
                });
                self.rewire_through(&mut c, g, d);
                c.rationale = format!(
                    "'{dep}' waits for gate '{}' but consumes {}, not its result; it can start \
                     on the code instead",
                    g.id,
                    edge_consumes(d, &g.id)
                        .map_or("nothing stated".to_string(), |t| format!("'{t}'")),
                );
                c.trade_offs.push(format!(
                    "'{dep}' may build on code '{}' later rejects",
                    g.id
                ));
                out.push(c);
            }
        }
        out
    }

    // ------------------------------------------------------- validation

    fn map<'m>(removed: &'m BTreeMap<String, String>, id: &'m str) -> &'m str {
        let mut cur = id;
        while let Some(next) = removed.get(cur) {
            cur = next;
        }
        cur
    }

    /// Edges that keep every anchor waiting for the work it waited for.
    fn compensate(&self, c: &Candidate, edited: &PlanGraph) -> Result<Vec<GraphEdit>, Vec<String>> {
        let mut edits = Vec::new();
        let mut reasons = Vec::new();
        let edited_ids: HashSet<&str> = edited.deliverables.iter().map(|d| d.id.as_str()).collect();
        for anchor in self.graph.deliverables.iter().filter(|d| is_anchor(d)) {
            let target = Self::map(&c.removed, &anchor.id);
            if !edited_ids.contains(target) {
                continue; // reported by the preservation check
            }
            let original = ancestors(self.graph, &anchor.id);
            let mut need: Vec<&str> = original
                .iter()
                .map(|x| Self::map(&c.removed, x))
                .filter(|m| *m != target && edited_ids.contains(m))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            need.sort_by_key(|m| std::cmp::Reverse(self.topo.get(m).copied().unwrap_or(0)));
            let mut have = ancestors(edited, target);
            for m in need {
                if have.contains(m) {
                    continue;
                }
                if ancestors(edited, m).contains(target) {
                    reasons.push(format!(
                        "'{}' would stop waiting for '{m}', and '{m}' already waits for it",
                        anchor.id
                    ));
                    continue;
                }
                edits.push(GraphEdit::AddEdge {
                    from: m.to_string(),
                    to: target.to_string(),
                    consumes: Some(format!(
                        "'{m}' result, kept for '{target}' by plan.optimize"
                    )),
                });
                have.insert(m.to_string());
                have.extend(ancestors(edited, m));
            }
        }
        if reasons.is_empty() {
            Ok(edits)
        } else {
            Err(reasons)
        }
    }

    /// Reasons `edited` loses something the original promised.
    fn preservation(&self, c: &Candidate, edited: &PlanGraph) -> (Vec<String>, Vec<String>) {
        let after: HashMap<&str, &Deliverable> = edited
            .deliverables
            .iter()
            .map(|d| (d.id.as_str(), d))
            .collect();
        let mut reasons = Vec::new();
        let (mut n_criteria, mut n_anchors, mut n_contracts, mut n_files, mut n_milestones) =
            (0, 0, 0, 0, 0);
        for d in &self.graph.deliverables {
            let target_id = Self::map(&c.removed, &d.id);
            let Some(target) = after.get(target_id) else {
                reasons.push(format!("'{}' is removed and nothing carries it", d.id));
                continue;
            };
            let kept = criteria(target);
            for crit in criteria(d) {
                n_criteria += 1;
                if !kept.iter().any(|k| k.contains(crit.as_str())) {
                    reasons.push(format!("acceptance criteria of '{}' lost: '{crit}'", d.id));
                }
            }
            if is_anchor(d) || is_contract(d) {
                n_anchors += usize::from(!d.is_milestone() && !is_contract(d));
                n_milestones += usize::from(d.is_milestone());
                n_contracts += usize::from(is_contract(d));
                let same = target_id == d.id
                    && target.is_milestone() == d.is_milestone()
                    && waves::is_gate(target) == waves::is_gate(d)
                    && is_manual_or_external(target) == is_manual_or_external(d)
                    && is_contract(target) == is_contract(d);
                let consolidated = target_id != d.id
                    && waves::is_gate(d)
                    && waves::is_gate(target)
                    && !is_manual_or_external(d);
                if !same && !consolidated {
                    let what =
                        protected_reason(d).unwrap_or_else(|| format!("'{}' is protected", d.id));
                    reasons.push(format!("{what} and would be removed or changed"));
                }
            }
            for f in &d.owned_files {
                n_files += 1;
                if !target.owned_files.iter().any(|g| g.path() == f.path()) {
                    reasons.push(format!(
                        "ownership of '{}' by '{}' lost",
                        f.path().display(),
                        d.id
                    ));
                }
            }
            if owner(d) != owner(target) && !waves::is_gate(d) {
                reasons.push(format!("owner of '{}' would change", d.id));
            }
            for p in d
                .prerequisites
                .iter()
                .filter(|p| p.kind() == Some(PrerequisiteKind::Interface))
            {
                let from = Self::map(&c.removed, p.id());
                if from != target_id
                    && !target
                        .prerequisites
                        .iter()
                        .any(|q| q.id() == from && q.kind() == Some(PrerequisiteKind::Interface))
                {
                    reasons.push(format!(
                        "interface contract edge '{}' -> '{}' lost",
                        p.id(),
                        d.id
                    ));
                }
            }
        }
        reasons.dedup();
        let preserved = vec![
            format!("acceptance criteria: {n_criteria}"),
            format!("gates and manual/external approvals: {n_anchors}"),
            format!("interface contracts: {n_contracts}"),
            format!("owned files: {n_files}"),
            format!("milestones: {n_milestones}"),
        ];
        (reasons, preserved)
    }

    fn evaluate(
        &self,
        mut c: Candidate,
        sim_req: &SimulateRequest,
        base_lint: &LintReport,
        baseline: &PlanMetrics,
    ) -> Result<OptimizeProposal, RejectedCandidate> {
        if !c.blockers.is_empty() {
            let reasons = std::mem::take(&mut c.blockers);
            return Err(c.reject(reasons));
        }
        let edited = match apply_edits(self.graph, &c.edits) {
            Ok(g) => g,
            Err(e) => return Err(c.reject(vec![format!("edits do not apply: {e}")])),
        };
        let edited = match self.compensate(&c, &edited) {
            Ok(extra) if extra.is_empty() => edited,
            Ok(extra) => {
                c.edits.extend(extra);
                match apply_edits(self.graph, &c.edits) {
                    Ok(g) => g,
                    Err(e) => return Err(c.reject(vec![format!("edits do not apply: {e}")])),
                }
            }
            Err(reasons) => return Err(c.reject(reasons)),
        };
        let known: HashSet<(String, Vec<String>)> = base_lint
            .findings
            .iter()
            .filter(|f| f.severity != Severity::Info)
            .map(|f| {
                let ids = f
                    .ids
                    .iter()
                    .map(|i| Self::map(&c.removed, i).to_string())
                    .collect();
                (f.code.clone(), ids)
            })
            .collect();
        // Edges the edits made redundant go: the ordering they state is kept
        // through another prerequisite. Interface edges stay (they are
        // contracts, not ordering).
        let redundant: Vec<GraphEdit> = crate::lint::lint(&edited)
            .findings
            .iter()
            .filter(|f| f.code == "REDUNDANT_EDGE" && f.ids.len() == 2)
            .filter(|f| !known.contains(&(f.code.clone(), f.ids.clone())))
            .filter(|f| {
                edited.deliverables.iter().any(|d| {
                    d.id == f.ids[0]
                        && d.prerequisites.iter().any(|p| {
                            p.id() == f.ids[1] && p.kind() != Some(PrerequisiteKind::Interface)
                        })
                })
            })
            .map(|f| GraphEdit::RemoveEdge {
                from: f.ids[1].clone(),
                to: f.ids[0].clone(),
            })
            .collect();
        let edited = if redundant.is_empty() {
            edited
        } else {
            c.edits.extend(redundant);
            match apply_edits(self.graph, &c.edits) {
                Ok(g) => g,
                Err(e) => return Err(c.reject(vec![format!("edits do not apply: {e}")])),
            }
        };
        let (reasons, preserved) = self.preservation(&c, &edited);
        if !reasons.is_empty() {
            return Err(c.reject(reasons));
        }
        let sim = match simulate(&edited, sim_req) {
            Ok(s) => s,
            Err(e) => return Err(c.reject(vec![format!("simulate rejects it: {e}")])),
        };
        let new_issues: Vec<String> = sim
            .lint
            .findings
            .iter()
            .filter(|f| f.severity != Severity::Info)
            .filter(|f| !known.contains(&(f.code.clone(), f.ids.clone())))
            .map(|f| format!("lint {} [{}]: {}", f.code, f.ids.join(", "), f.message))
            .collect();
        if !new_issues.is_empty() {
            return Err(c.reject(new_issues));
        }
        let after = metrics(&edited, &sim);
        let deltas = deltas(baseline, &after);
        let span = deltas
            .resource_makespan_hours
            .unwrap_or(deltas.makespan_hours);
        let mut reasons = Vec::new();
        if span > EPS {
            reasons.push(format!("makespan grows by {} h", fmt_hours(span)));
        }
        if deltas.effort_hours > EPS {
            reasons.push(format!(
                "effort grows by {} h",
                fmt_hours(deltas.effort_hours)
            ));
        }
        let score = round_hours(
            -span
                + EFFORT_WEIGHT * -deltas.effort_hours
                + DELIVERABLE_WEIGHT * (-deltas.deliverables) as f32
                + WARNING_WEIGHT * (-deltas.lint_warnings) as f32,
        );
        if reasons.is_empty() && score < MIN_SCORE {
            reasons.push(
                "no measurable improvement: makespan, effort, deliverable count and warnings \
                 unchanged"
                    .to_string(),
            );
        }
        if !reasons.is_empty() {
            return Err(c.reject(reasons));
        }
        if deltas.criticality_risk > 0.0 {
            c.trade_offs.push(format!(
                "criticality risk rises by {}",
                deltas.criticality_risk
            ));
        }
        let rationale = format!(
            "{}; simulated makespan {} h -> {} h{}",
            c.rationale,
            fmt_hours(baseline.makespan),
            fmt_hours(after.makespan),
            match (baseline.resource_makespan, after.resource_makespan) {
                (Some(b), Some(a)) => format!(", leveled {} h -> {} h", fmt_hours(b), fmt_hours(a)),
                _ => String::new(),
            }
        );
        Ok(OptimizeProposal {
            id: c.id,
            kind: c.kind,
            ids: c.ids,
            edits: c.edits,
            deltas,
            after,
            score,
            rationale,
            preserved,
            trade_offs: c.trade_offs,
        })
    }
}
