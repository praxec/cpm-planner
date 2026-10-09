# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

### Fixed

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

### Changed

- `critical_path` now includes the synthetic endpoints.
- Completing a deliverable without a lease now requires its prerequisites to be complete and is audited.
- `plan.acquire_cohort` no longer returns the `LAPSE_LIMIT` error; lapse-limited deliverables appear in `blocked[]` with code `LAPSE_LIMIT` and the response sets `needs_operator: true` (drivers matching on the error must read `blocked`).
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
