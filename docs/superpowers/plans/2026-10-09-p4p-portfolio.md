# P4P — Plan-as-Code Portfolio: projects, named plans, variants, revisions, compare, select Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A repo holds many named plans as files under `<repo root>/.cpm-planner/plans/<name>/<variant>.json`; agents sync files into the planner, design many variants, compare them on the scorecard, select exactly one to execute, and revise it over time without losing progress.

**Architecture:** Files are the source of truth for plan *definitions*; SQLite holds runtime state. A new `src/project.rs` resolves the project root and confines every path. Schema v3 adds `plan_lines` (project, name, selected variant, archived), `variants` (project, name, variant → `plan_id`, source path, head revision, archived) and `revisions` (plan_id, revision, graph, content hash, created_at). A variant's `plan_id` is the stable execution handle; revising updates that plan's graph in place with carry-over rules (`src/revise.rs`). Plans created by plain `plan.submit` (no name) keep working exactly as before and are always executable. Execution tools reject non-selected variants (`VARIANT_NOT_SELECTED`).

**Tech Stack:** Rust 1.99.0, rusqlite, serde, rmcp. Builds on P4 (`lint`, `simulate`, `Scorecard`, `MonteCarloSummary`).

**Spec:** `docs/superpowers/plans/2026-10-09-backlog-roadmap.md` § P4P (plan-as-code decision, design-vs-execution decision) + issue #23 part b.

## Global Constraints

- Plan directory: `<repo root>/.cpm-planner/plans/<name>/<variant>.json`. Repo root = `CPM_PROJECT_ROOT` if set, else the nearest ancestor of the server's cwd containing `.cpm-planner/` or `.git`. `name` and `variant` match `^[a-z0-9][a-z0-9._-]{0,63}$`.
- Path safety: every path argument is relative to the repo root, canonicalized, and must resolve inside `<root>/.cpm-planner/plans/`; `..`, absolute paths, and symlinks escaping the root are rejected with `INVALID_PATH:`. Writes never follow symlinks.
- When the server cannot read the project (no root found), `plan.sync` accepts the graph inline (`graph` + `path` used only for naming); file-reading features report `definition_drift: null` (unknown), never an error.
- Many variants may be designed; exactly one variant per plan line is selected. Execution tools (`acquire_cohort`, `heartbeat`, `mark_status`, `accept`, later `baseline`) on a non-selected variant's `plan_id` → `VARIANT_NOT_SELECTED: plan <plan_id> is variant '<v>' of '<name>'; selected is '<s>'`. Read/analysis tools work on any variant.
- Legacy and unnamed plans (`plan.submit` without `name`) behave exactly as today and are never subject to `VARIANT_NOT_SELECTED`.
- `plan.submit` dedup is scoped: named submissions dedup within (project, name, variant) only; unnamed keep the global graph-hash dedup.
- Revise carry-over rules (exact): unchanged deliverable (same canonical definition) keeps status + counters; changed definition keeps status unless its prerequisite id set changed, in which case a Complete/Ready/Pending deliverable is re-derived (Ready if all prerequisites Complete, else Pending) and listed in `reopened`; a deliverable whose prerequisite was reopened or removed is re-derived the same way (transitively); a deliverable with an `interface` edge to a changed deliverable whose `metadata.contract == true` is reopened; new deliverables start Ready/Pending by prerequisites; removed deliverables are dropped — refused with `LOCK_HELD` if they hold a live lock unless `force: true` (then the lock is released and audited). In-progress (leased) deliverables keep their lease unless removed.
- Every state-changing portfolio operation (sync-created plan, revise, select, archive, forced removal) emits an audit event `plan.portfolio.<op>`.
- New tools: constant + `PLAN_TOOL_NAMES` + schema + `instructions()` + README + server roundtrip tests; tool-count text updated. Arg structs `deny_unknown_fields`.
- Schema migration v3 runs inside the existing single `BEGIN IMMEDIATE` migration transaction; `user_version` becomes 3; newer-schema rejection still works.
- CRLF files keep CRLF (`src/plan.rs`, `src/planner.rs`, `src/ports.rs`, `src/locks.rs`, `src/task.rs`, `tests/algorithm.rs`); new files LF.
- `cargo fmt --all --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings` green.

