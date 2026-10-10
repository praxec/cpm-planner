# Roadmap earned value: backlog-roadmap / main (0.1.0 dogfood)

This is cpm-planner's own roadmap
([`.cpm-planner/plans/backlog-roadmap/main.json`](../../.cpm-planner/plans/backlog-roadmap/main.json)),
tracked with cpm-planner's earned-value tools on the 0.1.0 release-candidate binary.

## Method

- **Plan.** `plan.sync {path: ".cpm-planner/plans/backlog-roadmap/main.json"}` registers
  plan line `backlog-roadmap`, variant `main`. The file was not edited. It sets no
  `earning_rule` or `metadata.cost_rate`, so the defaults apply: `zero_hundred` (credit only
  when complete) and rate 1. Budgets are therefore the estimated hours, and BAC is 27.5 h.
- **Baseline.** `plan.baseline {start: "2026-10-09T15:00:00Z", reason: "initial baseline"}` is
  baseline 1. Its CPM finish is 21 h, at 2026-10-10T12:00Z. No calendar was given, so PV
  accrues on **wall-clock** hours, 24 per day.
- **Completion.** A phase is complete only with a merged PR. Each was closed with
  `plan.accept` (the evidence is the PR URL and merge commit), then a lockless
  `plan.mark_status {status: complete, actual_effort_hours}` to record AC. The merged phases
  are P0 #29, P1 #30 (merged into the P0 branch, reached `dev` via #29), P2 #31, P3 #32,
  P7a #33, P4 #34 and P4P #35.
- **Open work.** P5 (#36) and P6 (#37) are open PRs. They are marked `in_progress` with
  their actual hours so far. Under `zero_hundred` they earn 0 until merged.
- **P7b** (this release) is in progress on `feat/p7b-release`. Its prerequisites, P5 and P6,
  are not complete, so the planner refuses a lockless `in_progress` mark
  (`PREREQUISITES_INCOMPLETE`, by design). P7b therefore shows `pending`, and its roughly
  0.5 h so far is **not** in AC.
- **Actual hours** are the elapsed span from a phase's first to its last authored non-merge
  commit, read from `gh pr view N --json commits`. For stacked PRs the parent PR's commits
  are removed first. This approximates elapsed wall time, not effort:
  - phases P4, P4P, P5 and P6 ran in parallel, so their hours overlap;
  - time before the first commit and after the last is missing;
  - a phase committed in one burst reads short (P0 is 5 minutes).

  The low actuals, and with them the high CPI, mostly reflect this method and
  subagent-driven execution against human-sized estimates.
- **as_of.** `as_of` is the PV status date, here the moment the snapshot was taken
  (2026-10-10T02:24Z). EV and AC are as recorded when the call ran.
- **Database.** These numbers come from a consistent `sqlite3` backup copy of the
  maintainer's planner database, not the live one. The named line is new there, so the
  completions were re-accepted on it rather than carried over from the earlier unnamed
  roadmap plan.

## Per-phase actuals

| Phase | Budget h | Status | Evidence | Commit span (UTC) | AC h |
|---|---:|---|---|---|---:|
| P0 | 1.0 | complete | #29 (ef1c242) | 10-09 19:19 → 19:24 | 0.09 |
| P1 | 1.5 | complete | #30 (fe44f59) | 19:28 → 19:45 | 0.28 |
| P2 | 2.0 | complete | #31 (7073a31) | 19:48 → 20:09 | 0.35 |
| P3 | 4.0 | complete | #32 (56aac1c) | 20:16 → 20:45 | 0.49 |
| P7a | 1.0 | complete | #33 (6c3c934) | 21:08 → 21:32 | 0.39 |
| P4 | 4.0 | complete | #34 (06c2a53) | 21:01 → 22:22 | 1.35 |
| P4P | 4.0 | complete | #35 (419a5c4) | 21:16 → 10-10 00:19 | 3.05 |
| P5 | 4.0 | in_progress | #36 open | 21:48 → 00:36 | 2.81 |
| P6 | 4.0 | in_progress | #37 open | 23:09 → 01:13 | 2.06 |
| P7b | 2.0 | pending (see above) | `feat/p7b-release` | 01:25 → ongoing | not recorded |

## Snapshot

`plan.snapshot {format: "markdown"}`, baseline 1:

| date | PV | EV | AC | SPI | CPI | EAC |
|---|---:|---:|---:|---:|---:|---:|
| 2026-10-10T02:24Z | 13.92 | 17.50 | 10.87 | 1.26 | 1.61 | 17.08 |

From the same reading (`plan.ev`):

| Metric | Value |
|---|---:|
| BAC | 27.5 |
| SV | +3.58 |
| CV | +6.63 |
| ETC | 6.21 |
| VAC | +10.42 |
| TCPI | 0.60 |
| Critical float consumed | 0 h |
| Alerts | none |

The project is ahead of schedule (SPI 1.26) and under budget on the hours recorded
(CPI 1.61, with the caveats above).

## Forecast

**Remaining-work makespan (point estimates; no Monte Carlo spread): 6 h.** P5 and P6 run in
parallel at their 4 h budgets, then P7b takes 2 h. That forecasts a finish around
2026-10-10T08:25Z, against the baseline finish of 12:00Z. The figure uses the full budgets of
the open phases and does not net off hours already spent.

It comes from `plan.simulate {graph: <P5, P6, P7b only>, monte_carlo: {iterations: 2000,
seed: 42}}`, which returns a critical path of P5 → P7b, `critical_path_hours: 6`, and
p50 = p80 = p95 = 6. `plan.simulate` on the stored plan ignores progress. It returns the
whole-plan makespan of 21 h (p50 = p80 = p95 = 21), which is **not** a remaining-work
figure. The roadmap has point estimates only, so Monte Carlo has no variance and its
percentiles carry no risk information. A meaningful P80 needs three-point `estimate`s.

## Known limitations (0.1.x follow-ups)

- `plan.simulate` on a `plan_id` ignores completion. A remaining-work forecast needs an
  inline graph of the unfinished deliverables, or a fork with completed work at effort 0.
- Monte Carlo on point estimates collapses to the deterministic makespan, and the response
  does not flag that there is no spread.
- Planner refusals (`PREREQUISITES_INCOMPLETE` and others) come back as JSON-RPC `-32603`
  internal errors, not as tool errors or invalid-params errors.
- `plan.accept` records no hours. Each phase needs a second lockless `plan.mark_status` call
  to record AC.
- Work started before its prerequisites are complete (P7b here) has no way to record
  actual cost until they close.
- An unnamed plan cannot be adopted into a named line. Its completions have to be
  re-accepted.
