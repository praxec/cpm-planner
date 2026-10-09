# P1 — CPM Correctness (#18) + Schedule Exposure (#15) + plan.get Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `plan.status` reports a real prerequisite chain as the critical path with correct hours, exposes per-deliverable ES/EF/LS/LF/float and a float-ordered ready set, stored plans are repaired automatically on upgrade, and `plan.get` returns a submitted graph.

**Architecture:** Fix the kernel (`algorithm.rs`) to trace the critical path backwards along tight prerequisite edges from the max-EF sink. Move "graph → CPM result" into a new `src/schedule.rs` so both the planner (submit) and the store (upgrade sweep) can compute it. Introduce a `PRAGMA user_version` migration ladder plus a `plans.cpm_version` column; any row with `cpm_version < CPM_VERSION` is recomputed when the store opens. Status gains parallel arrays (append-only wire change).

**Tech Stack:** Rust 1.99.0 (edition 2024, let-chains OK), rusqlite, serde, tokio tests, rmcp tool façade.

**Spec:** `docs/superpowers/plans/2026-10-09-backlog-roadmap.md` § P1 + issues #18, #15 (+ `plan.get` added by plan-as-code decision).

## Global Constraints

- Every new `Deliverable` / prerequisite field MUST be added to `planner.rs::hash_graph` (none are added in P1).
- `PlanStatus.deliverables` rows are positional tuples — do not change them; extend `PlanStatus` only with new named fields carrying `#[serde(default)]`.
- Persisted `cached_result` must be recomputed when CPM math changes: `CPM_VERSION` constant, bumped whenever kernel output changes.
- Every new tool: constant + `PLAN_TOOL_NAMES` + schema in `plan_tool_definitions` + `instructions()` text + README tool table + `tests/server_integration.rs` roundtrip.
- Error-code prefixes in `PlannerError` messages MUST NOT change.
- `cargo fmt --all --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings` green on 1.99.0.
- Float tolerance for "tight"/"critical" comparisons: `1e-3` hours (matches existing `calculate_float`).

## Review Focus

1. **Plans persisted before this change** — opening the store must repair their `cached_result` (Task 3 test writes a stale row then reopens).
2. **Ties** — two sinks with equal max EF, or two tight predecessors: output must be deterministic (smallest id wins) — Task 1 test.
3. **Duplicate prerequisite ids** (`["A","A"]`) — must schedule, not silently drop the task into `unscheduled` — Task 1 test.
4. **Zero-effort deliverables** (milestone-like, effort 0) on the path — the backward trace must not loop or stop early — Task 1 test.
5. **Ready set with live locks** — locked Ready deliverables must not appear in `ready` — Task 2 test.

---

### Task 1: Kernel — true critical path, project duration, duplicate-prerequisite fix (#18)

**Files:**
- Modify: `src/algorithm.rs` (`forward_pass` in-degree init; `build_result`; new private `trace_critical_path`; new `pub const CPM_VERSION`)
- Modify: `src/task.rs` (`CriticalPathResult`: new field `critical_ids`, doc fixes)
- Test: `tests/algorithm.rs`

**Interfaces:**
- Consumes: existing `Task` (`id`, `dependencies`, `effort_hours`, `earliest_start`, `earliest_finish`, `is_critical`).
- Produces:
  - `pub const CPM_VERSION: i64 = 1;` in `src/algorithm.rs` (re-exported path `cpm_planner::algorithm::CPM_VERSION`).
  - `CriticalPathResult.critical_path: Vec<String>` — now a real chain, execution order, each element a prerequisite of the next.
  - `CriticalPathResult.critical_path_duration: f32` — max EF over all tasks (= sum of effort along `critical_path`).
  - `CriticalPathResult.optimal_duration_parallel: f32` — also max EF (unconstrained-resource makespan).
  - `CriticalPathResult.critical_ids: Vec<String>` (`#[serde(default)]`) — every zero-float task, sorted by (ES, id).

- [ ] **Step 1: Write failing tests** — append to `tests/algorithm.rs` (it already has a `make_task(id, effort, deps)` helper; reuse it):