## Review Focus

1. **Path escape attempts** (`../x.json`, absolute `/etc/passwd`, a symlink inside `plans/` pointing outside) are rejected (Task 1).
2. **Revising a plan while a deliverable is leased** keeps the lease and the lease holder can still complete it (Task 3).
3. **Acquiring on a draft variant** fails with `VARIANT_NOT_SELECTED`; after `plan.select` it succeeds (Task 4).
4. **Switching the selected variant** carries Complete statuses of deliverables that exist in both (Task 4).
5. **Comparing variants with and without three-point estimates** — Pareto/rank still well-defined (P80 = deterministic makespan when no estimates) (Task 6).

---

### Task 1: Project root and path safety (`src/project.rs`)

**Files:** Create `src/project.rs`; Modify `src/lib.rs`, `src/plan.rs` (`PlannerError::InvalidPath { reason }` → `INVALID_PATH: {reason}`). Test: `tests/project.rs`.

**Interfaces:**

```rust
pub struct ProjectRoot { root: PathBuf } // canonical
impl ProjectRoot {
    pub fn discover(cwd: &Path) -> Option<ProjectRoot>;          // CPM_PROJECT_ROOT, else walk up for .cpm-planner/ or .git
    pub fn from_path(root: &Path) -> Result<ProjectRoot, PlannerError>;
    pub fn root(&self) -> &Path;
    pub fn project_key(&self) -> String;                          // canonical root as UTF-8 string
    pub fn plans_dir(&self) -> PathBuf;                           // <root>/.cpm-planner/plans
    pub fn resolve_plan_file(&self, rel: &str) -> Result<PlanFileRef, PlannerError>; // validates + canonicalizes
    pub fn plan_file(&self, name: &str, variant: &str) -> Result<PlanFileRef, PlannerError>;
    pub fn list_plan_files(&self) -> Result<Vec<PlanFileRef>, PlannerError>; // sorted (name, variant)
    pub fn read_graph(&self, f: &PlanFileRef) -> Result<(PlanGraph, String /*content sha256 hex*/), PlannerError>;
    pub fn write_graph(&self, f: &PlanFileRef, g: &PlanGraph) -> Result<String, PlannerError>; // atomic: temp + rename; creates dirs; returns hash
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFileRef { pub name: String, pub variant: String, pub rel_path: String /* ".cpm-planner/plans/<name>/<variant>.json" */ }
pub fn validate_slug(kind: &str, s: &str) -> Result<(), PlannerError>;
```
  Content hash = sha256 of the file bytes (drift detection). `write_graph` writes pretty JSON with a trailing newline.

- [ ] Tests (one assertion each, tempdirs): `discover_finds_git_root_from_nested_dir`, `discover_prefers_cpm_planner_dir`, `env_override_wins` (set `CPM_PROJECT_ROOT` — serialize env tests with a mutex), `resolve_rejects_parent_traversal`, `resolve_rejects_absolute_path`, `resolve_rejects_symlink_escaping_root` (unix only, `#[cfg(unix)]`), `resolve_rejects_path_outside_plans_dir`, `resolve_parses_name_and_variant`, `invalid_slug_is_rejected`, `write_then_read_round_trips_graph`, `write_is_atomic_and_creates_directories`, `list_plan_files_is_sorted`.
- [ ] Commit: `feat(project): project root discovery and confined plan-file paths`

### Task 2: Schema v3 portfolio store + `sync`, `list`, `export`, named `submit`

