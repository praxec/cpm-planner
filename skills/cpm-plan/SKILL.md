---
name: cpm-plan
description: Use when planning work as artifact deliverables with dependencies, estimating effort, finding the critical path (CPM), checking milestones, or levelling the schedule against real capacity to see the critical path and the levelled bottleneck (resource levelling, makespan). Covers authoring a cpm-planner plan file of artifact deliverables and consumption edges, plan.lint until clean, plan.sync, reading critical_path and milestones in plan.status, and plan.schedule with capacities.
---

# cpm-plan: author, lint, sync and level a plan

Turn the user's goal into a tracked plan file, register it and check that its critical path
and levelled schedule make sense. The full method (artifact rules, edge rules, examples) is
in [the deliverable-cpm skill](../deliverable-cpm/SKILL.md); this skill is the short path.
Summarises deliverable-cpm; if they ever disagree, the server's behaviour wins.

Calling the tools: see [Calling the tools](../deliverable-cpm/SKILL.md#calling-the-tools)
(registered MCP server or a host wrapper such as `junior tools call`).

## When to use

- A new project or plan line, or a new first variant (`main`).
- The user asks "what is the critical path", "how long will this take with N agents", or
  "where is the bottleneck".
- Next steps: shorten it with `cpm-improve`, execute it with `cpm-run`, baseline it with
  `cpm-ev`, change it later with `cpm-revise`.
- Not for: shortening or comparing variants → use cpm-improve; changing a synced plan → use cpm-revise.

## 1. Write the file

Path, relative to the project root (`CPM_PROJECT_ROOT`, else the nearest ancestor with
`.cpm-planner/` or `.git`):

```
.cpm-planner/plans/<name>/main.json
```

That file is the only definition. Never keep a scratch graph elsewhere. Rules in brief:
- every deliverable is an inspectable artifact, named in `metadata.artifact`; turn
  activities ("review", "gate") into what they produce, and fold acceptance review into the
  deliverable it accepts;
- `metadata.owner` is the executor class (`agent`, `worker`, `owner`, ...);
  `metadata.kind: "manual"` marks work no agent may lease (closed with `plan.accept`);
- acceptance points are `"milestone": true`;
- `owned_files` lists exactly the paths the implementer writes;
- effort is `estimated_effort_hours`, or a three-point `estimate`
  `{optimistic, likely, pessimistic}` when unsure (Monte Carlo samples only that case);
- `earning_rule`: `zero_hundred` for 2 h or less, `fifty_fifty` or `weighted` for longer;
- an edge exists only when B consumes something A produces:
  `{"id": "A", "consumes": "request/response schema", "kind": "interface"}`. `kind` is
  `artifact` (default) or `interface` (target sets `metadata.contract: true`). Both are
  finish-to-start. `lag_hours` is a minimum wait.

### Code in bulk; put heavy work in gates

Anything slow and fixed-cost is a gate deliverable with `metadata.kind: "gate"`: full build, full
regression, a push or PR that starts CI, merge, deploy, release. Give it an observed `duration_hours`
or `estimate`. Work deliverables list only cheap targeted checks in `metadata.verify` (changed code,
affected contracts and consumers) and never push, open a PR or run the full suite. Read the CI
triggers first, so you know which actions start a pipeline.

Shape: work deliverables → integration candidate (one build and lint on the merged tree) →
regression gate → PR and release. Default to **one wave for the whole project**. Add a boundary only
for a concrete reason in `metadata.boundary_reason`: later work consumes a released artifact, a human
decision changes scope, an irreversible migration, or a trust boundary. Edges are physical hand-overs
only; worker limits go in `plan.schedule`, never in an edge. Later work consumes code and contracts
rather than a gate's receipt where it can. Example:
[milestone-plan.json](../deliverable-cpm/examples/milestone-plan.json); full method in
[deliverable-cpm](../deliverable-cpm/SKILL.md#software-delivery-code-in-bulk-gate-at-milestones).

A worked file: [small-plan.json](../deliverable-cpm/examples/small-plan.json).

## 2. Lint until clean

```text
plan.lint {"path": ".cpm-planner/plans/<name>/main.json"}
```

Fix every finding and lint again until `clean` is true. Never work around a finding.

| Code | Fix |
|---|---|
| `CYCLE` | break the loop it lists; one edge is not real consumption |
| `UNKNOWN_PREREQUISITE`, `DUPLICATE_ID`, `RESERVED_ID`, `INVALID_VALUE` | fix the id or value |
| `UNORDERED_FILE_OVERLAP` | order the two, split the file, or make every claim `{path, mode: "append"}` |
| `TOO_MANY_DELIVERABLES` | at most 5000; split into several plan lines |
| `REDUNDANT_EDGE` | delete it; another path implies it |
| `NO_RATIONALE` | add `consumes`, or delete the edge |
| `INTERFACE_EDGE_NOT_CONTRACT` | set `metadata.contract: true` on the target, or use `artifact` |
| `FEEDS_NO_MILESTONE` | cut the deliverable, or add the milestone it serves |
| `NO_ARTIFACT`, `NO_MILESTONE` | add `metadata.artifact` or a milestone |
| `RESOURCE_CONSTRAINT_EDGE` | delete the edge; set the limit in `plan.schedule` capacities |
| `UNDECLARED_GATE`, `UNGATED_WORK` | move heavy work to a gate (`metadata.heavy: false` if only mentioned); make a gate consume the work |
| `WAVE_TOO_SMALL`, `GATE_TIME_DOMINATES`, `WAVE_BOUNDARY_UNJUSTIFIED` | merge waves, or state a real `metadata.boundary_reason` |
| `GATE_NO_DURATION`, `GATE_WITHOUT_MILESTONE` | add the gate's observed time; add the milestone it proves |

Read `waves` in the result: each wave's `gates`, `deliverables` and `work_per_gate_hour`, each
milestone's `class` (`wave` or `checkpoint`), and `gate_fraction`. Don't judge heaviness from prose.

## 3. Sync

```text
plan.sync {"path": ".cpm-planner/plans/<name>/main.json"}
```

It returns `plan_id`, `name`, `variant`, `revision`, `created`, `changed` and `diff`. Keep the
`plan_id`. Any `name`, `variant` or `project` you pass must match the path. Always sync a named line
from its file: an unnamed `plan.submit` plan cannot be adopted into a named line later.

## 4. Check the critical path and milestones

```text
plan.status {"plan_id": "<plan_id>"}
```

Check:
- `critical_path` runs `__start__ → … → __finish__`, and each consecutive pair of real
  deliverables is an edge in the file;
- `critical_path_hours` is the longest chain you expect;
- every acceptance point is under `milestones` with the `critical_path` and `hours` you
  expect;
- `definition_drift` is `false` (`true`: sync again; `null`: no root, no tracked file, or a
  file missing, unreadable or over 8 MiB).

A surprising critical path usually means a false or missing edge. Fix the file, lint, sync.

## 5. Level against real capacity

CPM assumes unlimited workers. Level it:

```text
plan.schedule {"plan_id": "<plan_id>", "capacities": {"agent": 1, "worker": 2, "owner": 1}}
```

- `plan.schedule` takes `graph` or `plan_id`, not `path`. Optional: `resource_key` (default
  `owner`) and `project_buffer_pct` (0..100, default 25).
- Every resource that carries work needs at least 1 unit, else `INVALID_CAPACITIES`.

Read:
- `makespan` against `cpm_makespan`: the levelled one is the real schedule;
- `driving_chain`: steps with `waited_on: "resource"` wait for capacity, not a dependency;
- `load`: a resource near `utilisation` 1.0 is the bottleneck;
- `project_buffer_hours` and `feeding_buffers`: the buffers to protect.

## Stop when

- lint is `clean`, the file is synced, `definition_drift` is `false`;
- the critical path and milestones match your expectation, or you have explained why not;
- you have reported the critical path, CPM and levelled makespan, the bottleneck and the wave
  count with `gate_fraction`, and offered `cpm-improve` if the user wants it shorter.

## Pitfalls

- Planner refusals come back as JSON-RPC `-32603` internal errors, not tool errors. Read
  the message: it names the code (`INVALID_GRAPH`, `INVALID_CAPACITIES`, ...).
- Monte Carlo on point estimates has no spread, so P80 equals the deterministic makespan.
- A false "do A first" edge lengthens the critical path; a missing edge shows up later as
  rework. See section 2 of [deliverable-cpm](../deliverable-cpm/SKILL.md).
