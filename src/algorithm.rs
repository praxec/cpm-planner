//! CPM Algorithm Implementation
//!
//! Implements the Critical Path Method (CPM) algorithm:
//! 1. Forward pass: Calculate earliest start/finish times (ES/EF)
//! 2. Backward pass: Calculate latest start/finish times (LS/LF)
//! 3. Float calculation: slack = LS - ES
//! 4. Critical path: Tasks where float = 0
//! 5. Batch identification: Group tasks by earliest start time
//! 6. Bottleneck analysis: Identify tasks that block the most work

use crate::task::{Bottleneck, CriticalPathResult, Task, TaskBatch};
use std::collections::{HashMap, HashSet, VecDeque};

/// CPM Algorithm calculator
pub struct CpmAlgorithm;

impl CpmAlgorithm {
    /// Calculate critical path and all related metrics
    ///
    /// # Arguments
    /// * `tasks` - Mutable slice of tasks to analyze
    ///
    /// # Returns
    /// `CriticalPathResult` with all calculated metrics
    pub fn calculate(tasks: &mut [Task]) -> CriticalPathResult {
        if tasks.is_empty() {
            return CriticalPathResult::default();
        }

        // Build dependency graphs
        let (successors, predecessors) = Self::build_dependency_graphs(tasks);

        // Forward pass: calculate ES/EF. This is the authoritative cycle
        // detector: an id here is a task whose dependencies could not be
        // topologically ordered.
        let mut unscheduled = Self::forward_pass(tasks, &predecessors);

        // Backward pass: calculate LS/LF. It returns any task whose
        // latest-finish never relaxed off the sentinel. In a well-formed
        // (acyclic) graph that set is always empty, so the only way it is
        // non-empty is a cycle the forward pass already flagged.
        //
        // When the forward pass already identified the unschedulable set we
        // treat it as authoritative and do NOT widen it with the backward
        // signal: backward also flags acyclic nodes that merely sit upstream
        // of a cycle (their LF can't be finalised), and those were correctly
        // scheduled by the forward pass. We only fold the backward findings
        // in as a defence-in-depth net for the "forward saw nothing wrong but
        // a task still never relaxed" case, which should not occur.
        let backward_unscheduled = Self::backward_pass(tasks, &successors);
        if unscheduled.is_empty() {
            unscheduled = backward_unscheduled;
        }
        unscheduled.sort_unstable();
        unscheduled.dedup();

        // Calculate float and identify critical path
        Self::calculate_float(tasks);

        // Identify parallel batches
        let batches = Self::identify_parallel_batches(tasks);

        // Identify bottlenecks
        let bottlenecks = Self::identify_bottlenecks(tasks, &successors);

        // Build result
        Self::build_result(tasks, batches, bottlenecks, unscheduled)
    }

    /// Build forward (successors) and reverse (predecessors) dependency graphs
    fn build_dependency_graphs(
        tasks: &[Task],
    ) -> (HashMap<String, Vec<String>>, HashMap<String, Vec<String>>) {
        let mut successors: HashMap<String, Vec<String>> = HashMap::new();
        let mut predecessors: HashMap<String, Vec<String>> = HashMap::new();

        // Initialize empty lists for all tasks
        for task in tasks {
            successors.entry(task.id.clone()).or_default();
            predecessors.entry(task.id.clone()).or_default();
        }

        // Build the graphs from dependencies
        for task in tasks {
            for dep in &task.dependencies {
                // task depends on dep, so:
                // dep -> task (dep is predecessor of task)
                // task is successor of dep
                successors
                    .entry(dep.clone())
                    .or_default()
                    .push(task.id.clone());
                predecessors
                    .entry(task.id.clone())
                    .or_default()
                    .push(dep.clone());
            }
        }

        (successors, predecessors)
    }

