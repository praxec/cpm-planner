//! Devaux DRAG and graph diameter for CPM schedules.
//!
//! # Definitions
//!
//! **DRAG** (Devaux's Removed Activity Gauge) measures how much the project
//! duration would *shrink* if a critical activity's duration were reduced by
//! one unit.  For each critical activity (`float == 0`):
//!
//! ```text
//! drag(a) = min(
//!     duration(a),                      // can't remove more than its own duration
//!     min { float(p) | p ∈ parallel(a), !p.is_critical }
//!                                       // bounded by the smallest parallel float
//! )
//! ```
//!
//! If there are *no* non-critical activities running in parallel, the drag
//! equals the activity's own duration (nothing else constrains the squeeze).
//!
//! Non-critical activities have drag `0.0` (they are already off the critical
//! path).
//!
//! **Diameter** is the critical-path length: the maximum `EF` value across all
//! tasks (i.e. the project finish time, identical to the sum of critical
//! activity durations on an acyclic, single-path network).

use crate::task::Task;
use std::collections::HashMap;

/// Drag value for a single activity.
#[derive(Debug, Clone, PartialEq)]
pub struct DragResult {
    /// Task id this drag value belongs to.
    pub task_id: String,
    /// Drag in the same time unit as `Task::effort_hours` (tokens / hours).
    /// `0.0` for non-critical activities.
    pub drag: f32,
}

/// Compute DRAG for every activity in `tasks`.
///
/// Two activities are considered *parallel* when their windows
/// `[ES, EF)` overlap: ES_a < EF_b && ES_b < EF_a.
///
/// The returned `Vec` is in the same order as the input slice.
#[must_use]
pub fn drag(tasks: &[Task]) -> Vec<DragResult> {
    // Index for fast float lookup
    let float_by_id: HashMap<&str, f32> = tasks.iter().map(|t| (t.id.as_str(), t.float)).collect();

    tasks
        .iter()
        .map(|a| {
            if !a.is_critical {
                return DragResult {
                    task_id: a.id.clone(),
                    drag: 0.0,
                };
            }

            // Find the minimum float among non-critical tasks that overlap
            // the window [ES_a, EF_a).
            let min_parallel_float: Option<f32> = tasks
                .iter()
                .filter(|p| {
                    !p.is_critical
                        && p.id != a.id
                        && windows_overlap(
                            a.earliest_start,
                            a.earliest_finish,
                            p.earliest_start,
                            p.earliest_finish,
                        )
                })
                .map(|p| *float_by_id.get(p.id.as_str()).expect("same slice"))
                .reduce(f32::min);

            let d = match min_parallel_float {
                Some(pf) => a.effort_hours.min(pf),
                None => a.effort_hours,
            };

            DragResult {
                task_id: a.id.clone(),
                drag: d,
            }
        })
        .collect()
}

/// True iff the half-open intervals [s1, e1) and [s2, e2) overlap.
#[inline]
fn windows_overlap(s1: f32, e1: f32, s2: f32, e2: f32) -> bool {
    s1 < e2 && s2 < e1
}