```rust
fn assert_chain_is_prerequisite_path(result: &cpm_planner::task::CriticalPathResult) {
    for pair in result.critical_path.windows(2) {
        let succ = result.tasks.iter().find(|t| t.id == pair[1]).expect("succ");
        assert!(
            succ.dependencies.contains(&pair[0]),
            "{} -> {} is not a prerequisite edge",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn critical_path_of_parallel_equal_chains_is_one_real_chain() {
    // Two independent, equally long chains: both fully zero-float.
    let mut tasks = vec![
        make_task("P0a", 2.0, vec![]),
        make_task("P0b", 3.0, vec!["P0a"]),
        make_task("P1a", 2.0, vec![]),
        make_task("P1b", 3.0, vec!["P1a"]),
    ];
    let result = CpmAlgorithm::calculate(&mut tasks);
    assert_eq!(result.critical_path, vec!["P0a", "P0b"]);
}

#[test]
fn critical_path_pairs_are_prerequisite_edges_for_parallel_chains() {
    let mut tasks = vec![
        make_task("P0a", 2.0, vec![]),
        make_task("P0b", 3.0, vec!["P0a"]),
        make_task("P1a", 2.0, vec![]),
        make_task("P1b", 3.0, vec!["P1a"]),
    ];
    let result = CpmAlgorithm::calculate(&mut tasks);
    assert_chain_is_prerequisite_path(&result);
}

#[test]
fn critical_path_duration_is_project_length_not_sum_of_zero_float_tasks() {
    let mut tasks = vec![
        make_task("P0a", 2.0, vec![]),
        make_task("P0b", 3.0, vec!["P0a"]),
        make_task("P1a", 2.0, vec![]),
        make_task("P1b", 3.0, vec!["P1a"]),
    ];
    let result = CpmAlgorithm::calculate(&mut tasks);
    assert!((result.critical_path_duration - 5.0).abs() < 1e-3);
}

#[test]
fn critical_ids_lists_every_zero_float_task_by_start_then_id() {
    let mut tasks = vec![
        make_task("P0a", 2.0, vec![]),
        make_task("P0b", 3.0, vec!["P0a"]),
        make_task("P1a", 2.0, vec![]),
        make_task("P1b", 3.0, vec!["P1a"]),
    ];
    let result = CpmAlgorithm::calculate(&mut tasks);
    assert_eq!(result.critical_ids, vec!["P0a", "P1a", "P0b", "P1b"]);
}

#[test]
fn unrelated_zero_float_task_is_not_chained_into_critical_path() {
    // X stands alone (5h); Y -> Z also totals 5h. Both have zero float.
    let mut tasks = vec![
        make_task("X", 5.0, vec![]),
        make_task("Y", 1.0, vec![]),
        make_task("Z", 4.0, vec!["Y"]),
    ];
    let result = CpmAlgorithm::calculate(&mut tasks);
    assert_eq!(result.critical_path, vec!["X"]);
}

#[test]
fn optimal_parallel_duration_is_max_earliest_finish() {
    // A(1) -> B(2), A -> C(4), B,C -> D(1): makespan 6, batches would sum higher.
    let mut tasks = vec![
        make_task("A", 1.0, vec![]),
        make_task("B", 2.0, vec!["A"]),
        make_task("C", 4.0, vec!["A"]),
        make_task("D", 1.0, vec!["B", "C"]),
    ];
    let result = CpmAlgorithm::calculate(&mut tasks);
    assert!((result.optimal_duration_parallel - 6.0).abs() < 1e-3);
}

#[test]
fn duplicate_prerequisite_ids_still_schedule() {
    let mut tasks = vec![make_task("A", 1.0, vec![]), make_task("B", 2.0, vec!["A", "A"])];
    let result = CpmAlgorithm::calculate(&mut tasks);
    assert!(result.unscheduled.is_empty());
}

#[test]
fn zero_effort_milestone_stays_on_critical_path() {
    let mut tasks = vec![
        make_task("A", 3.0, vec![]),
        make_task("M", 0.0, vec!["A"]),
        make_task("B", 2.0, vec!["M"]),
    ];
    let result = CpmAlgorithm::calculate(&mut tasks);
    assert_eq!(result.critical_path, vec!["A", "M", "B"]);
}

#[test]
fn tight_predecessor_tie_picks_smallest_id() {
    // B and C both finish at 2 and both feed D.
    let mut tasks = vec![
        make_task("C", 2.0, vec![]),
        make_task("B", 2.0, vec![]),
        make_task("D", 1.0, vec!["C", "B"]),
    ];
    let result = CpmAlgorithm::calculate(&mut tasks);
    assert_eq!(result.critical_path, vec!["B", "D"]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test algorithm`
