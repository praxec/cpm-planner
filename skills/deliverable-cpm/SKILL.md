---
name: deliverable-cpm
description: Use when you need the full method reference behind the cpm-plan, cpm-improve, cpm-run, cpm-ev and cpm-revise skills, or when no narrower cpm-* skill fits a cpm-planner (plan.* tools) task that spans the whole lifecycle. Covers the deliverable-based CPM method end to end: deliverables as artifacts, consumption edges, lint findings, sync and drift, resource levelling, the review/fork/simulate/compare/select improvement loop, leases and acceptance, baselines and earned value, and revising a plan file under .cpm-planner/plans/<name>/<variant>.json.
---

# Deliverable-based CPM with cpm-planner

A plan is a graph of **deliverables** (artifacts you can inspect) joined by **consumption
edges**. It is not a to-do list. The graph lives in a tracked file:

```
<repo root>/.cpm-planner/plans/<name>/<variant>.json
```

That file is the only definition. Never keep untracked scratch graphs or a second copy
anywhere else. Every change goes through the file (or through `plan.fork`, which writes its
own file next to it).

The project root is `CPM_PROJECT_ROOT`, or else the nearest ancestor with `.cpm-planner/`
or `.git`. Paths you pass to tools are relative to that root.

A worked plan file: [examples/small-plan.json](examples/small-plan.json).
A worked improvement loop: [examples/improvement-loop.md](examples/improvement-loop.md).

## 1. Nodes are artifacts

Each deliverable names what can be inspected when it is done, in `metadata.artifact`:
- a document;
- code with passing tests;
- a published release;
- a deployed service with evidence;
- an evidence report;
- a decision record.

Rules:
- **Turn activities into artifacts.** "Review", "rulings", "gate" and "run" become what they
  produce: a sign-off recorded in the contract, a decision record, an evidence report.
- **Fold acceptance review into the deliverable it accepts.** Don't make it a separate node.
  Add the reviewer's time to that deliverable's estimate.
- **Mark acceptance points** with `"milestone": true`. A milestone is zero-length unless you
  give it an estimate, but someone still has to complete it.
- **Give every deliverable an executor class** in `metadata.owner`, e.g. `agent`, `worker`
  or `owner`. Resource levelling and targeted acquire both use it.
- **Mark work no agent may lease** (owner decisions, manual sign-off) with
  `metadata.kind: "manual"`. It is closed with `plan.accept`.
- **List `owned_files` exactly**: the paths the implementer will write. Leases never hand out
  two deliverables with conflicting claims at the same time.
- **Estimate effort.** Use `estimated_effort_hours`, or a three-point `estimate`
  `{optimistic, likely, pessimistic}` where you're unsure. Monte Carlo only samples the
  estimate when neither `estimated_effort_hours` nor `duration_hours` is set.
  `duration_hours` is calendar time, used when it differs from effort.
- **Choose an `earning_rule`** for earned value: `zero_hundred` (default), `fifty_fifty` or
  `weighted`. Use `zero_hundred` for deliverables of 2 hours or less, and `fifty_fifty` or
  `weighted` for longer ones.

**If you can't state an artifact precisely yet, ticket it.** Open the ticket in the repo that
will produce it, and put its link in the deliverable's metadata. The ticket carries:
- the artifact and its acceptance criteria;
- the ids of the deliverables it consumes and of those that consume it;
- open questions.

Lanes owned by other agents or machines run from their tickets. Shared contracts are tickets
first.

## 2. Edges are consumption

`B ← A` only when **B consumes something specific that A produces**. Write it as an object
prerequisite on B:

```json
{ "id": "A", "consumes": "request/response schema", "kind": "interface" }
```

- `consumes` says what is handed over. An edge without it is a lint warning.
- `kind` labels what is consumed. Both kinds are finish-to-start: B can't start until A is
  complete, so an interface edge does not let B overlap A.
  - `artifact` (the default): B consumes A's finished output.
  - `interface`: B builds against a contract A defines. Its target must set
    `metadata.contract: true`; otherwise lint warns `INTERFACE_EDGE_NOT_CONTRACT`. When a
    revise changes the contract, the deliverables with interface edges to it are reopened.
  - To let a consumer start early, split the producer: a small contract deliverable the
    consumer has an interface edge to, and the implementation behind it.
- `lag_hours` is a minimum wait after A finishes, such as a soak period.

If nothing is consumed, there is no edge.

**Common false edges:**
- "A should happen first" with nothing consumed, such as publishing a pipeline before its
  content exists;
- two deliverables in the same repo that don't touch the same files;
- a gate that lists every upstream deliverable. Keep only the direct ones; the rest are
  implied, and lint flags them as `REDUNDANT_EDGE`.

