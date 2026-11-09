---
name: deliverable-cpm
description: "Use when you need the full method reference behind the cpm-plan, cpm-improve, cpm-run, cpm-ev and cpm-revise skills, or when no narrower cpm-* skill fits a cpm-planner (plan.* tools) task that spans the whole lifecycle. Covers the deliverable-based CPM method end to end: deliverables as artifacts, consumption edges, lint findings, sync and drift, resource levelling, the review/fork/simulate/compare/select improvement loop, leases and acceptance, baselines and earned value, and revising a tracked cpm-planner plan file."
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

## Calling the tools

The tools are the same whichever way you reach them; the method names and
arguments in this skill do not change.

- **Registered MCP server.** When `cpm-planner` is registered with your agent
  host, call the tools directly (`plan.status plan_id=<id>` with its arguments).
- **Through a host wrapper.** When the server is not registered, a wrapper can
  spawn the binary per call. With Junior, `junior tools call cpm plan.status
  key=value ...` takes inline arguments (values parse as JSON),
  `echo '{"tool":"plan.status","arguments":{...}}' | junior tools call cpm -`
  reads the request from stdin, and `junior tools call cpm request.json`
  reads it from a file. Each call prints the bare result.

Do not register a server just to follow this skill; use whichever route your
host already has.

## 1. Nodes are artifacts

Each deliverable names what can be inspected when it is done, in `metadata.artifact`:
- a document;
- code with its targeted tests passing (the full suite belongs to a gate, section 3);
- a published release;
- a deployed service with evidence;
- an evidence report;
- a decision record.

Rules:
- **Turn activities into artifacts.** "Review", "rulings", "gate" and "run" become what they
  produce: a sign-off recorded in the contract, a decision record, an evidence report.
- **Fold acceptance review into the deliverable it accepts.** Don't make it a separate node.
  Add the reviewer's time to that deliverable's estimate. The exception is a fixed-cost
  operation shared by many deliverables: that is a gate of its own (section 3).
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

If nothing is consumed, there is no edge. **Edges are physical hand-overs only.** "Only one
agent is free" is not a dependency: capacity belongs in `plan.schedule`, where it can change
without rewriting the graph. An edge that encodes it turns a worker limit into a false critical
path, and lint flags the common wordings as `RESOURCE_CONSTRAINT_EDGE`.

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
| `RESOURCE_CONSTRAINT_EDGE` | warning | delete the edge; set the limit in `plan.schedule` capacities |
| `UNDECLARED_GATE` | warning | move the heavy operation to a gate and keep targeted checks here; `metadata.heavy: false` if it only mentions one |
| `UNGATED_WORK` | warning | make a gate consume the work, or cut it |
| `WAVE_TOO_SMALL`, `GATE_TIME_DOMINATES` | warning | merge waves so more work shares each gate run |
| `WAVE_BOUNDARY_UNJUSTIFIED` | warning | merge into the next wave, or set `metadata.boundary_reason` on the gate |
| `GATE_NO_DURATION` | warning | give the gate `duration_hours` or a three-point `estimate` from observed runs |
| `GATE_WITHOUT_MILESTONE` | warning | add the milestone the gate proves, or cut the gate |
| `NO_ARTIFACT`, `NO_MILESTONE` | info | add `metadata.artifact` or a milestone |
| `WORK_WAITS_ON_GATE` | info | consume the code or contract instead, unless the work needs the proven result |

### Software delivery: code in bulk, gate at milestones

The plan is the execution contract for LLM workers, who do what the deliverable in front of them
says. So decide where every expensive operation happens when you author the plan, and maximise the
coding between those points.

**Classify every operation.**

| Cheap: inside any work deliverable | Heavy: only in a gate |
|---|---|
| incremental build of the touched package | full or release build, cross-platform matrix |
| lint and format of touched files | full regression or end-to-end suite |
| tests for changed code and affected contracts and consumers | any push or PR action that starts a pipeline |
| local commits and local merges | opening or merging a PR, promotion, deploy, tag, publish |

