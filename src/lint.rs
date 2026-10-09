//! `plan.lint`: static checks over a [`PlanGraph`] that never submit it.
//!
//! Every submit-time rule of the planner is reported here as a finding
//! instead of an error, plus advisory checks (rationale, redundancy,
//! milestone coverage). Lint never panics on a malformed graph and its
//! output is deterministic.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::graph::{prerequisite_ids, reachability};
use crate::plan::{Deliverable, FINISH_ID, FileMode, PlanGraph, PrerequisiteKind, START_ID};

/// How serious a finding is. Orders errors first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// One lint result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LintFinding {
    /// Stable machine-readable code, e.g. `CYCLE`.
    pub code: String,
    pub severity: Severity,
    pub message: String,
    /// Deliverables involved (loop order for `CYCLE`).
    pub ids: Vec<String>,
    /// File path, for file findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// All findings for a graph. `clean` means no error or warning findings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LintReport {
    pub clean: bool,
    pub findings: Vec<LintFinding>,
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

fn bad(v: f32) -> bool {
    !v.is_finite() || v < 0.0
}

fn has_legacy_rationale(d: &Deliverable, prereq: &str) -> bool {
    d.metadata
        .get("consumes")
        .and_then(|c| c.get(prereq))
        .is_some_and(serde_json::Value::is_string)
}

/// Back edges found by a DFS along prerequisite links, one loop per back
/// edge. Each loop starts and ends at the same id.
fn find_cycles(graph: &PlanGraph, known: &HashSet<&str>) -> Vec<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Active,
        Done,
    }
    let mut by_id: HashMap<&str, &Deliverable> = HashMap::new();
    for d in &graph.deliverables {
        by_id.entry(d.id.as_str()).or_insert(d);
    }
    let mut marks: HashMap<&str, Mark> = HashMap::new();
    let mut cycles = Vec::new();
    for root in &graph.deliverables {
        if marks.contains_key(root.id.as_str()) {
            continue;
        }
        // Iterative DFS: (node, index of next prerequisite to visit).
        let mut stack: Vec<(&str, usize)> = vec![(root.id.as_str(), 0)];
        marks.insert(root.id.as_str(), Mark::Active);
        while let Some(&mut (node, ref mut next)) = stack.last_mut() {
            let prereqs: Vec<&str> = by_id.get(node).map_or_else(Vec::new, |d| {
                prerequisite_ids(d).filter(|p| known.contains(p)).collect()
            });
            if let Some(&p) = prereqs.get(*next) {
                *next += 1;
                match marks.get(p) {
                    None => {
                        marks.insert(p, Mark::Active);
                        stack.push((p, 0));
                    }
                    Some(Mark::Active) => {
                        if let Some(pos) = stack.iter().position(|(n, _)| *n == p) {
                            let mut ids: Vec<String> =
                                stack[pos..].iter().map(|(n, _)| (*n).to_string()).collect();
                            ids.push(p.to_string());
                            cycles.push(ids);
                        }
                    }
                    Some(Mark::Done) => {}
                }
            } else {
                marks.insert(node, Mark::Done);
                stack.pop();
            }
        }
    }
    cycles
}

