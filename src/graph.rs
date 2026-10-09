//! Crate-private helpers over the deliverable graph.

use crate::plan::Deliverable;

/// Ids of the deliverables `d` depends on, in declaration order.
pub(crate) fn prerequisite_ids(d: &Deliverable) -> impl Iterator<Item = &str> {
    d.prerequisites.iter().map(|p| p.id())
}
