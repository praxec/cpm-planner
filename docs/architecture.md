# Architecture

This page describes how cpm-planner is put together: the modules in `src/`, the
path a request takes through them, the SQLite schema, the concurrency model and
the security boundaries. It is derived from the module headers (`//!`) and the
migrations in `src/plan_store.rs`; when the two disagree, the code wins.

## Layers

```text
MCP client (stdio)
   |
src/bin/server.rs      startup: env, store, project root, LLM config
   |
src/server.rs          PlanServer: MCP tool surface, param checks, error mapping
   |
src/planner.rs         BasicCpmPlanner: the lock-aware Planner implementation
src/planner/ev.rs      earned-value operations
   |            \
   |             pure engines: algorithm, schedule, lint, simulate, resource_schedule,
   |             monte_carlo, compare, metrics, drag, risk, network_health,
   |             edits, revise, earned_value, review
   |
src/plan_store.rs      SqlitePlanStore: one connection, BEGIN IMMEDIATE transactions
src/portfolio.rs       plan lines, variants, revisions (schema v3)
src/ev_store.rs        baselines, actuals, snapshots (schema v4)
src/project.rs         confined plan-file I/O under the project root
src/llm/               OpenRouter / Jev judge for plan.review
```

The pure engines do no I/O and read no clocks: the planner and the server pass
them a graph, a state and a time.

## Module map

### Kernel

- `algorithm.rs`: the CPM forward and backward passes, float, critical path,
  batches by earliest start, bottleneck analysis. `CPM_VERSION` versions the
  cached results.
- `task.rs`: the kernel's internal model (`Task`, `TaskKind`, `TaskBatch`,
  `Bottleneck`, `CriticalPathResult`).
- `estimator.rs`: the default effort for a task kind, clamped to a range.
- `schedule.rs`: `compute_cpm`, which turns a validated `PlanGraph` into a
  `CriticalPathResult` with synthetic `__start__` and `__finish__` endpoints.
  Submit, status, the store's recompute and the analysis tools share it.
- `graph.rs`: crate-private helpers over the deliverable graph.

### Wire model and planner

- `plan.rs`: the wire and domain types that cross the `Planner` boundary
  (deliverables, cohorts, locks, requests, `PlannerError`). Every type is
  serde, because the server sends them as JSON.
- `ports.rs`: the `Planner` trait, the seam between a caller and a scheduling
  implementation.
- `planner.rs`: `BasicCpmPlanner`, the shipped implementation. It validates
  graphs, runs every mutating method inside one store transaction, and emits
  audit events after commit.
- `planner/ev.rs`: `plan.baseline`, `plan.ev` and `plan.snapshot`, gated to
  the selected, unarchived variant for writes.
- `locks.rs`: `PlanState`, the per-plan state the planner loads from the store
  for one operation: graph, statuses, the lock map, an inverse
  file-to-deliverable index for overlap checks, and the cached CPM result.
- `audit.rs`: `AuditEvent` and the `AuditSink` trait. The binary installs
  `NullAuditSink`; an embedder can supply its own.

### Persistence

- `plan_store.rs`: `SqlitePlanStore`, the single source of truth for planner
  state, its migrations and the startup quarantine of expired leases.
- `portfolio.rs`: named plan lines, their variants and every plan's revision
  history; drift detection via `variants.content_hash`.
- `ev_store.rs`: frozen baselines, per-deliverable actuals and EV snapshots.
  Every function runs inside a transaction its caller opened, so EV writes
  commit with the planner state they accompany.
