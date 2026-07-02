//! CPM Task data structures
//!
//! Defines the core types for Critical Path Management:
//! - [`Task`]: A unit of work to be scheduled
//! - [`TaskKind`]: Domain-neutral classification of work
//! - [`TaskBatch`]: A group of tasks that can run in parallel
//! - [`Bottleneck`]: A task that blocks significant downstream work
//! - [`CriticalPathResult`]: The full CPM analysis output
//!
//! These types are the *algorithm's internal* data model. The wire model
//! that crosses the [`crate::ports::Planner`] boundary lives in
//! `crate::plan`. PA3 will provide the bridge.

use serde::{Deserialize, Serialize};

/// Kind of task that can be planned.
///
/// Variants are deliberately domain-neutral. Specialised variants for any
/// given problem domain belong outside this crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskKind {
    /// Break a circular dependency cycle in the task graph.
    ///
    /// This is a generic graph operation, not coupled to any particular
    /// problem domain.
    BreakCycle {
        /// Module identifiers (or generally, node ids) forming the cycle.
        cycle: Vec<String>,
    },
    /// Implement a spec requirement.
    ImplementSpec {
        /// Opaque identifier of the specification to implement.
        spec_id: String,
    },
    /// Custom user-defined task.
    Custom {
        /// Free-form description of the work.
        description: String,
    },
}

impl std::fmt::Display for TaskKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BreakCycle { .. } => write!(f, "CYCLE"),
            Self::ImplementSpec { spec_id, .. } => write!(f, "SPEC-{spec_id}"),
            Self::Custom { .. } => write!(f, "CUSTOM"),
        }
    }
}

/// Status of a task in the execution plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Not started, dependencies may not be met.
    #[default]
    Pending,
    /// Dependencies met, can start.
    Ready,
    /// Currently being worked on.
    InProgress,
    /// Successfully completed.
    Completed,
    /// Has unmet dependencies.
    Blocked,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Ready => write!(f, "ready"),
            Self::InProgress => write!(f, "in_progress"),
            Self::Completed => write!(f, "completed"),
            Self::Blocked => write!(f, "blocked"),
        }
    }
}

/// A single task in the CPM plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Type of task.
    pub kind: TaskKind,
    /// Estimated effort in hours.
    pub effort_hours: f32,
    /// Task IDs this depends on (must complete before this can start).
    pub dependencies: Vec<String>,
    /// Current status.
    pub status: TaskStatus,
    /// Earliest start time (calculated by forward pass).
    pub earliest_start: f32,
    /// Earliest finish time (ES + effort).
    pub earliest_finish: f32,
    /// Latest start time (calculated by backward pass).
    pub latest_start: f32,
    /// Latest finish time.
    pub latest_finish: f32,
    /// Float/slack time (LS - ES).
    pub float: f32,
    /// Is this task on the critical path?
    pub is_critical: bool,
    /// Files affected by this task (for context building, domain-neutral).
    pub affected_files: Vec<String>,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            kind: TaskKind::Custom {
                description: String::new(),
            },
            effort_hours: 0.0,
            dependencies: Vec::new(),
            status: TaskStatus::Pending,
            earliest_start: 0.0,
            earliest_finish: 0.0,
            latest_start: 0.0,
            latest_finish: 0.0,
            float: 0.0,
            is_critical: false,
            affected_files: Vec::new(),
        }
    }
}

impl Task {
    /// Create a new task with the given ID, name, kind, and effort.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        kind: TaskKind,
        effort_hours: f32,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind,
            effort_hours,
            ..Default::default()
        }
    }

    /// Add a dependency to this task.
    #[must_use]
    pub fn depends_on(mut self, task_id: impl Into<String>) -> Self {
        self.dependencies.push(task_id.into());
        self
    }

    /// Add an affected file.
    #[must_use]
    pub fn affects_file(mut self, file: impl Into<String>) -> Self {
        self.affected_files.push(file.into());
        self
    }
}