Expected: FAIL — `critical_ids` field does not exist (compile error).

- [ ] **Step 3: Add `critical_ids` to `CriticalPathResult`** in `src/task.rs`, after `critical_path`:

```rust
    /// Every zero-float (critical) task, sorted by earliest start then id.
    /// Unlike `critical_path` this may contain several parallel chains.
    #[serde(default)]
    pub critical_ids: Vec<String>,
```

Update the doc comments: `critical_path` → "One longest prerequisite chain, in execution order: each element is a prerequisite of the next. Ties resolve to the smallest id."; `critical_path_duration` → "Project length: the maximum earliest finish (equals the effort summed along `critical_path`)."; `optimal_duration_parallel` → "Makespan with unlimited parallelism (maximum earliest finish)."

- [ ] **Step 4: Fix duplicate-prerequisite wedge** in `forward_pass` (`src/algorithm.rs`): initialise in-degree with the number of *distinct* dependency ids:

```rust
        for task in tasks.iter() {
            let distinct: HashSet<&String> = task.dependencies.iter().collect();
            in_degree.insert(task.id.clone(), distinct.len());
        }
```

- [ ] **Step 5: Add `CPM_VERSION` and `trace_critical_path`**, and rewrite `build_result` in `src/algorithm.rs`:

```rust
/// Version of the CPM kernel's output semantics. Bump whenever a change
/// alters any field of [`CriticalPathResult`] for the same input, so stored
/// plans are recomputed on open (see `plan_store`).
pub const CPM_VERSION: i64 = 1;

const TIGHT_EPS: f32 = 1e-3;
```

```rust
    /// Trace one longest chain backwards from the task with the maximum
    /// earliest finish, following predecessors whose EF equals the
    /// successor's ES. Ties resolve to the smallest id. Bounded by the task
    /// count so malformed (cyclic) input cannot loop.
    fn trace_critical_path(tasks: &[Task]) -> Vec<String> {
        let by_id: HashMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        let Some(sink) = tasks.iter().max_by(|a, b| {
            a.earliest_finish
                .partial_cmp(&b.earliest_finish)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.id.cmp(&a.id))
        }) else {
            return Vec::new();
        };
        let mut path = vec![sink.id.clone()];
        let mut current = sink;
        let mut seen: HashSet<&str> = HashSet::from([sink.id.as_str()]);
        while path.len() <= tasks.len() {
            let next = current
                .dependencies
                .iter()
                .filter_map(|d| by_id.get(d.as_str()).copied())
                .filter(|p| (p.earliest_finish - current.earliest_start).abs() < TIGHT_EPS)
                .filter(|p| !seen.contains(p.id.as_str()))
                .min_by(|a, b| a.id.cmp(&b.id));
            let Some(pred) = next else { break };
            seen.insert(pred.id.as_str());
            path.push(pred.id.clone());
            current = pred;
        }
        path.reverse();
        path
    }
```

In `build_result`, replace the critical-path/duration computation:

```rust
        let critical_path = Self::trace_critical_path(tasks);
        let project_length = tasks
            .iter()
            .map(|t| t.earliest_finish)
            .fold(0.0_f32, f32::max);

        let mut critical_tasks: Vec<&Task> = tasks.iter().filter(|t| t.is_critical).collect();
        critical_tasks.sort_by(|a, b| {
            a.earliest_start
                .partial_cmp(&b.earliest_start)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        let critical_ids: Vec<String> = critical_tasks.iter().map(|t| t.id.clone()).collect();

        let total_duration_sequential: f32 = tasks.iter().map(|t| t.effort_hours).sum();
        let optimal_duration_parallel = project_length;
```

and set `critical_path_duration: project_length` and `critical_ids` in the returned struct. Fix any `CriticalPathResult { .. }` literals elsewhere (e.g. `Default`-based ones need nothing; explicit literals need `critical_ids`). Update in-module unit tests in `src/algorithm.rs` whose expectations encoded the old (buggy) semantics — change the expectation, never delete the test, and note each in the report.