/// Lint `graph`. Never errors or panics.
pub fn lint(graph: &PlanGraph) -> LintReport {
    use Severity::{Error, Info, Warning};
    let mut out: Vec<LintFinding> = Vec::new();

    // Identity.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut dup_reported: HashSet<&str> = HashSet::new();
    for d in &graph.deliverables {
        if d.id == START_ID || d.id == FINISH_ID {
            out.push(finding(
                "RESERVED_ID",
                Error,
                format!("deliverable id '{}' is reserved", d.id),
                vec![d.id.clone()],
            ));
        }
        if !seen.insert(d.id.as_str()) && dup_reported.insert(d.id.as_str()) {
            out.push(finding(
                "DUPLICATE_ID",
                Error,
                format!("duplicate deliverable id '{}'", d.id),
                vec![d.id.clone()],
            ));
        }
    }
    let known: HashSet<&str> = seen;

    // Values.
    for d in &graph.deliverables {
        for (field, v) in [
            ("estimated_effort_hours", d.estimated_effort_hours),
            ("duration_hours", d.duration_hours),
        ] {
            if let Some(h) = v.filter(|h| bad(*h)) {
                out.push(finding(
                    "INVALID_VALUE",
                    Error,
                    format!(
                        "deliverable '{}' has invalid {field} {h}; must be a finite number >= 0",
                        d.id
                    ),
                    vec![d.id.clone()],
                ));
            }
        }
        for p in &d.prerequisites {
            let lag = p.lag_hours();
            if bad(lag) {
                out.push(finding(
                    "INVALID_VALUE",
                    Error,
                    format!(
                        "prerequisite '{}' of deliverable '{}' has invalid lag_hours {lag}; must be a finite number >= 0",
                        p.id(),
                        d.id
                    ),
                    vec![d.id.clone(), p.id().to_string()],
                ));
            }
        }
    }

    // References and cycles.
    let mut unknown = false;
    for d in &graph.deliverables {
        for p in prerequisite_ids(d) {
            if !known.contains(p) {
                unknown = true;
                out.push(finding(
                    "UNKNOWN_PREREQUISITE",
                    Error,
                    format!(
                        "prerequisite '{p}' for deliverable '{}' does not exist",
                        d.id
                    ),
                    vec![d.id.clone(), p.to_string()],
                ));
            }
        }
    }
    let cycles = find_cycles(graph, &known);
    let cyclic = !cycles.is_empty();
    for ids in cycles {
        out.push(finding(
            "CYCLE",
            Error,
            format!("prerequisite cycle: {}", ids.join(" -> ")),
            ids,
        ));
    }

    // Shared files: same rule as submit.
    let reach = reachability(graph);
    let mut claimants: HashMap<&std::path::Path, Vec<(&str, FileMode)>> = HashMap::new();
    let mut path_order: Vec<&std::path::Path> = Vec::new();
    for d in &graph.deliverables {
        for f in &d.owned_files {
            claimants
                .entry(f.path())
                .or_insert_with(|| {
                    path_order.push(f.path());
                    Vec::new()
                })
                .push((d.id.as_str(), f.mode()));
        }
    }
    for path in path_order {
        let list = &claimants[path];
        for (i, &(x, xm)) in list.iter().enumerate() {
            for &(y, ym) in &list[i + 1..] {
                if x == y || (xm == FileMode::Append && ym == FileMode::Append) {
                    continue;
                }
                let ordered = reach.get(x).is_some_and(|r| r.contains(y))
                    || reach.get(y).is_some_and(|r| r.contains(x));
                if !ordered {
                    let mut f = finding(
                        "UNORDERED_FILE_OVERLAP",
                        Error,
                        format!(
                            "file '{}' is claimed by '{x}' and '{y}', which are not ordered by prerequisites",
                            path.display()
                        ),
                        vec![x.to_string(), y.to_string()],
                    );
                    f.path = Some(path.display().to_string());
                    out.push(f);
                }
            }
        }
    }

    // Edge quality.
    let by_id: HashMap<&str, &Deliverable> = {
        let mut m = HashMap::new();
        for d in &graph.deliverables {
            m.entry(d.id.as_str()).or_insert(d);
        }
        m
    };
    for d in &graph.deliverables {
        for p in &d.prerequisites {
            if p.consumes().is_none() && !has_legacy_rationale(d, p.id()) {
                out.push(finding(
                    "NO_RATIONALE",
                    Warning,
                    format!(
                        "'{}' depends on '{}' without saying what it consumes",
                        d.id,
                        p.id()
                    ),
                    vec![d.id.clone(), p.id().to_string()],
                ));
            }
            if p.kind() == Some(PrerequisiteKind::Interface)
                && let Some(target) = by_id.get(p.id())
                && target
                    .metadata
                    .get("contract")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
            {
                out.push(finding(
                    "INTERFACE_EDGE_NOT_CONTRACT",
                    Warning,
                    format!(
                        "'{}' has an interface edge to '{}', which is not marked metadata.contract = true",
                        d.id,
                        p.id()
                    ),
                    vec![d.id.clone(), p.id().to_string()],
                ));
            }
        }
    }

    // Structure checks need a well-formed DAG.
    if !cyclic && !unknown {
        for d in &graph.deliverables {
            let mut done: HashSet<&str> = HashSet::new();
            for p in prerequisite_ids(d) {
                if !done.insert(p) {
                    continue;
                }
                let implied = prerequisite_ids(d)
                    .any(|q| q != p && reach.get(p).is_some_and(|r| r.contains(q)));
                if implied {
                    out.push(finding(
                        "REDUNDANT_EDGE",
                        Warning,
                        format!(
                            "'{}' <- '{p}' is redundant: '{p}' is already an ancestor of another prerequisite",
                            d.id
                        ),
                        vec![d.id.clone(), p.to_string()],
                    ));
                }
            }
        }
        let milestones: Vec<&str> = graph
            .deliverables
            .iter()
            .filter(|d| d.is_milestone())
            .map(|d| d.id.as_str())
            .collect();
        if !milestones.is_empty() {
            for d in graph.deliverables.iter().filter(|d| !d.is_milestone()) {
                let feeds = reach
                    .get(d.id.as_str())
                    .is_some_and(|r| milestones.iter().any(|m| r.contains(*m)));
                if !feeds {
                    out.push(finding(
                        "FEEDS_NO_MILESTONE",
                        Warning,
                        format!("'{}' is neither a milestone nor an ancestor of one", d.id),
                        vec![d.id.clone()],
                    ));
                }
            }
        }
    }

    if !graph.deliverables.iter().any(Deliverable::is_milestone) {
        out.push(finding(
            "NO_MILESTONE",
            Info,
            "the graph defines no milestone".to_string(),
            Vec::new(),
        ));
    }
    for d in &graph.deliverables {
        if d.metadata.get("artifact").is_none() {
            out.push(finding(
                "NO_ARTIFACT",
                Info,
                format!("'{}' declares no metadata.artifact", d.id),
                vec![d.id.clone()],
            ));
        }
    }

    out.sort_by(|a, b| {
        (a.severity, &a.code, &a.ids, &a.path, &a.message)
            .cmp(&(b.severity, &b.code, &b.ids, &b.path, &b.message))
    });
    out.dedup();
    let clean = out.iter().all(|f| f.severity == Info);
    LintReport {
        clean,
        findings: out,
    }
}
