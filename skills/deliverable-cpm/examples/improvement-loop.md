# Worked example: the improvement loop

This walks [small-plan.json](small-plan.json) through optimize → reason → fork → simulate and
compare → explain → select, then checks the chosen variant against working calendars. Responses
are real output from cpm-planner, cut down to the fields that matter (`…` marks omissions; ids
and hours are as returned). `plan.optimize` and the calendar schedule show the output of the
library functions those tools serialize (`cpm_planner::optimize::optimize` and
`resource_schedule_on_calendar`). The optional AI review is described but not shown: its
probabilities depend on the judge.

The plan ships an export feature:

```
export-api-contract (contract, 2h) ──interface──► export-service (4–6–12h) ──┐
                    │              └─interface──► export-ui      (3–5–8h)  ──┴─► export-e2e-evidence (1–2–4h) ──┐
                    └─────────────────────────► export-api-approval (owner, manual, 0.5h) ──────────────────────┴─► export-release (milestone)
masking-decision    (owner, manual, 1h) ─────────► export-service
```

`export-api-approval` is the UI and service owners' approval of the contract. It is a separate
`manual` deliverable with `metadata.evidence_scope: "manual_approval"` (owners inside the team), so
the approval is not folded into the contract (`SCOPE_FOLDED`), and `export-release` consumes it.

The team has one agent and one owner. A contract worker is available if it pays off.

## 0. The file is registered

The file is `.cpm-planner/plans/export/main.json`. It lints clean and is synced:

```text
plan.lint {"path": ".cpm-planner/plans/export/main.json"}
→ {"clean": true, "findings": [],
   "waves": {"gate_count": 0, "wave_count": 0, "work_hours": 15.0, "gate_fraction": 0.0,
             "milestones": [{"id": "export-release", "class": "checkpoint"}], …}}

plan.sync {"path": ".cpm-planner/plans/export/main.json"}
→ {"plan_id": "plan_a5d026fb5c3c4f29809ecb275e527b46", "name": "export", "variant": "main",
   "revision": 1, "created": true, "changed": true, "diff": null}
```

`plan.status` shows `critical_path` `__start__ → export-api-contract → export-service →
export-e2e-evidence → export-release → __finish__`, 10 h long. The approval has float: it takes
0.5 h after the contract, while the service path takes 8 h more.

## 1. Level, then optimize

```text
plan.schedule {"plan_id": "plan_a5d026fb5c3c4f29809ecb275e527b46",
               "capacities": {"agent": 1, "owner": 1}}
→ {"makespan": 15.0, "cpm_makespan": 10.0,
   "rows": [ …, {"id": "export-api-approval", "resource": "owner", "start": 2.0, "finish": 2.5},
                {"id": "export-service", "resource": "agent", "start": 2.0, "finish": 8.0},
                {"id": "export-ui", "resource": "agent", "start": 8.0, "finish": 13.0}, … ],
   "load": [{"resource": "agent", "capacity": 1, "busy_hours": 15.0, "utilisation": 1.0},
            {"resource": "owner", "capacity": 1, "busy_hours": 1.5, "utilisation": 0.1}],
   "driving_chain": [{"id": "export-api-contract", "waited_on": "start"},
                     {"id": "export-service", "waited_on": "dependency"},
                     {"id": "export-ui", "waited_on": "resource", "waited_on_resource": "agent"},
                     {"id": "export-e2e-evidence", "waited_on": "dependency"}],
   "project_buffer_hours": 3.75, …}
```

Optimize runs first because it is offline, deterministic and free:

```text
plan.optimize {"path": ".cpm-planner/plans/export/main.json",
               "capacities": {"agent": 1, "owner": 1}}
→ {"baseline": {"deliverables": 7, "makespan": 10.0, "resource_makespan": 15.0,
                "resource_wait_hours": 5.0, "total_effort": 16.5, "gate_hours": 0.0,
                "criticality_risk": 0.8214, "lint_warnings": 0},
   "proposals": [], "rejected": [], "candidates": 0, "truncated": false}
```

No candidates: every edge names what it hands over, no two same-owner deliverables share a file,
and there are no gates to consolidate or bypass. The graph has no structural waste; the 5 h of
`resource_wait_hours` is the cost of one agent, not of a false edge.

