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

/// One invalid hour value found by [`value_problems`].
pub(crate) struct ValueProblem {
    pub(crate) message: String,
    /// Deliverable id, then the prerequisite id for a lag problem.
    pub(crate) ids: Vec<String>,
}

/// True when `v` is a usable hour value: finite and within
/// `0..=MAX_HOURS`.
fn hours_ok(v: f32) -> bool {
    v.is_finite() && (0.0..=crate::plan::MAX_HOURS).contains(&v)
}

fn range_message(subject: &str, field: &str, v: f32) -> String {
    format!(
        "{subject} has invalid {field} {v}; must be a finite number between 0 and {}",
        crate::plan::MAX_HOURS
    )
}

/// Every invalid hour value in `graph`, in graph order: effort, duration,
/// three-point estimate (range, then ordering) and edge lag. Shared by
/// submit-time validation (which reports the first) and `plan.lint` (which
/// reports all), so both use identical messages.
pub(crate) fn value_problems(graph: &crate::plan::PlanGraph) -> Vec<ValueProblem> {
    let mut out = Vec::new();
    for d in &graph.deliverables {
        let subject = format!("deliverable '{}'", d.id);
        for (field, v) in [
            ("estimated_effort_hours", d.estimated_effort_hours),
            ("duration_hours", d.duration_hours),
        ] {
            if let Some(h) = v.filter(|h| !hours_ok(*h)) {
                out.push(ValueProblem {
                    message: range_message(&subject, field, h),
                    ids: vec![d.id.clone()],
                });
            }
        }
        if let Some(e) = d.estimate {
            let fields = [
                ("estimate.optimistic", e.optimistic),
                ("estimate.likely", e.likely),
                ("estimate.pessimistic", e.pessimistic),
            ];
            let mut in_range = true;
            for (field, v) in fields {
                if !hours_ok(v) {
                    in_range = false;
                    out.push(ValueProblem {
                        message: range_message(&subject, field, v),
                        ids: vec![d.id.clone()],
                    });
                }
            }
            if in_range && !(e.optimistic <= e.likely && e.likely <= e.pessimistic) {
                out.push(ValueProblem {
                    message: format!(
                        "{subject} estimate must satisfy 0 <= optimistic <= likely <= pessimistic"
                    ),
                    ids: vec![d.id.clone()],
                });
            }
        }
        for p in &d.prerequisites {
            let lag = p.lag_hours();
            if !hours_ok(lag) {
                out.push(ValueProblem {
                    message: range_message(
                        &format!("prerequisite '{}' of deliverable '{}'", p.id(), d.id),
                        "lag_hours",
                        lag,
                    ),
                    ids: vec![d.id.clone(), p.id().to_string()],
                });
            }
        }
    }
    out
}
