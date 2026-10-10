# P3 — Graph Schema Batch + Synthetic Endpoints (#21, #22, #27, #28, #12) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every plan has well-defined `__start__`/`__finish__` endpoints; prerequisites can say what they consume (and carry kind + lag); deliverables can be milestones and carry calendar duration; files can be shared append-only; and file ownership may overlap between deliverables that can never run at the same time.

**Architecture:** All `Deliverable`/prerequisite/file schema changes land together so `hash_graph`, validation and every fixture are touched once. Object wire forms are additive: plain strings keep working. A new `src/graph.rs` holds graph-structure helpers (prerequisite ids, reachability) shared by validation, CPM input building and (later) lint. Synthetic endpoints are injected only into the CPM input (`schedule::compute_cpm`) — never stored in the graph or the status map — and their status is derived at read time.

**Tech Stack:** Rust 1.99.0, serde (untagged enums), rusqlite, rmcp.

**Spec:** `docs/superpowers/plans/2026-10-09-backlog-roadmap.md` § P3 (incl. the synthetic-endpoints decision) + issues #21, #22, #27, #28, #12.

## Global Constraints

- Wire compatibility: plain-string `prerequisites` entries and plain-path `owned_files` entries keep working forever; object forms are additive. Existing response fields keep names/types; new response fields are appended with `#[serde(default)]`.
- Every new `Deliverable` / prerequisite / owned-file field MUST be included in `planner.rs::hash_graph` (graphs differing only in that field must hash differently).
- `CPM_VERSION` bumps to `2` in Task 1 and to `3` in Task 4 (each changes kernel output for existing graphs); stored plans recompute on open.
- Reserved ids: `__start__`, `__finish__`.
- Error-code prefixes unchanged; new validation failures use `INVALID_GRAPH:`.
- Every new tool argument / response field documented in `instructions()`; README + CHANGELOG `[Unreleased]` updated per user-visible change (single Added/Fixed/Changed headings).
- `src/plan.rs`, `src/planner.rs`, `src/ports.rs`, `src/locks.rs`, `src/task.rs`, `tests/algorithm.rs` use CRLF — preserve each file's endings (cargo fmt may flip planner.rs; restore). No whole-file churn.
- `cargo fmt --all --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings` green on 1.99.0.

## Review Focus

1. **A stored plan from P2** (string prerequisites, path files, no endpoints) opens, recomputes, and reports `__start__ → … → __finish__` (Task 1).
2. **Mixed wire forms in one graph** — `["a", {"id":"b","consumes":"schema"}]` and `["src/x.rs", {"path":"REGISTRY.md","mode":"append"}]` parse, hash stably, and round-trip through `plan.get` unchanged (Tasks 2, 5).
3. **A user deliverable named `__finish__`** is rejected at submit (Task 1).
4. **Two unordered deliverables sharing an exclusive file** are still rejected; ordered ones are accepted and never co-leased (Task 6).
5. **Lag on an edge into a critical deliverable** shifts ES and the project length by exactly the lag (Task 4).

---

### Task 1: Synthetic `__start__` / `__finish__` endpoints

**Files:**
- Modify: `src/schedule.rs` (`compute_cpm` injects endpoints), `src/algorithm.rs` (`CPM_VERSION = 2`), `src/task.rs` (nothing structural — endpoints are ordinary zero-effort `Task`s), `src/plan.rs` (`ScheduleRow.synthetic`, `PlanStatus.plan_complete`, reserved-id constants), `src/planner.rs` (`validate_graph` reserved ids; `status()` endpoint rows + derived statuses; completion path emits `plan.completed`), `src/server.rs` (`instructions()`), `CHANGELOG.md`, `README.md`
- Test: `tests/planner.rs`, `tests/persistence.rs`, plus updating existing expectations in `tests/*.rs` and in-module tests that assert exact `critical_path` vectors

**Interfaces:**
- Produces:

