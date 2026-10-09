//! Pure structured edits over a [`PlanGraph`].
//!
//! A [`GraphEdit`] is the wire-stable description of one mutation an
//! operator (or an agent) may apply to a submitted graph. [`apply_edits`]
//! applies a batch in order to a clone of the input, then enforces the same
//! structural invariants `plan.submit` enforces via
//! [`crate::planner::validate_graph`]. It never touches the persisted plan;
//! callers decide whether to resubmit the result.

use serde::{Deserialize, Serialize};

use crate::plan::{Deliverable, Estimate, PlanGraph, PlannerError, Prerequisite};
use crate::planner::validate_graph;

/// One structured mutation of a [`PlanGraph`].
///
/// Serialised with an `op` tag (`snake_case` variant names) and strict
/// unknown-field rejection so a typo in an edit fails loudly rather than
/// silently no-op'ing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum GraphEdit {
    /// Remove prerequisite `from` from deliverable `to`.
    RemoveEdge { from: String, to: String },
    /// Make deliverable `to` depend on deliverable `from`.
    AddEdge {
        from: String,
        to: String,
        #[serde(default)]
        consumes: Option<String>,
    },
    /// Set a deliverable's estimated effort in hours.
    SetEffort { id: String, hours: f32 },
    /// Set (or clear) a deliverable's calendar duration in hours.
    SetDuration { id: String, hours: Option<f32> },
    /// Set (or clear) a deliverable's three-point estimate.
    SetEstimate {
        id: String,
        estimate: Option<Estimate>,
    },
    /// Set one metadata key on a deliverable.
    SetMetadata {
        id: String,
        key: String,
        value: serde_json::Value,
    },
    /// Remove a deliverable entirely.
    RemoveDeliverable { id: String },
    /// Append a new deliverable.
    AddDeliverable { deliverable: Deliverable },
}

/// Apply `edits` in order to a clone of `graph`, then validate the result
/// with the same rules as `plan.submit`.
///
/// An edit that names a deliverable id absent from the graph being edited
/// returns [`PlannerError::InvalidGraph`] whose `reason` starts with
/// `edit <index>: ` and names the offending id. A failure of the final
/// whole-graph validation is `INVALID_GRAPH` whose `reason` starts with
/// `after applying <n> edits: `.
pub fn apply_edits(graph: &PlanGraph, edits: &[GraphEdit]) -> Result<PlanGraph, PlannerError> {
    let mut out = graph.clone();
    for (index, edit) in edits.iter().enumerate() {
        apply_one(&mut out, index, edit)?;
    }
    validate_graph(&out).map_err(|err| match err {
        PlannerError::InvalidGraph { reason } => PlannerError::InvalidGraph {
            reason: format!("after applying {} edits: {reason}", edits.len()),
        },
        other => other,
    })?;
    Ok(out)
}

fn unknown_id(index: usize, id: &str) -> PlannerError {
    PlannerError::InvalidGraph {
        reason: format!("edit {index}: unknown deliverable id '{id}'"),
    }
}

fn contains_id(graph: &PlanGraph, id: &str) -> bool {
    graph.deliverables.iter().any(|d| d.id == id)
}

fn find_mut<'a>(graph: &'a mut PlanGraph, id: &str) -> Option<&'a mut Deliverable> {
    graph.deliverables.iter_mut().find(|d| d.id == id)
}

fn apply_one(graph: &mut PlanGraph, index: usize, edit: &GraphEdit) -> Result<(), PlannerError> {
    match edit {
        GraphEdit::RemoveEdge { from, to } => {
            if !contains_id(graph, to) {
                return Err(unknown_id(index, to));
            }
            if !contains_id(graph, from) {
                return Err(unknown_id(index, from));
            }
            let target = find_mut(graph, to).expect("checked present above");
            let before = target.prerequisites.len();
            target.prerequisites.retain(|p| p.id() != from);
            if target.prerequisites.len() == before {
                return Err(PlannerError::InvalidGraph {
                    reason: format!("edit {index}: no edge '{from}' -> '{to}'"),
                });
            }
        }
        GraphEdit::AddEdge { from, to, consumes } => {
            if !contains_id(graph, to) {
                return Err(unknown_id(index, to));
            }
            if !contains_id(graph, from) {
                return Err(unknown_id(index, from));
            }
            let edge = match consumes {
                Some(consumes) => Prerequisite::Edge {
                    id: from.clone(),
                    consumes: Some(consumes.clone()),
                    kind: None,
                    lag_hours: None,
                },
                None => Prerequisite::Id(from.clone()),
            };
            let target = find_mut(graph, to).expect("checked present above");
            if target.prerequisites.iter().any(|p| p.id() == from) {
                return Err(PlannerError::InvalidGraph {
                    reason: format!("edit {index}: edge '{from}' -> '{to}' already exists"),
                });
            }
            target.prerequisites.push(edge);
        }
        GraphEdit::SetEffort { id, hours } => {
            let target = find_mut(graph, id).ok_or_else(|| unknown_id(index, id))?;
            target.estimated_effort_hours = Some(*hours);
        }
        GraphEdit::SetDuration { id, hours } => {
            let target = find_mut(graph, id).ok_or_else(|| unknown_id(index, id))?;
            target.duration_hours = *hours;
        }
        GraphEdit::SetEstimate { id, estimate } => {
            let target = find_mut(graph, id).ok_or_else(|| unknown_id(index, id))?;
            target.estimate = *estimate;
        }
        GraphEdit::SetMetadata { id, key, value } => {
            let target = find_mut(graph, id).ok_or_else(|| unknown_id(index, id))?;
            if !target.metadata.is_null() && !target.metadata.is_object() {
                return Err(PlannerError::InvalidGraph {
                    reason: format!("edit {index}: metadata of '{id}' is not an object"),
                });
            }
            if target.metadata.is_null() {
                target.metadata = serde_json::json!({});
            }
            target
                .metadata
                .as_object_mut()
                .expect("metadata coerced to an object above")
                .insert(key.clone(), value.clone());
        }
        GraphEdit::RemoveDeliverable { id } => {
            if !contains_id(graph, id) {
                return Err(unknown_id(index, id));
            }
            let mut dependents: Vec<&str> = graph
                .deliverables
                .iter()
                .filter(|d| d.id != *id && d.prerequisites.iter().any(|p| p.id() == id))
                .map(|d| d.id.as_str())
                .collect();
            if !dependents.is_empty() {
                dependents.sort_unstable();
                return Err(PlannerError::InvalidGraph {
                    reason: format!(
                        "edit {index}: '{id}' is a prerequisite of [{}]; remove those edges first",
                        dependents.join(", ")
                    ),
                });
            }
            graph.deliverables.retain(|d| d.id != *id);
        }
        GraphEdit::AddDeliverable { deliverable } => {
            if contains_id(graph, &deliverable.id) {
                return Err(PlannerError::InvalidGraph {
                    reason: format!(
                        "edit {index}: deliverable '{}' already exists",
                        deliverable.id
                    ),
                });
            }
            for prereq in &deliverable.prerequisites {
                if !contains_id(graph, prereq.id()) {
                    return Err(unknown_id(index, prereq.id()));
                }
            }
            graph.deliverables.push(deliverable.clone());
        }
    }
    Ok(())
}