    /// Forward pass: Calculate earliest start (ES) and earliest finish (EF)
    ///
    /// ES = max(EF of all predecessors), or 0 if no predecessors
    /// EF = ES + effort
    ///
    /// Returns the ids of any tasks that could not be scheduled because
    /// they (or their predecessors) sit on a dependency cycle. An empty
    /// return means the whole graph was schedulable.
    fn forward_pass(
        tasks: &mut [Task],
        _predecessors: &HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        let task_count = tasks.len();
        let task_map: HashMap<String, usize> = tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id.clone(), i))
            .collect();

        // Use Kahn's algorithm (topological sort) for forward pass
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        for task in tasks.iter() {
            in_degree.insert(task.id.clone(), task.dependencies.len());
        }

        // Start with tasks that have no dependencies
        let mut queue: VecDeque<String> = tasks
            .iter()
            .filter(|t| t.dependencies.is_empty())
            .map(|t| t.id.clone())
            .collect();

        // Initialize ES/EF for starting tasks
        for task in tasks.iter_mut() {
            if task.dependencies.is_empty() {
                task.earliest_start = 0.0;
                task.earliest_finish = task.effort_hours;
            }
        }

        // Track which ids actually drained out of the topo queue. Any task
        // that never reaches in-degree 0 sits on (or downstream of) a cycle
        // and keeps its default ES/EF — i.e. it was not scheduled.
        let mut scheduled: HashSet<String> = HashSet::with_capacity(task_count);
        let mut processed = 0;
        while let Some(current_id) = queue.pop_front() {
            processed += 1;
            scheduled.insert(current_id.clone());

            // Get current task's EF. The queue only ever carries ids that
            // came from `tasks` (initialized from `tasks.iter()` and pushed
            // from successor walks), and `task_map` was populated from the
            // same iterator above. The `None` branch is unreachable; assert
            // it loudly so a future refactor that breaks the invariant
            // doesn't degrade into silent skipping.
            let current_ef = match task_map.get(&current_id) {
                Some(&idx) => tasks[idx].earliest_finish,
                None => unreachable!(
                    "task_map missing id {current_id} that was queued from the same tasks slice"
                ),
            };

            // Find successors by scanning all tasks
            for task in tasks.iter_mut() {
                if task.dependencies.contains(&current_id) {
                    // Update ES if this predecessor has later EF
                    if current_ef > task.earliest_start {
                        task.earliest_start = current_ef;
                        task.earliest_finish = task.earliest_start + task.effort_hours;
                    }

                    // Decrement in-degree and add to queue if ready
                    if let Some(deg) = in_degree.get_mut(&task.id) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            // All predecessors have been processed; lock in
                            // the final EF.  Without this, when every
                            // predecessor finishes at time 0 (e.g. a zero-
                            // effort "start" milestone) the condition
                            // `current_ef > task.earliest_start` above is
                            // false and EF stays at the default 0 instead
                            // of ES + effort_hours.
                            task.earliest_finish =
                                task.earliest_start + task.effort_hours;
                            queue.push_back(task.id.clone());
                            // EF finalised when all predecessors are in
                        }
                    }
                }
            }
        }

        // Handle case where not all tasks were processed (cycle in deps).
        // Collect the unschedulable ids so callers can detect and reject
        // the otherwise confidently-wrong result instead of only seeing a
        // log line.
        if processed < task_count {
            let mut unscheduled: Vec<String> = tasks
                .iter()
                .filter(|t| !scheduled.contains(&t.id))
                .map(|t| t.id.clone())
                .collect();
            unscheduled.sort_unstable();
            tracing::warn!(
                unscheduled = unscheduled.len(),
                "CPM forward pass could not schedule all tasks (possible dependency cycle)"
            );
            return unscheduled;
        }

        Vec::new()
    }

    /// Backward pass: Calculate latest start (LS) and latest finish (LF)
    ///
    /// LF = min(LS of all successors), or `project_end` if no successors
    /// LS = LF - effort
    ///
    /// Returns the ids of any tasks whose `latest_finish` never relaxed off
    /// the `f32::MAX` sentinel. In a well-formed graph this is empty; a
    /// non-empty return means those tasks are unschedulable (the same cycle
    /// signal the forward pass surfaces) and the result must not be trusted.
    fn backward_pass(tasks: &mut [Task], successors: &HashMap<String, Vec<String>>) -> Vec<String> {
        // Find project duration (max EF)
        let project_duration = tasks
            .iter()
            .map(|t| t.earliest_finish)
            .fold(0.0_f32, f32::max);

        // Create a map for quick lookup
        let task_map: HashMap<String, usize> = tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id.clone(), i))
            .collect();

        // Initialize LF/LS for ending tasks (no successors)
        for task in tasks.iter_mut() {
            let has_successors = successors.get(&task.id).is_some_and(|s| !s.is_empty());
            if has_successors {
                // Initialize to max values for later minimization
                task.latest_finish = f32::MAX;
                task.latest_start = f32::MAX;
            } else {
                task.latest_finish = project_duration;
                task.latest_start = project_duration - task.effort_hours;
            }
        }

        // Reverse topological order processing
        // We iterate until no changes (simpler than proper reverse topo sort)
        let mut changed = true;
        let mut iterations = 0;
        let max_iterations = tasks.len() * 2;

        while changed && iterations < max_iterations {
            changed = false;
            iterations += 1;

            for i in 0..tasks.len() {
                let task_id = tasks[i].id.clone();
                let task_effort = tasks[i].effort_hours;

                // Find minimum LS of all successors
                if let Some(succ_ids) = successors.get(&task_id) {
                    if !succ_ids.is_empty() {
                        let min_succ_ls = succ_ids
                            .iter()
                            .filter_map(|sid| task_map.get(sid).map(|&idx| tasks[idx].latest_start))
                            .filter(|&ls| ls < f32::MAX)
                            .fold(f32::MAX, f32::min);

                        if min_succ_ls < f32::MAX && min_succ_ls < tasks[i].latest_finish {
                            tasks[i].latest_finish = min_succ_ls;
                            tasks[i].latest_start = min_succ_ls - task_effort;
                            changed = true;
                        }
                    }
                }
            }
        }

        // Any task still pinned at the MAX sentinel never had its LF/LS
        // relaxed — in a well-formed DAG that cannot happen, so treat it as
        // the same unschedulable signal the forward pass raises rather than
        // silently clamping to project_duration and emitting a plausible-
        // looking (but wrong) plan. We still clamp the numeric fields so the
        // struct holds finite values, but the returned ids let callers
        // reject the result.
        let mut unscheduled: Vec<String> = Vec::new();
        for task in tasks.iter_mut() {
            if task.latest_finish >= f32::MAX - 1.0 {
                task.latest_finish = project_duration;
                task.latest_start = project_duration - task.effort_hours;
                unscheduled.push(task.id.clone());
            }
        }
        unscheduled.sort_unstable();
        unscheduled
    }

    /// Calculate float (slack) for each task and mark critical path
    fn calculate_float(tasks: &mut [Task]) {
        for task in tasks.iter_mut() {
            task.float = task.latest_start - task.earliest_start;
            // Critical if float is essentially zero (within tolerance)
            task.is_critical = task.float.abs() < 0.001;
        }
    }

    /// Identify parallel batches by grouping tasks with same earliest start
    fn identify_parallel_batches(tasks: &[Task]) -> Vec<TaskBatch> {
        // Group tasks by earliest start time (rounded to avoid float issues)
        let mut batches_map: HashMap<i32, Vec<&Task>> = HashMap::new();

        for task in tasks {
            // Round to nearest 0.1 hour for batching
            let es_key = (task.earliest_start * 10.0).round() as i32;
            batches_map.entry(es_key).or_default().push(task);
        }

        // Convert to sorted Vec<TaskBatch>
        let mut sorted_keys: Vec<i32> = batches_map.keys().copied().collect();
        sorted_keys.sort_unstable();

        let mut batches = Vec::new();
        let mut batch_num = 0;

        for es_key in sorted_keys {
            if let Some(batch_tasks) = batches_map.get(&es_key) {
                let total_effort: f32 = batch_tasks.iter().map(|t| t.effort_hours).sum();
                let duration = batch_tasks
                    .iter()
                    .map(|t| t.effort_hours)
                    .fold(0.0_f32, f32::max);
                let start_time = es_key as f32 / 10.0;

                batches.push(TaskBatch {
                    id: format!("Batch-{batch_num}"),
                    tasks: batch_tasks.iter().map(|t| t.id.clone()).collect(),
                    total_effort_hours: total_effort,
                    duration_hours: duration,
                    start_time,
                });
                batch_num += 1;
            }
        }

        batches
    }

    /// Identify bottleneck tasks based on transitive impact
    fn identify_bottlenecks(
        tasks: &[Task],
        successors: &HashMap<String, Vec<String>>,
    ) -> Vec<Bottleneck> {
        let task_map: HashMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();

        let mut bottlenecks = Vec::new();

        for task in tasks {
            // Count transitively blocked tasks
            let blocked_ids = Self::get_transitive_successors(&task.id, successors);
            let blocks_count = blocked_ids.len();

            if blocks_count == 0 {
                continue;
            }

            // Calculate total blocked hours
            let blocked_hours: f32 = blocked_ids
                .iter()
                .filter_map(|id| task_map.get(id.as_str()).map(|t| t.effort_hours))
                .sum();

            // Calculate ROI
            let roi = if task.effort_hours > 0.0 {
                blocked_hours / task.effort_hours
            } else {
                0.0
            };

            bottlenecks.push(Bottleneck {
                task_id: task.id.clone(),
                task_name: task.name.clone(),
                blocks_count,
                blocked_hours,
                roi,
                effort_hours: task.effort_hours,
            });
        }

        // Sort by ROI descending
        bottlenecks.sort_by(|a, b| {
            b.roi
                .partial_cmp(&a.roi)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        bottlenecks
    }

    /// Get all tasks transitively blocked by the given task
    fn get_transitive_successors(
        task_id: &str,
        successors: &HashMap<String, Vec<String>>,
    ) -> HashSet<String> {
        let mut visited = HashSet::new();
        let mut stack = vec![task_id.to_string()];

        while let Some(current) = stack.pop() {
            if let Some(succs) = successors.get(&current) {
                for succ in succs {
                    if !visited.contains(succ) {
                        visited.insert(succ.clone());
                        stack.push(succ.clone());
                    }
                }
            }
        }

        visited
    }

    /// Build the final result
    fn build_result(
        tasks: &[Task],
        batches: Vec<TaskBatch>,
        bottlenecks: Vec<Bottleneck>,
        unscheduled: Vec<String>,
    ) -> CriticalPathResult {
        // Extract critical path (sorted by ES)
        let mut critical_tasks: Vec<&Task> = tasks.iter().filter(|t| t.is_critical).collect();
        critical_tasks.sort_by(|a, b| {
            a.earliest_start
                .partial_cmp(&b.earliest_start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let critical_path: Vec<String> = critical_tasks.iter().map(|t| t.id.clone()).collect();

        let critical_path_duration: f32 = critical_tasks.iter().map(|t| t.effort_hours).sum();

        let total_duration_sequential: f32 = tasks.iter().map(|t| t.effort_hours).sum();

        // Parallel duration is the sum of batch durations
        let optimal_duration_parallel: f32 = batches.iter().map(|b| b.duration_hours).sum();

        let speedup_factor = if optimal_duration_parallel > 0.0 {
            total_duration_sequential / optimal_duration_parallel
        } else {
            1.0
        };

        CriticalPathResult {
            total_tasks: tasks.len(),
            critical_path,
            critical_path_duration,
            total_duration_sequential,
            optimal_duration_parallel,
            speedup_factor,
            parallelizable_batches: batches,
            bottlenecks,
            tasks: tasks.to_vec(),
            unscheduled,
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn make_task(id: &str, effort: f32, deps: Vec<&str>) -> Task {
        Task {
            id: id.to_string(),
            name: format!("Task {id}"),
            effort_hours: effort,
            dependencies: deps.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_simple_chain() {
        // A -> B -> C (linear dependency)
        let mut tasks = vec![
            make_task("A", 2.0, vec![]),
            make_task("B", 3.0, vec!["A"]),
            make_task("C", 1.0, vec!["B"]),
        ];

        let result = CpmAlgorithm::calculate(&mut tasks);

        // All tasks should be on critical path
        assert_eq!(result.critical_path.len(), 3);
        assert_eq!(result.critical_path_duration, 6.0);
        // No parallelization possible
        assert!((result.speedup_factor - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_parallel_tasks() {
        // A -> B
        // A -> C (B and C can run in parallel)
        let mut tasks = vec![
            make_task("A", 1.0, vec![]),
            make_task("B", 2.0, vec!["A"]),
            make_task("C", 3.0, vec!["A"]),
        ];

        let result = CpmAlgorithm::calculate(&mut tasks);

        // Critical path: A -> C (1 + 3 = 4h)
        assert_eq!(result.critical_path_duration, 4.0);
        // Total sequential: 1 + 2 + 3 = 6h
        assert_eq!(result.total_duration_sequential, 6.0);
        // Speedup should be > 1
        assert!(result.speedup_factor > 1.0);
    }

    #[test]
    fn test_diamond_dependency() {
        //     B
        //   /   \
        // A       D
        //   \   /
        //     C
        let mut tasks = vec![
            make_task("A", 1.0, vec![]),
            make_task("B", 2.0, vec!["A"]),
            make_task("C", 4.0, vec!["A"]),
            make_task("D", 1.0, vec!["B", "C"]),
        ];

        let result = CpmAlgorithm::calculate(&mut tasks);

        // Critical path: A -> C -> D (1 + 4 + 1 = 6h)
        assert_eq!(result.critical_path_duration, 6.0);
        // B should not be on critical path (has float)
        assert!(!result.critical_path.contains(&"B".to_string()));
    }

    #[test]
    fn test_bottleneck_identification() {
        // A blocks B, C, D (high ROI)
        let mut tasks = vec![
            make_task("A", 1.0, vec![]),
            make_task("B", 5.0, vec!["A"]),
            make_task("C", 5.0, vec!["A"]),
            make_task("D", 5.0, vec!["A"]),
        ];

        let result = CpmAlgorithm::calculate(&mut tasks);

        // A should be identified as top bottleneck
        assert!(!result.bottlenecks.is_empty());
        assert_eq!(result.bottlenecks[0].task_id, "A");
        // ROI = 15h blocked / 1h effort = 15
        assert!(result.bottlenecks[0].roi >= 14.0);
    }

    #[test]
    fn test_empty_tasks() {
        let mut tasks: Vec<Task> = vec![];
        let result = CpmAlgorithm::calculate(&mut tasks);
        assert_eq!(result.total_tasks, 0);
        assert!(result.critical_path.is_empty());
    }

    #[test]
    fn test_single_task() {
        let mut tasks = vec![make_task("A", 5.0, vec![])];
        let result = CpmAlgorithm::calculate(&mut tasks);

        assert_eq!(result.total_tasks, 1);
        assert_eq!(result.critical_path_duration, 5.0);
        assert_eq!(result.critical_path, vec!["A".to_string()]);
    }

    #[test]
    fn test_cyclic_graph_is_reported_unscheduled() {
        // A -> B -> C -> A forms a cycle: none of these can be scheduled.
        let mut tasks = vec![
            make_task("A", 1.0, vec!["C"]),
            make_task("B", 1.0, vec!["A"]),
            make_task("C", 1.0, vec!["B"]),
        ];

        let result = CpmAlgorithm::calculate(&mut tasks);

        // The forward pass cannot drain the topo queue, so every member of
        // the cycle must surface in `unscheduled` (sorted) rather than the
        // result silently looking valid.
        assert_eq!(
            result.unscheduled,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
    }

    #[test]
    fn test_partial_cycle_reports_only_cycle_members() {
        // ROOT is schedulable; X<->Y form a 2-cycle and are not.
        let mut tasks = vec![
            make_task("ROOT", 1.0, vec![]),
            make_task("X", 1.0, vec!["ROOT", "Y"]),
            make_task("Y", 1.0, vec!["X"]),
        ];

        let result = CpmAlgorithm::calculate(&mut tasks);

        assert_eq!(result.unscheduled, vec!["X".to_string(), "Y".to_string()]);
    }

    #[test]
    fn test_acyclic_graph_has_empty_unscheduled() {
        let mut tasks = vec![make_task("A", 2.0, vec![]), make_task("B", 3.0, vec!["A"])];
        let result = CpmAlgorithm::calculate(&mut tasks);
        assert!(result.unscheduled.is_empty());
    }

    #[test]
    fn test_batch_grouping() {
        // Two independent tasks starting at same time
        let mut tasks = vec![make_task("A", 2.0, vec![]), make_task("B", 3.0, vec![])];

        let result = CpmAlgorithm::calculate(&mut tasks);

        // Should be in same batch (start at time 0)
        assert_eq!(result.parallelizable_batches.len(), 1);
        assert_eq!(result.parallelizable_batches[0].tasks.len(), 2);
        // Duration is max (3h), not sum (5h)
        assert_eq!(result.parallelizable_batches[0].duration_hours, 3.0);
        assert_eq!(result.parallelizable_batches[0].total_effort_hours, 5.0);
    }
}