/// A batch of tasks that can run in parallel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskBatch {
    /// Batch identifier (e.g., "Batch-1").
    pub id: String,
    /// Tasks in this batch (all have same ES).
    pub tasks: Vec<String>,
    /// Total effort if done sequentially.
    pub total_effort_hours: f32,
    /// Actual duration (max of task efforts in batch).
    pub duration_hours: f32,
    /// Earliest start time for this batch.
    pub start_time: f32,
}

impl Default for TaskBatch {
    fn default() -> Self {
        Self {
            id: String::new(),
            tasks: Vec::new(),
            total_effort_hours: 0.0,
            duration_hours: 0.0,
            start_time: 0.0,
        }
    }
}

/// A bottleneck task that blocks significant downstream work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bottleneck {
    /// The bottleneck task ID.
    pub task_id: String,
    /// Task name for display.
    pub task_name: String,
    /// Number of tasks blocked by this one (directly or transitively).
    pub blocks_count: usize,
    /// Total hours of work blocked.
    pub blocked_hours: f32,
    /// ROI: `blocked_hours / task_effort` (higher = higher priority).
    pub roi: f32,
    /// Effort to complete this task.
    pub effort_hours: f32,
}

/// Critical Path Analysis Results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CriticalPathResult {
    /// Total number of tasks in the plan.
    pub total_tasks: usize,
    /// Tasks on the critical path (ordered by execution sequence).
    pub critical_path: Vec<String>,
    /// Duration of critical path in hours.
    pub critical_path_duration: f32,
    /// Total duration if done sequentially.
    pub total_duration_sequential: f32,
    /// Optimal duration with parallelization.
    pub optimal_duration_parallel: f32,
    /// Speedup factor (sequential / parallel).
    pub speedup_factor: f32,
    /// Batches for parallel execution.
    pub parallelizable_batches: Vec<TaskBatch>,
    /// Bottleneck tasks (sorted by ROI descending).
    pub bottlenecks: Vec<Bottleneck>,
    /// All tasks with calculated times.
    pub tasks: Vec<Task>,
    /// Task ids the forward pass could not schedule. Non-empty iff the
    /// dependency graph is cyclic or otherwise unschedulable; in that case
    /// every other field is confidently-wrong and must not be trusted.
    /// Callers that do not pre-validate (e.g. direct
    /// [`CpmAlgorithm::calculate`][crate::algorithm::CpmAlgorithm::calculate]
    /// users) MUST check this is empty before using the result.
    pub unscheduled: Vec<String>,
}

impl CriticalPathResult {
    /// Get a task by ID.
    #[must_use]
    pub fn get_task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_task_creation() {
        let task = Task::new(
            "SPEC-001",
            "Implement login flow",
            TaskKind::ImplementSpec {
                spec_id: "auth.login".to_string(),
            },
            2.0,
        )
        .depends_on("SPEC-000")
        .affects_file("src/auth/login.rs")
        .affects_file("src/auth/mod.rs");

        assert_eq!(task.id, "SPEC-001");
        assert_eq!(task.effort_hours, 2.0);
        assert_eq!(task.dependencies, vec!["SPEC-000"]);
        assert_eq!(
            task.affected_files,
            vec!["src/auth/login.rs", "src/auth/mod.rs"]
        );
    }

    #[test]
    fn test_task_kind_display() {
        assert_eq!(
            TaskKind::ImplementSpec {
                spec_id: "auth.login".to_string()
            }
            .to_string(),
            "SPEC-auth.login"
        );

        assert_eq!(
            TaskKind::BreakCycle {
                cycle: vec!["a".to_string(), "b".to_string()]
            }
            .to_string(),
            "CYCLE"
        );

        assert_eq!(
            TaskKind::Custom {
                description: "anything".to_string()
            }
            .to_string(),
            "CUSTOM"
        );
    }

    #[test]
    fn test_get_task() {
        let result = CriticalPathResult {
            total_tasks: 2,
            tasks: vec![
                Task {
                    id: "T1".to_string(),
                    effort_hours: 5.0,
                    ..Default::default()
                },
                Task {
                    id: "T2".to_string(),
                    effort_hours: 3.0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        assert_eq!(result.get_task("T2").expect("T2 present").effort_hours, 3.0);
        assert!(result.get_task("missing").is_none());
    }
}