- [ ] **Step 6: Run tests**

Run: `cargo test --test algorithm && cargo test --lib algorithm`
Expected: PASS.

- [ ] **Step 7: Full check + commit**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`

```bash
git add src/algorithm.rs src/task.rs tests/algorithm.rs
git commit -m "fix(cpm): critical path is a real prerequisite chain; duration = max EF; dedupe prerequisite in-degree (#18)"
```

---

### Task 2: Schedule rows + float-ordered ready set in plan.status; float-first cohort priority (#15)

**Files:**
- Modify: `src/plan.rs` (`ScheduleRow`, `PlanStatus` new fields + doc of `critical_path_hours`)
- Modify: `src/planner.rs` (`status()`, `priority_key` + its unit tests, `acquire_cohort` table building)
- Modify: `src/server.rs` (`instructions()` mentions new status fields)
- Test: `tests/planner.rs`

**Interfaces:**
- Consumes: `CriticalPathResult.critical_ids`, `tasks[*].{earliest_start, earliest_finish, latest_start, latest_finish, float, is_critical}` (Task 1).
- Produces:

```rust
/// One deliverable's CPM schedule, in hours from plan start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRow {
    pub id: String,
    pub es: f32,
    pub ef: f32,
    pub ls: f32,
    pub lf: f32,
    pub float: f32,
    pub critical: bool,
}
```
  `PlanStatus` gains (each `#[serde(default)]`, appended after `locks_held`): `critical_ids: Vec<String>`, `schedule: Vec<ScheduleRow>` (graph insertion order), `ready: Vec<String>` (status `Ready`, no live lock, sorted by `(float, es, id)` ascending).
  `priority_key(deliverable_id, sched_by_id: &HashMap<&str, (f32, f32)>) -> (i64, i64, String)` where the tuple is `(float, es)` scaled ×1000 and rounded.

- [ ] **Step 1: Write failing tests** in `tests/planner.rs` (reuse its existing planner/graph construction helpers; build the graph `A(1h) -> B(2h)`, `A -> C(4h)`, `B,C -> D(1h)` plus independent `E(1h)`):

```rust
#[tokio::test]
async fn status_schedule_reports_float_for_non_critical_branch() {
    // B has 2h float (C is 4h on the parallel branch).
    let (planner, plan_id) = submit_diamond_with_spare().await;
    let status = planner.status(&plan_id).await.unwrap();
    let b = status.schedule.iter().find(|r| r.id == "B").unwrap();
    assert!((b.float - 2.0).abs() < 1e-3);
}

#[tokio::test]
async fn status_schedule_marks_critical_rows() {
    let (planner, plan_id) = submit_diamond_with_spare().await;
    let status = planner.status(&plan_id).await.unwrap();
    let critical: Vec<&str> = status.schedule.iter().filter(|r| r.critical).map(|r| r.id.as_str()).collect();
    assert_eq!(critical, vec!["A", "C", "D"]);
}

#[tokio::test]
async fn status_ready_is_sorted_by_float_ascending() {
    // Ready at start: A (float 0) and E (float 6-1=5).
    let (planner, plan_id) = submit_diamond_with_spare().await;
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.ready, vec!["A", "E"]);
}

#[tokio::test]
async fn status_ready_excludes_locked_deliverables() {
    let (planner, plan_id) = submit_diamond_with_spare().await;
    // acquire exactly one deliverable (highest priority = A)
    planner.acquire_cohort(&plan_id, &CallerId("w1".into()), 1).await.unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.ready, vec!["E"]);
}

#[tokio::test]
async fn acquire_cohort_prefers_lowest_float() {
    let (planner, plan_id) = submit_diamond_with_spare().await;
    let cohort = planner.acquire_cohort(&plan_id, &CallerId("w1".into()), 1).await.unwrap();
    assert_eq!(cohort_ids(&cohort), vec!["A"]);
}
```

