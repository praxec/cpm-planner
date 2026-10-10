# Worked example: the improvement loop

This walks [small-plan.json](small-plan.json) through review → reason → fork → simulate and
compare → explain → select. Responses are real output from cpm-planner, cut down to the
fields that matter (`…` marks omissions). The review used a stand-in judge, so its
probabilities are illustrative.

The plan ships an export feature:

```
export-api-contract (contract, 2h) ──interface──► export-service (4–6–12h) ──┐
                                   └─interface──► export-ui      (3–5–8h)  ──┼─► export-e2e-evidence (1–2–4h) ─► export-release (milestone)
masking-decision    (owner, manual, 1h) ─────────► export-service ───────────┘
```

The team has one agent and one owner. A contract worker is available if it pays off.

## 0. The file is registered

The file is `.cpm-planner/plans/export/main.json`. It lints clean and is synced:

```text
plan.sync {"path": ".cpm-planner/plans/export/main.json"}
→ {"plan_id": "plan_cab072135239491788c977c68100ea28", "name": "export", "variant": "main",
   "revision": 1, "created": true, "changed": true, "diff": null}
```

`plan.status` shows `critical_path` `__start__ → export-api-contract → export-service →
export-e2e-evidence → export-release → __finish__`, 10 h long.

## 1. Level and review

```text
plan.schedule {"plan_id": "plan_cab072135239491788c977c68100ea28",
               "capacities": {"agent": 1, "owner": 1}}
→ {"makespan": 15.0, "cpm_makespan": 10.0,
   "rows": [ …, {"id": "export-service", "resource": "agent", "start": 2.0, "finish": 8.0},
                {"id": "export-ui", "resource": "agent", "start": 8.0, "finish": 13.0}, … ],
   "load": [{"resource": "agent", "capacity": 1, "busy_hours": 15.0, "utilisation": 1.0},
            {"resource": "owner", "capacity": 1, "busy_hours": 1.0, "utilisation": 0.067}],
   "driving_chain": [{"id": "export-api-contract", "waited_on": "start"},
                     {"id": "export-service", "waited_on": "dependency"},
                     {"id": "export-ui", "waited_on": "resource"},
                     {"id": "export-e2e-evidence", "waited_on": "dependency"}],
   "project_buffer_hours": 3.75, …}
```

```text
plan.review {"path": ".cpm-planner/plans/export/main.json",
             "capacities": {"agent": 1, "owner": 1}}
→ {"status": "ok", "lint": {"clean": true, "findings": []},
   "findings": [
     {"kind": "split_candidate", "ids": ["export-service"], "probability": 0.76,
      "evidence": "split score 3.00 of 4; P(level >= 3) = 0.76"},
     {"kind": "crash_option", "ids": ["export-service"], "probability": 0.71,
      "evidence": "Jev picks add_capacity for 'export-service' (P = 0.71)"}],
   "proposals": [
     {"id": "crash_option:add_capacity:export-service", "kind": "crash_option",
      "edits": [{"op": "set_duration", "id": "export-service", "hours": 4.2}],
      "hours_saved": 1.8, "cost": 1.8, "score": 1.0,
      "rationale": "… 6.00 h -> 4.20 h at 1.80 h added effort; simulated makespan 15.00 h -> 13.20 h"}],
   "jev_called": true, "question_count": 15, …}
```

## 2. Reason

- The levelled makespan is 15 h, against 10 h for CPM. `export-ui` waits 6 h for the only
  agent (`waited_on: "resource"`). The agent pool is the bottleneck, at 100% utilisation.
- The proposal crashes `export-service` from 6 h to 4.2 h. That shortens the critical chain
  but leaves the resource wait in place. Its 1.8 h of added effort is real, but an
  `add_capacity` proposal's edits are only `set_duration`. Append `set_effort` with the
  current effort plus `cost` (6 + 1.8 = 7.8 h), so `plan.compare` counts it.
- Moving `export-ui` to the contract worker removes the wait without changing any estimate.
  It depends on the worker being available.
- The split finding is plausible, but a split needs new artifacts and file seams. Don't try
  it until the cheaper moves are measured.

So try two variants, one move each.

## 3. Fork

