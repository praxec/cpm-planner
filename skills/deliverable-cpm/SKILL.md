---
name: deliverable-cpm
description: "Use when you need the full method reference behind the cpm-plan, cpm-improve, cpm-run, cpm-ev and cpm-revise skills, or when no narrower cpm-* skill fits a cpm-planner (plan.* tools) task that spans the whole lifecycle. Covers the deliverable-based CPM method end to end: deliverables as artifacts, consumption edges, continuity threads, gate evidence scopes, lint findings, sync and drift, resource levelling on working calendars, the optimize/review/fork/simulate/compare/select improvement loop, overview and explain, leases, evidence records and acceptance, levelled baselines and earned value, and revising a tracked cpm-planner plan file."
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
  `metadata.kind: "manual"`. It is closed with `plan.accept`. Declare who signs with
  `metadata.evidence_scope`: `manual_approval` for an owner inside the team,
  `external_signoff` for a party outside it (section 7).
- **Group steps one agent should carry** with `metadata.thread` ([Threads](#threads)).
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
| `EVIDENCE_SCOPE_INVALID` | error | `metadata.evidence_scope` must be a scope the kind may declare: work `focused`; gate `gate`, `merge`, `publish` or `activation`; manual `manual_approval`, `external_signoff`, `merge`, `publish` or `activation` |
| `GATE_CANDIDATE_INVALID` | error | point `metadata.candidate` at a direct prerequisite of the gate, or add that edge |
| `GATE_SCOPE_MISSING` | warning | declare the gate's `metadata.evidence_scope`: `gate` for a regression or CI run, else `merge`, `publish` or `activation` |
| `GATE_CANDIDATE_MISSING` | warning | name the direct prerequisite that produces the integrated candidate in `metadata.candidate` |
| `SCOPE_FOLDED` | warning | split the approval, sign-off, merge, publish or activation the text names into its own deliverable (`manual` for approvals) with the matching `evidence_scope`; `metadata.scope_folded: false` if the text only mentions it |
| `THREAD_INVALID` | warning | use a token of 1 to 64 ASCII letters, digits, `.`, `_` or `-` starting with a letter or digit, or remove `metadata.thread`; until then the deliverable is unthreaded |
| `THREAD_BROKEN` | warning | a thread must be one dependency chain with no other thread inside it: add the edge if one step consumes the other, move the work into the thread, or end the thread and start a new one |

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

**Declare what closes each gate.** Set `metadata.evidence_scope` on every gate: `gate` for a full
regression, end-to-end or CI run, or `merge`, `publish` or `activation` for that step. A gate whose
scope is `gate` also names `metadata.candidate`, the direct prerequisite that produces the integrated
candidate it verifies (the integration-candidate deliverable, or the last work before it). The
candidate's evidence identity then becomes the target the gate's evidence must match, so a green run
on a different tree, lockfile, toolchain, test contract or environment cannot close it. A milestone
that is also a gate (`"milestone": true` with `metadata.kind: "gate"`) follows the gate rules and must
declare a scope. Keep one operation per deliverable: an approval, sign-off, merge, publish or deploy
folded into a work or gate deliverable is `SCOPE_FOLDED`. Reuse evidence across commits only when the
whole identity is identical ([docs/evidence.md](https://github.com/praxec/cpm-planner/blob/main/docs/evidence.md)). Native release workflows compile to the same idea through `delivery_gates`; see
[docs/release-workflows.md](https://github.com/praxec/cpm-planner/blob/main/docs/release-workflows.md).

### Threads

A thread groups deliverables one agent session should do in order, so the context it builds on one
step carries into the next: the failing tests and then the implementation of one feature, or a
contract and its first consumer. Put the same token in `metadata.thread` on each step (1 to 64 ASCII
letters, digits, `.`, `_` or `-`, starting with a letter or digit). When a caller completes a
threaded deliverable, `plan.acquire_cohort` offers that caller the thread's next ready step first.
It is a preference, never a lock: file conflicts, prerequisites, manual and gate exclusion, the
lapse limit and the circuit breaker all still apply.

**Size deliverables per thread.**
- **Thread the steps that share context, not everything one person owns.** A thread is worth having
  when the second step is cheaper for the agent that did the first: the same files, the same
  contract, the same failing test.
- **Keep each step a whole artifact.** Threading is not a reason to cut work into
  micro-deliverables; each step still has its own artifact, acceptance criteria and owned files, and
  could be done by a fresh agent if it had to.
- **Keep threads short.** Two to five deliverables, a few hours to a working day in total, is what one
  agent session holds in context. Split a longer chain into two threads at a natural hand-off, such
  as a contract document another agent can read.
- **Make a thread one dependency chain.** Each step after the first consumes the one before it,
  directly or through steps of the same thread. A thread split across parallel branches or
  interleaved with another thread's step is `THREAD_BROKEN`.
- **Leave parallel work unthreaded.** Deliverables that can run beside the thread belong to other
  threads or none, so other agents can take them meanwhile.
- **Name threads by the line of work** (`evidence`, `overview`, `lint`), not by the agent; who holds a
  thread is decided at lease time.

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

**Working calendars.** Agents and CI run around the clock; people work office hours. Pass
`calendar` (and `start`, hour 0 as RFC 3339, default now) to place each deliverable in its
resource's working windows:

```text
plan.schedule {"plan_id": "...", "capacities": {"agent": 3, "owner": 1},
               "start": "2026-11-13T19:00:00Z",
               "calendar": {"default": {"hours_per_day": 24, "workdays": ["mon","tue","wed","thu","fri","sat","sun"]},
                            "owner": {"hours_per_day": 8, "day_start": "09:00",
                                      "workdays": ["mon","tue","wed","thu","fri"], "utc_offset_minutes": -300}}}
```

- A calendar is `hours_per_day` (0 < h <= 24, default 8), `day_start` (`HH:MM` local, default
  `00:00`), `workdays` (default mon..fri) and `utc_offset_minutes` (fixed, default 0). An object
  with a `default` key is a per-resource map keyed by the `resource_key` value (default
  `metadata.owner`); `default` covers every resource without its own entry. A single calendar
  applies to everyone. A deliverable with `metadata.wall_clock_only: true` ignores calendars.
- Hours are then wall-clock hours from `start`; rows add `start_at`, `finish_at`, `calendars` and
  `calendar_wait_hours`, and `cpm_makespan` is the unlevelled schedule on the same calendars.
- Work that becomes ready outside its window starts at the next open, and longer work pauses at
  each close. A driving-chain step that waited for a window reports `waited_on: "calendar"` with
  `waited_on_calendar` and `calendar_wait_hours`: a handover to a person late on Friday shows here.
- Calendar errors: a map entry or `day_start` that is malformed is `INVALID_VALUE`; a single
  calendar keeps `INVALID_PARAMS` for unknown fields and `INVALID_GRAPH` for out-of-range values.
  `plan.status` and `plan.lint` stay calendar-free.

## 6. The improvement loop

Design several variants and pick one. Never edit the selected variant to try an idea.

1. **Optimize first.** Run `plan.optimize {path, capacities}` (one of `graph`, `plan_id` or
   `path`; optional `capacities`, `resource_key`, `project_buffer_pct`, `max_proposals`, default
   32). It is offline, deterministic and free: no model, provider or network call, the same
   proposals for the same graph, and it never writes. It looks for structural waste the graph shows
   by itself:
   - `merge`: two same-owner work deliverables claim the same file exclusively;
   - `false_dependency`: an edge lint flags `RESOURCE_CONSTRAINT_EDGE`, `NO_RATIONALE` or
     `LOW_INFORMATION_CONSUMES`;
   - `parallel_cohort`: three or more work deliverables joined only by such edges;
   - `gate_consolidation`: two consecutive waves, the first with a single gate and no
     `boundary_reason`;
   - `gate_bypass`: work waits on a gate but consumes neither its result nor its artifact.

   Each proposal is a `plan.fork` edit list already applied to a copy and checked by lint, the
   schedule and simulate. It never lengthens the makespan (the levelled one with capacities) or adds
   effort, and it keeps every acceptance criterion, gate, milestone, manual or external approval,
   interface contract, interface edge and owned file. Read `deltas` (`after − before`, so negative
   is better), `score`, `rationale`, `preserved` and `trade_offs`, and `rejected` with its
   `reasons`. Optimize suggests; you still decide whether each edge really hands nothing over.
2. **Review (optional, AI).** Run `plan.review {path, capacities}` after optimize, for judgments
   the graph cannot make alone (whether an edge that names an artifact is still unnecessary, which
   crash technique fits). It sends one batched request
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
3. **Reason.** Choose moves from the optimize proposals, the review, the levelled schedule and the Monte Carlo
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
4. **Fork.** Run `plan.fork {plan_id, variant, edits}`. The edit ops are `remove_edge
   {from, to}`, `add_edge {from, to, consumes}`, `set_effort {id, hours}`, `set_duration
   {id, hours}`, `set_estimate {id, estimate}`, `set_metadata {id, key, value}`,
   `remove_deliverable {id}` and `add_deliverable {deliverable}`. `remove_deliverable` is
   refused while others depend on it: `remove_edge` each dependent first, then `add_edge`
   them back after the `add_deliverable`. The fork is written to
   `.cpm-planner/plans/<name>/<variant>.json` as a draft. It is not selected.
5. **Measure.** Run `plan.simulate {plan_id, schedule: {capacities}, monte_carlo: {}}` on
   any variant, then `plan.compare {plan: "<name>", schedule: {capacities}, monte_carlo: {}}`.
   This scores every live variant on makespan (levelled when `schedule` is given), P80,
   criticality risk, total effort and peak load. The capacities must cover every pool of
   every variant, or the call fails with `INVALID_CAPACITIES`. P80 comes from Monte Carlo
   on the unlevelled network, so it ignores resource waits. `plan.simulate` also takes `calendar`
   and `start` (section 5) at the top level and applies them to the levelled schedule and to Monte
   Carlo, so P50/P80 become wall-clock hours that include the nights and weekends of people's steps. Read `pareto_optimal`, `rank` and
   `rationale` on each `variants[]` entry, and the top-level `recommended` (with
   `tied_with_recommended` when scores tie; then the pick is only a tie-break). Change the
   priorities with `weights`.
6. **Explain the trade-off** to the user before you select, in numbers: what each candidate
   gains, what it costs (effort, risk band, P80), and which assumption it depends on (for
   example, that a worker pool exists). Rejected variants are results too.
7. **Select.** Run `plan.select {plan_id}`. Only the selected variant can execute. Complete
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

- **See where the plan is** with `plan.overview {plan | plan_id | active, as_of?, detail?,
  limit?, filter?: {owner, milestone}, evidence_target?, format?}`. It is read-only and
  deterministic, and joins `plan.status`, `plan.get` and `plan.ev` at one `as_of`: `counts`,
  `ev`, `critical_path` (with `remaining_hours` and the milestones each step feeds),
  `milestones` (with `next_on_path`), `ready` (each with `action`: `acquire`, `accept` for manual
  or gate work, or `operator` at a failure or lapse cap, plus `file_conflicts`), `blocked` (with
  `class` `dependency`, `manual`, `external` or `gate`, and its `root_blockers`), `leases`,
  `attention`, `evidence_counts` and `stale_evidence`. Every entry carries its `evidence` verdict:
  `current`, `stale` (with the `changed` fields), `unknown`, `unbound` (notes only) or `none`.
  `format: "markdown"` prints a short report for people; `detail: "full"` adds chains and files.
  Filters narrow the lists, never the counts or EV.
- **Ask why one deliverable is not moving** with `plan.explain {plan | plan_id | active,
  deliverable_id?, as_of?, limit?, format?}`. It reports the deliverable's `state` (`complete`,
  `leased`, `in_progress`, `ready` with its `action`, `blocked`, `failed`), the blocker `chain`
  to its driving root and that root's `action`, what completing it `unlocks`, the milestones it
  feeds with their `slack_hours`, and `critical_path_impact` in one sentence. Without a
  `deliverable_id` it returns only `next_work`, every startable or blocked deliverable in exactly
  one class: `file_conflicting`, `ready_critical`, `ready_slack`, `blocked_manual`,
  `blocked_external`, `blocked_gate`, `blocked_dependency`. Take `ready_critical` first, act on
  root blockers rather than on what they block, and chase people for `blocked_manual` and
  `blocked_external`.
- **Claim work** with `plan.acquire_cohort {plan_id, caller_id, max_count}`. It hands out
  ready deliverables whose files don't conflict, ordered by priority:
  - narrow it with `ids: [...]`, or with `filter: {"metadata": {"owner": "agent"}}`;
  - pass `ttl_seconds` for long work. The default lease is 5 minutes, up to the server
    maximum;
  - keep a `caller_id` stable per agent session: when it completes a threaded deliverable, the
    next acquire by that caller ranks the thread's next ready step first, then unthreaded work and
    threads nobody advanced, then threads another caller advanced. `thread: "<token>"` asks for one
    thread explicitly (an invalid token is `INVALID_VALUE`). `affinity_seconds` (default 0, clamped
    to the maximum TTL) holds back a deliverable whose thread another caller advanced less than that
    long ago, so its agent can come back for it. The ranking only reorders what the acquire would
    lease anyway;
  - read `blocked`: each requested id that was not leased, with a code (`MANUAL`,
    `NOT_READY`, `LOCKED`, `LAPSE_LIMIT`, `FILE_CONFLICT`, `MAX_COUNT`, or `AFFINITY_HOLD`
    naming the affine caller and the seconds left);
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
    `evidence` (appended to the list; at most 100 entries per deliverable. Past that, any
    mark carrying `evidence`, including `complete`, is refused with `INVALID_ACTUALS`, so omit
    `evidence` then);
  - mark `complete` only when the artifact meets its acceptance criteria, with evidence. A
    report of done is not acceptance. A `failed` mark leaves it Failed (`NOT_READY`);
    retry with a lockless `{"status": "ready"}` mark. Three explicit failures, across
    those retries, trip the circuit breaker.
  - a lockless `in_progress` (owner or manual work, no lease) persists across server
    restarts and is not leasable. Hand it back with a lockless `{"status": "ready"}` (or
    another status) mark; `plan.force_release` does not affect it, since there is no lock.
- **Record evidence with its identity.** `evidence` on `plan.mark_status` and `plan.accept` is a
  note string (up to 2048 characters; it can never satisfy a declared scope) or a record:

  ```json
  {"schema_version": 1, "scope": "focused",
   "summary": "cargo nextest run --test export_service: 14 passed, 0 failed",
   "ref": "https://ci.example/runs/4411",
   "identity": {"schema_version": 1, "tree": "git-tree:<hex of HEAD^{tree}>",
                "dependency_lock": "sha256:<lockfile hash>", "toolchain": "rustc 1.89.0; cargo-nextest 0.9.104",
                "test_contract": "nextest:export_service@sha256:<config digest>",
                "environment": "linux-x86_64; ubuntu-24.04"}}
  ```

  The scope says what the record proves, and who records it:

  | Scope | Proves | Recorded by | Identity |
  |---|---|---|---|
  | `focused` | the deliverable's own targeted checks passed | the worker, on `mark_status complete` | all five fields |
  | `gate` | a full regression, end-to-end or CI run on one integrated candidate | the gate's `plan.accept` | all five fields |
  | `manual_approval` | a named owner **inside the team** approved the artifact | the owner's `plan.accept` | optional |
  | `external_signoff` | a party **outside the team** (customer, auditor, vendor) accepted it; needs `party` | the owner's `plan.accept` | optional |
  | `merge`, `publish`, `activation` | the merge, the release or package push, the deploy or flag flip | that step | `tree` (activation adds `environment`) |

  Work closes with `focused` or `gate`; a gate with exactly its declared scope (default `gate`);
  a manual deliverable with its declared scope, or any scope when it declares none. A gate with
  `metadata.candidate` closes only with a record that is `current` against the newest identity
  the candidate recorded, so record the candidate's `focused` evidence with an identity and run
  the gate on that same tree. The server adds `recorded_at`, `recorded_by`, `via`,
  `plan_revision` and `definition_digest`; never send them. Refusals roll back the whole call:
  - `INVALID_EVIDENCE`: a malformed record or identity (unknown field, scope or version, a value
    out of bounds, a missing identity field, `party` on the wrong scope);
  - `EVIDENCE_SCOPE_MISMATCH`: the record's scope may not close this kind of deliverable;
  - `EVIDENCE_REQUIRED`: a declared `evidence_scope` is not met by this call or a current record;
  - `EVIDENCE_STALE`: the gate's record differs from its candidate's identity; the message names
    the changed fields.

  A stale verdict in `plan.overview` (a changed tree, lock, toolchain, test contract,
  environment or definition) is reported, never acted on: it reopens nothing. Re-run the check, or
  revise the plan (section 9).
- **Close owner, gate or manual work** with `plan.accept {plan_id, deliverable_id, accepted_by,
  evidence}`. Evidence is required, and is stored with the deliverable as well as audited. `override_lock: true` takes over a live lease, so use it
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
  green run's `gate` record for the final tree. After 2 red runs, or for a fix that changes scope or a
  contract, stop and take it through section 9 so the gate consumes a new deliverable. Mark
  `failed` only when it needs a decision. After the merge, delete the wave's merged worktrees and
  branches.
- **Re-optimise at natural pauses (adaptive execution).** When a cohort closes or a gate returns,
  check for these signals: `plan.ev` shows SPI or CPI below 0.9, a gate took over 1.5 times its
  estimate, the `critical_path` moved, a deliverable overran its estimate by 50%, `plan.overview`
  lists stale evidence, or a ready deliverable is waiting only on capacity. On a signal, run section 6 on the *remaining* work, and
  apply the result when it saves more than it costs. Stay non-disruptive: change only unleased,
  incomplete work, never interrupt a lease or a running gate, don't reopen completed work, and keep
  the current plan when the saving is small or uncertain. With no signal, keep working. A scope
  change still needs its usual approval.

## 8. Baseline and earned value

1. **Baseline** the selected variant before work starts: `plan.baseline {plan_id, start,
   calendar, capacities?, resource_key?, project_buffer_pct?, schedule?}`. It freezes a
   schedule and the budgets (effort × `metadata.cost_rate`, default 1) as baseline 1.
   - **Levelled or CPM.** With `capacities` (as `plan.schedule`) it freezes the levelled
     schedule; without, the CPM schedule. When a fixed pool executes the plan (three agents, one
     owner), baseline the levelled schedule: a CPM baseline assumes unlimited workers, so PV runs
     ahead of what the pool can do and SPI reads low from the first day. `schedule: "levelled"`
     or `"cpm"` forces the kind; `"cpm"` is the only way back from a levelled baseline, and
     `capacities` with it is `INVALID_VALUE`, as is `"levelled"` without capacities.
   - **Calendars.** `calendar` is one calendar or a per-resource map with a `default` entry, as
     for `plan.schedule` (section 5). Give agents and CI a 24/7 calendar and people their working
     hours with `day_start`:
     `{"default": {"hours_per_day": 24, "workdays": ["mon","tue","wed","thu","fri","sat","sun"]},
     "owner": {"hours_per_day": 8, "day_start": "09:00", "utc_offset_minutes": 60}}`. With a map,
     each row's PV accrues only while its calendar is open: flat overnight and over weekends for a
     person's row, around the clock for an agent's. A planned wait for 09:00 is then no schedule
     variance. One calendar for everyone (the plan clock) puts agent work into office hours it
     does not keep. Without `calendar`, PV uses wall-clock hours. `day_start` defaults to `00:00`,
     so set it to when people start.
   - **Reuse and reset.** A re-baseline reuses the previous baseline's `calendar`, `capacities`,
     `resource_key` and `project_buffer_pct` unless you pass them. `"calendar": null` clears the
     calendar back to wall-clock hours; `"schedule": "cpm"` drops the capacities. `start` is never
     reused.
   - The result reports `schedule`, `capacities`, `resource_key`, `project_buffer_hours`,
     `finish_hours` (the frozen finish, levelled when levelled) and `calendar_wait_hours` (the
     planned window waits on the driving chain). `plan.status` reports the latest `baseline` as
     `{baseline_number, as_of, schedule}`. The automatic first baseline taken by execution is
     always CPM on wall-clock hours; re-baseline it with a reason when the pool is fixed.
2. **Read** with `plan.ev {plan_id, as_of}`, and never calculate earned value by hand: progress,
   budgets and actuals are all stored on the plan, and `plan.ev` is the only source of these
   numbers. If it fails, report the error and fix the cause. It returns `bac`, `pv`, `ev`, `ac`, `sv`, `cv`,
   `spi`, `cpi`, `eac`, `etc`, `vac`, `tcpi`, per-deliverable rows (each with its planned
   `calendar_wait_hours`), `critical_float_consumed_hours` (against the levelled finish for a
   levelled baseline), `schedule`, `capacities` (levelled baselines only) and the total
   `calendar_wait_hours`. A ratio with a zero denominator is `null`; `undefined` explains why.
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
