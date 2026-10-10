# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `plan.sync` registers or updates one variant of a named plan line from a plan file (`.cpm-planner/plans/<name>/<variant>.json`) or an inline graph; files are tracked by content hash for drift.
- `plan.list` lists a project's plan lines and variants (archived hidden unless `include_archived`).
- `plan.export` writes a plan's head graph to its variant file or a confined path.
- `plan.revise` replaces a plan's graph in place with progress carry-over and returns the new revision and diff.
- `plan.fork` copies a named variant, applies structured edits, and registers a new draft variant.
- `plan.select` makes exactly one variant of a line the executable one, carrying progress over.
- `plan.archive` archives or unarchives a whole line or one variant; archived variants stay readable but refuse sync, selection and execution (`ARCHIVE_REFUSED`).
- `plan.compare` scores plans (by id or per line variant) on the scorecard with a Pareto front, weighted rank and recommended plan. Weights must be finite and `>= 0`.
- Portfolio SQLite schema v3 (`plan_lines`, `variants`, `revisions`); named plans dedup within `(project, name, variant)` and keep a stable `plan_id` across revisions.
- Plan-as-code files under `.cpm-planner/plans/<name>/<variant>.json`, resolved from `CPM_PROJECT_ROOT` or the nearest ancestor containing `.cpm-planner/` or `.git`; path arguments are confined and rejected with `INVALID_PATH`.
- `plan.lint` and `plan.simulate` accept a plan-file `path` as a third input alongside `graph` and `plan_id`; `plan.status` reports `definition_drift` when a tracked file no longer matches its synced hash.
- Stable `VARIANT_NOT_SELECTED`, `ARCHIVE_REFUSED` and `INVALID_PATH` error prefixes.
- `plan.compare` limits: at most 16 variants (`INVALID_GRAPH: compare accepts at most 16 variants`; schema `maxItems: 16`), distinct `plan_ids` (`plan_ids must be distinct`), and one Monte Carlo work budget shared across all variants; `plan.compare` takes an optional `project` for `plan`.
- `plan.export` `force: true` overwrites another variant's tracked file or unsynced local edits, which are otherwise refused (`INVALID_PATH`).
- Plan files over 8 MiB are refused (`INVALID_PATH: plan file exceeds 8 MiB`).
- `plan.lint` reports cycles (with the loop), redundant edges, edges without rationale, interface edges not targeting a contract, deliverables feeding no milestone, and unordered file overlaps — without creating a plan (#20).
- `plan.schedule` levels a plan against resource capacities (`metadata.owner` by default): makespan, per-deliverable start/finish, per-resource load, the driving chain (dependency vs resource waits), project and feeding buffers (#19).
- Plan scorecard (makespan, criticality risk and band, DRAG, diameter, cyclomatic complexity, merge bias, parallelism, peak load, lint counts) returned by `plan.simulate`.
- Optional three-point `estimate {optimistic, likely, pessimistic}` per deliverable and seeded Monte Carlo schedule risk (P50/P80/P95 makespan, criticality index, sensitivity).
- `plan.simulate` computes critical path, schedule, milestones, optional resource schedule and Monte Carlo, and the scorecard for a graph or stored plan without persisting anything (#23).
- Prebuilt, checksum-verified x64/ARM64 packages and installers; per-client MCP install instructions (Claude Code, Claude Desktop, Cursor, VS Code, Codex, Docker).
- `plan.acquire_cohort` accepts `ids` and `filter.metadata`; deliverables with `metadata.kind = "manual"` are never leased; requested ids that cannot be leased are reported in `blocked` with a code (#14).
- `plan.accept` for manager/owner acceptance without a lease (#24).
- `blocked` (codes MANUAL, NOT_READY, LOCKED, LAPSE_LIMIT, FILE_CONFLICT, MAX_COUNT), `blocked_count` and `needs_operator` on `plan.acquire_cohort` responses.
- `PREREQUISITES_INCOMPLETE` returned by `plan.accept` and by lockless `plan.mark_status` completion.
- `plan.force_release` `reset_counters: true` clears lapse/failure counters and revives circuit-broken deliverables (#17).
- `plan.status` returns `critical_ids` (every zero-float deliverable),
  `schedule` (per-deliverable es/ef/ls/lf/float/critical, in hours) and
  `ready` (ready, unlocked deliverables in cohort priority order) (#15).
- `plan.get` returns the submitted plan graph for a `plan_id`.
- Versioned SQLite schema (`PRAGMA user_version` = 2) with a `cpm_version`
  column; databases newer than the running binary are rejected.
- `ttl_seconds` on `plan.acquire_cohort` and `plan.heartbeat`, clamped to `CPM_MAX_TTL_SECS` (default 8h) (#13).
- Synthetic `__start__`/`__finish__` endpoints in every plan's CPM: `critical_path` always runs start → finish; `schedule` rows carry `synthetic`; `plan.status` reports `plan_complete`; `plan.completed` audit event. `__start__`/`__finish__` are reserved ids.
- Prerequisites may be objects `{id, consumes?, kind?: artifact|interface, lag_hours?}` (#21).
- `duration_hours` (calendar time) per deliverable and `lag_hours` per prerequisite edge drive the schedule; effort stays the cost basis (#27).
- `milestone: true` deliverables; `plan.status` reports per-milestone critical path and hours (#22).
- `owned_files` entries may be `{path, mode: "append"}`; append claims may be co-leased and are reported in the cohort's `shared_paths` (#28).
- `plan.review` (optional, read-only): lint plus one batched call to Jev (`typesafe/jev-1.13`) on OpenRouter about likely false and missing dependencies, split and interface-split candidates and crash options, returning advisory findings and `plan.fork`-ready proposals each verified by simulate and ranked by `hours_saved / max(cost, 1)`. At most 64 questions (`max_questions`, default 64; out of range is `invalid_params`); lint errors return `invalid_graph` with no call; no key, an unusable LLM setting or a provider failure returns `review_unavailable` with a reason (and `failure_class` for a provider failure). Every call except one rejected for its params records a `plan.review` audit event (`status`, `code`, `failure_class`, `plan_id`, `question_count`, `jev_called`, `prompt_hash`, `model`, `endpoint`; never the key or prompt), including failed reviews (`status: "error"` with the error code, e.g. `PLAN_NOT_FOUND`, `INVALID_CAPACITIES`). At most 2 reviews that call the judge run at once (`MAX_CONCURRENT_REVIEWS`); further ones wait. The plan graph is sent to OpenRouter (#26).
- LLM settings `OPENROUTER_API_KEY`, `CPM_OPENROUTER_KEY_FILE` (ignored when world-readable or empty), `CPM_JEV_MODEL` (at most 128 chars of `[A-Za-z0-9._:/-]`, never containing the key), `CPM_JEV_ENDPOINT`, `CPM_LLM_TIMEOUT_SECS` and `CPM_LLM_MODEL`, read once at startup; an invalid one is logged once (naming the variable, never its value or the key-file path) and disables only `plan.review`. The key is redacted from `Debug`, errors, logs, audit records and tool output (#26).
- Dependencies: `rig-core` and `rig-typesafeai` (with `reqwest`), built with rustls only (no native TLS / OpenSSL), plus `url`: +78 crates in the normal dependency graph (113 → 191 unique, `cargo tree --locked -e normal`). `wiremock` for tests only (#26).
- Library API (#26): the `llm` module (`LlmConfig`, `ConfigError`, `JudgmentError` / `JudgmentErrorKind`, the `JudgmentModel` trait, `ApiKey`, `llm::jev::JevJudge`, `llm::openrouter::chat_client`); the `review` module (`review`, `ReviewRequest`, `ReviewReport`, `Judge`, findings, proposals and their constants); `PlanServer::with_judge` / `with_llm_config`; `BasicCpmPlanner::record_audit`; `TOOL_REVIEW` and `server::MAX_CONCURRENT_REVIEWS`.

### Fixed

- `JevJudge` owns its HTTP client instead of rig's process-wide one, whose keep-alive connections could be handed to a judge on another tokio runtime and then stall until the timeout or fail at once as a `transport` error ("runtime dropped the dispatch task") (#37).
- `plan.submit` accepts a file owned by deliverables ordered by prerequisites; only unordered exclusive overlaps are rejected (#12).
- One lapse-limited deliverable no longer fails `plan.acquire_cohort` for the whole plan; it is reported in the new `blocked` list (#17).
- `plan.status` `critical_path` is now one real prerequisite chain (each id is a
  prerequisite of the next) and `critical_path_hours` is the project length
  (maximum earliest finish) — previously every zero-float task was chained and
  their efforts summed (#18).
- `optimal_duration_parallel` is the unconstrained makespan (maximum earliest
  finish); `speedup_factor` changes accordingly.
- Duplicate prerequisite ids no longer leave a deliverable unscheduled.
- Plans stored by older versions are recomputed automatically when the store
  opens.
- `CpmAlgorithm::calculate` resets earliest start/finish on re-run (stale values when called twice on the same tasks).
- `plan.schedule`, `plan.simulate` and the `resource_schedule` / `monte_carlo` library calls validate the graph exactly as `plan.submit` does, so a reserved or duplicate id or a cycle is `INVALID_GRAPH` instead of a hang or a wrong schedule; `CpmAlgorithm::calculate` now terminates on duplicate task ids (every task is reported unscheduled).
- `plan.lint` reports an invalid three-point estimate as `INVALID_VALUE`, with the same message as `plan.submit`, and no longer reports `NO_ARTIFACT` for milestones.
- Monte Carlo samples an estimate only when neither `duration_hours` nor `estimated_effort_hours` is set, matching the scheduled-length precedence.
- `plan.simulate` rejects unknown fields inside `schedule` and `monte_carlo`.
- Critical-path bottleneck `blocked_hours` is now summed deterministically (previously order-dependent float noise).

### Changed

- The lease reaper and the startup quarantine re-derive a released deliverable's status with the single rule used by revision (`Ready` when every prerequisite is `Complete`, else `Pending`).
- `definition_drift` compares the tracked file's graph with the head graph (re-formatting is not drift; an inline revise is drift until re-export or re-sync); an identical inline sync no longer resets the tracked file hash.
- `plan.sync {path}` rejects a `name`, `variant` or `project` that contradicts the path; project keys reject invisible Unicode format characters (bidi overrides, zero-width, BOM, separators).
- `plan.fork` refuses an archived line before writing the plan file and removes the file if registration fails; on filesystems without hard links the create-only write falls back to an exclusive (non-atomic) create.
- Undoing a carried `Complete` on select restores the deliverable's previous status (a `Failed` keeps its reason).
- Errors from the final validation of `plan.fork` edits start with `after applying <n> edits: `.
- `plan.submit` accepts optional `project`/`name`/`variant` (variant defaults to `main`); when `name` is given it registers a named variant (via `sync_plan`) instead of an unnamed plan.
- A plan may have at most 5000 deliverables (`INVALID_GRAPH: plan has <n> deliverables; maximum is 5000`); `plan.lint` reports a larger graph as one `TOO_MANY_DELIVERABLES` error.
- Every hour value (effort, duration, lag and estimate points) must be finite and between 0 and 1000000 (`... must be a finite number between 0 and 1000000`).
- Monte Carlo rejects runs where `iterations × (deliverables + prerequisite edges)` exceeds 200000000 (`INVALID_GRAPH: monte carlo budget exceeded ...`).
- `plan.schedule` / `plan.simulate` reject an out-of-range `project_buffer_pct` or `iterations` as invalid params before doing any work.
- The CPM kernel, lint, leveling and Monte Carlo no longer do quadratic string-set work or recursion: a 5000-deliverable chain is handled in well under a second.
- Library: `cpm_planner::schedule::compute_cpm` is public.
- The scorecard's merge bias and cyclomatic complexity count distinct prerequisite ids; an empty plan scores criticality risk 0.
- `plan.acquire_cohort` and `ready` order by longest remaining tail (smallest latest start), then float, then id (#19).
- A lockless `plan.mark_status` to `ready` or `in_progress` now requires the deliverable's prerequisites to be complete (`PREREQUISITES_INCOMPLETE`), so dependency order can't be bypassed.
- Plan identity hashes changed (prerequisites, owned_files, duration_hours and milestone are normalised into the hash): re-submitting any graph stored by an earlier version creates a new plan.
- `critical_path` now includes the synthetic endpoints.
- Completing a deliverable without a lease now requires its prerequisites to be complete and is audited.
- `plan.acquire_cohort` no longer returns the `LAPSE_LIMIT` error; lapse-limited deliverables appear in `blocked[]` with code `LAPSE_LIMIT` and the response sets `needs_operator: true` (drivers matching on the error must read `blocked`).
- Library: `Deliverable.prerequisites` is `Vec<Prerequisite>`.
- Library: `Deliverable` gains public `duration_hours` and `milestone`; `Task` gains `lag_by_dependency` and its `effort_hours` means scheduled length; `PlanStatus` gains `plan_complete` and `milestones`; `ScheduleRow` gains `synthetic`; new public types `Prerequisite`, `PrerequisiteKind`, `OwnedFile`, `FileMode`, `MilestoneRow` and consts `START_ID`, `FINISH_ID`.
- A milestone is zero-length unless you give it an estimate or `duration_hours`; it is still an ordinary deliverable someone must complete (or accept).
- Library: `Deliverable.owned_files` is `Vec<OwnedFile>`; `Cohort` gains `shared_paths`.
- Library: `Planner` gains required method `accept`; `Cohort` gains public field `blocked`; `PlannerError` gains `PrerequisitesIncomplete` (breaks exhaustive matches); new `DEFAULT_MAX_TTL` / `BasicCpmPlanner::with_max_ttl`.
- Library: `Planner` methods `acquire_cohort`, `mark_status`, `heartbeat`,
  `force_release` take request structs (`AcquireRequest`, `MarkStatusRequest`,
  `HeartbeatRequest`, `ForceReleaseRequest`).
- `plan.acquire_cohort` prefers the least-float ready deliverables (then
  earliest start, then id).
- `plan.submit` rejects negative `estimated_effort_hours` with `INVALID_GRAPH`.
- Library: the `Planner` trait gains the required method `get_plan`;
  `CriticalPathResult` gains the public field `critical_ids`.

## [0.0.1] - 2026-06-17

### Added

- Initial release.
- Critical Path Method (CPM) planner as a reusable kernel: earliest/latest
  start, slack, the critical path, and bottleneck tasks that gate completion.
- A stdio MCP server exposing the planner over the standard protocol.
- Lock-aware parallel execution coordination so multiple workers can run
  file-disjoint deliverables concurrently without colliding.
- Six-tool MCP surface: `plan.submit`, `plan.acquire_cohort`, `plan.heartbeat`,
  `plan.mark_status`, `plan.status`, and `plan.force_release`.

[0.0.1]: https://github.com/praxec/cpm-planner/releases/tag/v0.0.1
