---
name: cpm-plan
description: Use when planning work as artifact deliverables with dependencies, estimating effort, finding the critical path (CPM), checking milestones, sizing deliverables into agent threads, declaring gate evidence scopes, or levelling the schedule against real capacity and working calendars to see the critical path and the levelled bottleneck (resource levelling, makespan). Covers authoring a cpm-planner plan file of artifact deliverables and consumption edges, plan.lint until clean, plan.sync, reading critical_path and milestones in plan.status, and plan.schedule with capacities and per-resource calendars.
---

# cpm-plan: author, lint, sync and level a plan

Turn the user's goal into a tracked plan file, register it and check that its critical path
and levelled schedule make sense. The full method (artifact rules, edge rules, examples) is
in [the deliverable-cpm skill](../deliverable-cpm/SKILL.md); this skill is the short path.
Summarises deliverable-cpm; if they ever disagree, the server's behaviour wins.

Calling the tools: see [Calling the tools](../deliverable-cpm/SKILL.md#calling-the-tools)
(registered MCP server or a host wrapper such as `junior tools call`).

## When to use

- A new project or plan line, or a new first variant (`main`); "what is the critical path",
  "how long with N agents", "where is the bottleneck".
- Next: shorten it with `cpm-improve`, run it with `cpm-run`, baseline it with `cpm-ev`.
- Not for: shortening or comparing variants → use cpm-improve; changing a synced plan → use cpm-revise.

## 1. Write the file

The only definition is `.cpm-planner/plans/<name>/main.json`, relative to the project root
(`CPM_PROJECT_ROOT`, else the nearest ancestor with `.cpm-planner/` or `.git`). No scratch copies.
- every deliverable is an inspectable artifact, named in `metadata.artifact`; turn activities
  ("review", "gate") into what they produce, and fold acceptance review into what it accepts;
- `metadata.owner` is the executor class (`agent`, `worker`, `owner`, ...); `metadata.kind:
  "manual"` is work no agent may lease (closed with `plan.accept`); acceptance points are `"milestone": true`;
- `owned_files` lists exactly the paths the implementer writes;
- effort is `estimated_effort_hours`, or a three-point `estimate` `{optimistic, likely, pessimistic}`;
- `earning_rule`: `zero_hundred` for 2 h or less, `fifty_fifty` or `weighted` for longer;
- an edge exists only when B consumes something A produces:
  `{"id": "A", "consumes": "request/response schema", "kind": "interface"}` (`interface` needs
  `metadata.contract: true` on A). Both kinds are finish-to-start; `lag_hours` is a minimum wait.

### Code in bulk; put heavy work in gates

Anything slow and fixed-cost is a gate, `metadata.kind: "gate"`: full build, full regression, a push
or PR that starts CI, merge, deploy, release. Give it an observed `duration_hours` or `estimate`.
Work lists only cheap targeted checks in `metadata.verify`. Read the CI triggers first.

Declare what closes each gate: `metadata.evidence_scope` is `gate` for a regression or CI run, or
`merge`, `publish`, `activation` for that step; a `gate`-scoped gate names its
`metadata.candidate`, the direct prerequisite that produces the integrated candidate. One step per
deliverable: an approval or sign-off is its own `manual` deliverable with `evidence_scope`
`manual_approval` (owners in the team) or `external_signoff` (a party outside it). A milestone that
is also a gate follows the gate rules.

Shape: work → integration candidate (one build and lint on the merged tree) → regression gate → PR
and release. Default to **one wave**; add a boundary only for a concrete `metadata.boundary_reason`
(a released artifact consumed later, a human decision that changes scope, an irreversible migration,
a trust boundary). Edges are physical hand-overs only; worker limits go in `plan.schedule`. Examples:
[milestone-plan.json](../deliverable-cpm/examples/milestone-plan.json),
[small-plan.json](../deliverable-cpm/examples/small-plan.json); method in
[deliverable-cpm](../deliverable-cpm/SKILL.md#software-delivery-code-in-bulk-gate-at-milestones).

### Size deliverables per thread

`metadata.thread` (1-64 of letters, digits, `.`, `_`, `-`) groups steps one agent should do in
order; leasing offers the next step to the agent that finished the last ([threads](../deliverable-cpm/SKILL.md#threads)).
- Thread steps that share context (same files, contract or failing test), not everything one owner has.
- Keep each step a whole artifact with its own acceptance criteria and owned files.
- Keep threads short: 2 to 5 deliverables, a few hours to a day. Split longer chains at a hand-off.
- A thread is one dependency chain; leave parallel work unthreaded or in other threads.
- Name threads by the line of work (`evidence`, `lint`), never by the agent.

## 2. Lint until clean

`plan.lint {"path": ".cpm-planner/plans/<name>/main.json"}`. Fix every finding and lint again until
`clean` is true. Never work around a finding.

| Code | Fix |
|---|---|
| `CYCLE` | break the loop it lists; one edge is not real consumption |
| `UNKNOWN_PREREQUISITE`, `DUPLICATE_ID`, `RESERVED_ID`, `INVALID_VALUE` | fix the id or value |
| `UNORDERED_FILE_OVERLAP` | order the two, split the file, or make every claim `{path, mode: "append"}` |
| `TOO_MANY_DELIVERABLES` | at most 5000; split into several plan lines |
| `REDUNDANT_EDGE`, `NO_RATIONALE` | delete the edge; or add `consumes` |
| `INTERFACE_EDGE_NOT_CONTRACT` | set `metadata.contract: true` on the target, or use `artifact` |
| `FEEDS_NO_MILESTONE`, `NO_ARTIFACT`, `NO_MILESTONE` | cut it or add the milestone; add `metadata.artifact` |
| `RESOURCE_CONSTRAINT_EDGE` | delete the edge; set the limit in `plan.schedule` capacities |
| `UNDECLARED_GATE`, `UNGATED_WORK` | move heavy work to a gate (`metadata.heavy: false` if only mentioned); make a gate consume the work |
| `WAVE_TOO_SMALL`, `GATE_TIME_DOMINATES`, `WAVE_BOUNDARY_UNJUSTIFIED` | merge waves, or state a real `metadata.boundary_reason` |
| `GATE_NO_DURATION`, `GATE_WITHOUT_MILESTONE` | add the gate's observed time; add the milestone it proves |
| `EVIDENCE_SCOPE_INVALID` | use a scope the kind may declare (work: `focused`; gate: `gate`, `merge`, `publish`, `activation`; manual: those three, `manual_approval`, `external_signoff`) |
| `GATE_SCOPE_MISSING`, `GATE_CANDIDATE_MISSING` | declare the gate's `evidence_scope`; name its `candidate` |
| `GATE_CANDIDATE_INVALID` | point `metadata.candidate` at a direct prerequisite, or add that edge |
| `SCOPE_FOLDED` | split the approval, sign-off, merge, publish or deploy into its own deliverable; `metadata.scope_folded: false` if only mentioned |
| `THREAD_INVALID` | fix the token, or remove `metadata.thread` |
| `THREAD_BROKEN` | add the edge if one step consumes the other; else split or re-thread the steps |

Read `waves`: each wave's `gates`, `deliverables` and `work_per_gate_hour`, each milestone's
`class` (`wave` or `checkpoint`), and `gate_fraction`. Don't judge heaviness from prose.

## 3. Sync and check

`plan.sync {"path": ...}` returns `plan_id`, `name`, `variant`, `revision`, `created`, `changed`
and `diff`; keep the `plan_id`. Always sync a named line from its file. Then `plan.status {plan_id}`:
- `critical_path` runs `__start__ → … → __finish__`, each consecutive pair an edge in the file;
  `critical_path_hours` is the longest chain you expect;
- every acceptance point is under `milestones` with the path and `hours` you expect;
- `definition_drift` is `false` (`true`: sync again; `null`: no root, tracked file or readable file).

A surprising critical path usually means a false or missing edge. Fix the file, lint, sync.

## 4. Level against real capacity and calendars

```text
plan.schedule {"plan_id": "<plan_id>", "capacities": {"agent": 3, "owner": 1},
               "start": "2026-11-16T08:00:00Z",
               "calendar": {"default": {"hours_per_day": 24, "workdays": ["mon","tue","wed","thu","fri","sat","sun"]},
                            "owner": {"hours_per_day": 8, "day_start": "09:00", "utc_offset_minutes": 60}}}
```

- Takes `graph` or `plan_id`, not `path`. Optional `resource_key` (default `owner`) and
  `project_buffer_pct` (0..100, default 25). Every resource carrying work needs a unit
  (`INVALID_CAPACITIES`).
- `calendar` is optional. A map with a `default` entry gives each resource its own working windows
  (agents 24/7, people on working hours); one calendar applies to all. `start` is hour 0.
- Read `makespan` against `cpm_makespan` (the levelled one is real), `driving_chain` (`waited_on`
  `resource` is capacity, `calendar` is a closed window, with `calendar_wait_hours`), `load`
  (`utilisation` near 1.0 is the bottleneck), `project_buffer_hours` and `feeding_buffers`.

## Stop when

- lint is `clean`, the file is synced, `definition_drift` is `false`;
- you have reported the critical path, CPM and levelled makespan, the bottleneck, calendar waits
  and the wave count with `gate_fraction`, and offered `cpm-improve` if the user wants it shorter.

## Pitfalls

- Planner refusals come back as JSON-RPC `-32603` internal errors; the message names the code.
- Monte Carlo on point estimates has no spread, so P80 equals the deterministic makespan.
- A false edge lengthens the critical path; a missing edge shows up later as rework.
