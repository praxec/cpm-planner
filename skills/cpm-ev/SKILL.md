---
name: cpm-ev
description: Use when tracking a cpm-planner plan (plan.* tools) against a baseline with earned value; when asked to baseline or re-baseline a plan, report schedule or cost performance (PV, EV, AC, SV, CV, SPI, CPI, EAC, ETC, VAC, TCPI), take a status snapshot, explain SPI_BELOW_0_9 or CPI_BELOW_0_9 alerts, or read null or undefined ratios. Covers plan.baseline (start, working calendar, reason), plan.ev (as_of), plan.snapshot (json or markdown, backfilled) and acting on alerts.
---

# cpm-ev: baseline, measure, snapshot, act

Freeze the selected variant's schedule and budgets as a baseline, then measure progress
against it. The full method is in [the deliverable-cpm skill](../deliverable-cpm/SKILL.md)
(section 8).

## When to use

- Right after `plan.select` (each variant has its own baselines), before work starts.
- At a steady cadence during `cpm-run`, such as each working day or each cohort.
- After an approved scope change made with `cpm-revise`.

## 1. Baseline

```text
plan.baseline {"plan_id": "<plan_id>", "start": "2026-10-12T00:00:00+02:00",
               "calendar": {"hours_per_day": 8, "workdays": ["mon","tue","wed","thu","fri"],
                            "utc_offset_minutes": 120}}
```

- `start`: RFC 3339, default now. `calendar`: `hours_per_day` (0 < h <= 24, default 8),
  `workdays` (lowercase three-letter names, default mon..fri), `utc_offset_minutes`
  (-1440..1440, default 0). Unknown fields are rejected.
- Without a calendar the first baseline uses wall-clock hours; a re-baseline reuses the
  previous calendar.
- **The working window opens at local midnight** (UTC shifted by `utc_offset_minutes`) and
  lasts `hours_per_day` hours. Give `start` as 00:00 local on a workday. A 09:00 start with
  8 hours per day earns no PV that day.
- Budgets are effort × `metadata.cost_rate` (default 1).
- Selected, unarchived variant only.

## 2. Read

```text
plan.ev {"plan_id": "<plan_id>", "as_of": "2026-10-14T18:00:00Z"}
```

- Returns `bac`, `pv`, `ev`, `ac`, `sv`, `cv`, `spi`, `cpi`, `eac`, `etc`, `vac`, `tcpi`,
  per-deliverable `rows`, `critical_float_consumed_hours`, `alerts` and
  `excluded_unbaselined`.
- `as_of` is the PV status date (default now). EV and AC are what is recorded when the call
  runs. `plan.ev` is read-only; `NOT_BASELINED` before the first baseline.
- **Undefined ratios.** A ratio with a zero denominator is `null`, never NaN, and
  `undefined` lists `{field, reason}` for each. Typical: before any PV, `spi` is null;
  before any AC, `cpi` is null, so `eac`, `etc` and `vac` are null too (also when CPI is
  0: cost spent, nothing earned). Report "not yet measurable", not zero.
- AC sums every recorded hour, including removed and unbaselined deliverables. Unbaselined
  ids (added after the baseline) are listed in `excluded_unbaselined`.

## 3. Snapshot

```text
plan.snapshot {"plan_id": "<plan_id>", "format": "markdown"}
```

- Optional `as_of` (RFC 3339, default now) and `format`: `"json"` (default: a list of
  summaries) or `"markdown"` (a table of date, PV, EV, AC, SPI, CPI, EAC).
- Returns `summary`, `snapshot_count` and `export` of the newest 100 snapshots, oldest
  first. Selected, unarchived variant only.
- Alerts (`SPI_BELOW_0_9`, `CPI_BELOW_0_9`) fire only when the two latest stored,
  non-backfilled snapshots of the current baseline are both below 0.9. No snapshots, no
  alerts: take them at a steady cadence.
- A snapshot whose `as_of` is more than an hour before it was taken is `backfilled: true`,
  raises no alerts and is skipped by later alerts.

## 4. Act on alerts

Find the cause in `rows`: overrun effort (AC above EV), slipped critical work, or consumed
critical float (`critical_float_consumed_hours`). Then run `cpm-improve` on the remaining
work and report the cause and the plan to the user.

## Re-baseline rules

- Re-baseline only for an approved change of scope or plan, never to hide a variance.
- A re-baseline is `plan.baseline` again with a non-blank `reason` (up to 2048 chars;
  missing is `INVALID_GRAPH`). It keeps actuals and snapshots and numbers the new
  baseline; alerts compare only snapshots of the current baseline.
- A newly selected variant takes its own baseline 1, with no reason needed. Baseline it as
  soon as you select it.

## Stop when

- the baseline exists with the calendar the team works to;
- you have reported the current indices, the null ratios and why, and any alert with its
  cause;
- snapshots are being taken at the agreed cadence.

## Pitfalls

- `plan.accept` records no hours, so AC misses owner work unless a lockless
  `plan.mark_status` with `actual_effort_hours` follows it (see `cpm-run`).
- Work started before its prerequisites are complete cannot record AC until they close, so
  CPI can read optimistic for a while.
- `plan.simulate` on a `plan_id` ignores completion, so it is not a remaining-work
  forecast. Use `eac` and `etc`, or fork the remaining work (`cpm-improve`).
- Planner refusals (`NOT_BASELINED`, `VARIANT_NOT_SELECTED`, ...) come back as JSON-RPC
  `-32603` internal errors; read the code in the message.