Write the helpers `submit_diamond_with_spare() -> (BasicCpmPlanner, PlanId)` (in-memory store; deliverables with disjoint `owned_files` and explicit `estimated_effort_hours`) and `cohort_ids(&Cohort) -> Vec<String>` in the test file, matching how existing tests in `tests/planner.rs` build planners, graphs, and read cohorts (adapt the `acquire_cohort` call to its real signature).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test planner status_`
Expected: FAIL — no field `schedule` / `ready` on `PlanStatus`.

- [ ] **Step 3: Add `ScheduleRow` and the `PlanStatus` fields** in `src/plan.rs` exactly as in Interfaces. Change `critical_path_hours` doc to: "Project length in hours: the maximum earliest finish, equal to the effort summed along `critical_path`."

- [ ] **Step 4: Populate them in `status()`** (`src/planner.rs`): build `schedule` by mapping `state.graph.deliverables` (insertion order) to the matching task in `state.cached_result.tasks`; `critical_ids` from `state.cached_result.critical_ids`; `ready` = deliverables whose status is `DeliverableStatus::Ready` and `!state.locks.contains_key(id)`, sorted by `(float, es, id)` using `f32::total_cmp`.

- [ ] **Step 5: Float-first priority** — replace `priority_key` with:

```rust
/// Sort key for the ready-set priority pass: least total float first
/// (critical work leads), then earliest start, then id for determinism.
fn priority_key(deliverable_id: &str, sched_by_id: &HashMap<&str, (f32, f32)>) -> (i64, i64, String) {
    let (float, es) = match sched_by_id.get(deliverable_id) {
        Some(&v) => v,
        None => unreachable!(
            "deliverable '{deliverable_id}' is in the ready set but absent from the cached \
             CPM schedule — ready set and CPM result are out of sync"
        ),
    };
    let scale = |h: f32| (h * 1000.0).round() as i64;
    (scale(float), scale(es), deliverable_id.to_string())
}
```

In `acquire_cohort` step 3 replace the `cp_positions`/`es_by_id` tables with `sched_by_id: HashMap<&str, (f32, f32)>` built from `state.cached_result.tasks` (`(t.float, t.earliest_start)`) and update the call sites. Rewrite the in-module unit tests `priority_key_critical_tier_orders_by_position`, `priority_key_noncritical_uses_es`, `priority_key_missing_es_is_invariant_breach` into: `priority_key_orders_lower_float_first`, `priority_key_breaks_float_ties_by_es`, `priority_key_missing_entry_is_invariant_breach` (`#[should_panic]`).

- [ ] **Step 6: Document** — in `src/server.rs` `instructions()`, extend the `plan.status` line: "…returns statuses, critical_path (one real chain), critical_ids, per-deliverable schedule (es/ef/ls/lf/float, hours) and the ready set ordered by float".

- [ ] **Step 7: Run + commit**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`

```bash
git add src/plan.rs src/planner.rs src/server.rs tests/planner.rs
git commit -m "feat(status): per-deliverable schedule + float-ordered ready set; cohorts prefer least float (#15)"
```

---

### Task 3: Shared CPM computation + migration ladder that repairs stored plans

**Files:**
- Create: `src/schedule.rs`
- Modify: `src/lib.rs` (`mod schedule;` — crate-private)
- Modify: `src/planner.rs` (submit uses `schedule::compute_cpm`; `deliverable_to_task` moves out)
- Modify: `src/plan_store.rs` (`init` → migration ladder; `cpm_version` column; insert writes it; recompute sweep)
- Test: `tests/persistence.rs`

**Interfaces:**
- Consumes: `CPM_VERSION` (Task 1), `EffortEstimator`, `PlanGraph`, `PlannerError::InvalidGraph`.
- Produces: `pub(crate) fn compute_cpm(graph: &PlanGraph) -> Result<CriticalPathResult, PlannerError>` in `src/schedule.rs` — builds tasks with a default `EffortEstimator`, runs `CpmAlgorithm::calculate`, returns `InvalidGraph` (same "internal CPM inconsistency…" message as today) when `unscheduled` is non-empty. `pub(crate) fn deliverable_to_task` moves with it unchanged.
  Schema: `PRAGMA user_version` = 2 after open; `plans.cpm_version INTEGER NOT NULL DEFAULT 0`.

- [ ] **Step 1: Write failing tests** in `tests/persistence.rs` (it already opens file-backed stores in temp dirs; follow its helpers). Use the parallel-chains graph from Task 1 (`P0a(2)->P0b(3)`, `P1a(2)->P1b(3)`) as deliverables:

```rust
#[tokio::test]
async fn reopening_store_repairs_stale_cached_critical_path() {
    let dir = tempdir_for_test();
    let db = dir.join("plans.db");
    let plan_id = submit_parallel_chains(&db).await; // open store, submit, drop
    // Simulate a row written by an older kernel: buggy path + version 0.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let mut result: serde_json::Value = serde_json::from_str(
            &conn.query_row("SELECT cached_result FROM plans WHERE plan_id = ?1", [&plan_id.0], |r| r.get::<_, String>(0)).unwrap(),
        ).unwrap();
        result["critical_path"] = serde_json::json!(["P0a", "P1a", "P0b", "P1b"]);
        conn.execute(
            "UPDATE plans SET cached_result = ?1, cpm_version = 0 WHERE plan_id = ?2",
            rusqlite::params![result.to_string(), plan_id.0],
        ).unwrap();
    }
    let planner = planner_at(&db);
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.critical_path, vec!["P0a", "P0b"]);
}

