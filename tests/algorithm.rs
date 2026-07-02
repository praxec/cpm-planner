//! Integration tests for the CPM algorithm.
//!
//! These complement the in-module unit tests in `src/algorithm.rs` and
//! exercise the algorithm through the crate's public re-exports.

use cpm_planner::{CpmAlgorithm, Task, TaskKind};

fn make_task(id: &str, effort: f32, deps: Vec<&str>) -> Task {
    Task {
        id: id.to_string(),
        name: format!("Task {id}"),
        kind: TaskKind::Custom {
            description: format!("integration fixture for {id}"),
        },
        effort_hours: effort,
        dependencies: deps.into_iter().map(String::from).collect(),
        ..Default::default()
    }
}

#[test]
fn integration_linear_chain_marks_all_critical() {
    let mut tasks = vec![
        make_task("A", 1.0, vec![]),
        make_task("B", 2.0, vec!["A"]),
        make_task("C", 3.0, vec!["B"]),
    ];

    let result = CpmAlgorithm::calculate(&mut tasks);

    assert_eq!(result.total_tasks, 3);
    assert_eq!(result.critical_path, vec!["A", "B", "C"]);
    assert!((result.critical_path_duration - 6.0).abs() < 0.001);
    assert!((result.optimal_duration_parallel - 6.0).abs() < 0.001);
}

#[test]
fn integration_diamond_es_ef_values() {
    //     B (2h)
    //   /        \
    // A (1h)     D (1h)
    //   \        /
    //     C (4h)
    let mut tasks = vec![
        make_task("A", 1.0, vec![]),
        make_task("B", 2.0, vec!["A"]),
        make_task("C", 4.0, vec!["A"]),
        make_task("D", 1.0, vec!["B", "C"]),
    ];

    let result = CpmAlgorithm::calculate(&mut tasks);

    let by_id = |id: &str| result.tasks.iter().find(|t| t.id == id).expect("task");

    // A starts at 0, finishes at 1
    assert!((by_id("A").earliest_start - 0.0).abs() < 0.001);
    assert!((by_id("A").earliest_finish - 1.0).abs() < 0.001);
    // B starts at 1, finishes at 3
    assert!((by_id("B").earliest_start - 1.0).abs() < 0.001);
    assert!((by_id("B").earliest_finish - 3.0).abs() < 0.001);
    // C starts at 1, finishes at 5
    assert!((by_id("C").earliest_start - 1.0).abs() < 0.001);
    assert!((by_id("C").earliest_finish - 5.0).abs() < 0.001);
    // D starts at 5 (max of B.EF=3, C.EF=5), finishes at 6
    assert!((by_id("D").earliest_start - 5.0).abs() < 0.001);
    assert!((by_id("D").earliest_finish - 6.0).abs() < 0.001);

    // B has slack (C is the long edge); B should not be critical.
    assert!(!by_id("B").is_critical);
    assert!(by_id("A").is_critical);
    assert!(by_id("C").is_critical);
    assert!(by_id("D").is_critical);
}

#[test]
fn integration_parallel_batches_collapse_independent_tasks() {
    // Three independent tasks all start at time 0 -> single batch.
    let mut tasks = vec![
        make_task("X", 1.0, vec![]),
        make_task("Y", 2.0, vec![]),
        make_task("Z", 4.0, vec![]),
    ];
    let result = CpmAlgorithm::calculate(&mut tasks);

    assert_eq!(result.parallelizable_batches.len(), 1);
    let batch = &result.parallelizable_batches[0];
    assert_eq!(batch.tasks.len(), 3);
    // Sequential cost: 7. Parallel cost: max(1,2,4) = 4.
    assert!((batch.duration_hours - 4.0).abs() < 0.001);
    assert!((result.total_duration_sequential - 7.0).abs() < 0.001);
    assert!(result.speedup_factor > 1.7);
}

#[test]
fn integration_bottleneck_roi_ordering() {
    // ROOT blocks N downstream tasks; should top the bottleneck list.
    let mut tasks = vec![
        make_task("ROOT", 1.0, vec![]),
        make_task("L1", 2.0, vec!["ROOT"]),
        make_task("L2", 2.0, vec!["ROOT"]),
        make_task("L3", 2.0, vec!["ROOT"]),
        make_task("L4", 2.0, vec!["ROOT"]),
    ];
    let result = CpmAlgorithm::calculate(&mut tasks);

    let first = result.bottlenecks.first().expect("at least one bottleneck");
    assert_eq!(first.task_id, "ROOT");
    assert_eq!(first.blocks_count, 4);
    // ROI = 8 blocked hours / 1 effort hour = 8
    assert!(first.roi >= 7.9);
}