**Common missing edges:**
- a release that consumes the features it ships;
- a decision record that implementation consumes, such as a masking policy, a naming scheme
  or an identity scheme;
- external state that acceptance consumes: something provisioned or deployed;
- a file another deliverable already changes. Order the two, or split the file;
- a contract, schema or format that both producer and consumer use. Make it its own
  `metadata.contract: true` deliverable, with `interface` edges to it;
- a fast-tracked edge that hides real consumption. It shows up later as rework.

## 3. Author the file, lint until clean

1. Write `.cpm-planner/plans/<name>/<variant>.json`. Use `main` as the first variant.
2. Run `plan.lint {path: ".cpm-planner/plans/<name>/main.json"}`.
3. Fix every finding, then lint again. Repeat until `clean` is true. Don't work around a
   finding.

| Code | Severity | Fix |
|---|---|---|
| `TOO_MANY_DELIVERABLES` | error | keep the plan to at most 5000 deliverables; split it into several plan lines |
| `CYCLE` | error | break the loop it lists; one of those edges is not real consumption |
| `UNKNOWN_PREREQUISITE`, `DUPLICATE_ID`, `RESERVED_ID`, `INVALID_VALUE` | error | fix the id or value (`__start__` and `__finish__` are reserved) |
| `UNORDERED_FILE_OVERLAP` | error | order the two deliverables, split the file, or make every claim `{path, mode: "append"}` |
| `REDUNDANT_EDGE` | warning | delete the edge; another path already implies it |
| `NO_RATIONALE` | warning | add `consumes`, or delete the edge if nothing is consumed |
| `INTERFACE_EDGE_NOT_CONTRACT` | warning | set `metadata.contract: true` on the target, or make the edge `artifact` |
| `FEEDS_NO_MILESTONE` | warning | cut the deliverable, or add the milestone it really serves |
| `NO_ARTIFACT`, `NO_MILESTONE` | info | add `metadata.artifact` or a milestone |

## 4. Sync and verify the critical path

1. Run `plan.sync {path}`. It returns `plan_id`, `name`, `variant`, `revision`, `created`,
   `changed` and `diff`. The server tracks the file by content hash.
2. Read `plan.status {plan_id}` and check:
   - `critical_path` runs `__start__ → … → __finish__`, and each consecutive pair of real
     deliverables is an edge in the file;
   - `critical_path_hours` is the longest chain you expect;
   - every acceptance point appears under `milestones`, with the path and `hours` you
     expect;
   - `definition_drift` is `false`:
     - `true` means the file and the stored head disagree. If you edited the file, sync it
       again. If the head changed inline (`plan.revise` or `plan.sync {graph}`), run
       `plan.export {plan_id}` to write the head back to the file;
     - `null` means unknown: no project root for the plan's project, no tracked file, or a
       file that is missing, unreadable or over 8 MiB. Find out which.

If the critical path surprises you, the graph is usually wrong (a false or missing edge).
Fix the file, lint it and sync again.

## 5. Level resources

CPM assumes unlimited workers. Level against the capacity you really have:

```text
plan.schedule {"plan_id": "...", "capacities": {"agent": 1, "worker": 2, "owner": 1}}
```

Capacities are keyed by `metadata.owner` (`resource_key` picks another key). Every resource
that carries work needs at least 1 unit; otherwise you get `INVALID_CAPACITIES`.

Read the response:
- `makespan` against `cpm_makespan`: the levelled schedule is the real one;
- `driving_chain`: the steps marked `waited_on: "resource"` are waiting for capacity, not
  for a dependency;
- `load`: a resource near `utilisation` 1.0 is the bottleneck;
- `project_buffer_hours` and `feeding_buffers`: the buffers to protect.

## 6. The improvement loop

Design several variants and pick one. Never edit the selected variant to try an idea.

1. **Review (optional).** Run `plan.review {path, capacities}`. It sends one batched request
   to Jev (TypeSafe's calibrated-judgment model, through OpenRouter) and needs an OpenRouter
   key. Without one it returns `review_unavailable` plus lint; carry on without it. The
   graph is sent to OpenRouter, so skip review for plans you may not share.
   - `findings` are advisory probabilities: `false_dependency`, `missing_dependency`,
     `split_candidate`, `interface_split`, `crash_option`. Check each one against what you
     know.
   - `proposals` are ready-made `plan.fork` edit lists. Each has been checked by simulation
     (it lints clean and shortens the makespan) and is ranked by `hours_saved / max(cost, 1)`.
   - An `add_capacity` crash proposal's edits are only `set_duration`. Its `cost` (the
     added effort) is not in the edits, so `plan.compare` understates `total_effort`.
     Before you fork it, append `set_effort {id, hours: <current effort + cost>}`.