#[test]
fn opened_store_reports_schema_version_2() {
    let dir = tempdir_for_test();
    let db = dir.join("plans.db");
    drop(cpm_planner::plan_store::SqlitePlanStore::open(&db).unwrap());
    let conn = rusqlite::Connection::open(&db).unwrap();
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v, 2);
}

#[tokio::test]
async fn newly_submitted_plan_is_stamped_with_current_cpm_version() {
    let dir = tempdir_for_test();
    let db = dir.join("plans.db");
    let plan_id = submit_parallel_chains(&db).await;
    let conn = rusqlite::Connection::open(&db).unwrap();
    let v: i64 = conn.query_row("SELECT cpm_version FROM plans WHERE plan_id = ?1", [&plan_id.0], |r| r.get(0)).unwrap();
    assert_eq!(v, cpm_planner::algorithm::CPM_VERSION);
}
```

Implement `tempdir_for_test`, `submit_parallel_chains(&Path) -> PlanId`, `planner_at(&Path) -> BasicCpmPlanner` with whatever the file already uses (reuse existing helpers if equivalent ones exist; `rusqlite` is already a dependency).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test persistence`
Expected: FAIL — no `cpm_version` column / `user_version` is 0.

- [ ] **Step 3: Create `src/schedule.rs`** with `compute_cpm` and the moved `deliverable_to_task` (keep its doc comment). Register `mod schedule;` in `src/lib.rs`. In `planner.rs::submit_plan`, replace the inline estimator/tasks/calculate/unscheduled block with `let cached_result = crate::schedule::compute_cpm(&graph)?;`.

- [ ] **Step 4: Migration ladder** in `src/plan_store.rs`. Replace the body of `init` between the pragmas and `Self { conn }` with `migrate(&conn)?;` and add:

```rust
/// Schema migrations, applied in order. `PRAGMA user_version` records the
/// last one applied. Each step must be idempotent against databases created
/// before versioning existed (user_version 0 but tables present).
const MIGRATIONS: &[fn(&Connection) -> anyhow::Result<()>] = &[
    migrate_v1_base_schema,   // tables + counter columns (pre-versioning layout)
    migrate_v2_cpm_version,   // plans.cpm_version
];

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, step) in MIGRATIONS.iter().enumerate() {
        let version = i as i64 + 1;
        if version > current {
            step(conn).with_context(|| format!("applying schema migration v{version}"))?;
            conn.pragma_update(None, "user_version", version)?;
        }
    }
    recompute_stale_results(conn)
}
```

`migrate_v1_base_schema` = the existing `CREATE TABLE IF NOT EXISTS …` batch followed by `migrate_counter_columns(conn)`. `migrate_v2_cpm_version` adds the column only if `PRAGMA table_info(plans)` lacks it: `ALTER TABLE plans ADD COLUMN cpm_version INTEGER NOT NULL DEFAULT 0`.

