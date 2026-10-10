---
name: cpm-plan
description: Use when the cpm-planner MCP server (plan.* tools) is available and the user wants to plan a project, break work into deliverables with dependencies, estimate effort, find the critical path (CPM), check milestones, or level the schedule against real capacity to see the critical path and the levelled bottleneck (resource levelling, makespan). Covers authoring a .cpm-planner/plans/<name>/<variant>.json plan file of artifact deliverables and consumption edges, plan.lint until clean, plan.sync, reading critical_path and milestones in plan.status, and plan.schedule with capacities.
---

# cpm-plan: author, lint, sync and level a plan

Turn the user's goal into a tracked plan file, register it and check that its critical path
and levelled schedule make sense. The full method (artifact rules, edge rules, examples) is
in [the deliverable-cpm skill](../deliverable-cpm/SKILL.md); this skill is the short path.
Summarises deliverable-cpm; if they ever disagree, the server's behaviour wins.

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

A worked file: [small-plan.json](../deliverable-cpm/examples/small-plan.json).

## 2. Lint until clean

```text
plan.lint {"path": ".cpm-planner/plans/<name>/main.json"}
```

Fix every finding and lint again until `clean` is true. Never work around a finding.

| Code | Fix |
|---|---|
| `CYCLE` | break the loop it lists; one edge is not real consumption |
| `UNKNOWN_PREREQUISITE`, `DUPLICATE_ID`, `RESERVED_ID`, `INVALID_VALUE` | fix the id or value (`__start__`, `__finish__` are reserved) |
| `UNORDERED_FILE_OVERLAP` | order the two, split the file, or make every claim `{path, mode: "append"}` |
| `TOO_MANY_DELIVERABLES` | at most 5000; split into several plan lines |
| `REDUNDANT_EDGE` | delete it; another path implies it |
| `NO_RATIONALE` | add `consumes`, or delete the edge |
| `INTERFACE_EDGE_NOT_CONTRACT` | set `metadata.contract: true` on the target, or use `artifact` |
| `FEEDS_NO_MILESTONE` | cut the deliverable, or add the milestone it serves |
| `NO_ARTIFACT`, `NO_MILESTONE` | add `metadata.artifact` or a milestone |

## 3. Sync

```text
plan.sync {"path": ".cpm-planner/plans/<name>/main.json"}
```

It returns `plan_id`, `name`, `variant`, `revision`, `created`, `changed` and `diff`. Keep the
`plan_id`; every later call uses it. Any `name`, `variant` or `project` you pass must match the
path. Always sync a named line from its file: an unnamed `plan.submit` plan cannot be
adopted into a named line later, and its completions would have to be re-accepted.

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
  `owner`, read from `metadata`) and `project_buffer_pct` (0..100, default 25).
- Every resource that carries work needs at least 1 unit, else `INVALID_CAPACITIES` (it
  lists the missing ones).

Read:
- `makespan` against `cpm_makespan`: the levelled one is the real schedule;
- `driving_chain`: steps with `waited_on: "resource"` wait for capacity, not a dependency;
- `load`: a resource near `utilisation` 1.0 is the bottleneck;
- `project_buffer_hours` and `feeding_buffers`: the buffers to protect.

## Stop when

- lint is `clean`, the file is synced, `definition_drift` is `false`;
- the critical path and milestones match your expectation, or you have explained why not;
- you have reported the critical path, CPM and levelled makespan, and the bottleneck, and
  offered `cpm-improve` if the user wants it shorter.

## Pitfalls

- Planner refusals come back as JSON-RPC `-32603` internal errors, not tool errors. Read
  the message: it names the code (`INVALID_GRAPH`, `INVALID_CAPACITIES`, ...).
- Monte Carlo on point estimates has no spread, so P80 equals the deterministic makespan
  and the response does not say so. Use three-point `estimate`s where risk matters.
- A false "do A first" edge with nothing consumed lengthens the critical path; a missing
  edge shows up later as rework. See section 2 of
  [deliverable-cpm](../deliverable-cpm/SKILL.md) for the common cases.
