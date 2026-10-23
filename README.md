# cpm-planner

[![CI](https://github.com/praxec/cpm-planner/actions/workflows/ci.yml/badge.svg)](https://github.com/praxec/cpm-planner/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/praxec/cpm-planner)](https://github.com/praxec/cpm-planner/releases/latest)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

cpm-planner is a Critical Path Method (CPM) planner for agent work, exposed as an
MCP server. You submit a task graph and it computes the schedule: earliest and
latest start and finish, slack, the critical path and the bottleneck tasks that
gate completion. It then leases **lock-aware cohorts** of ready deliverables with
disjoint file sets, so several workers can run in parallel without colliding.
Plans can live in the repository as plan-as-code files with forkable, comparable
variants; read-only tools lint, level against resource capacities and run Monte
Carlo risk; earned-value tools track PV, EV, AC, SPI and CPI against a frozen
baseline; and an optional AI review proposes verified schedule improvements. Any
MCP client (Claude Code, Cursor, VS Code, Codex, a custom orchestrator, or a
[praxec](https://github.com/praxec/praxec) workflow) drives it over stdio. It is a
standalone tool with no dependency on praxec.

## Contents

- [Install](#install)
  - [Prebuilt binary (recommended)](#prebuilt-binary-recommended)
  - [Pin a version](#pin-a-version)
  - [npm](#npm)
  - [From source](#from-source)
  - [Docker](#docker)
  - [Verify the install](#verify-the-install)
- [Quickstart: plan as code](#quickstart-plan-as-code)
- [Quickstart: earned value](#quickstart-earned-value)
- [Use with AI agents](#use-with-ai-agents)
- [MCP tools](#mcp-tools)
  - [Plan-as-code workflow](#plan-as-code-workflow)
  - [Plan review (optional)](#plan-review-optional)
- [Use as a library](#use-as-a-library)
- [Praxec](#praxec)
- [Environment variables](#environment-variables)
- [Documentation](#documentation)
- [Contributing](#contributing)
- [License](#license)

## Install

### Prebuilt binary (recommended)

Prebuilt packages are published for Linux, macOS and Windows on x86_64 and ARM64.

The installers resolve your OS and CPU and download the matching archive. They
**verify its SHA-256 against the release's `checksums.sha256` before installing**,
then atomically replace the binary in a user-local directory. A checksum mismatch
aborts with a non-zero exit and installs nothing. Re-running the installer upgrades
in place.

```sh
# Linux / macOS  ->  ~/.local/bin/cpm-planner
curl -fsSL https://github.com/praxec/cpm-planner/releases/latest/download/install.sh | sh

# Windows (PowerShell)  ->  %LOCALAPPDATA%\Programs\cpm-planner\cpm-planner.exe
irm https://github.com/praxec/cpm-planner/releases/latest/download/install.ps1 | iex
```

The installer prints the absolute path it installed to. Use that path when you
register the server with a GUI client.

<details>
<summary>Installer details: inspecting the script, PATH, upgrades and direct downloads</summary>

If you would rather read the script before running it, download, inspect, then run
(see [Pin a version](#pin-a-version) for `<version>`):

```sh
# Linux / macOS
curl -fsSLO https://github.com/praxec/cpm-planner/releases/latest/download/install.sh
less install.sh
sh install.sh --version <version>
```

```powershell
# Windows
irm https://github.com/praxec/cpm-planner/releases/latest/download/install.ps1 -OutFile install.ps1
Get-Content install.ps1
.\install.ps1 -Version <version>
```

**PATH.** If the install directory is not on your `PATH`, the installer prints the
exact line to add. Pass `--add-to-path` (`-AddToPath` on Windows) to have it do that
for you, user-level only and never with sudo.

On Linux and macOS it appends the line only if it is not already there, to:

- `~/.zshrc` for zsh;
- `~/.bash_profile` if it exists, else `~/.bashrc`, for bash;
- `~/.profile` for other shells.

For fish it prints `fish_add_path <dir>` for you to run instead of writing a file.

**Downloads.** The installers only accept `https://` download URLs.

**Upgrades.** After upgrading, restart your MCP client so it launches the new
binary. On Windows the old exe is renamed to `cpm-planner.exe.old` and removed on
the next run.

**Direct downloads.** You can also download an archive yourself and check it
against `checksums.sha256`; a machine-readable `release-manifest.json` is published
too. All assets are at <https://github.com/praxec/cpm-planner/releases/latest>.

| Platform | Asset |
|----------|-------|
| Linux x86_64 | `cpm-planner-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `cpm-planner-aarch64-unknown-linux-gnu.tar.gz` |
| macOS x86_64 | `cpm-planner-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `cpm-planner-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `cpm-planner-x86_64-pc-windows-msvc.zip` |
| Windows ARM64 | `cpm-planner-aarch64-pc-windows-msvc.zip` |

</details>

### Pin a version

In the commands in this README, `<version>` is a release tag such as `v0.2.0`. Pick
one from the [releases page](https://github.com/praxec/cpm-planner/releases).

```sh
# Linux / macOS
curl -fsSL https://github.com/praxec/cpm-planner/releases/latest/download/install.sh | sh -s -- --version <version>

# Windows (PowerShell): the piped form cannot take parameters, so use the environment variable...
$env:PRAXEC_VERSION = '<version>'; irm https://github.com/praxec/cpm-planner/releases/latest/download/install.ps1 | iex
# ...or a script block
& ([scriptblock]::Create((irm https://github.com/praxec/cpm-planner/releases/latest/download/install.ps1))) -Version <version>
```

### npm

With Node 18 or later, install the `cpm-planner` command from npm:

```sh
npm install -g @matthew-cochran/cpm
cpm-planner --version
```

The package is a small launcher. It downloads the release binary for your
platform, verifies its checksum and caches it, during install or, where npm
skips install scripts, on first run. The package
version is the release version, so `npm install -g @matthew-cochran/cpm@0.2.0`
pins one. To run it without installing, use `npx -y @matthew-cochran/cpm`.
For details and environment variables, see [npm/README.md](npm/README.md).

### From source

Build and install a tagged release with Cargo (Rust 1.99 or newer):

```sh
cargo install --git https://github.com/praxec/cpm-planner --tag <version> --locked
```

The crates.io listing lags behind the GitHub releases until the next
`cargo publish`, so `cargo install cpm-planner` does not give you the current
version yet. Install from the tag as above.

### Docker

```sh
docker pull ghcr.io/praxec/cpm-planner
docker run -i --rm -v cpm-planner-data:/data ghcr.io/praxec/cpm-planner
```

`-i` keeps stdin open for the stdio transport.

The image sets `CPM_PLANNER_DB=/data/cpm-planner.db` and declares `/data` as a
volume owned by the unprivileged `app` user, so a named volume persists plan state
across runs. For a bind mount (`-v "$PWD:/data"`), run as yourself so the directory
is writable: `--user "$(id -u):$(id -g)"`. Pass `-e CPM_PLANNER_DB=...` only to
override the path.

To register the image with a client, put `docker` in `command` and the rest in
`args`. For example, in Claude Code:

```sh
claude mcp add cpm-planner --scope user -- docker run -i --rm -v cpm-planner-data:/data ghcr.io/praxec/cpm-planner
```

### Verify the install

```sh
cpm-planner --version
```

It prints `cpm-planner <version>`. To check the MCP side without a client, run the smoke
script from the repository. It performs the MCP `initialize` and `tools/list` handshake and
fails if `plan.submit`, `plan.status` or `plan.get` is missing:

```sh
node scripts/mcp-smoke.mjs /absolute/path/to/cpm-planner
```

## Quickstart: plan as code

Write a plan as `.cpm-planner/plans/<name>/<variant>.json` (a `PlanGraph`):

1. `plan.lint {path: ".cpm-planner/plans/checkout/main.json"}` — static checks, no state written.
2. `plan.sync {path: ".cpm-planner/plans/checkout/main.json"}` — register the variant and return its `plan_id`.
3. `plan.status {plan_id}` — schedule, critical path, ready set and locks.

`plan.sync` tracks the file by content hash; `plan.status` reports
`definition_drift` when the tracked file and the stored head graph disagree.

## Quickstart: earned value

Freeze the baseline, record progress, then read the report (`earned_pct` is earned in
proportion under `earning_rule: "weighted"`; `fifty_fifty` credits 50% once a deliverable is in progress or has any
reported `earned_pct`, and the default `zero_hundred`
earns at completion):

1. `plan.baseline {plan_id}` — freeze the CPM schedule and budgets as baseline 1.
2. `plan.mark_status {plan_id, deliverable_id, caller_id, status: {"status": "in_progress"}, earned_pct: 50, actual_effort_hours: 4}` — report progress and actual cost.
3. `plan.ev {plan_id}` — PV, EV, AC, SV, CV, SPI, CPI, EAC and alerts.
4. `plan.snapshot {plan_id, format: "markdown"}` — append the reading and export a Markdown table.

A ratio with a zero denominator is `null` and explained in `undefined`; alerts
(`SPI_BELOW_0_9`, `CPI_BELOW_0_9`) compare the two latest stored readings.

## Use with AI agents

Any MCP client can run cpm-planner over stdio: register the `cpm-planner`
binary as the command, or, with Node 18 or later, `npx -y @matthew-cochran/cpm`.
For a GUI client, use the absolute path the installer printed.

Install the agent skills (`/cpm-plan` and friends) with
`cpm-planner skills install --target <tool> --user` (or `--project <dir>`), where
`<tool>` is `claude`, `codex`, `cursor`, `copilot`, `gemini`, `all`, or
`agents-md` (project only).

[docs/AGENT-INSTALL.md](docs/AGENT-INSTALL.md) has the per-client steps (Claude
Code, Claude Desktop, Codex, Cursor, VS Code, Gemini CLI and others), `env`
blocks, verification and troubleshooting. [llms.txt](llms.txt) indexes the docs
for LLMs.

## MCP tools

| Tool | Does |
|------|------|
| `plan.submit` | Submit a task graph; returns a plan id (idempotent on the graph + caller). A prerequisite is a deliverable id string or an object `{id, consumes?, kind?: artifact\|interface, lag_hours?}`; both forms round-trip through `plan.get`. A deliverable may set `milestone: true` (an acceptance point, zero-length unless given an estimate or duration, reported in `plan.status` `milestones`). An optional `earning_rule` (`zero_hundred`, the default: 100% only when complete; `fifty_fifty`: 50% once in progress or any `earned_pct` is reported; `weighted`: the reported `earned_pct`) sets how earned value credits partial progress and is part of the plan's identity. A deliverable's optional `duration_hours` (calendar time) replaces effort as its scheduled length and a prerequisite's `lag_hours` delays the dependent's start; effort stays the cost basis. An optional `estimate` `{optimistic, likely, pessimistic}` (`0 <= optimistic <= likely <= pessimistic`) is a three-point effort estimate. Scheduled-length precedence: `duration_hours` > `estimated_effort_hours` > `estimate.likely` > 0 for a milestone > the estimator default; Monte Carlo samples the estimate only when neither `duration_hours` nor `estimated_effort_hours` is set. Limits: at most 5000 deliverables, and every hour value (effort, duration, lag, estimate) finite and between 0 and 1000000 (`INVALID_GRAPH`). With `name` (optional `project`/`variant`, variant defaults to `main`) it registers a named variant instead of an unnamed plan. |
| `plan.acquire_cohort` | Atomically acquire up to N ready deliverables with mutually disjoint file sets (an `owned_files` entry may be `{path, mode: "append"}`; append claims on one path may be co-leased and appear in `shared_paths`; a file may be owned by several deliverables only if they are ordered by prerequisites, or all claims are append); optional `ttl_seconds` sets the lease TTL (clamped to the server maximum); optional `ids` and `filter.metadata` target deliverables; `metadata.kind = "manual"` deliverables are never leased; the response lists unleased candidates in `blocked` (with `blocked_count`, `needs_operator`). |
| `plan.heartbeat` | Refresh the TTL on a held lock; optional `ttl_seconds` sets the new TTL (clamped to the server maximum). |
| `plan.mark_status` | Mark a deliverable complete/failed; releases its lock. Without a lock, Complete requires complete prerequisites (`PREREQUISITES_INCOMPLETE`) and every lockless mark is audited. A lockless `in_progress` (owner or manual work) persists across server restarts and is not leasable; hand it back with a lockless `ready` (or any other status) mark. Optional earned-value progress: `earned_pct` (integer 0 to 100, only with `in_progress`; accepted and ignored with `complete`), `actual_effort_hours` (total effort so far, finite, 0 to 1000000; replaces leased hours as actual cost) and `evidence` (at most 2048 characters, appended to the deliverable's evidence list, which keeps at most 100 entries); violations are `INVALID_ACTUALS`, including a negative, fractional or over-100 `earned_pct`. `plan.select` copies the actuals of every deliverable whose Complete status it carries (leased hours add up, the carried deliverable's reported percent and hours win, the newest 100 evidence entries are kept). Every lease that ends (complete, failed, force release, expiry, accept override, forced revise/select/archive) adds its hours to the deliverable's leased hours; a lapsed lease counts up to its expiry. |
| `plan.status` | Read-only snapshot of the plan and its locks. `critical_path` always runs `__start__` to `__finish__` (synthetic zero-effort endpoints; their `schedule` rows carry `synthetic: true`, and `__start__`/`__finish__` are reserved deliverable ids rejected with `INVALID_GRAPH`). `plan_complete` is true once every deliverable is Complete; the `plan.completed` audit event fires once when that happens. `milestones` has one row per deliverable with `milestone: true` (or legacy `metadata.milestone == true`): `id`, `critical_path` (longest chain from `__start__` to it), `hours` (its earliest finish) and `complete`. For a named variant it also reports `name`, `variant`, `selected`, `archived` and `active_plan` (the project's single active line's name when the variant is archived; `archived` and `active_plan` are `null` for an unnamed plan), and `definition_drift`: `null` (unknown) without a project root for the variant's project, without a tracked plan file, or when the file is missing, unreadable or over 8 MiB; otherwise `false` when the file's bytes match the last synced or exported hash or the file's graph equals the head graph (re-formatting is not drift), and `true` when the file's graph differs from the head (or does not parse). An inline `plan.revise`/`plan.sync` of a file-backed variant therefore reports `true` until the file is re-exported or re-synced. |
| `plan.get` | Return the stored plan graph for a plan selected by `plan_id`, `plan` line name or `active: true` (read back what was submitted). |
| `plan.force_release` | Operator escape hatch: release a lock regardless of holder/TTL; `reset_counters: true` also clears lapse/failure counters. A lockless `in_progress` has no lock, so `plan.force_release` leaves it as is; hand it back with a lockless `plan.mark_status`. |
| `plan.accept` | Manager/owner acceptance: complete a deliverable without holding its lease (audited, with evidence). |
| `plan.lint` | Static checks without submitting: cycles (with the loop), redundant edges, edges without rationale, interface edges not targeting a contract, deliverables feeding no milestone, and unordered file overlaps (#20). |
| `plan.schedule` | Level a graph against resource capacities (`metadata.owner` by default): makespan, per-deliverable start/finish, per-resource load, the driving chain (dependency vs resource waits), and project/feeding buffers (#19). `capacities` is required: every resource carrying work needs at least 1 unit, otherwise `INVALID_CAPACITIES:` lists the missing resources. `project_buffer_pct` is 0 to 100 (default 25). |
| `plan.simulate` | Read-only what-if for a graph, stored plan or plan file (persists nothing): lint, critical path, schedule, milestones, optional resource schedule (`schedule`, same inputs and `INVALID_CAPACITIES:` rule as `plan.schedule`) and Monte Carlo (`monte_carlo`: `iterations` 1 to 50000, default 2000; `seed`, default `0xC0FFEE`; `iterations × (deliverables + prerequisite edges)` must not exceed 200000000), and the scorecard (#23). |
| `plan.sync` | Register or update one variant of a named plan line from a plan file (`path`, of the form `.cpm-planner/plans/<name>/<variant>.json`) or an inline `graph` (requires `name`; `variant` defaults to `main`). A file is read with a no-follow confined path and tracked by content hash for drift detection; `project` defaults to the discovered project root; with `path`, any `name`/`variant`/`project` given must match the file's (`INVALID_PATH: name/variant/project must match the plan file path`); `force` releases live locks of removed deliverables. |
| `plan.list` | List every plan line of `project` (default: discovered project root), sorted by name with variants sorted; archived lines and variants are omitted unless `include_archived`. `active: true` returns only the project's active lines and cannot be combined with `include_archived`. |
| `plan.export` | Write the head graph of `plan_id` to its own variant file, or to a confined `path`, and return the root-relative path. Refuses (`INVALID_PATH`) another variant's tracked plan file, or the variant's own file while it holds local edits never synced, unless `force: true`. The written file is not synced, except that exporting to the variant's own tracked file records its hash (drift is then `false`). |
| `plan.revise` | Replace a plan's graph in place, carrying progress over (unchanged deliverables keep status/counters; changed or reopened ones are re-derived). Returns the new revision and diff; `force` releases live locks of removed deliverables. |
| `plan.fork` | Copy a named variant's head graph, apply ordered `edits`, and register the result as a new draft (not selected) variant of the same line. Forking into an archived line is `ARCHIVE_REFUSED` before any file is written; forking from an archived variant of a live line is allowed. If registration fails after the file was written, the file is removed. |
| `plan.select` | Make a named variant its line's selected (only executable) variant, carrying progress over; `force` releases locks on the previously selected variant. |
| `plan.archive` | Archive (`archived` defaults `true`) or unarchive a whole line or one `variant`. Archived variants stay readable but refuse sync, selection and execution (`ARCHIVE_REFUSED`). |
| `plan.compare` | Compare stored plans on the scorecard (`plan_ids`, 2 to 16 distinct ids, or `plan` line name in `project` (default: the discovered root) for every non-archived variant, at most 16): Pareto front, weighted rank and recommended plan; equal scores share a rank and `tied_with_recommended` lists the variants tied with the recommendation. Weights must be finite and `>= 0`. With `monte_carlo`, `iterations × (deliverables + prerequisite edges)` summed over all variants must not exceed 200000000 (`INVALID_GRAPH`). Read-only; scoring runs off the async runtime. |
| `plan.baseline` | Freeze the plan's current CPM schedule (earliest start/finish per deliverable) and budgets (effort basis × `metadata.cost_rate`, default 1) as its next numbered earned-value baseline. Optional `start` (RFC 3339, default now) and `calendar` `{hours_per_day (0 < h ≤ 24, default 8), workdays (default mon–fri), utc_offset_minutes (default 0)}` (omitted: wall-clock hours on the first baseline; a re-baseline keeps the previous baseline's calendar). The first baseline is number 1; re-baselining needs a non-blank `reason` (at most 2048 characters, `INVALID_GRAPH` otherwise) and keeps actuals and snapshots. Baselines, actuals and snapshots belong to one variant: a newly selected variant takes its own baseline 1, with no reason needed. Execution-side: only the selected, unarchived variant (`VARIANT_NOT_SELECTED` / `ARCHIVE_REFUSED`). Audited as `plan.ev.baselined`. |
| `plan.ev` | Earned-value report against the latest baseline as of `as_of` (RFC 3339, default now). `as_of` is the PV status date; EV and AC reflect progress and actuals recorded up to the moment the call runs. Returns BAC, PV, EV, AC, SV, CV, SPI, CPI, EAC, ETC, VAC, TCPI, per-deliverable rows, critical float consumed, `SPI_BELOW_0_9` / `CPI_BELOW_0_9` alerts (metric below 0.9 on the two latest *stored*, non-backfilled snapshots by `as_of` of the current baseline; the current reading is not one of them) and `excluded_unbaselined` (deliverables added since the baseline). AC sums every recorded hour of the plan, removed and unbaselined deliverables included (at the baseline row's cost rate, or 1 without one), so spend never disappears, even after a re-baseline. A baselined deliverable that `plan.revise` removes keeps the percent it had earned at removal (100 if complete, else its earning rule's percent) and reports row status `removed`; adding it back restarts its earned percent while its hours keep accumulating. A ratio with a zero denominator is `null` and explained in `undefined`; output never contains NaN or infinity. Read-only on any variant, including an archived one, which reports `archived: true` and, when the project has exactly one active line, its `active_plan`; `NOT_BASELINED` before `plan.baseline`. |
| `plan.snapshot` | Compute the `plan.ev` report and append it as a snapshot (`as_of` is the PV status date; EV and AC are the progress recorded when the call runs). A snapshot whose `as_of` is more than an hour before its `taken_at` is marked `backfilled: true`: it raises no alerts and later alerts skip it. Returns its `summary` (with `undefined` explaining each null ratio; alerts consider only readings up to its own position: this snapshot and the latest earlier non-backfilled one by `as_of` of the current baseline, so a backfill never takes alerts from newer readings), `snapshot_count`, and `export` of the newest 100 snapshots by `as_of` (ties by when they were taken), oldest first, so a backfilled `as_of` lands in date order; a backfill older than those 100 is stored and counted but not listed: a list of summaries (`format: "json"`, default) or a Markdown table with columns date, PV, EV, AC, SPI, CPI, EAC (`format: "markdown"`, undefined ratios shown as `n/a`). Execution-side like `plan.baseline`; `NOT_BASELINED` before it. |
| `plan.review` | Optional AI review of a graph, stored plan or plan file (read-only; exactly one of `graph`, `plan_id`, `path`; optional `capacities` / `resource_key` / `project_buffer_pct` to level makespans, and `max_questions` 1 to 64, default 64). Lint always runs; lint errors return `status: "invalid_graph"` without calling the judge. Otherwise one batched call to Jev asks about likely false and missing dependencies, split and interface-split candidates and crash options, and returns advisory `findings` plus `proposals`: `plan.fork` edit lists, each verified by simulate (lints clean, shortens the makespan) and ranked by `hours_saved / max(cost, 1)`. Without a key, or with an unusable LLM setting, `status` is `review_unavailable` with a `reason`, plus lint. See [Plan review (optional)](#plan-review-optional). |

`plan.status`, `plan.get`, `plan.ev`, `plan.snapshot`, `plan.baseline`,
`plan.acquire_cohort`, `plan.heartbeat`, `plan.mark_status`, `plan.accept` and
`plan.force_release` address a plan by exactly one of `plan_id`, `plan` (a plan line
name, resolved to its selected variant) or `active: true` (the project's single line that
is unarchived and whose selected variant is unarchived and not complete). `plan` and
`active` also take an optional `project` (default: the discovered project root); `project`
with `plan_id` is invalid params. No active line is `NO_ACTIVE_PLAN`; several are
`AMBIGUOUS_ACTIVE_PLAN`, naming them; an unknown line, or a line with no selected
variant, is `PLAN_NOT_FOUND`.
Recurring jobs and agents should look the plan up with `active` (or `plan`) rather than
pin a `plan_id`, so they follow the work to the next line. `active` matches only an
incomplete line, and it changes (or becomes ambiguous) when another line goes live:
resolve once (`plan.status` and `plan.acquire_cohort` return the `plan_id`), then keep that
`plan_id` for heartbeats, marks and the final `plan.status`/`plan.ev` of that plan. A pinned
id of an archived variant still reads through `plan.status`/`plan.ev`, which report
`archived: true` and `active_plan` naming where the live work is, while snapshots, baselines
and execution refuse with `ARCHIVE_REFUSED`.

`plan.lint`, `plan.simulate` and `plan.review` take exactly one of an inline `graph`, a stored
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

### Plan review (optional)

`plan.review` asks Jev (`typesafe/jev-1.13`), TypeSafe's
calibrated-judgment model, about a lint-clean plan through OpenRouter: one
batched call of at most 64 questions per review. It is off until you set
`OPENROUTER_API_KEY` (or `CPM_OPENROUTER_KEY_FILE`); without a key the tool
still answers, with `review_unavailable` and the lint report. An invalid LLM
setting never stops the server: it is logged at startup and `plan.review`
reports `review_unavailable` with a reason naming the setting.

- **Probabilities are advisory.** Treat findings as a second opinion. Proposals
  are verified mechanically (the edited plan must lint clean and simulate to a
  shorter makespan); apply one with `plan.fork {edits}`, then `plan.compare`.
- **Cost.** Each `plan.review` call that reaches the judge is one billed
  OpenRouter request; Jev costs about $0.042 per million input tokens on
  OpenRouter. Each report carries the provider's `usage` when given. At most 2
  reviews that call the judge run at once per server; further calls wait for a
  slot (reviews that end before the call, such as no key or lint errors, never
  wait).
- **Privacy.** This is everything sent to OpenRouter in that one request:
  - a fixed task sentence, the plan makespan and the critical path (ids);
  - the questions: for each, its kind, the deliverable ids it names, and a
    question sentence that quotes those ids and, depending on the kind, the
    prerequisite's `consumes` text, the deliverable's scheduled hours, the
    plan's median scheduled hours, and the heuristic signals that raised a
    missing-dependency question (a description mentions the other, files in
    the same or nested directories, a shared `metadata.owner`);
  - for every deliverable a question names: its id, scheduled hours, float
    hours, `critical` and `milestone` flags, owned file paths (sent in full,
    not truncated), prerequisites (id, `consumes`, kind) and its
    `description`, `artifact` and `owner` metadata. `description`,
    `artifact`, `owner` and `consumes` are cut to 500 characters each.

  Nothing else in the graph (other deliverables, other metadata keys,
  estimates) is sent. Do not review plans whose contents you may not share
  with OpenRouter.
- **Audit.** The key is never logged or returned, and neither is a misplaced
  value of any LLM variable. Every `plan.review` call records one
  `plan.review` audit event, whatever its outcome (including a missing plan,
  a bad path or a failed review); the only unaudited case is a call rejected
  for its params (`invalid_params`). The event has exactly these fields:
  - `status`: `ok`, `review_unavailable`, `invalid_graph`, or `error` (the
    review failed: `PLAN_NOT_FOUND`, `INVALID_PATH`, `INVALID_CAPACITIES`, …);
  - `code`: the error prefix for `error`, else null;
  - `failure_class`: for a `review_unavailable` caused by a failed judge call,
    its class (`unauthorized`, `rate_limited`, `timeout`, `upstream`,
    `decode`, `transport`, `invalid_request`); null otherwise (including no
    key and an unusable LLM setting);
  - `plan_id`: the stored plan reviewed, null for an inline `graph` or `path`;
  - `question_count`: questions sent (0 when the judge was not called);
  - `jev_called`: whether the judge was called;
  - `prompt_hash`: sha256 of the canonical request, null when the judge was
    not called;
  - `model`: the model the provider reported, else the configured model when
    the call failed; null when the judge was not called;
  - `endpoint`: the judge endpoint's host; null when no judge is configured,
    lint found errors, or the review ended in `error`.

  The prompt itself is never recorded.

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

The crate-level docs (`cargo doc --open`, or [docs.rs](https://docs.rs/cpm-planner)
once a current version is published there) have a runnable example that drives the
planner through submit, lease and complete.

`llm::jev::JevJudge` (the `plan.review` judge) owns its HTTP client and
connection pool. A pooled connection is driven by the tokio runtime that
opened it, so use a judge (and its clones) from one runtime, and build a new
judge for another runtime.

## Praxec

cpm-planner is fully standalone: it speaks plain MCP and has no code dependency on
any particular client. [Praxec](https://github.com/praxec/praxec) packs use it as an
MCP tool. To wire it into a praxec workflow, declare an MCP connection (protocol
only, no shared code):

```yaml
connections:
  planner:
    kind: mcp
    command: cpm-planner
```

The easiest way to get it, plus a workflow pack that uses it, up and running is the
one-command setup:

```bash
curl -fsSL https://raw.githubusercontent.com/praxec/packs/main/setup.sh | bash
```

See the [pack registry](https://github.com/praxec/packs) for this tool's provider
coordinates (container image or release binary) and which packs depend on it.

## Environment variables

Set these in a client's `env` block (or `-e`/`--env` flag, or `docker run -e`). All are optional.

| Variable | Default | Meaning |
|----------|---------|---------|
| `CPM_PLANNER_DB` | `~/.local/share/praxec/cpm-planner.db` on every OS (`~` is `HOME`, else `USERPROFILE`: `%USERPROFILE%\.local\share\praxec\cpm-planner.db` on Windows); `/data/cpm-planner.db` in the Docker image | SQLite path for durable, cross-process planner state; `:memory:` gives ephemeral state. |
| `CPM_PROJECT_ROOT` | nearest ancestor of cwd with `.cpm-planner/` or `.git` | Repo root for plan-as-code files (`.cpm-planner/plans/<name>/<variant>.json`). Tools that need a root report `INVALID_PATH: no project root (set CPM_PROJECT_ROOT or run inside a repo)` when none is found. |
| `CPM_MAX_TTL_SECS` | `28800` (8h) | Server-side ceiling for `ttl_seconds` on `plan.acquire_cohort` and `plan.heartbeat`; larger requested values are clamped. Must be an integer in 1..=2592000 (at most 30 days) — any other value aborts startup. |
| `OPENROUTER_API_KEY` | unset | OpenRouter key for `plan.review`. Unset or blank: `plan.review` reports `review_unavailable` ("no OpenRouter key configured"). Never logged or returned. |
| `CPM_OPENROUTER_KEY_FILE` | unset | File holding the OpenRouter key (contents trimmed; at most 4096 bytes), used when `OPENROUTER_API_KEY` is unset. On unix a world-readable (`o+r`) or empty file is ignored with a warning (`chmod 600` it). |
| `CPM_JEV_MODEL` | `typesafe/jev-1.13` | Jev model id for `plan.review`: at most 128 characters of `A-Z a-z 0-9 . _ : / -`, and never containing the key; anything else is logged (naming the variable, not the value) and makes `plan.review` unavailable. |
| `CPM_JEV_ENDPOINT` | `https://openrouter.ai/api/v1/systemone` | Jev endpoint URL: `https://`, or `http://` only for a loopback host, with no credentials. |
| `CPM_LLM_TIMEOUT_SECS` | `30` | Bound on each LLM call, integer seconds in 1..=300. |
| `CPM_LLM_MODEL` | `deepseek/deepseek-v4.1-flash` | Generative (chat) model id: at most 128 characters of `A-Z a-z 0-9 . _ : / -`. A frontier vendor is refused before any call, with a typed error naming the model: `anthropic/`, `openai/` (except `openai/gpt-oss*`), `google/`, `x-ai/`, `xai/`. |

Leases default to 5 minutes. For long-running work pass `ttl_seconds` (≤ the server maximum) on acquire/heartbeat, and heartbeat at least every `ttl/3`.

## Documentation

- [docs/README.md](docs/README.md): index of the project documentation.
- [docs/AGENT-INSTALL.md](docs/AGENT-INSTALL.md): install, register and verify cpm-planner for your AI coding tool.
- [docs/architecture.md](docs/architecture.md): modules, request flow, store schema and concurrency.
- [docs/releasing.md](docs/releasing.md): the release runbook.
- [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), [SUPPORT.md](SUPPORT.md), [CHANGELOG.md](CHANGELOG.md).

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the toolchain,
the gates and the branch model. Everyone taking part agrees to the
[Code of Conduct](CODE_OF_CONDUCT.md).

## License

[Apache-2.0](LICENSE).