/// Graph diameter: the critical-path length (max EF across all tasks).
///
/// Equals the project duration — for a serial critical chain it equals the
/// sum of durations; for a network with parallel paths it is still the
/// longest-path length.
///
/// Returns `0.0` for an empty task list.
#[must_use]
pub fn diameter(tasks: &[Task]) -> f32 {
    tasks
        .iter()
        .map(|t| t.earliest_finish)
        .fold(0.0_f32, f32::max)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::algorithm::CpmAlgorithm;
    use crate::task::{Task, TaskKind};

    fn make_task(id: &str, effort: f32, deps: Vec<&str>) -> Task {
        Task {
            id: id.to_string(),
            name: format!("Task {id}"),
            kind: TaskKind::Custom {
                description: String::new(),
            },
            effort_hours: effort,
            dependencies: deps.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // Test 1: single parallel non-critical activity constrains drag
    //
    // Network (both A and B start at time 0 with no shared start node):
    //
    //   A(1) --\
    //            --> end(0)
    //   B(5) --/
    //
    // ES/EF: A: [0,1), B: [0,5), end: [5,5)
    // Critical: B (float=0), end (float=0).
    // A has float = LS_A - ES_A = 4.
    //
    // drag(B): parallel non-critical = A (overlaps [0,5) vs [0,1)? yes).
    //   float(A) = 4, duration(B) = 5 => drag = min(5,4) = 4.
    // drag(end): duration=0 => drag=0.
    //
    // NOTE: We do NOT use a zero-effort "start" gateway node because the CPM
    // forward pass uses `current_ef > task.earliest_start` (strict), so a
    // start node with effort=0 never propagates ES to its successors —
    // they all keep EF=0 and the backward pass incorrectly marks them
    // all non-critical.  Direct roots (no deps) avoid the issue entirely.
    // -----------------------------------------------------------------------
    #[test]
    fn test_drag_parallel_constrains() {
        // A(1) -> end
        // B(5) -> end   (both A and B start at t=0)
        let mut tasks = vec![
            make_task("A", 1.0, vec![]),
            make_task("B", 5.0, vec![]),
            make_task("end", 0.0, vec!["A", "B"]),
        ];
        let result = CpmAlgorithm::calculate(&mut tasks);
        assert!(result.unscheduled.is_empty(), "should be acyclic");

        // B must be critical, A must NOT be
        let b = result.get_task("B").expect("B");
        let a = result.get_task("A").expect("A");
        assert!(b.is_critical, "B should be on critical path");
        assert!(!a.is_critical, "A should NOT be on critical path");

        let drags = drag(&result.tasks);

        let drag_b = drags.iter().find(|d| d.task_id == "B").expect("drag for B");
        let drag_a = drags.iter().find(|d| d.task_id == "A").expect("drag for A");

        // float(A) = 4, duration(B) = 5 => drag(B) = min(5,4) = 4
        assert_eq!(
            drag_b.drag, 4.0,
            "drag(B) should be min(duration=5, float_A=4) = 4"
        );
        // Non-critical => 0
        assert_eq!(drag_a.drag, 0.0, "drag(A) should be 0 (non-critical)");

        // Diameter = max EF = 5 (end has EF = 5 + 0 = 5)
        assert_eq!(diameter(&result.tasks), 5.0, "diameter should be 5");
    }

    // -----------------------------------------------------------------------
    // Test 2: serial chain — no parallel tasks, drag = own duration for each
    //
    // A(2) -> B(3) -> C(1)  all critical, no parallel activities.
    // drag(A)=2, drag(B)=3, drag(C)=1. diameter=6.
    // -----------------------------------------------------------------------
    #[test]
    fn test_drag_serial_chain() {
        let mut tasks = vec![
            make_task("A", 2.0, vec![]),
            make_task("B", 3.0, vec!["A"]),
            make_task("C", 1.0, vec!["B"]),
        ];
        let result = CpmAlgorithm::calculate(&mut tasks);
        assert!(result.unscheduled.is_empty());

        // All three must be critical
        for id in &["A", "B", "C"] {
            assert!(
                result.get_task(id).expect(id).is_critical,
                "{id} should be critical"
            );
        }

        let drags = drag(&result.tasks);

        let drag_a = drags.iter().find(|d| d.task_id == "A").expect("A").drag;
        let drag_b = drags.iter().find(|d| d.task_id == "B").expect("B").drag;
        let drag_c = drags.iter().find(|d| d.task_id == "C").expect("C").drag;

        assert_eq!(drag_a, 2.0, "serial drag(A)=duration=2");
        assert_eq!(drag_b, 3.0, "serial drag(B)=duration=3");
        assert_eq!(drag_c, 1.0, "serial drag(C)=duration=1");

        // diameter = 2+3+1 = 6
        assert_eq!(diameter(&result.tasks), 6.0, "diameter of serial chain = 6");
    }

    // -----------------------------------------------------------------------
    // Test 3: drag is bounded by the SMALLEST parallel float when there are
    // multiple non-critical activities in parallel with the same critical one.
    // -----------------------------------------------------------------------
    #[test]
    fn test_drag_bounded_by_smallest_float() {
        // CRIT(10) -> end    (critical; float=0)
        // NC1(8)   -> end    float = 10-8 = 2
        // NC2(5)   -> end    float = 10-5 = 5
        // All three start at t=0 (no shared start gateway node — see note in
        // test_drag_parallel_constrains about zero-effort gateway nodes).
        // Parallel to CRIT: NC1 (float=2) and NC2 (float=5).
        // drag(CRIT) = min(10, min(2,5)) = min(10,2) = 2.
        let mut tasks = vec![
            make_task("CRIT", 10.0, vec![]),
            make_task("NC1", 8.0, vec![]),
            make_task("NC2", 5.0, vec![]),
            make_task("end", 0.0, vec!["CRIT", "NC1", "NC2"]),
        ];
        let result = CpmAlgorithm::calculate(&mut tasks);
        assert!(result.unscheduled.is_empty());

        let crit = result.get_task("CRIT").expect("CRIT");
        assert!(crit.is_critical, "CRIT should be critical");

        let drags = drag(&result.tasks);
        let d = drags
            .iter()
            .find(|d| d.task_id == "CRIT")
            .expect("CRIT drag")
            .drag;
        assert_eq!(d, 2.0, "drag(CRIT) = min(10, min_float(2,5)) = 2");

        // diameter = max EF = CRIT.EF(10) == end.EF(10) = 10
        assert_eq!(diameter(&result.tasks), 10.0);
    }

    // -----------------------------------------------------------------------
    // Test 4: non-critical activities always have drag == 0
    // -----------------------------------------------------------------------
    #[test]
    fn test_non_critical_drag_is_zero() {
        let mut tasks = vec![
            make_task("A", 1.0, vec![]),
            make_task("B", 5.0, vec![]),
            make_task("C", 1.0, vec!["A", "B"]),
        ];
        let result = CpmAlgorithm::calculate(&mut tasks);
        let drags = drag(&result.tasks);
        // A has float, so drag = 0
        let da = drags.iter().find(|d| d.task_id == "A").unwrap().drag;
        assert_eq!(da, 0.0);
    }

    // -----------------------------------------------------------------------
    // Test 5: empty task list
    // -----------------------------------------------------------------------
    #[test]
    fn test_empty() {
        assert!(drag(&[]).is_empty());
        assert_eq!(diameter(&[]), 0.0);
    }
}