2. **Reason.** Choose moves from the review, the levelled schedule and the Monte Carlo
   `sensitivity`. Use one move per variant, so each effect can be measured.

   | Move | Edits | Watch for |
   |---|---|---|
   | Add capacity to the bottleneck | `set_metadata owner`, or more capacity in the schedule | review becomes the next bottleneck |
   | Move work to a cheaper pool | `set_metadata owner`, plus `set_effort` for review time | security- or judgment-heavy work stays put |
   | Crash a critical deliverable | `set_duration`, plus `set_effort` for the added cost | the cost is real effort: record it. Either edit stops Monte Carlo sampling a deliverable that has an `estimate`, so much of a better P80 can come from removing its uncertainty rather than from the shorter length. `set_estimate` with a shorter range keeps it sampled, but lowers its effort basis, so note the crash cost outside the scorecard |
   | Split along file or contract seams | `remove_edge` for each of its dependents, `remove_deliverable`, `add_deliverable` for the parts, then `add_edge` the dependents back | each part needs its own artifact |
   | Contract first | `add_deliverable` with `metadata.contract`; then for each consumer `remove_edge` its dependents, `remove_deliverable` + `add_deliverable` with an `interface` edge (`add_edge` has no `kind`), and `add_edge` the dependents back | the contract must be accepted, not just drafted |
   | Fast-track by removing an edge | `remove_edge` | only if nothing was actually consumed |
   | Split scope into milestones | `add_deliverable` with `"milestone": true` | each milestone needs an acceptance artifact |
3. **Fork.** Run `plan.fork {plan_id, variant, edits}`. The edit ops are `remove_edge
   {from, to}`, `add_edge {from, to, consumes}`, `set_effort {id, hours}`, `set_duration
   {id, hours}`, `set_estimate {id, estimate}`, `set_metadata {id, key, value}`,
   `remove_deliverable {id}` and `add_deliverable {deliverable}`. `remove_deliverable` is
   refused while others depend on it: `remove_edge` each dependent first, then `add_edge`
   them back after the `add_deliverable`. The fork is written to
   `.cpm-planner/plans/<name>/<variant>.json` as a draft. It is not selected.
4. **Measure.** Run `plan.simulate {plan_id, schedule: {capacities}, monte_carlo: {}}` on
   any variant, then `plan.compare {plan: "<name>", schedule: {capacities}, monte_carlo: {}}`.
   This scores every live variant on makespan (levelled when `schedule` is given), P80,
   criticality risk, total effort and peak load. The capacities must cover every pool of
   every variant, or the call fails with `INVALID_CAPACITIES`. P80 comes from Monte Carlo
   on the unlevelled network, so it ignores resource waits. Read `pareto_optimal`, `rank` and
   `rationale` on each `variants[]` entry, and the top-level `recommended`. Change the
   priorities with `weights`.
5. **Explain the trade-off** to the user before you select, in numbers: what each candidate
   gains, what it costs (effort, risk band, P80), and which assumption it depends on (for
   example, that a worker pool exists). Rejected variants are results too.
6. **Select.** Run `plan.select {plan_id}`. Only the selected variant can execute. Complete
   statuses of identically defined deliverables, and their actuals, carry over. Archive
   rejected variants with `plan.archive {name, variant}`. `plan.list` shows the line.

See [examples/improvement-loop.md](examples/improvement-loop.md) for the whole loop with real
responses.

## 7. Execute

Execute the selected variant's `plan_id`.

- **Claim work** with `plan.acquire_cohort {plan_id, caller_id, max_count}`. It hands out
  ready deliverables whose files don't conflict, ordered by priority:
  - narrow it with `ids: [...]`, or with `filter: {"metadata": {"owner": "agent"}}`;
  - pass `ttl_seconds` for long work. The default lease is 5 minutes, up to the server
    maximum;
  - read `blocked`: each requested id that was not leased, with a code (`MANUAL`,
    `NOT_READY`, `LOCKED`, `LAPSE_LIMIT`, `FILE_CONFLICT`, `MAX_COUNT`);
  - `exhausted: true` with `needs_operator: true` means the plan is stalled on
    lapse-limited work, not finished. Fix the environment, then run `plan.force_release
    {plan_id, deliverable_id, reason, reset_counters: true}`.
- **Keep the lease** with `plan.heartbeat {plan_id, deliverable_id, caller_id, ttl_seconds}`
  at least every ttl/3.
