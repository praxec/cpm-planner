//! Crate-private helpers over the deliverable graph.

use crate::plan::Deliverable;

/// Ids of the deliverables `d` depends on, in declaration order.
pub(crate) fn prerequisite_ids(d: &Deliverable) -> impl Iterator<Item = &str> {
    d.prerequisites.iter().map(|p| p.id())
}

/// Transitive successors of every deliverable: `reach[x]` holds each id that
/// must wait (directly or through a chain) for `x`. Computed once per
/// validation; assumes nothing about acyclicity (a cycle is rejected later,
/// and the walk terminates regardless).
pub(crate) fn reachability(
    graph: &crate::plan::PlanGraph,
) -> std::collections::HashMap<String, std::collections::HashSet<String>> {
    use std::collections::{HashMap, HashSet};
    let mut direct: HashMap<&str, Vec<&str>> = HashMap::new();
    for d in &graph.deliverables {
        direct.entry(d.id.as_str()).or_default();
        for p in prerequisite_ids(d) {
            direct.entry(p).or_default().push(d.id.as_str());
        }
    }
    let mut reach = HashMap::new();
    for d in &graph.deliverables {
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack: Vec<&str> = direct.get(d.id.as_str()).cloned().unwrap_or_default();
        while let Some(n) = stack.pop() {
            if seen.insert(n.to_string())
                && let Some(next) = direct.get(n)
            {
                stack.extend(next.iter().copied());
            }
        }
        reach.insert(d.id.clone(), seen);
    }
    reach
}