```rust
// src/plan.rs
pub const START_ID: &str = "__start__";
pub const FINISH_ID: &str = "__finish__";
// ScheduleRow gains (appended):
#[serde(default)]
pub synthetic: bool,
// PlanStatus gains (appended):
#[serde(default)]
pub plan_complete: bool,
```
  `schedule::compute_cpm(graph)` builds tasks for every deliverable, then adds `Task` `__start__` (effort 0, no deps) and `__finish__` (effort 0, deps = every deliverable id that no other deliverable lists as a prerequisite); every deliverable with no prerequisites gets `__start__` added to its CPM `dependencies` (the stored graph is untouched). For an empty graph: `__start__ → __finish__` only.
  `critical_path` therefore always begins with `__start__` and ends with `__finish__`; `critical_path_hours` value unchanged.
  `status()`: `schedule` = `[__start__ row] + deliverable rows (graph order) + [__finish__ row]`, endpoint rows `synthetic: true`; `deliverables` tuples do NOT include endpoints (positional rows unchanged); `plan_complete` = every deliverable `Complete`.
  When a completion (`mark_status` Complete or `accept`) makes every deliverable Complete, push audit event `plan.completed` with payload `{plan_id, deliverable_count}` — emitted exactly once per plan (guard: only when this completion flips `plan_complete` from false to true).
  `validate_graph`: a deliverable id equal to `START_ID` or `FINISH_ID` → `INVALID_GRAPH: deliverable id '__start__' is reserved`.

- [ ] **Step 1: Failing tests** (`tests/planner.rs`, one assertion each):

```rust
#[tokio::test]
async fn critical_path_starts_and_ends_at_synthetic_endpoints() {
    // a(1) -> b(2)
    assert_eq!(status.critical_path, vec!["__start__", "a", "b", "__finish__"]);
}
#[tokio::test]
async fn schedule_lists_endpoints_as_synthetic_rows() {
    let ids: Vec<(&str, bool)> = status.schedule.iter().map(|r| (r.id.as_str(), r.synthetic)).collect();
    assert_eq!(ids, vec![("__start__", true), ("a", false), ("b", false), ("__finish__", true)]);
}
#[tokio::test]
async fn finish_depends_on_every_sink() {
    // a(1), b(3) independent: __finish__.es == 3
    assert!((finish_row.es - 3.0).abs() < 1e-3);
}
#[tokio::test]
async fn endpoints_are_never_ready_or_leased() {
    // fresh plan: ready == ["a"], not containing endpoints
    assert_eq!(status.ready, vec!["a"]);
}
#[tokio::test]
async fn submit_rejects_reserved_finish_id() {
    assert!(err.to_string().starts_with("INVALID_GRAPH"));
}
#[tokio::test]
async fn plan_complete_after_every_deliverable_completes() {
    assert!(status.plan_complete);
}
#[tokio::test]
async fn plan_complete_is_false_while_work_remains() {
    assert!(!status.plan_complete);
}
#[tokio::test]
async fn completing_last_deliverable_emits_plan_completed_once() {
    // MemoryAuditSink (see tests/locks.rs helpers); complete a then b; then a redundant lockless Complete re-mark of b
    assert_eq!(events.iter().filter(|e| e.event_type == "plan.completed").count(), 1);
}
#[tokio::test]
async fn empty_graph_has_start_to_finish_critical_path() {
    assert_eq!(status.critical_path, vec!["__start__", "__finish__"]);
}
```
  `tests/persistence.rs`: `stored_p2_plan_gains_endpoints_after_reopen` — submit, then `UPDATE plans SET cpm_version = 1` via raw rusqlite, reopen, assert `critical_path.first() == Some("__start__")`.