`plan.review {path, capacities}` could be run next for judgments the graph cannot make alone,
such as a crash technique for `export-service`. It is optional, needs an OpenRouter key or a
review adapter, and sends the graph out. Its proposals are `plan.fork` edit lists like those below.

## 2. Reason

- The levelled makespan is 15 h, against 10 h for CPM. `export-ui` waits 6 h for the only
  agent (`waited_on: "resource"`). The agent pool is the bottleneck, at 100% utilisation.
- Moving `export-ui` to the contract worker removes the wait without changing any estimate.
  It depends on the worker being available.
- Crashing `export-service` from 6 h to 4.2 h shortens the critical chain but leaves the resource
  wait in place. Its 1.8 h of added effort is real, so the fork sets the effort to 7.8 h too.

So try two variants, one move each.

## 3. Fork

```text
plan.fork {"plan_id": "plan_a5d026fb5c3c4f29809ecb275e527b46", "variant": "crash-service",
           "edits": [{"op": "set_duration", "id": "export-service", "hours": 4.2},
                     {"op": "set_effort", "id": "export-service", "hours": 7.8}]}
→ {"plan_id": "plan_83b436c631914e65abb0753ea0a8dc8a", "name": "export",
   "variant": "crash-service", "revision": 1, "created": true, "changed": true, "diff": null}

plan.fork {"plan_id": "plan_a5d026fb5c3c4f29809ecb275e527b46", "variant": "ui-worker",
           "edits": [{"op": "set_metadata", "id": "export-ui", "key": "owner", "value": "worker"}]}
→ {"plan_id": "plan_13dda72d7cea43349568e997b563f732", "name": "export",
   "variant": "ui-worker", "revision": 1, "created": true, "changed": true, "diff": null}
```

Each fork is written to `.cpm-planner/plans/export/<variant>.json` as an unselected draft.

## 4. Simulate and compare

The capacities must cover every pool any variant uses, so `worker` is included. Without
it, `ui-worker` fails with `INVALID_CAPACITIES`.

```text
plan.simulate {"plan_id": "plan_13dda72d7cea43349568e997b563f732",
               "schedule": {"capacities": {"agent": 1, "owner": 1, "worker": 1}},
               "monte_carlo": {"iterations": 2000}}
→ {"lint": {"clean": true, …}, "critical_path_hours": 10.0, "milestones": [ … ],
   "resource_schedule": {"makespan": 10.0, "cpm_makespan": 10.0, …},
   "scorecard": {"makespan": 10.0, "resource_makespan": 10.0, "criticality_risk": 0.82,
                 "risk_band": "high_risk", "total_effort": 16.5, "peak_load": 1.0,
                 "monte_carlo": {"p50": 10.83, "p80": 12.15, "p95": 13.56,
                   "sensitivity": [{"id": "export-service", "correlation": 0.88}, …]}, …}}

plan.compare {"plan": "export",
              "schedule": {"capacities": {"agent": 1, "owner": 1, "worker": 1}},
              "monte_carlo": {"iterations": 2000}}
→ {"variants": [ {"variant": "crash-service", "rank": 2, "pareto_optimal": true, "score": 4.37,
                  "rationale": "best: p80; worst: total_effort", …},
                 {"variant": "main", "rank": 3, "pareto_optimal": false, "score": 4.68,
                  "rationale": "best: total_effort; worst: makespan, p80", …},
                 {"variant": "ui-worker", "rank": 1, "pareto_optimal": true, "score": 4.18,
                  "rationale": "best: makespan, total_effort; worst: p80", …} ],
   "recommended": "plan_13dda72d7cea43349568e997b563f732", "tied_with_recommended": []}
```

From each variant's `scorecard`:

| variant | levelled makespan | P80 | criticality risk | effort |
|---|---:|---:|---|---:|
| main | 15.0 h | 12.2 h | 0.82 high_risk | 16.5 h |
| crash-service | 13.2 h | 10.3 h | 0.82 high_risk | 18.3 h |
| ui-worker | 10.0 h | 12.2 h | 0.82 high_risk | 16.5 h |

All three are `high_risk` on criticality: most of the network is on or near the critical path,
and no single move here changes that.