Read the repository's CI triggers before planning, so you know which actions start a pipeline.

**Make heavy work a gate.** A gate is a deliverable with `metadata.kind: "gate"`, an observed
`duration_hours` or `estimate`, and usually its own `metadata.owner` pool such as `ci`. It is never
leased and it closes with `plan.accept`, so it cannot close without evidence or before the work it
consumes is complete. Work deliverables list their cheap checks in `metadata.verify`; the lease
response returns them, so a worker's instructions come from the plan. See
[examples/milestone-plan.json](examples/milestone-plan.json).

**Use this shape.** Work deliverables commit to one integration branch, or to local branches merged
into it. An integration-candidate deliverable consumes them and runs the build and lint once on the
merged tree, so the gate isn't wasted on a compile error. The regression gate consumes that
candidate, and the PR, merge and release gates or milestone follow it.

**Run the full suite once per candidate, before the pull request.** If the regression can run on the
integration branch, run it there, then open the PR. If it runs only as PR checks, opening the PR is
the gate, so open it once, when the candidate is complete. Don't pay for the same suite twice on one
tree: when PR checks repeat it, reuse the evidence if policy accepts it, otherwise model the second
run as a gate so its cost is visible.

**Fewer waves is better.** Start from one wave for the whole project. Add a boundary only for a
concrete reason, and record it in `metadata.boundary_reason`: later work consumes a *released*
artifact, a person's decision at that point changes later scope, an irreversible migration, or a
trust or approval boundary. Size alone isn't a reason. Later work consumes code and contracts rather
than a gate's receipt wherever it can, so coding keeps going while a gate runs.

**Read the computed structure.** `plan.lint` returns `waves`, so nobody has to judge from prose
whether a milestone is heavy:

- each `waves[]` entry has its `gates`, the `deliverables` it proves, `work_hours`, `gate_hours` and
  `work_per_gate_hour`;
- each `milestones[]` entry has a `class`: `wave` when a gate proves it, `checkpoint` when nothing
  heavy happens there;
- `ungated` lists work no gate proves, and `gate_fraction` is gate time's share of all scheduled time.

**Keep testing honest.** Author tests with the code. Batching delays the full suite, not testing.
Choose targeted scope from dependency and contract impact, not just changed filenames, and broaden
it when a failure, shared dependency, migration, auth or privacy boundary, or uncertain blast radius
warrants it. If policy truly requires per-change evidence, say so and model those gates; if a
repository rule or hook runs the full suite on every commit, raise the conflict rather than bypass
it. Implementation-complete with the gate pending is what the graph itself shows.

Record the evidence identity in gate metadata: complete tree or image, dependency lock/toolchain,
test-contract version and environment. Reuse evidence across commits only when that identity is
identical. Native release workflows compile to the same idea through `delivery_gates`; see
[docs/release-workflows.md](https://github.com/praxec/cpm-planner/blob/main/docs/release-workflows.md).

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
     `split_candidate`, `interface_split`, `crash_option`, `wave_merge` (two waves could share
     one gate run) and `merge_candidate` (two small adjacent deliverables are one artifact).
     Check each one against what you know.
   - With `CPM_REVIEW_PROVIDER=command`, an operator's adapter answers instead of this process,
     so this server needs no key; `cpm-planner review doctor` reports readiness by name only.
   - `proposals` are ready-made `plan.fork` edit lists. Each has been checked by simulation
     (it lints clean and shortens the makespan) and is ranked by `hours_saved / max(cost, 1)`.
   - An `add_capacity` crash proposal's edits are only `set_duration`. Its `cost` (the
     added effort) is not in the edits, so `plan.compare` understates `total_effort`.
     Before you fork it, append `set_effort {id, hours: <current effort + cost>}`.
   - **Then vet the critical path until it stabilises.** Review the whole plan once, then
     loop: run `plan.review {plan_id, scope: "critical_path"}`, which spends every question on
     edges between zero-float deliverables. Check each `false_dependency` finding against the
     artifact it claims to consume, and remove the false edges in a fork or the file. Review
     again: if the reported `critical_path` moved to other deliverables, those edges are now the
     ones to vet. Stop when a pass changes nothing, the chain repeats a chain you already vetted,
     or after 5 passes, and report which. Remove an edge only on your own check of the hand-over;
     a probability alone is not proof.
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
   | Merge waves | remove the earlier wave's gates, repoint their dependents, and let the later gate prove both | count added integration effort and the longer feedback batch; keep a boundary only with a real `boundary_reason` |
   | Reuse immutable evidence | make the consumer depend on a prior evidence artifact and remove the duplicate gate | the complete tree/image, lock/toolchain, test contract and environment identity must all match |

   The graph has no cache-hit or conditional-execution primitive. Model reuse as explicit
   edges and metadata, and disclose that approximation when it materially affects the forecast.
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
   `rationale` on each `variants[]` entry, and the top-level `recommended` (with
   `tied_with_recommended` when scores tie; then the pick is only a tie-break). Change the
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