```rust
/// Recompute `cached_result` for every plan stored by an older CPM kernel.
/// A graph that no longer computes is left untouched and logged.
fn recompute_stale_results(conn: &Connection) -> anyhow::Result<()> {
    let stale: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT plan_id, graph FROM plans WHERE cpm_version < ?1")?;
        stmt.query_map([crate::algorithm::CPM_VERSION], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    for (plan_id, graph_json) in stale {
        let graph: PlanGraph = serde_json::from_str(&graph_json)
            .with_context(|| format!("decoding stored graph for {plan_id}"))?;
        match crate::schedule::compute_cpm(&graph) {
            Ok(result) => {
                conn.execute(
                    "UPDATE plans SET cached_result = ?1, cpm_version = ?2 WHERE plan_id = ?3",
                    params![serde_json::to_string(&result)?, crate::algorithm::CPM_VERSION, plan_id],
                )?;
            }
            Err(e) => tracing::warn!(%plan_id, error = %e, "could not recompute stored CPM result"),
        }
    }
    Ok(())
}
```

Make the plan INSERT in `submit_or_get` also write `cpm_version` = `crate::algorithm::CPM_VERSION`. Keep the existing legacy-layout unit tests in `plan_store.rs` passing (they build pre-versioning tables by hand and then open the store).

- [ ] **Step 5: Run + commit**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`

```bash
git add src/schedule.rs src/lib.rs src/planner.rs src/plan_store.rs tests/persistence.rs
git commit -m "feat(store): versioned schema migrations; recompute stale CPM results on open"
```

---

### Task 4: `plan.get` returns the stored graph

**Files:**
- Modify: `src/ports.rs` (trait method), `src/planner.rs` (impl), `src/plan.rs` (`PlanDefinition` response type), `src/server.rs` (constant, `PLAN_TOOL_NAMES`, args struct, schema, dispatch arm, handler, `instructions()`), `README.md` (tool table row)
- Test: `tests/server_integration.rs`

**Interfaces:**
- Consumes: `SqlitePlanStore::read_plan`, `PlanState.graph`.
- Produces:
  - `async fn get_plan(&self, plan_id: &PlanId) -> Result<PlanDefinition, PlannerError>;` on `Planner`.
  - `#[derive(Debug, Clone, Serialize, Deserialize)] pub struct PlanDefinition { pub plan_id: PlanId, pub graph: PlanGraph }` in `src/plan.rs`.
  - Tool `plan.get` (`pub const TOOL_GET: &str = "plan.get";`), args `{ "plan_id": string }` (`deny_unknown_fields`), result = `PlanDefinition` JSON. Unknown plan → same `PLAN_NOT_FOUND` mapping as `plan.status`.

- [ ] **Step 1: Write failing tests** in `tests/server_integration.rs` (reuse `server()`, `call_args`, `sample_graph()`, `submit_plan`):

```rust
#[tokio::test]
async fn plan_get_returns_submitted_graph() {
    let server = server();
    let plan_id = submit_plan(&server).await;
    let got = server
        .dispatch_call(call_args("plan.get", json!({ "plan_id": plan_id })))
        .await
        .unwrap();
    assert_eq!(got["graph"]["deliverables"], sample_graph()["deliverables"]);
}

#[tokio::test]
async fn plan_get_unknown_plan_is_an_error() {
    let server = server();
    let err = server
        .dispatch_call(call_args("plan.get", json!({ "plan_id": "plan_missing" })))
        .await
        .unwrap_err();
    assert!(err.message.contains("PLAN_NOT_FOUND"));
}
```

(If `sample_graph()` omits optional fields that serialize back with defaults — e.g. `metadata: {}` — compare after normalising both sides through `serde_json::from_value::<PlanGraph>`; never weaken to comparing ids only.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test server_integration plan_get`
Expected: FAIL — unknown tool `plan.get`.

- [ ] **Step 3: Implement** the trait method, planner impl (`self.store.read_plan(plan_id, |state| PlanDefinition { plan_id: plan_id.clone(), graph: state.graph.clone() })`), the `PlanDefinition` type, and the server wiring in the same style as `plan.status`. `instructions()` line: `plan.get — return the submitted PlanGraph (deliverables, estimates, files, metadata) for a plan_id`. README tool table row: `| \`plan.get\` | Return the stored plan graph for a plan_id (read back what was submitted). |`.

- [ ] **Step 4: Run + commit**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check`

```bash
git add src/ports.rs src/planner.rs src/plan.rs src/server.rs README.md tests/server_integration.rs
git commit -m "feat(server): plan.get returns the stored plan graph"
```
