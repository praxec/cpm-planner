---
name: cpm-improve
description: Use when a cpm-planner plan (plan.* tools) is synced and the user wants it shorter, cheaper or less risky; when asked to crash or fast-track a schedule, remove a bottleneck, split a deliverable, try contract-first, compare plan variants, run Monte Carlo (P80, sensitivity), or get an AI plan review (Jev via OpenRouter). Covers optional plan.review, plan.fork with edits, plan.simulate and plan.compare, explaining the trade-off in numbers, plan.select and plan.archive.
---

# cpm-improve: fork, measure, compare, select

Design several variants, measure each, explain the trade-off, and select one. Never edit the
selected variant to try an idea. The full method and move table are in
[the deliverable-cpm skill](../deliverable-cpm/SKILL.md) (section 6), with a worked run in
[improvement-loop.md](../deliverable-cpm/examples/improvement-loop.md).

## When to use

- After `cpm-plan`: the plan lints clean, is synced, and you have a `plan_id`.
- During execution, on the remaining work, when `cpm-ev` alerts fire.

## 1. Review (optional)

```text
plan.review {"path": ".cpm-planner/plans/<name>/main.json", "capacities": {"agent": 1, "owner": 1}}
```

- Takes exactly one of `graph`, `plan_id` or `path`; optional `capacities`, `resource_key`,
  `project_buffer_pct`, `max_questions` (1..64, default 64).
- Needs an OpenRouter key (`OPENROUTER_API_KEY`, or `CPM_OPENROUTER_KEY_FILE`). Without one,
  `status` is `review_unavailable` plus lint: carry on without it.
- The graph is sent to OpenRouter. Skip review for plans you may not share.
- Lint errors return `status: "invalid_graph"` and no judge call; fix them first.
- `findings` (`false_dependency`, `missing_dependency`, `split_candidate`,
  `interface_split`, `crash_option`) are advisory probabilities. Check each against what
  you know.
- `proposals` are ready `plan.fork` edit lists, simulation-checked and ranked by
  `hours_saved / max(cost, 1)`. An `add_capacity` crash proposal only has `set_duration`;
  append `set_effort {id, hours: <current effort + cost>}` so the cost is counted.

## 2. Reason: one move per variant

| Move | Edits |
|---|---|
| Add capacity to the bottleneck | `set_metadata` `owner`, or more capacity in `schedule` |
| Move work to a cheaper pool | `set_metadata` `owner`, plus `set_effort` for review time |
| Crash a critical deliverable | `set_duration`, plus `set_effort` for the added cost |
| Split along file or contract seams | `remove_deliverable`, `add_deliverable`, `add_edge` |
| Contract first | `add_deliverable` with `metadata.contract`; re-add consumers with an `interface` edge |
| Fast-track | `remove_edge`, only if nothing was actually consumed |

Use the levelled `driving_chain`, `load` and Monte Carlo `sensitivity` to pick moves.

## 3. Fork

```text
plan.fork {"plan_id": "<main plan_id>", "variant": "ui-worker",
           "edits": [{"op": "set_metadata", "id": "export-ui", "key": "owner", "value": "worker"}]}
```

Edit ops (tagged by `op`, unknown fields rejected):
- `remove_edge {from, to}`; `add_edge {from, to, consumes}` (no `kind`: for an interface
  edge, `remove_deliverable` then `add_deliverable` with the object prerequisite);
- `set_effort {id, hours}`; `set_duration {id, hours}` (`null` clears);
  `set_estimate {id, estimate}` (`{optimistic, likely, pessimistic}` or `null`);
- `set_metadata {id, key, value}`; `remove_deliverable {id}`;
  `add_deliverable {deliverable}`.

The fork is written to `.cpm-planner/plans/<name>/<variant>.json` as a draft, not
selected. An archived line is refused with `ARCHIVE_REFUSED`.

## 4. Measure

```text
plan.simulate {"plan_id": "<fork plan_id>", "schedule": {"capacities": {...}}, "monte_carlo": {}}
plan.compare  {"plan": "<name>", "schedule": {"capacities": {...}}, "monte_carlo": {}}
```

- `plan.simulate` takes one of `graph`, `plan_id` or `path`; `schedule` is
  `{capacities, resource_key?, project_buffer_pct?}`; `monte_carlo` is
  `{iterations? (1..50000, default 2000), seed?}`. It persists nothing.
- `plan.compare` takes `plan` (a line name, every live variant, at most 16) or `plan_ids`
  (2..16), optional `project`, `schedule`, `monte_carlo` and `weights`
  `{makespan, p80, criticality_risk, total_effort, peak_load}` (finite, >= 0).
- `capacities` must cover every pool of every variant, or `INVALID_CAPACITIES`.
- Read `pareto_optimal`, `rank`, `rationale` and `recommended`.

## 5. Explain, then select

Before selecting, tell the user in numbers what each candidate gains (makespan, P80), what
it costs (effort, peak load, risk band) and which assumption it depends on (for example,
that a worker pool exists). Rejected variants are results too. Let the user choose when the
trade-off is not clear-cut.

```text
plan.select  {"plan_id": "<chosen plan_id>"}
plan.archive {"name": "<name>", "variant": "<rejected variant>"}
plan.list    {}
```

- Only the selected variant can execute. `plan.select` carries over Complete statuses of
  identically defined deliverables, with their actuals. `force: true` releases locks held
  on the previous variant; only use it when those holders are gone.
- `plan.archive` defaults to `archived: true`; pass `archived: false` to unarchive. Without
  `variant` it archives the whole line.
- A newly selected variant needs its own baseline: go to `cpm-ev` straight away.

## Stop when

- every candidate is measured with the same capacities and Monte Carlo settings;
- the trade-off has been explained and the user agrees with the choice;
- one variant is selected, the rejected ones are archived.

## Pitfalls

- P80 comes from Monte Carlo on the unlevelled network, so it ignores resource waits.
- Monte Carlo on point estimates collapses to the deterministic makespan with no warning.
  `set_effort` or `set_duration` on a deliverable with an `estimate` stops it being sampled,
  so part of a "better" P80 can be lost uncertainty, not a shorter schedule.
- `plan.simulate` on a `plan_id` ignores completion. For a remaining-work forecast, fork
  with completed work at effort 0, or simulate an inline graph of the unfinished work.
- Planner refusals come back as JSON-RPC `-32603` internal errors; read the code in the
  message.
