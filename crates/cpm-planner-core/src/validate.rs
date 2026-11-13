//! Structural validation of a [`PlanGraph`], shared by submit, revision and
//! every analysis entry point.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::plan::{FINISH_ID, FileMode, MAX_DELIVERABLES, PlanGraph, PlannerError, START_ID};

/// Reject graphs that fail any structural invariant. Returns
/// [`PlannerError::InvalidGraph`] with a precise `reason` on first failure.
/// Run by submit and by every analysis entry point (`resource_schedule`,
/// `monte_carlo`, `simulate`).
pub fn validate_graph(graph: &PlanGraph) -> Result<(), PlannerError> {
    if graph.deliverables.len() > MAX_DELIVERABLES {
        return Err(PlannerError::InvalidGraph {
            reason: format!(
                "plan has {} deliverables; maximum is {MAX_DELIVERABLES}",
                graph.deliverables.len()
            ),
        });
    }
    for d in &graph.deliverables {
        if d.id == START_ID || d.id == FINISH_ID {
            return Err(PlannerError::InvalidGraph {
                reason: format!("deliverable id '{}' is reserved", d.id),
            });
        }
    }

    // Duplicate ids.
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for d in &graph.deliverables {
        if !seen_ids.insert(d.id.as_str()) {
            return Err(PlannerError::InvalidGraph {
                reason: format!("duplicate deliverable id '{}'", d.id),
            });
        }
    }

    // Effort, duration, estimate and lag hours: finite, within
    // 0..=MAX_HOURS, and estimates ordered. Same messages as `plan.lint`.
    if let Some(problem) = crate::graph::value_problems(graph).into_iter().next() {
        return Err(PlannerError::InvalidGraph {
            reason: problem.message,
        });
    }

    // Prerequisite references resolve.
    let id_set: HashSet<&str> = graph.deliverables.iter().map(|d| d.id.as_str()).collect();
    for d in &graph.deliverables {
        for p in crate::graph::prerequisite_ids(d) {
            if !id_set.contains(p) {
                return Err(PlannerError::InvalidGraph {
                    reason: format!(
                        "prerequisite '{p}' for deliverable '{}' does not exist",
                        d.id
                    ),
                });
            }
        }
        if let Some(barrier) = d.metadata.get("consumes_revision_from") {
            let Some(barrier) = barrier.as_str().filter(|id| !id.is_empty()) else {
                return Err(PlannerError::InvalidGraph {
                    reason: format!(
                        "deliverable '{}' metadata.consumes_revision_from must be a non-empty deliverable id",
                        d.id
                    ),
                });
            };
            if !d.prerequisites.iter().any(|p| p.id() == barrier) {
                return Err(PlannerError::InvalidGraph {
                    reason: format!(
                        "deliverable '{}' consumes revision from '{barrier}' but does not declare it as a prerequisite",
                        d.id
                    ),
                });
            }
        }
    }

    // Shared files: any pair of claimants where at least one claim is
    // exclusive must be ordered by prerequisites, so one can never run
    // while the other holds the file. Append/append needs no ordering.
    let mut reach: Option<HashMap<String, HashSet<String>>> = None;
    let mut claimants: HashMap<&Path, Vec<(&str, FileMode)>> = HashMap::new();
    let mut path_order: Vec<&Path> = Vec::new();
    for d in &graph.deliverables {
        for f in &d.owned_files {
            let entry = claimants.entry(f.path()).or_insert_with(|| {
                path_order.push(f.path());
                Vec::new()
            });
            entry.push((d.id.as_str(), f.mode()));
        }
    }
    for path in path_order {
        let list = &claimants[path];
        for (i, &(x, xm)) in list.iter().enumerate() {
            for &(y, ym) in &list[i + 1..] {
                if x == y || (xm == FileMode::Append && ym == FileMode::Append) {
                    continue;
                }
                let reach = reach.get_or_insert_with(|| crate::graph::reachability(graph));
                let ordered = reach.get(x).is_some_and(|r| r.contains(y))
                    || reach.get(y).is_some_and(|r| r.contains(x));
                if !ordered {
                    return Err(PlannerError::InvalidGraph {
                        reason: format!(
                            "file '{}' is claimed by '{x}' and '{y}', which are not ordered by prerequisites (one could run while the other holds it)",
                            path.display(),
                        ),
                    });
                }
            }
        }
    }

    // Cycle detection via Kahn's algorithm on the prerequisite DAG.
    let mut indeg: HashMap<&str, usize> = HashMap::new();
    let mut succs: HashMap<&str, Vec<&str>> = HashMap::new();
    for d in &graph.deliverables {
        indeg.entry(d.id.as_str()).or_insert(0);
        succs.entry(d.id.as_str()).or_default();
    }
    for d in &graph.deliverables {
        for p in crate::graph::prerequisite_ids(d) {
            *indeg.entry(d.id.as_str()).or_insert(0) += 1;
            succs.entry(p).or_default().push(d.id.as_str());
        }
    }
    let mut queue: Vec<&str> = indeg
        .iter()
        .filter_map(|(k, v)| if *v == 0 { Some(*k) } else { None })
        .collect();
    let mut popped = 0_usize;
    while let Some(node) = queue.pop() {
        popped += 1;
        if let Some(s) = succs.get(node).cloned() {
            for next in s {
                if let Some(deg) = indeg.get_mut(next) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push(next);
                    }
                }
            }
        }
    }
    if popped < graph.deliverables.len() {
        // SPEC §33 audit fixup (F6 STUB-007) — name the cycle members.
        // After Kahn's terminates, any node with residual in-degree > 0
        // is part of (or downstream of) at least one cycle. Listing
        // them sorted gives operators a starting set to debug from
        // instead of "somewhere in your 50-deliverable graph there's
        // a cycle, good luck."
        let mut cycle_members: Vec<&str> = indeg
            .iter()
            .filter_map(|(k, v)| if *v > 0 { Some(*k) } else { None })
            .collect();
        cycle_members.sort_unstable();
        return Err(PlannerError::InvalidGraph {
            reason: format!(
                "cycle detected in prerequisite graph involving deliverables: [{}]",
                cycle_members.join(", ")
            ),
        });
    }

    Ok(())
}