Read P80 with care, for two reasons:
- **Crashing removes uncertainty.** `crash-service` sets `duration_hours` and
  `estimated_effort_hours` on `export-service`, so Monte Carlo no longer samples its
  4–6–12 h estimate. That deliverable had the highest sensitivity (correlation 0.88), so
  most of the better P80 comes from removing its uncertainty, not from faster work. To keep
  a crashed deliverable sampled, use `set_estimate` with a shorter range.
- **Monte Carlo ignores resource limits.** It samples the CPM network without levelling, so
  no variant's P80 includes resource waits. In `crash-service`, `export-ui` still waits for the
  agent: the CPM makespan is 9 h, the levelled one 13.2 h.

## 5. Explain the trade-off

> **ui-worker** finishes in 10 h levelled, 5 h sooner than main, at no extra effort and with
> the same risk band. It depends on the contract worker taking the UI (5 h of their time).
> **crash-service** shows the best P80, 10.3 h. Most of that comes from no longer sampling
> the service's uncertain estimate, not from finishing sooner. It costs 1.8 h of extra
> effort, and with one agent it still levels to 13.2 h. I recommend ui-worker if the worker is
> confirmed. If not, crash-service is the fallback.

## 6. Select

Once the user confirms the worker:

```text
plan.select {"plan_id": "plan_13dda72d7cea43349568e997b563f732"}
→ {"plan_id": "plan_13dda72d7cea43349568e997b563f732", "name": "export",
   "variant": "ui-worker", "previous": "main", "changed": true, "carried": [],
   "released_locks": [], …}

plan.archive {"name": "export", "variant": "crash-service"}
→ {"ok": true}
```

## 7. Check it against working calendars

The 10 h above assume everyone works around the clock. The agent and the worker do; the owner
works 09:00 to 17:00, Monday to Friday, at UTC-5. Starting on Friday at 16:00 local:

```text
plan.schedule {"plan_id": "plan_13dda72d7cea43349568e997b563f732",
               "capacities": {"agent": 1, "owner": 1, "worker": 1},
               "start": "2026-11-13T21:00:00Z",
               "calendar": {"default": {"hours_per_day": 24, "workdays": ["mon","tue","wed","thu","fri","sat","sun"]},
                            "owner": {"hours_per_day": 8, "day_start": "09:00",
                                      "workdays": ["mon","tue","wed","thu","fri"], "utc_offset_minutes": -300}}}
→ {"start": "2026-11-13T21:00:00Z", "makespan": 65.5, "cpm_makespan": 65.5,
   "rows": [ {"id": "export-api-contract", "start": 0.0, "finish": 2.0, "calendars": ["default"], …},
             {"id": "export-api-approval", "start": 65.0, "finish": 65.5,
              "start_at": "2026-11-16T14:00:00Z", "finish_at": "2026-11-16T14:30:00Z",
              "calendars": ["owner"], "calendar_wait_hours": 63.0},
             {"id": "export-e2e-evidence", "start": 8.0, "finish": 10.0, …},
             {"id": "export-release", "start": 65.5, "finish": 65.5, …}, … ],
   "driving_chain": [{"id": "export-api-contract", "waited_on": "start"},
                     {"id": "export-api-approval", "waited_on": "calendar",
                      "waited_on_calendar": "owner", "calendar_wait_hours": 63.0}],
   "load": [ …, {"resource": "owner", "capacity": 1, "busy_hours": 1.5,
                 "available_hours": 1.5, "utilisation": 1.0}, … ], …}
```

The agents finish the evidence by Saturday 02:00 local (hour 10), but the contract is ready only
at 18:00 local on Friday, after the owner's day ends, so the 30-minute approval waits 63 hours
for Monday 09:00 and the release lands Monday 09:30 local (hour 65.5). Capacity is no longer the
constraint; the owner's calendar is. Two cheap fixes follow from this, each a variant to measure
the same way: start on a morning so the approval falls inside the owner's day, or let the owner
approve the contract before the weekend. Baseline whichever you choose on the same calendar map
(`cpm-ev`), so PV expects the weekend wait instead of reporting it as a slip.

Then baseline the newly selected variant (`plan.baseline`; its first baseline needs no
reason) and execute it.