```text
plan.fork {"plan_id": "plan_cab072135239491788c977c68100ea28", "variant": "crash-service",
           "edits": [{"op": "set_duration", "id": "export-service", "hours": 4.2},
                     {"op": "set_effort", "id": "export-service", "hours": 7.8}]}
→ {"plan_id": "plan_32cc1a2f90154def85a0a7e490d211b3", "name": "export",
   "variant": "crash-service", "revision": 1, "created": true, "changed": true, "diff": null}

plan.fork {"plan_id": "plan_cab072135239491788c977c68100ea28", "variant": "ui-worker",
           "edits": [{"op": "set_metadata", "id": "export-ui", "key": "owner", "value": "worker"}]}
→ {"plan_id": "plan_6df8500bbe0142c4b7eccd03f4d4b523", "name": "export",
   "variant": "ui-worker", "revision": 1, "created": true, "changed": true, "diff": null}
```

Each fork is written to `.cpm-planner/plans/export/<variant>.json` as an unselected draft.

## 4. Simulate and compare

The capacities must cover every pool any variant uses, so `worker` is included. Without
it, `ui-worker` fails with `INVALID_CAPACITIES`.

```text
plan.simulate {"plan_id": "plan_6df8500bbe0142c4b7eccd03f4d4b523",
               "schedule": {"capacities": {"agent": 1, "owner": 1, "worker": 1}},
               "monte_carlo": {"iterations": 2000}}
→ {"lint": {"clean": true, …}, "critical_path_hours": 10.0, "milestones": [ … ],
   "resource_schedule": {"makespan": 10.0, "cpm_makespan": 10.0, …},
   "scorecard": {"makespan": 10.0, "resource_makespan": 10.0, "criticality_risk": 0.75,
                 "risk_band": "in_target", "total_effort": 16.0, "peak_load": 1.0,
                 "monte_carlo": {"p50": 10.83, "p80": 12.15, "p95": 13.56,
                   "sensitivity": [{"id": "export-service", "correlation": 0.88}, …]}, …}}

plan.compare {"plan": "export",
              "schedule": {"capacities": {"agent": 1, "owner": 1, "worker": 1}},
              "monte_carlo": {"iterations": 2000}}
→ {"variants": [ {"variant": "crash-service", "rank": 2, "pareto_optimal": true, "score": 4.43,
                  "rationale": "best: p80; worst: criticality_risk, total_effort", …},
                 {"variant": "main", "rank": 3, "pareto_optimal": false, "score": 4.68, …},
                 {"variant": "ui-worker", "rank": 1, "pareto_optimal": true, "score": 4.18,
                  "rationale": "best: makespan, criticality_risk, total_effort; worst: p80", …} ],
   "recommended": "plan_6df8500bbe0142c4b7eccd03f4d4b523"}
```

From each variant's `scorecard`:

| variant | levelled makespan | P80 | criticality risk | effort |
|---|---:|---:|---|---:|
| main | 15.0 h | 12.2 h | 0.75 in_target | 16.0 h |
| crash-service | 13.2 h | 10.3 h | 0.79 high_risk | 17.8 h |
| ui-worker | 10.0 h | 12.2 h | 0.75 in_target | 16.0 h |

Read P80 with care, for two reasons:
- **Crashing removes uncertainty.** `crash-service` sets `duration_hours` and
  `estimated_effort_hours` on `export-service`, so Monte Carlo no longer samples its
  4–6–12 h estimate. That deliverable had the highest sensitivity (correlation 0.88), so
  most of the better P80 comes from removing its uncertainty, not from faster work. To keep
  a crashed deliverable sampled, use `set_estimate` with a shorter range.
- **Monte Carlo ignores resource limits.** It samples the CPM network without levelling, so
  no variant's P80 includes resource waits. In `crash-service`, `export-ui` still waits
  4.2 h for the agent.

## 5. Explain the trade-off

> **ui-worker** finishes in 10 h levelled, 5 h sooner than main, at no extra effort and with
> the same risk band. It depends on the contract worker taking the UI (5 h of their time).
> **crash-service** shows the best P80, 10.3 h. Most of that comes from no longer sampling
> the service's uncertain estimate, not from finishing sooner. It costs 1.8 h of extra
> effort and pushes the plan into `high_risk` criticality. With one agent it still levels
> to 13.2 h. I recommend ui-worker if the worker is confirmed. If not, crash-service is the
> fallback.

## 6. Select

Once the user confirms the worker:

```text
plan.select {"plan_id": "plan_6df8500bbe0142c4b7eccd03f4d4b523"}
→ {"plan_id": "plan_6df8500bbe0142c4b7eccd03f4d4b523", "name": "export",
   "variant": "ui-worker", "previous": "main", "changed": true, "carried": [],
   "released_locks": [], …}

plan.archive {"name": "export", "variant": "crash-service"}
→ {"ok": true}
```

Then baseline the newly selected variant (`plan.baseline`; its first baseline needs no
reason) and execute it.
