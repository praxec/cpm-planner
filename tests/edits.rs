//! Behavioral tests for the pure structured-edit applier.
//!
//! Each test drives one edit operation through the public
//! [`apply_edits`] interface and observes one outcome on the returned graph.

use cpm_planner::edits::{GraphEdit, apply_edits};
use cpm_planner::plan::{Deliverable, Estimate, OwnedFile, PlanGraph, PlannerError};
use serde_json::json;

fn deliverable(id: &str, prereqs: &[&str]) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: vec![OwnedFile::Path(format!("src/{id}.rs").into())],
        prerequisites: prereqs.iter().map(|s| (*s).into()).collect(),
        estimated_effort_hours: Some(1.0),
        duration_hours: None,
        estimate: None,
        metadata: serde_json::Value::Null,
        milestone: false,
    }
}

/// `b` depends on `a`.
fn graph() -> PlanGraph {
    PlanGraph {
        deliverables: vec![deliverable("a", &[]), deliverable("b", &["a"])],
        max_chained_dispatch: None,
    }
}

fn find<'a>(graph: &'a PlanGraph, id: &str) -> &'a Deliverable {
    graph
        .deliverables
        .iter()
        .find(|d| d.id == id)
        .expect("deliverable present")
}

fn invalid_reason(err: PlannerError) -> String {
    match err {
        PlannerError::InvalidGraph { reason } => reason,
        other => panic!("expected InvalidGraph, got {other:?}"),
    }
}

#[test]
fn remove_edge_removes_prerequisite() {
    let out = apply_edits(
        &graph(),
        &[GraphEdit::RemoveEdge {
            from: "a".into(),
            to: "b".into(),
        }],
    )
    .expect("valid edit");
    assert!(find(&out, "b").prerequisites.is_empty());
}

#[test]
fn add_edge_adds_prerequisite() {
    let g = PlanGraph {
        deliverables: vec![deliverable("a", &[]), deliverable("b", &[])],
        max_chained_dispatch: None,
    };
    let out = apply_edits(
        &g,
        &[GraphEdit::AddEdge {
            from: "a".into(),
            to: "b".into(),
            consumes: None,
        }],
    )
    .expect("valid edit");
    assert!(find(&out, "b").prerequisites.iter().any(|p| p.id() == "a"));
}

#[test]
fn set_effort_sets_hours() {
    let out = apply_edits(
        &graph(),
        &[GraphEdit::SetEffort {
            id: "a".into(),
            hours: 7.5,
        }],
    )
    .expect("valid edit");
    assert_eq!(find(&out, "a").estimated_effort_hours, Some(7.5));
}

#[test]
fn set_duration_sets_duration() {
    let out = apply_edits(
        &graph(),
        &[GraphEdit::SetDuration {
            id: "a".into(),
            hours: Some(4.0),
        }],
    )
    .expect("valid edit");
    assert_eq!(find(&out, "a").duration_hours, Some(4.0));
}

#[test]
fn set_estimate_sets_estimate() {
    let estimate = Estimate {
        optimistic: 1.0,
        likely: 2.0,
        pessimistic: 3.0,
    };
    let out = apply_edits(
        &graph(),
        &[GraphEdit::SetEstimate {
            id: "a".into(),
            estimate: Some(estimate),
        }],
    )
    .expect("valid edit");
    assert_eq!(find(&out, "a").estimate, Some(estimate));
}

#[test]
fn set_metadata_sets_key() {
    let out = apply_edits(
        &graph(),
        &[GraphEdit::SetMetadata {
            id: "a".into(),
            key: "owner".into(),
            value: json!("platform"),
        }],
    )
    .expect("valid edit");
    assert_eq!(
        find(&out, "a").metadata.get("owner"),
        Some(&json!("platform"))
    );
}

#[test]
fn remove_deliverable_removes_it() {
    let out = apply_edits(&graph(), &[GraphEdit::RemoveDeliverable { id: "b".into() }])
        .expect("valid edit");
    assert!(out.deliverables.iter().all(|d| d.id != "b"));
}

#[test]
fn add_deliverable_adds_it() {
    let out = apply_edits(
        &graph(),
        &[GraphEdit::AddDeliverable {
            deliverable: deliverable("c", &["a"]),
        }],
    )
    .expect("valid edit");
    assert!(out.deliverables.iter().any(|d| d.id == "c"));
}