- [ ] **Step 2:** `cargo test --test planner` → FAIL (no `synthetic` / `plan_complete`).
- [ ] **Step 3:** Implement as specified. Bump `CPM_VERSION` to 2 with a doc line "v2: synthetic __start__/__finish__ endpoints".
- [ ] **Step 4:** Update every existing test that asserts an exact `critical_path` vector, a `schedule.len()`, or `CriticalPathResult.total_tasks`/`tasks.len()` to include the endpoints (keep the assertion's intent; list each changed test in the report). `tests/algorithm.rs` tests call `CpmAlgorithm::calculate` directly and are NOT affected (endpoints live in `compute_cpm`).
- [ ] **Step 5:** `instructions()` + README: describe endpoints, `synthetic`, `plan_complete`, reserved ids. CHANGELOG: Added "- Synthetic `__start__`/`__finish__` endpoints in every plan's CPM: `critical_path` always runs start → finish; `schedule` rows carry `synthetic`; `plan.status` reports `plan_complete`; `plan.completed` audit event. `__start__`/`__finish__` are reserved ids." Changed "- `critical_path` now includes the synthetic endpoints."
- [ ] **Step 6:** Full check → PASS. Commit: `feat(cpm): synthetic __start__/__finish__ endpoints; plan_complete (CPM v2)`

---

### Task 2: Prerequisites can carry what they consume, kind and lag (#21)

**Files:**
- Create: `src/graph.rs` (`pub(crate) fn prerequisite_ids(d: &Deliverable) -> impl Iterator<Item = &str>` and later helpers)
- Modify: `src/plan.rs` (`Prerequisite`, `PrerequisiteKind`, `Deliverable.prerequisites: Vec<Prerequisite>`), `src/lib.rs`, `src/planner.rs` (every `.prerequisites` use: `validate_graph`, submit statuses, `complete_deliverable` dependents, `accept` prereq check, lockless-complete prereq check, `force_release` revival), `src/schedule.rs` (`deliverable_to_task` dependencies), `hash_graph`, `CHANGELOG.md`, `README.md`, `src/server.rs` (`instructions()` + submit schema description)
- Test: `tests/planner.rs`, `tests/server_integration.rs`; mechanical fixture updates elsewhere

**Interfaces:**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrerequisiteKind { Artifact, Interface }

/// A prerequisite edge. Wire: a bare id string, or an object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Prerequisite {
    Id(String),
    Edge {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consumes: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<PrerequisiteKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lag_hours: Option<f32>,
    },
}
impl Prerequisite {
    pub fn id(&self) -> &str;
    pub fn consumes(&self) -> Option<&str>;
    pub fn kind(&self) -> Option<PrerequisiteKind>;
    pub fn lag_hours(&self) -> f32; // 0.0 when absent
}
impl From<&str> for Prerequisite { /* Id */ }
impl From<String> for Prerequisite { /* Id */ }
```
  `Deliverable.prerequisites: Vec<Prerequisite>`. A bare string round-trips as a bare string (untagged `Id` serializes as a string). `lag_hours` is stored and hashed here but only used by the kernel in Task 4. Validation: `lag_hours` must be finite and ≥ 0 → else `INVALID_GRAPH`; duplicate prerequisite ids on one deliverable remain allowed (deduped as today).
  `hash_graph`: prerequisites normalised to objects `{id, consumes, kind, lag_hours}` sorted by id, so `"a"` and `{"id":"a"}` hash identically (same edge), while differing `consumes`/`kind`/`lag_hours` hash differently.

- [ ] **Step 1: Failing tests** (one assertion each): `object_prerequisite_parses_and_schedules` (b depends on `{"id":"a","consumes":"api schema"}` → status ready after a completes); `string_and_object_prerequisite_hash_identically` (two submits → same plan_id); `consumes_difference_changes_plan_identity` (→ different plan_id); `plan_get_round_trips_mixed_prerequisite_forms` (server: submit `["a", {"id":"c","kind":"interface"}]`, `plan.get` returns them byte-equal as JSON); `negative_lag_is_rejected` (INVALID_GRAPH).
- [ ] **Step 2:** FAIL. **Step 3:** Implement; replace every `d.prerequisites.iter()` over strings with `crate::graph::prerequisite_ids(d)` (or `.iter().map(Prerequisite::id)`). Test fixtures that build `Deliverable { prerequisites: vec!["x".into()] }` keep compiling via `From<&str>`; update the rest mechanically.
- [ ] **Step 4:** Docs: `instructions()` + README describe the object form; CHANGELOG Added "- Prerequisites may be objects `{id, consumes?, kind?: artifact|interface, lag_hours?}` (#21)." Changed (library) "- `Deliverable.prerequisites` is `Vec<Prerequisite>`."
- [ ] **Step 5:** Full check → PASS. Commit: `feat(graph): prerequisites carry consumes, kind and lag (#21)`

---

### Task 3: Milestones as first-class nodes (#22)

**Files:** Modify `src/plan.rs` (`Deliverable.milestone`, `MilestoneRow`, `PlanStatus.milestones`), `src/planner.rs` (`status()`, `hash_graph`), `src/schedule.rs` or `src/graph.rs` (per-milestone longest path), docs. Test: `tests/planner.rs`.

**Interfaces:**
- `Deliverable.milestone: bool` (`#[serde(default, skip_serializing_if = "std::ops::Not::not")]`); a deliverable whose `metadata.milestone == true` is also treated as a milestone (legacy convention from the skill).
- `pub struct MilestoneRow { pub id: String, pub critical_path: Vec<String>, pub hours: f32, pub complete: bool }` (`Serialize, Deserialize, Debug, Clone, PartialEq`).
- `PlanStatus.milestones: Vec<MilestoneRow>` (`#[serde(default)]`, graph order). `critical_path` = the longest chain from `__start__` to the milestone (same backward trace as the kernel, restricted to the milestone's ancestors; ends at the milestone id, begins with `__start__`); `hours` = milestone EF.

- [ ] **Step 1: Failing tests:** `milestone_row_reports_longest_chain_to_milestone` (a(1)→m(0, milestone)→c(5); also x(3)→m → row m path == ["__start__","x","m"]); `milestone_hours_is_milestone_earliest_finish` (== 3.0); `metadata_milestone_flag_is_honoured`; `milestone_flag_changes_plan_identity` (hash); `milestone_complete_reflects_status`.
- [ ] **Step 2–4:** FAIL → implement → docs (CHANGELOG Added "- `milestone: true` deliverables; `plan.status` reports per-milestone critical path and hours (#22).").
- [ ] **Step 5:** Commit: `feat(graph): first-class milestones with per-milestone critical path (#22)`

---

### Task 4: Calendar duration and edge lag in the kernel (#27)

**Files:** Modify `src/plan.rs` (`Deliverable.duration_hours`), `src/task.rs` (`Task.lag_by_dependency: HashMap<String, f32>` with `#[serde(default)]`; `Task.duration_hours` semantics doc), `src/algorithm.rs` (forward/backward passes add lag; `CPM_VERSION = 3`), `src/schedule.rs` (`deliverable_to_task`: CPM duration = `duration_hours.unwrap_or(effort)`; lag map from prerequisite `lag_hours`), `hash_graph`, docs. Test: `tests/algorithm.rs`, `tests/planner.rs`.

**Interfaces:**
- `Deliverable.duration_hours: Option<f32>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`), validated finite ≥ 0.
- Kernel: a task's scheduled length is `Task.effort_hours` **as set by `deliverable_to_task`** = `duration_hours` if present else effort (effort stays available to EV via the graph). Add `pub effort_for_ev: f32` to `Task`? — **No**: EV (P5) reads effort from the graph; keep `Task` minimal.
- Forward: `ES(succ) = max over preds (EF(pred) + lag(pred→succ))`. Backward: `LF(pred) = min over succs (LS(succ) − lag(pred→succ))`. Tight-edge test in `trace_critical_path` uses `EF(pred) + lag == ES(succ)`.
- `CriticalPathResult` unchanged in shape.

- [ ] **Step 1: Failing tests** (`tests/algorithm.rs`, using a new `make_task_with_lag` helper): `lag_delays_successor_start_by_lag` (a(2) →lag 3→ b(1): b.es == 5); `lag_extends_project_length` (duration == 6); `lag_edge_is_tight_on_critical_path` (path == [a, b]); `backward_pass_subtracts_lag` (a.lf == 2). `tests/planner.rs`: `duration_hours_overrides_effort_for_schedule` (d effort 8, duration 2 → finish es == 2); `duration_change_changes_plan_identity`.
- [ ] **Step 2–4:** FAIL → implement → docs (CHANGELOG Added "- `duration_hours` (calendar time) per deliverable and `lag_hours` per prerequisite edge drive the schedule; effort stays the cost basis (#27)."). `CPM_VERSION` → 3.
- [ ] **Step 5:** Commit: `feat(cpm): calendar duration and per-edge lag (#27, CPM v3)`

---

### Task 5: Shared append-only files (#28)

**Files:** Modify `src/plan.rs` (`OwnedFile`, `FileMode`, `Deliverable.owned_files: Vec<OwnedFile>`, `Cohort.shared_paths`), `src/locks.rs` (file index: exclusive holder vs append holders), `src/plan_store.rs` (index rebuild on load), `src/planner.rs` (acquire conflict check, release paths, `force_release`, `complete_deliverable`, `hash_graph`, audit acquired payload), `src/server.rs` (cohort wire `shared_paths`), docs. Test: `tests/locks.rs`, `tests/planner.rs`, `tests/server_integration.rs`.

**Interfaces:**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileMode { #[default] Exclusive, Append }

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OwnedFile {
    Path(PathBuf),
    Claim { path: PathBuf, #[serde(default)] mode: FileMode },
}
impl OwnedFile { pub fn path(&self) -> &Path; pub fn mode(&self) -> FileMode; }
impl From<&str> for OwnedFile { /* Path */ }
impl From<PathBuf> for OwnedFile { /* Path */ }
```
  Lock index: `file_to_deliverable: HashMap<PathBuf, String>` becomes `file_claims: HashMap<PathBuf, FileClaim>` with `pub(crate) enum FileClaim { Exclusive(String), Append(BTreeSet<String>) }`. Conflict rule: exclusive vs anything = conflict; append vs append = no conflict. Releasing removes only that deliverable's claim (an append set drops the id; empty set removes the path).
  `Cohort.shared_paths: Vec<PathBuf>` (`#[serde(default)]`; wire field appended) = paths in this cohort claimed in append mode by ≥ 2 deliverables (in-cohort or with held locks).
  Validation (submit) for this task: two deliverables both claiming a path in **append** mode is allowed; any exclusive overlap is still rejected (Task 6 relaxes that for ordered deliverables).
  `hash_graph`: files normalised to `{path, mode}` sorted by path; `"x"` and `{"path":"x"}` hash identically.

- [ ] **Step 1: Failing tests:** `append_claims_on_same_path_are_coleased` (two Ready deliverables both appending REGISTRY.md → one acquire with max 2 leases both); `cohort_reports_shared_append_paths` (`shared_paths == ["REGISTRY.md"]`); `exclusive_claim_conflicts_with_held_append_claim` (third deliverable claims REGISTRY.md exclusively and is NOT ordered with the others → submit rejected INVALID_GRAPH); `releasing_one_append_holder_keeps_the_other_claim` (complete one; the other remains locked; status locks_held has 1); `path_and_object_file_forms_hash_identically`; `plan_get_round_trips_mixed_file_forms` (server). Also the **FILE_CONFLICT ruling carry-over from P2**: `requested_append_and_exclusive_same_cohort_conflict` is not constructible until Task 6 — see Task 6.
- [ ] **Step 2–4:** FAIL → implement → docs (CHANGELOG Added "- `owned_files` entries may be `{path, mode: \"append\"}`; append claims may be co-leased and are reported in the cohort's `shared_paths` (#28)." Changed (library) "- `Deliverable.owned_files` is `Vec<OwnedFile>`; `Cohort` gains `shared_paths`.").
- [ ] **Step 5:** Commit: `feat(locks): shared append-only file claims (#28)`

---

### Task 6: Ownership overlap allowed between ordered deliverables (#12) + FILE_CONFLICT coverage

**Files:** Modify `src/graph.rs` (`pub(crate) fn reachability(graph: &PlanGraph) -> HashMap<String, HashSet<String>>` — transitive successors per id), `src/planner.rs` (`validate_graph` overlap rule), docs. Test: `tests/planner.rs`, `tests/locks.rs`.

**Interfaces:**
- Overlap rule: for every path claimed by ≥ 2 deliverables, every pair `(x, y)` where at least one claim is exclusive must be ordered (`y ∈ reach(x)` or `x ∈ reach(y)`), else `INVALID_GRAPH: file '<path>' is claimed by '<x>' and '<y>', which are not ordered by prerequisites (one could run while the other holds it)`. Append/append pairs need no ordering.
- The acquire-time disjointness check is unchanged (defence in depth).

- [ ] **Step 1: Failing tests:** `ordered_deliverables_may_share_an_exclusive_file` (a → b both own src/x.rs → submit ok); `transitively_ordered_deliverables_may_share_a_file` (a → m → b); `unordered_deliverables_sharing_an_exclusive_file_are_rejected` (error message names both ids); **FILE_CONFLICT (P2 ruling carry-over)**: `requested_ids_with_append_vs_exclusive_on_same_path_report_file_conflict` — build a → b where a appends REGISTRY.md and b claims it exclusively, plus c appending REGISTRY.md and ordered after b; lease b... (construct any reachable scenario where a requested Ready deliverable's claim conflicts with a held lock's claim; if no such state is reachable once validation holds, write the test against the acquire path with a held append lock vs a requested append+exclusive mix that IS reachable, or document in the report precisely why FILE_CONFLICT remains unreachable and add a unit test of the conflict predicate on `FileClaim` instead).
- [ ] **Step 2–4:** FAIL → implement → docs (CHANGELOG Fixed "- `plan.submit` accepts a file owned by deliverables ordered by prerequisites; only unordered exclusive overlaps are rejected (#12).").
- [ ] **Step 5:** Commit: `fix(validate): allow file overlap between ordered deliverables (#12)`