- `project.rs`: project-root discovery and confined, no-follow plan-file reads
  and writes (see [Security boundaries](#security-boundaries)).

### Analysis (read-only)

- `lint.rs`: `plan.lint`, every submit-time rule reported as a finding, plus
  advisory checks.
- `simulate.rs`: `plan.simulate`, lint, CPM, optional levelling and Monte
  Carlo, and the scorecard for a graph that is never persisted.
- `resource_schedule.rs`: `plan.schedule`, event-driven list scheduling against
  per-resource capacities, the driving chain and buffers.
- `monte_carlo.rs`: seeded PERT sampling over three-point estimates.
- `compare.rs`: `plan.compare`, the Pareto front and weighted rank of variants.
- `metrics.rs`: the scorecard, combining `drag.rs` (Devaux DRAG, diameter),
  `risk.rs` (activity and criticality risk) and `network_health.rs`
  (cyclomatic complexity, efficiency) with the schedule and lint.
- `edits.rs`: structured `GraphEdit`s for `plan.fork`, re-validated after
  application.
- `revise.rs`: `plan_revision`, which carries progress from an old state to a
  revised graph and reports the diff and reopened deliverables.
- `earned_value.rs`: the pure PV, EV, AC, SPI, CPI and EAC engine. Undefined
  ratios become `None` plus an explanation, never NaN or infinity.

### Optional AI review

- `review.rs`: `plan.review`. It runs lint, makes one batched judgment call,
  then verifies each proposal mechanically by simulating it.
- `llm/mod.rs`: `LlmConfig::from_env` and the `JudgmentModel` seam.
- `llm/jev.rs`: `JevJudge`, Jev via `rig-typesafeai` pointed at OpenRouter.
- `llm/openrouter.rs`: a rig-core chat client kept for future use; no tool calls
  it.

### Surface

- `server.rs`: `PlanServer`, the MCP tools (`PLAN_TOOL_NAMES`), their JSON
  schemas, parameter range checks and the mapping from `PlannerError` to MCP
  errors.
- `bin/server.rs`: the `cpm-planner` binary. It opens the store
  (`CPM_PLANNER_DB`), resolves the lease TTL ceiling (`CPM_MAX_TTL_SECS`),
  discovers the project root (`CPM_PROJECT_ROOT` or the nearest ancestor with
  `.cpm-planner/` or `.git`), reads the LLM settings once, and serves MCP over
  stdio.

## Request flow

The usual lifecycle of a plan, by tool:

1. **Define.** `plan.lint` checks a graph, a stored plan or a plan file without
   writing anything. `plan.submit` registers an unnamed plan: the planner
   validates the graph, hashes it, and inside one immediate transaction either
   returns the existing plan for that hash (submit dedup) or builds the initial
   `PlanState` (CPM result cached, deliverables without prerequisites `ready`)
   and inserts it. `plan.sync` does the same for one variant of a named plan
   line: `project.rs` reads the plan file through the confined root, and
   `portfolio.rs` creates or revises the variant and records the file's
   content hash.
2. **Schedule.** `plan.status` reads the plan and returns the schedule, the
   critical path, the ready set, the locks, milestones and, for a variant,
   `definition_drift`. `plan.schedule`, `plan.simulate` and `plan.compare`
   are read-only analysis run off the async runtime.
3. **Improve.** `plan.fork` applies structured edits to a variant's head graph
   and registers a draft; `plan.compare` scores variants; `plan.select` makes
   one the executable variant, carrying progress over; `plan.revise` replaces
   a graph in place through `revise.rs`.
4. **Lease.** `plan.acquire_cohort` loads `PlanState` in a `BEGIN IMMEDIATE`
   transaction, reaps expired locks, picks up to N ready deliverables whose
   `owned_files` claims are disjoint from each other and from held locks,
   inserts the locks, flips them to `in_progress` and commits. Drafts and
   archived variants are refused. `plan.heartbeat` extends a held lock's TTL,
   clamped to the server maximum.
5. **Mark.** `plan.mark_status` completes or fails a deliverable and releases
   its lock; dependents whose prerequisites are all complete become `ready`.
   The lease's hours and any reported `earned_pct`, `actual_effort_hours` and
   `evidence` are written to `ev_actuals` in the same transaction.
   `plan.accept` and `plan.force_release` are the owner and operator escape
   hatches.
6. **Earned value.** `plan.baseline` freezes the current schedule and budgets
   as a numbered baseline. `plan.ev` computes PV, EV, AC and the derived
   indices against the latest baseline; `plan.snapshot` appends that reading
   and exports the history. Both run on the blocking pool.
7. **Review.** `plan.review` lints the plan, and if it is clean and a key is
   configured, asks Jev one batch of questions and returns findings plus
   proposals that each simulate to a shorter makespan. It never writes.

Audit events are buffered during the transaction and drained to the
`AuditSink` after commit, so a slow sink never holds the write lock.

## Store schema

The database is SQLite with `journal_mode=WAL`, `busy_timeout=5000`,
`synchronous=NORMAL` and `foreign_keys=ON`. `PRAGMA user_version` records the
last migration applied. The ladder runs in one immediate transaction on every
open, so concurrent openers serialise. A database newer than the binary is
refused with "upgrade cpm-planner".

| Version | Migration | Adds |
|---|---|---|
| v1 | `migrate_v1_base_schema` | `plans` (graph, cached CPM result), `deliverable_statuses` (status and attempt, failure and lapse counters), `locks` (holder, acquired and expiry times), `submit_dedup` (graph hash to plan id) |
| v2 | `migrate_v2_cpm_version` | `plans.cpm_version`; plans cached by an older kernel are recomputed on open |
| v3 | `migrate_v3_portfolio` | `plan_lines` (selected variant, archived), `variants` (plan id, source path, content hash, head revision, archived), `revisions` (graph per revision) |
| v4 | `migrate_v4_earned_value` | `baselines`, `ev_actuals`, `ev_snapshots` and two snapshot position indexes |

After the ladder, `ensure_v4_columns` runs on every open and adds any v4
columns an unreleased v4 build lacked (`deliverable_statuses.lockless`,
`ev_actuals.removed_at_us`, `ev_actuals.frozen_pct`,
`ev_snapshots.baseline_number`). Then `recompute_stale_results` refreshes
cached CPM results whose `cpm_version` is behind `CPM_VERSION`.

On open, the store also quarantines expired leases: a lock past its TTL is
deleted, its deliverable returns to `ready`, and the lease's hours up to expiry
are added to `ev_actuals.leased_hours`. An `in_progress` row from a lease with
no lock row is reset too. A lockless `in_progress` (from `plan.mark_status`
without a lease) and locks still within TTL are left alone.

## Concurrency

- **One connection per process.** `SqlitePlanStore` holds a single
  `rusqlite::Connection` behind a `std::sync::Mutex`, which serialises callers
  within the process. The mutex is never held across an `.await`.
- **`BEGIN IMMEDIATE` for every write.** Each mutating planner method runs its
  whole read-modify-write (load `PlanState`, apply the logic, write back) in
  one immediate transaction, which takes the database write lock at `BEGIN`.
  The lock is database-wide, so two processes pointed at the same file (two
  MCP servers, or a server and another tool) can never both lease the same
  deliverable or overlapping files. WAL keeps readers cheap, and
  `busy_timeout` makes writers queue instead of failing.
- **Blocking work off the runtime.** CPU-bound tools (`plan.lint`,
  `plan.schedule`, `plan.simulate`, `plan.compare`) run through `run_blocking`,
  and `plan.ev` and `plan.snapshot` through `run_blocking_planner`; both use
  `tokio::task::spawn_blocking`, so a large plan never stalls the runtime's
  worker threads. A panic there becomes a generic "task failed" error; the
  detail goes to the log only.
- **Bounded reviews.** At most `MAX_CONCURRENT_REVIEWS` (2) `plan.review` calls
  that reach the judge run at once per server; later calls wait on a
  semaphore. Each judge call is bounded by `CPM_LLM_TIMEOUT_SECS`.

## Security boundaries

- **Project-root confinement.** Plan files live at
  `<root>/.cpm-planner/plans/<name>/<variant>.json`. Caller paths are checked
  component by component: no `..`, no absolute paths, no backslashes, no extra
  components, slug-checked name and variant. All access walks from a handle on
  the canonical root using `cap-std` directories opened with no-follow
  semantics, and every later operation uses the opened directory handle, so a
  symlink swapped in after the check cannot redirect I/O outside the root.
  Writes go to a temp file that is fsynced and renamed (or linked) into place.
  Reads refuse non-regular files and files over 8 MiB. The residual Windows
  limitations are listed in the header of `src/project.rs`.
- **LLM key handling.** The OpenRouter key comes from `OPENROUTER_API_KEY` or
  `CPM_OPENROUTER_KEY_FILE`. On unix, a world-readable or empty key file is
  ignored with a warning. The key lives in an `ApiKey` whose `Debug` is
  redacted and which has no `Display` or `Serialize`; judge error messages are
  scrubbed of it before they are built, so logs, errors, audit records and
  tool output never carry it. Invalid LLM settings are logged by name, never
  by value, and only disable `plan.review`.
- **Outbound network.** The only outbound connection is the optional
  `plan.review` HTTPS call to the Jev endpoint. `CPM_JEV_ENDPOINT` must be
  `https://`, or `http://` to a loopback host, with no credentials. What the
  request contains is listed in the README under
  [Plan review (optional)](../README.md#plan-review-optional).
- **Leases.** TTLs are clamped to `CPM_MAX_TTL_SECS` (at most 30 days), and an
  invalid value aborts startup rather than disabling the ceiling.

See [SECURITY.md](../SECURITY.md) for reporting vulnerabilities.