#[test]
fn edit_with_unknown_id_names_edit_index() {
    let edits = vec![
        GraphEdit::SetEffort {
            id: "a".into(),
            hours: 2.0,
        },
        GraphEdit::SetEffort {
            id: "ghost".into(),
            hours: 2.0,
        },
    ];
    let err = apply_edits(&graph(), &edits).expect_err("unknown id rejected");
    let PlannerError::InvalidGraph { reason } = err else {
        panic!("expected InvalidGraph, got {err:?}");
    };
    assert!(reason.starts_with("edit 1: ") && reason.contains("ghost"));
}

#[test]
fn edits_producing_cycle_are_rejected() {
    let result = apply_edits(
        &graph(),
        &[GraphEdit::AddEdge {
            from: "b".into(),
            to: "a".into(),
            consumes: None,
        }],
    );
    let PlannerError::InvalidGraph { reason } = result.expect_err("cycle rejected") else {
        panic!("expected InvalidGraph");
    };
    assert!(reason.contains("cycle"));
}

#[test]
fn remove_missing_edge_is_rejected() {
    let g = PlanGraph {
        deliverables: vec![deliverable("a", &[]), deliverable("b", &[])],
        max_chained_dispatch: None,
    };
    let err = apply_edits(
        &g,
        &[GraphEdit::RemoveEdge {
            from: "a".into(),
            to: "b".into(),
        }],
    )
    .expect_err("missing edge rejected");
    assert_eq!(invalid_reason(err), "edit 0: no edge 'a' -> 'b'");
}

#[test]
fn add_duplicate_edge_is_rejected() {
    let err = apply_edits(
        &graph(),
        &[GraphEdit::AddEdge {
            from: "a".into(),
            to: "b".into(),
            consumes: None,
        }],
    )
    .expect_err("duplicate edge rejected");
    assert_eq!(
        invalid_reason(err),
        "edit 0: edge 'a' -> 'b' already exists"
    );
}

#[test]
fn remove_deliverable_with_dependents_is_rejected() {
    let g = PlanGraph {
        deliverables: vec![
            deliverable("a", &[]),
            deliverable("c", &["a"]),
            deliverable("b", &["a"]),
        ],
        max_chained_dispatch: None,
    };
    let err = apply_edits(&g, &[GraphEdit::RemoveDeliverable { id: "a".into() }])
        .expect_err("dependent deliverable rejected");
    assert_eq!(
        invalid_reason(err),
        "edit 0: 'a' is a prerequisite of [b, c]; remove those edges first"
    );
}

#[test]
fn add_duplicate_deliverable_is_rejected() {
    let err = apply_edits(
        &graph(),
        &[GraphEdit::AddDeliverable {
            deliverable: deliverable("a", &[]),
        }],
    )
    .expect_err("duplicate deliverable rejected");
    assert_eq!(
        invalid_reason(err),
        "edit 0: deliverable 'a' already exists"
    );
}

#[test]
fn set_metadata_on_non_object_is_rejected() {
    let mut g = graph();
    g.deliverables[0].metadata = json!("text");
    let err = apply_edits(
        &g,
        &[GraphEdit::SetMetadata {
            id: "a".into(),
            key: "owner".into(),
            value: json!("platform"),
        }],
    )
    .expect_err("non-object metadata rejected");
    assert_eq!(
        invalid_reason(err),
        "edit 0: metadata of 'a' is not an object"
    );
}

#[test]
fn set_duration_none_clears_duration() {
    let mut g = graph();
    g.deliverables[0].duration_hours = Some(5.0);
    let out = apply_edits(
        &g,
        &[GraphEdit::SetDuration {
            id: "a".into(),
            hours: None,
        }],
    )
    .expect("valid edit");
    assert_eq!(find(&out, "a").duration_hours, None);
}

#[test]
fn set_estimate_none_clears_estimate() {
    let mut g = graph();
    g.deliverables[0].estimate = Some(Estimate {
        optimistic: 1.0,
        likely: 2.0,
        pessimistic: 3.0,
    });
    let out = apply_edits(
        &g,
        &[GraphEdit::SetEstimate {
            id: "a".into(),
            estimate: None,
        }],
    )
    .expect("valid edit");
    assert_eq!(find(&out, "a").estimate, None);
}

#[test]
fn graph_edit_rejects_unknown_fields_on_the_wire() {
    let parsed = serde_json::from_str::<GraphEdit>(
        r#"{"op":"set_effort","id":"a","hours":1.0,"extra":true}"#,
    );
    assert!(parsed.is_err());
}

#[test]
fn graph_edit_op_tag_round_trips() {
    let edit = GraphEdit::SetEffort {
        id: "a".into(),
        hours: 2.5,
    };
    let wire = serde_json::to_string(&edit).expect("serializes");
    let back: GraphEdit = serde_json::from_str(&wire).expect("deserializes");
    assert_eq!(back, edit);
}