Execute the selected variant. Execution calls take exactly one of `plan_id`, `plan` (a
plan line name) or `active: true` (the project's single active line), with an optional
`project` for `plan`/`active`. Find the plan with `{"active": true}` (or
`{"plan": "<line>"}`) rather than a remembered `plan_id`, then keep the `plan_id`
returned by `plan.acquire_cohort` for the lease: `active` matches only an incomplete
line and changes when another line goes live.

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
- **Hand work over with its contract.** A worker or subagent gets the deliverable's artifact,
  `owned_files`, `metadata.verify` and integration branch, plus this rule: run only those checks,
  and do not push, open a PR or run the full suite.
- **Run a gate only when `plan.status` lists it as ready.** That is the graph's guard against an
  early regression run. Mark it with a lockless `in_progress`, start it once and pick up other
  ready work while it runs; the wait belongs to the runner, not to an agent.
- **A red run is not a `failed` mark.** Failed marks trip the circuit breaker, and red runs are a
  normal part of a gate. Record the run as `evidence` on a lockless `in_progress`, read the whole
  run, fix every failure with focused checks, then re-run once. Close with `plan.accept` and the
  green run's evidence for the final tree. After 2 red runs, or for a fix that changes scope or a
  contract, stop and take it through section 9 so the gate consumes a new deliverable. Mark
  `failed` only when it needs a decision. After the merge, delete the wave's merged worktrees and
  branches.
- **Re-optimise at natural pauses (adaptive execution).** When a cohort closes or a gate returns,
  check for these signals: `plan.ev` shows SPI or CPI below 0.9, a gate took over 1.5 times its
  estimate, the `critical_path` moved, a deliverable overran its estimate by 50%, or a ready
  deliverable is waiting only on capacity. On a signal, run section 6 on the *remaining* work, and
  apply the result when it saves more than it costs. Stay non-disruptive: change only unleased,
  incomplete work, never interrupt a lease or a running gate, don't reopen completed work, and keep
  the current plan when the saving is small or uncertain. With no signal, keep working. A scope
  change still needs its usual approval.

## 8. Baseline and earned value

1. **Baseline** the selected variant before work starts: `plan.baseline {plan_id, start,
   calendar}`. It freezes the CPM schedule and budgets (effort × `metadata.cost_rate`,
   default 1) as baseline 1.
   - `calendar` is `{hours_per_day, workdays, utc_offset_minutes}`. Without it, PV uses
     wall-clock hours.
   - The working window opens at local midnight, local time being UTC shifted by
     `utc_offset_minutes`, and lasts `hours_per_day` hours. Give `start` as 00:00 local
     on a workday. A 09:00 start with 8 hours per day earns no PV that day.
2. **Read** with `plan.ev {plan_id, as_of}`, and never calculate earned value by hand: progress,
   budgets and actuals are all stored on the plan, and `plan.ev` is the only source of these
   numbers. If it fails, report the error and fix the cause. It returns `bac`, `pv`, `ev`, `ac`, `sv`, `cv`,
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