**Files:** Create `src/portfolio.rs` (store-facing portfolio logic); Modify `src/plan_store.rs` (migration v3; portfolio queries in one transaction with plan rows), `src/plan.rs` (types below), `src/ports.rs` + `src/planner.rs` (trait methods), `src/planner.rs` (`submit_plan` named path). Test: `tests/portfolio.rs`, `tests/persistence.rs`.

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariantSummary {
    pub variant: String, pub plan_id: PlanId, pub selected: bool, pub archived: bool,
    pub head_revision: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub source_path: Option<String>,
    pub complete: usize, pub total: usize, pub plan_complete: bool,
    pub makespan: f32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanLineSummary { pub project: String, pub name: String, pub selected_variant: Option<String>, pub archived: bool, pub variants: Vec<VariantSummary> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncOutcome { pub plan_id: PlanId, pub name: String, pub variant: String, pub revision: u32, pub created: bool, pub changed: bool, #[serde(default)] pub diff: Option<crate::revise::RevisionDiff> }
// Planner trait additions:
async fn sync_plan(&self, req: SyncRequest) -> Result<SyncOutcome, PlannerError>;   // SyncRequest { project, name, variant, graph, source_path: Option<String>, content_hash: Option<String>, force: bool }
async fn list_plans(&self, project: &str, include_archived: bool) -> Result<Vec<PlanLineSummary>, PlannerError>;
async fn revision_graph(&self, plan_id: &PlanId, revision: Option<u32>) -> Result<(u32, PlanGraph), PlannerError>;
```
  `sync_plan`: validate graph; if (project, name, variant) unknown → create plan (same as submit) + variants row + revision 1; first variant of a plan line becomes selected; audit `plan.portfolio.created`. If known and content hash (or graph hash when inline) unchanged → `changed: false`, no new revision. If changed → delegate to Task 3's revise (Task 2 ships with revise returning `Err(BackendError("revise not implemented"))` behind a single call site, replaced in Task 3; its test is `#[ignore]` until Task 3 — OR implement Task 2 after Task 3; the controller runs Task 3 first if preferred — see Execution order).
  `plan.submit` gains optional `project`, `name`, `variant` (default `main`) → routes to `sync_plan` with inline graph and no source path.
  Migration v3 tables: `plan_lines(project TEXT, name TEXT, selected_variant TEXT, archived INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(project,name))`; `variants(project TEXT, name TEXT, variant TEXT, plan_id TEXT NOT NULL UNIQUE REFERENCES plans(plan_id) ON DELETE CASCADE, source_path TEXT, content_hash TEXT, head_revision INTEGER NOT NULL, archived INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(project,name,variant))`; `revisions(plan_id TEXT REFERENCES plans(plan_id) ON DELETE CASCADE, revision INTEGER, graph TEXT NOT NULL, content_hash TEXT, created_at_us INTEGER NOT NULL, PRIMARY KEY(plan_id, revision))`.

- [ ] Tests: `sync_creates_plan_and_first_variant_is_selected`, `sync_same_content_is_unchanged`, `second_variant_is_a_draft`, `list_reports_variants_with_selected_flag`, `list_hides_archived_by_default`, `named_submit_dedups_within_variant_only` (same graph under two names → two plan_ids), `unnamed_submit_keeps_global_dedup`, `migration_v3_from_v2_database` (persistence: v2 DB opens, user_version 3, legacy plans intact), `revision_graph_returns_head`.
- [ ] Commit: `feat(portfolio): schema v3 plan lines, variants and revisions; plan sync and list`

### Task 3: Revise with carry-over (`src/revise.rs`) (#23b)

**Files:** Create `src/revise.rs`; Modify `src/planner.rs` (`revise_plan` trait impl; wire into `sync_plan`), `src/plan_store.rs` (graph update + new revision + status/lock/claim rebuild in one transaction), `src/ports.rs`, `src/plan.rs`. Test: `tests/revise.rs`.

**Interfaces:**

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RevisionDiff { pub added: Vec<String>, pub removed: Vec<String>, pub changed: Vec<String>, pub reopened: Vec<String>, pub released_locks: Vec<String> }
pub(crate) fn plan_revision(old: &PlanState, new: &PlanGraph, force: bool) -> Result<(PlanState /*new runtime state*/, RevisionDiff), PlannerError>; // pure
// trait: async fn revise_plan(&self, req: ReviseRequest) -> Result<(u32, RevisionDiff), PlannerError>; // ReviseRequest { plan_id, graph, force }
```
  Implements the carry-over rules in Global Constraints exactly; canonical deliverable definition = the `hash_graph` per-deliverable normalisation (reuse it; extract `canonical_deliverable(d) -> serde_json::Value`). Recompute CPM for the new graph. Rebuild the file-claim index from surviving locks. Audit `plan.portfolio.revised` with the diff; forced lock releases also emit the normal released event.

- [ ] Tests (one assertion each): `unchanged_deliverables_keep_status`, `added_deliverable_starts_by_prerequisites`, `removed_deliverable_is_dropped`, `removing_leased_deliverable_requires_force`, `forced_removal_releases_lock`, `changed_prerequisites_reopen_complete_deliverable`, `reopen_propagates_to_dependents`, `interface_consumer_reopens_when_contract_changes`, `metadata_only_change_keeps_status`, `leased_deliverable_survives_revision_and_can_complete`, `revision_number_increments`, `revise_recomputes_critical_path`.
- [ ] Commit: `feat(revise): revise a plan in place with progress carry-over (#23)`

### Task 4: Select, archive, and execution gating

**Files:** Modify `src/planner.rs` (`select_variant`, `archive`, gating helper used by acquire/heartbeat/mark_status/accept), `src/plan_store.rs`, `src/ports.rs`, `src/plan.rs` (`PlannerError::VariantNotSelected { plan_id, name, variant, selected }`). Test: `tests/portfolio.rs`.

**Interfaces:** `async fn select_variant(&self, plan_id: &PlanId, force: bool) -> Result<RevisionDiff, PlannerError>` — makes this variant selected; carries progress from the previously selected variant: for every deliverable id present in both with identical canonical definition, copy Complete status (and counters); refuse if the previously selected variant holds live locks unless `force` (then release them, audited). `async fn archive(&self, project: &str, name: &str, variant: Option<&str>) -> Result<(), PlannerError>` (archiving the selected variant is refused unless it's the whole line). Gating: execution methods look up `variants` by plan_id; if a row exists and it is not the line's selected variant → `VariantNotSelected`.

- [ ] Tests: `acquire_on_draft_variant_is_variant_not_selected`, `acquire_after_select_succeeds`, `select_carries_complete_status_for_identical_deliverables`, `select_does_not_carry_status_for_changed_deliverable`, `select_refuses_when_old_variant_holds_locks`, `select_with_force_releases_old_locks`, `unnamed_plans_are_never_gated`, `archive_hides_variant_from_list`, `archiving_selected_variant_alone_is_refused`.
- [ ] Commit: `feat(portfolio): select one executable variant; archive; VARIANT_NOT_SELECTED gating`

### Task 5: Fork with structured edits (`plan.fork`)

**Files:** Create `src/edits.rs`; Modify `src/planner.rs` (fork = read head graph → apply edits → write variant file if a project root is available → sync as draft), `src/plan.rs`. Test: `tests/edits.rs`, `tests/portfolio.rs`.

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum GraphEdit {
    RemoveEdge { from: String, to: String },               // remove prerequisite `from` from deliverable `to`
    AddEdge { from: String, to: String, #[serde(default)] consumes: Option<String> },
    SetEffort { id: String, hours: f32 },
    SetDuration { id: String, hours: Option<f32> },
    SetEstimate { id: String, estimate: Option<crate::plan::Estimate> },
    SetMetadata { id: String, key: String, value: serde_json::Value },
    RemoveDeliverable { id: String },
    AddDeliverable { deliverable: crate::plan::Deliverable },
}
pub fn apply_edits(graph: &PlanGraph, edits: &[GraphEdit]) -> Result<PlanGraph, PlannerError>; // pure; validates result; unknown ids → INVALID_GRAPH naming the edit index
```
  `fork { plan_id, variant, edits? }` → `SyncOutcome` for the new draft variant (same plan name). Fails if the variant exists.

- [ ] Tests: one per edit op (applies), `edit_with_unknown_id_names_edit_index`, `edits_producing_cycle_are_rejected`, `fork_creates_draft_variant_file`, `fork_without_project_root_registers_inline_variant`, `fork_existing_variant_is_rejected`.
- [ ] Commit: `feat(fork): fork a variant with structured graph edits`

### Task 6: Compare variants (`plan.compare`)

**Files:** Create `src/compare.rs`; Modify `src/planner.rs` (gather head graphs), `src/plan.rs`. Test: `tests/compare.rs`.

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CompareWeights { #[serde(default = "w1")] pub makespan: f32, #[serde(default = "w1")] pub p80: f32, #[serde(default = "w1")] pub criticality_risk: f32, #[serde(default = "w05")] pub total_effort: f32, #[serde(default = "w05")] pub peak_load: f32 }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariantComparison { pub plan_id: PlanId, pub variant: String, pub scorecard: crate::metrics::Scorecard, pub diff_vs_first: crate::revise::RevisionDiff, pub pareto_optimal: bool, pub score: f32, pub rank: u32, pub rationale: String }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison { pub variants: Vec<VariantComparison>, pub recommended: PlanId }
pub fn compare(inputs: &[(PlanId, String, PlanGraph)], req: &CompareRequest) -> Result<Comparison, PlannerError>; // CompareRequest { schedule: Option<ScheduleRequest>, monte_carlo: Option<MonteCarloRequest>, weights: CompareWeights }
```
  Criteria (all minimised): makespan (resource makespan if capacities given), P80 (Monte Carlo when requested, else deterministic makespan), criticality_risk, total_effort, peak_load (0 when no capacities). Pareto: not dominated on all five. Score = Σ weight × (value / min value across variants) (lower better; a min of 0 → that criterion contributes 0 for all); rank by score then plan_id. `rationale` names the variant's best and worst criteria relative to the others. Inputs: `plan_ids` (any variants, ≥ 2) or `plan` name (all non-archived variants of that line).

- [ ] Tests: `dominated_variant_is_not_pareto_optimal`, `rank_orders_by_weighted_score`, `weights_change_ranking`, `zero_min_criterion_contributes_nothing`, `p80_falls_back_to_deterministic_without_monte_carlo`, `diff_vs_first_lists_structural_changes`, `compare_requires_two_variants`, `compare_is_deterministic`.
- [ ] Commit: `feat(compare): compare variants on the scorecard with Pareto front and weighted rank`

### Task 7: Wire portfolio tools, path inputs, drift; docs

**Files:** `src/server.rs`, `src/bin/server.rs` (discover `ProjectRoot` at startup; pass into the server), `src/planner.rs` (`status()` gains `definition_drift: Option<bool>`, `name`, `variant`, `selected` — appended, `serde(default)`), README, CHANGELOG, `tests/server_integration.rs`.
- [ ] Tools: `plan.sync { path? , graph?, project?, name?, variant?, force? }` (path → read file; inline graph requires name), `plan.list { project?, include_archived? }`, `plan.export { plan_id, path? }` (writes the head graph to its variant file or the given confined path), `plan.revise { plan_id, graph, force? }`, `plan.fork { plan_id, variant, edits? }`, `plan.select { plan_id, force? }`, `plan.archive { name, variant?, project? }`, `plan.compare { plan_ids? | plan?, schedule?, monte_carlo?, weights? }`; `plan.lint` / `plan.simulate` accept `path` as a third input form; `plan.submit` gains `project`/`name`/`variant`. Project default = discovered root's key; tools needing a root without one → `INVALID_PATH: no project root (set CPM_PROJECT_ROOT or run inside a repo)`.
- [ ] Drift: `plan.status` sets `definition_drift` = `Some(file hash != synced hash)` when the variant has a source path and the root is readable; `None` otherwise.
- [ ] Server tests: one roundtrip per new tool + `plan_sync_rejects_path_escape` + `plan_status_reports_definition_drift` + `execution_on_draft_variant_returns_variant_not_selected`.
- [ ] Docs: `instructions()` — plan-as-code workflow paragraph (author file → `plan.lint {path}` → `plan.sync {path}` → execute; variants → `plan.fork` / `plan.compare` / `plan.select`; never keep untracked scratch graphs), README sections + env var `CPM_PROJECT_ROOT`, CHANGELOG Added/Changed lines for every tool and the v3 schema.
- [ ] Commit: `feat(server): plan-as-code portfolio tools (sync, list, export, revise, fork, select, archive, compare) + drift`

## Execution order

Task 1 ∥ Task 5's pure `apply_edits` part ∥ Task 6's pure `compare` part (worktrees) → Task 3's pure `plan_revision` (worktree, parallel with Task 2) → Task 2 (store) → integrate Task 3's store wiring → Task 4 → Task 5/6 planner wiring → Task 7 (Junior: mechanical tool wiring + docs).
