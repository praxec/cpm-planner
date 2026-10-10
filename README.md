# cpm-planner

[![CI](https://github.com/praxec/cpm-planner/actions/workflows/ci.yml/badge.svg)](https://github.com/praxec/cpm-planner/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/cpm-planner.svg)](https://crates.io/crates/cpm-planner)
[![docs.rs](https://docs.rs/cpm-planner/badge.svg)](https://docs.rs/cpm-planner)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

cpm-planner is a Critical Path Method (CPM) planner exposed as an MCP server. You
submit a task graph; it computes the schedule — earliest/latest start and finish,
slack, the critical path, and the bottleneck tasks that actually gate completion —
and it coordinates **lock-aware cohort scheduling** so multiple workers can run
disjoint deliverables in parallel without colliding. Any MCP client (Claude Code,
Cursor, a custom orchestrator, or an [praxec](https://github.com/praxec/praxec)
workflow) drives it over the standard protocol.

It is a standalone tool: it has no dependency on praxec and is consumed
purely over MCP.

## Install

From crates.io:

```sh
cargo install cpm-planner
```

Or download a pre-built binary for your platform from the
[latest release](https://github.com/praxec/cpm-planner/releases/latest)
(verify against the release's `checksums.sha256`):

| Platform | Download |
|----------|----------|
| Linux x86_64 | [`.tar.gz`](https://github.com/praxec/cpm-planner/releases/latest/download/cpm-planner-x86_64-unknown-linux-gnu.tar.gz) |
| Linux ARM64 | [`.tar.gz`](https://github.com/praxec/cpm-planner/releases/latest/download/cpm-planner-aarch64-unknown-linux-gnu.tar.gz) |
| macOS x86_64 | [`.tar.gz`](https://github.com/praxec/cpm-planner/releases/latest/download/cpm-planner-x86_64-apple-darwin.tar.gz) |
| macOS Apple Silicon | [`.tar.gz`](https://github.com/praxec/cpm-planner/releases/latest/download/cpm-planner-aarch64-apple-darwin.tar.gz) |
| Windows x86_64 | [`.zip`](https://github.com/praxec/cpm-planner/releases/latest/download/cpm-planner-x86_64-pc-windows-msvc.zip) |

It speaks MCP over stdio (the standard transport). Wire it into your editor like
any other MCP server:

```jsonc
{ "command": "cpm-planner", "args": [] }
```

## MCP tools

| Tool | Does |
|------|------|
| `plan.submit` | Submit a task graph; returns a plan id (idempotent on the graph + caller). A prerequisite is a deliverable id string or an object `{id, consumes?, kind?: artifact\|interface, lag_hours?}`; both forms round-trip through `plan.get`. A deliverable may set `milestone: true` (an acceptance point, zero-length unless given an estimate or duration, reported in `plan.status` `milestones`). An optional `earning_rule` (`zero_hundred`, the default: 100% only when complete; `fifty_fifty`: 50% once in progress or any `earned_pct` is reported; `weighted`: the reported `earned_pct`) sets how earned value credits partial progress and is part of the plan's identity. A deliverable's optional `duration_hours` (calendar time) replaces effort as its scheduled length and a prerequisite's `lag_hours` delays the dependent's start; effort stays the cost basis. An optional `estimate` `{optimistic, likely, pessimistic}` (`0 <= optimistic <= likely <= pessimistic`) is a three-point effort estimate. Scheduled-length precedence: `duration_hours` > `estimated_effort_hours` > `estimate.likely` > 0 for a milestone > the estimator default; Monte Carlo samples the estimate only when neither `duration_hours` nor `estimated_effort_hours` is set. Limits: at most 5000 deliverables, and every hour value (effort, duration, lag, estimate) finite and between 0 and 1000000 (`INVALID_GRAPH`). With `name` (optional `project`/`variant`, variant defaults to `main`) it registers a named variant instead of an unnamed plan. |
| `plan.acquire_cohort` | Atomically acquire up to N ready deliverables with mutually disjoint file sets (an `owned_files` entry may be `{path, mode: "append"}`; append claims on one path may be co-leased and appear in `shared_paths`; a file may be owned by several deliverables only if they are ordered by prerequisites, or all claims are append); optional `ttl_seconds` sets the lease TTL (clamped to the server maximum); optional `ids` and `filter.metadata` target deliverables; `metadata.kind = "manual"` deliverables are never leased; the response lists unleased candidates in `blocked` (with `blocked_count`, `needs_operator`). |
| `plan.heartbeat` | Refresh the TTL on a held lock; optional `ttl_seconds` sets the new TTL (clamped to the server maximum). |
| `plan.mark_status` | Mark a deliverable complete/failed; releases its lock. Without a lock, Complete requires complete prerequisites (`PREREQUISITES_INCOMPLETE`) and every lockless mark is audited. Optional earned-value progress: `earned_pct` (integer 0 to 100, only with `in_progress`; accepted and ignored with `complete`), `actual_effort_hours` (total effort so far, finite, 0 to 1000000; replaces leased hours as actual cost) and `evidence` (at most 2048 characters, appended to the deliverable's evidence list, which keeps at most 100 entries); violations are `INVALID_ACTUALS`, including a negative, fractional or over-100 `earned_pct`. `plan.select` copies the actuals of every deliverable whose Complete status it carries (leased hours add up, the carried deliverable's reported percent and hours win, the newest 100 evidence entries are kept). Every lease that ends (complete, failed, force release, expiry, accept override, forced revise/select/archive) adds its hours to the deliverable's leased hours; a lapsed lease counts up to its expiry. |
| `plan.status` | Read-only snapshot of the plan and its locks. `critical_path` always runs `__start__` to `__finish__` (synthetic zero-effort endpoints; their `schedule` rows carry `synthetic: true`, and `__start__`/`__finish__` are reserved deliverable ids rejected with `INVALID_GRAPH`). `plan_complete` is true once every deliverable is Complete; the `plan.completed` audit event fires once when that happens. `milestones` has one row per deliverable with `milestone: true` (or legacy `metadata.milestone == true`): `id`, `critical_path` (longest chain from `__start__` to it), `hours` (its earliest finish) and `complete`. For a named variant it also reports `name`, `variant` and `selected`, and `definition_drift`: `null` (unknown) without a project root for the variant's project, without a tracked plan file, or when the file is missing, unreadable or over 8 MiB; otherwise `false` when the file's bytes match the last synced or exported hash or the file's graph equals the head graph (re-formatting is not drift), and `true` when the file's graph differs from the head (or does not parse). An inline `plan.revise`/`plan.sync` of a file-backed variant therefore reports `true` until the file is re-exported or re-synced. |
| `plan.get` | Return the stored plan graph for a plan_id (read back what was submitted). |
| `plan.force_release` | Operator escape hatch: release a lock regardless of holder/TTL; `reset_counters: true` also clears lapse/failure counters. |
| `plan.accept` | Manager/owner acceptance: complete a deliverable without holding its lease (audited, with evidence). |
| `plan.lint` | Static checks without submitting: cycles (with the loop), redundant edges, edges without rationale, interface edges not targeting a contract, deliverables feeding no milestone, and unordered file overlaps (#20). |
| `plan.schedule` | Level a graph against resource capacities (`metadata.owner` by default): makespan, per-deliverable start/finish, per-resource load, the driving chain (dependency vs resource waits), and project/feeding buffers (#19). `capacities` is required: every resource carrying work needs at least 1 unit, otherwise `INVALID_CAPACITIES:` lists the missing resources. `project_buffer_pct` is 0 to 100 (default 25). |
| `plan.simulate` | Read-only what-if for a graph, stored plan or plan file (persists nothing): lint, critical path, schedule, milestones, optional resource schedule (`schedule`, same inputs and `INVALID_CAPACITIES:` rule as `plan.schedule`) and Monte Carlo (`monte_carlo`: `iterations` 1 to 50000, default 2000; `seed`, default `0xC0FFEE`; `iterations × (deliverables + prerequisite edges)` must not exceed 200000000), and the scorecard (#23). |
| `plan.sync` | Register or update one variant of a named plan line from a plan file (`path`, of the form `.cpm-planner/plans/<name>/<variant>.json`) or an inline `graph` (requires `name`; `variant` defaults to `main`). A file is read with a no-follow confined path and tracked by content hash for drift detection; `project` defaults to the discovered project root; with `path`, any `name`/`variant`/`project` given must match the file's (`INVALID_PATH: name/variant/project must match the plan file path`); `force` releases live locks of removed deliverables. |
| `plan.list` | List every plan line of `project` (default: discovered project root), sorted by name with variants sorted; archived lines and variants are omitted unless `include_archived`. |
| `plan.export` | Write the head graph of `plan_id` to its own variant file, or to a confined `path`, and return the root-relative path. Refuses (`INVALID_PATH`) another variant's tracked plan file, or the variant's own file while it holds local edits never synced, unless `force: true`. The written file is not synced, except that exporting to the variant's own tracked file records its hash (drift is then `false`). |
| `plan.revise` | Replace a plan's graph in place, carrying progress over (unchanged deliverables keep status/counters; changed or reopened ones are re-derived). Returns the new revision and diff; `force` releases live locks of removed deliverables. |
| `plan.fork` | Copy a named variant's head graph, apply ordered `edits`, and register the result as a new draft (not selected) variant of the same line. Forking into an archived line is `ARCHIVE_REFUSED` before any file is written; forking from an archived variant of a live line is allowed. If registration fails after the file was written, the file is removed. |
| `plan.select` | Make a named variant its line's selected (only executable) variant, carrying progress over; `force` releases locks on the previously selected variant. |
| `plan.archive` | Archive (`archived` defaults `true`) or unarchive a whole line or one `variant`. Archived variants stay readable but refuse sync, selection and execution (`ARCHIVE_REFUSED`). |
| `plan.compare` | Compare stored plans on the scorecard (`plan_ids`, 2 to 16 distinct ids, or `plan` line name in `project` (default: the discovered root) for every non-archived variant, at most 16): Pareto front, weighted rank and recommended plan. Weights must be finite and `>= 0`. With `monte_carlo`, `iterations × (deliverables + prerequisite edges)` summed over all variants must not exceed 200000000 (`INVALID_GRAPH`). Read-only; scoring runs off the async runtime. |
| `plan.baseline` | Freeze the plan's current CPM schedule (earliest start/finish per deliverable) and budgets (effort basis × `metadata.cost_rate`, default 1) as its next numbered earned-value baseline. Optional `start` (RFC 3339, default now) and `calendar` `{hours_per_day (0 < h ≤ 24, default 8), workdays (default mon–fri), utc_offset_minutes (default 0)}` (omitted: wall-clock hours on the first baseline; a re-baseline keeps the previous baseline's calendar). The first baseline is number 1; re-baselining needs a non-blank `reason` (at most 2048 characters, `INVALID_GRAPH` otherwise) and keeps actuals and snapshots. Baselines, actuals and snapshots belong to one variant: a newly selected variant takes its own baseline 1, with no reason needed. Execution-side: only the selected, unarchived variant (`VARIANT_NOT_SELECTED` / `ARCHIVE_REFUSED`). Audited as `plan.ev.baselined`. |
| `plan.ev` | Earned-value report against the latest baseline as of `as_of` (RFC 3339, default now). `as_of` is the PV status date; EV and AC reflect progress and actuals recorded up to the moment the call runs. Returns BAC, PV, EV, AC, SV, CV, SPI, CPI, EAC, ETC, VAC, TCPI, per-deliverable rows, critical float consumed, `SPI_BELOW_0_9` / `CPI_BELOW_0_9` alerts (metric below 0.9 on the two latest *stored*, non-backfilled snapshots by `as_of` of the current baseline; the current reading is not one of them) and `excluded_unbaselined` (deliverables added since the baseline). AC sums every recorded hour of the plan, removed and unbaselined deliverables included (at the baseline row's cost rate, or 1 without one), so spend never disappears, even after a re-baseline. A baselined deliverable that `plan.revise` removes keeps the percent it had earned at removal (100 if complete, else its earning rule's percent) and reports row status `removed`; adding it back restarts its earned percent while its hours keep accumulating. A ratio with a zero denominator is `null` and explained in `undefined`; output never contains NaN or infinity. Read-only on any variant; `NOT_BASELINED` before `plan.baseline`. |
| `plan.snapshot` | Compute the `plan.ev` report and append it as a snapshot (`as_of` is the PV status date; EV and AC are the progress recorded when the call runs). A snapshot whose `as_of` is more than an hour before its `taken_at` is marked `backfilled: true`: it raises no alerts and later alerts skip it. Returns its `summary` (with `undefined` explaining each null ratio; alerts consider only readings up to its own position: this snapshot and the latest earlier non-backfilled one by `as_of` of the current baseline, so a backfill never takes alerts from newer readings), `snapshot_count`, and `export` of the newest 100 snapshots by `as_of` (ties by when they were taken), oldest first, so a backfilled `as_of` lands in date order; a backfill older than those 100 is stored and counted but not listed: a list of summaries (`format: "json"`, default) or a Markdown table with columns date, PV, EV, AC, SPI, CPI, EAC (`format: "markdown"`, undefined ratios shown as `n/a`). Execution-side like `plan.baseline`; `NOT_BASELINED` before it. |

`plan.lint` and `plan.simulate` take exactly one of an inline `graph`, a stored
`plan_id`, or a plan-file `path`; `plan.schedule` takes `graph` or `plan_id`;
`plan.schedule` and `plan.simulate` reject what `plan.submit` rejects, and `plan.lint` reports it as findings. Monte Carlo output is reproducible for a given seed on the same platform
and toolchain; bit-identical results across targets or compiler versions are
not guaranteed, because float math functions can differ.

### Plan-as-code workflow

Author a plan as a file at `.cpm-planner/plans/<name>/<variant>.json`, check it
with `plan.lint {path}`, register it with `plan.sync {path}` and execute the
returned `plan_id`. A named line has many variants but exactly one selected:
`plan.fork` creates a draft from structured edits, `plan.compare` scores the
variants, and `plan.select` makes one executable. Keep definitions in tracked
files — never keep untracked scratch graphs.

## Use as a library

The CPM kernel is also a plain Rust library, independent of MCP:

```rust
use cpm_planner::{CpmAlgorithm, Task, TaskKind};

let mut tasks = vec![
    Task::new("design", "Design", TaskKind::Custom { description: "design".into() }, 4.0),
    Task::new("build", "Build", TaskKind::Custom { description: "build".into() }, 8.0)
        .depends_on("design"),
    Task::new("test", "Test", TaskKind::Custom { description: "test".into() }, 2.0)
        .depends_on("build"),
];

let result = CpmAlgorithm::calculate(&mut tasks);
println!("critical path: {:?}", result.critical_path); // ["design", "build", "test"]
// also: result.bottlenecks, result.optimal_duration_parallel,
// and per-task .float (slack) / .is_critical on each Task.
```

See the [API docs](https://docs.rs/cpm-planner).

## Use with an MCP client (e.g. praxec)

cpm-planner is fully standalone — it speaks plain MCP and has no code dependency
on any particular client. As one example, you can wire it into an
[praxec](https://github.com/praxec/praxec) workflow as an MCP
connection (protocol only, no shared code):

```yaml
connections:
  planner:
    kind: mcp
    command: cpm-planner
```

## Using it with Praxec

This is an MCP tool used by [Praxec](https://github.com/praxec/praxec) packs. The easiest way to
get it — and a workflow pack that uses it — up and running is the one-command setup:

```bash
curl -fsSL https://raw.githubusercontent.com/praxec/packs/main/setup.sh | bash
```

See the [pack registry](https://github.com/praxec/packs) for this tool's provider coordinates
(container image / release binary) and which packs depend on it.

## Environment variables

| Variable | Default | Meaning |
|----------|---------|---------|
| `CPM_PLANNER_DB` | OS data dir (`~/.local/share/praxec/cpm-planner.db`) | SQLite path for durable, cross-process planner state; `:memory:` gives ephemeral state. |
| `CPM_PROJECT_ROOT` | nearest ancestor of cwd with `.cpm-planner/` or `.git` | Repo root for plan-as-code files (`.cpm-planner/plans/<name>/<variant>.json`). Tools that need a root report `INVALID_PATH: no project root (set CPM_PROJECT_ROOT or run inside a repo)` when none is found. |
| `CPM_MAX_TTL_SECS` | `28800` (8h) | Server-side ceiling for `ttl_seconds` on `plan.acquire_cohort` and `plan.heartbeat`; larger requested values are clamped. Must be a positive integer — any other value aborts startup. |

Leases default to 5 minutes. For long-running work pass `ttl_seconds` (≤ the server maximum) on acquire/heartbeat, and heartbeat at least every `ttl/3`.

## License

[Apache-2.0](LICENSE).