- **Report progress and close work** with `plan.mark_status {plan_id, deliverable_id,
  caller_id, status}`:
  - `status` is `{"status": "in_progress"}`, `{"status": "complete"}` or `{"status":
    "failed", "reason": "..."}`;
  - for earned value add `earned_pct` (an integer 0 to 100, only with `in_progress`),
    `actual_effort_hours` (total so far; replaces the leased hours as actual cost) and
    `evidence` (a link or commit, appended to the list; at most 100 entries per
    deliverable. Past that, any mark carrying `evidence`, including `complete`, is refused
    with `INVALID_ACTUALS`, so omit `evidence` then);
  - mark `complete` only when the artifact meets its acceptance criteria, with evidence. A
    report of done is not acceptance. A `failed` mark leaves it Failed (`NOT_READY`);
    retry with a lockless `{"status": "ready"}` mark. Three explicit failures, across
    those retries, trip the circuit breaker.
  - a lockless `in_progress` (owner or manual work, no lease) persists across server
    restarts and is not leasable. Hand it back with a lockless `{"status": "ready"}` (or
    another status) mark; `plan.force_release` does not affect it, since there is no lock.
- **Close owner or manual work** with `plan.accept {plan_id, deliverable_id, accepted_by,
  evidence}`. Evidence is required. `override_lock: true` takes over a live lease, so use it
  only when the holder is gone. `plan.accept` records no hours. To record the actual cost
  (AC), follow it with a lockless `plan.mark_status {plan_id, deliverable_id, caller_id,
  status: {"status": "complete"}, actual_effort_hours}`.
- `plan.status` shows progress, the ready set, `plan_complete` and the held locks.

## 8. Baseline and earned value

1. **Baseline** the selected variant before work starts: `plan.baseline {plan_id, start,
   calendar}`. It freezes the CPM schedule and budgets (effort × `metadata.cost_rate`,
   default 1) as baseline 1.
   - `calendar` is `{hours_per_day, workdays, utc_offset_minutes}`. Without it, PV uses
     wall-clock hours.
   - The working window opens at local midnight, local time being UTC shifted by
     `utc_offset_minutes`, and lasts `hours_per_day` hours. Give `start` as 00:00 local
     on a workday. A 09:00 start with 8 hours per day earns no PV that day.
2. **Read** with `plan.ev {plan_id, as_of}`. It returns `bac`, `pv`, `ev`, `ac`, `sv`, `cv`,
   `spi`, `cpi`, `eac`, `etc`, `vac`, `tcpi`, per-deliverable rows and
   `critical_float_consumed_hours`. A ratio with a zero denominator is `null`; `undefined`
   explains why.
3. **Record** with `plan.snapshot {plan_id, format: "markdown"}` at a steady cadence, such as
   each working day or each cohort. Alerts (`SPI_BELOW_0_9`, `CPI_BELOW_0_9`) fire only
   when the two latest stored snapshots are both below 0.9, so without snapshots there are
   no alerts. A snapshot whose `as_of` is more than an hour in the past is `backfilled` and
   raises none.
4. **Act on alerts.** Find the cause in the rows: overrun effort, slipped critical work, or
   consumed float. Then run the improvement loop (section 6) on the remaining work.

**Re-baseline rules:**
- Re-baseline only for an approved change of scope or plan, never to hide a variance.
- A re-baseline is `plan.baseline` again, with a non-blank `reason`. It keeps actuals and
  snapshots and numbers the new baseline.
- Baselines belong to one variant. A newly selected variant takes its own baseline 1, with
  no reason needed. Baseline it as soon as you select it.

## 9. Revise

To change an executing plan:
1. Edit its file.
2. Run `plan.lint {path}` until it is clean.
3. Run `plan.sync {path}`.

Don't hand-write `plan.revise` graphs. If you used `plan.revise` or `plan.sync {graph}`
inline, run `plan.export {plan_id}` to write the head back to the file.

The `diff` in the response lists `added`, `removed`, `changed`, `reopened` and
`released_locks`. The carry-over rules:
- an unchanged deliverable keeps its status and counters;
- a changed deliverable keeps its status, unless its set of prerequisite ids changed. Then a
  complete, ready or pending deliverable is re-derived, and so are its dependents,
  transitively;
- a deliverable with an `interface` edge to a changed `metadata.contract` deliverable is
  reopened;
- new deliverables start ready or pending, according to their prerequisites;
- removing a leased deliverable is refused with `LOCK_HELD` unless you pass `force: true`,
  which releases its lock;
- in-progress leases survive, and failed deliverables stay failed.

After a revise, check `reopened` and resolve every reopened deliverable. If scope changed,
re-baseline it with a reason (section 8).
