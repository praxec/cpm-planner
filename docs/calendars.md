# Working calendars and levelled baselines

Status: contract for issues #131 (per-resource working calendars) and #132
(baseline against a capacity-levelled schedule). It fixes the wire shapes,
the placement and accrual rules and the error codes that three pieces of
work build against independently:

- `calendar-schedule`: window arithmetic in `crates/cpm-planner-core/src/calendar.rs`; `plan.schedule`,
  `plan.simulate` and Monte Carlo place each deliverable in its resource's
  windows and report `waited_on: "calendar"`.
- `levelled-baseline`: `plan.baseline` accepts `capacities`, `resource_key`
  and `project_buffer_pct` and freezes the levelled schedule.
- `calendar-ev`: baselines take per-resource calendars and PV accrues inside
  each deliverable's windows.

Where this document and the code disagree after those land, the code is
wrong or this document must be amended in the same pull request.

Today's code, for reference: the single [`Calendar`](../crates/cpm-planner-core/src/earned_value.rs)
and its `elapsed_hours` EV clock, `build_baseline` and `compute_ev` in the
same file, the baseline flow in [`src/planner/ev.rs`](../src/planner/ev.rs),
levelling in [`resource_schedule.rs`](../crates/cpm-planner-core/src/resource_schedule.rs),
Monte Carlo in [`monte_carlo.rs`](../crates/cpm-planner-core/src/monte_carlo.rs) and the
`baselines` table in [`src/ev_store.rs`](../src/ev_store.rs). The user-facing
guide is the [cpm-ev skill](../skills/cpm-ev/SKILL.md).

## 1. Terms

- **Plan start** (`start`): the RFC 3339 instant hour 0 of a schedule or
  baseline refers to.
- **Wall-clock hours**: elapsed real time from the plan start.
- **Working hours** of a calendar: the part of an interval that falls inside
  that calendar's windows.
- **Window**: one contiguous span in which a calendar is open (section 2.2).
- **Work length** `L` of a deliverable: its scheduled length as today
  (`duration_hours`, else `estimated_effort_hours`, else `estimate.likely`,
  else 0 for a milestone, else the estimator), see
  `crate::schedule::scheduled_length`. With calendars, `L` is measured in
  working hours of the deliverable's calendar.
- **Plan clock** (legacy): every hour of a schedule is one working hour of a
  single plan-wide calendar (or one wall-clock hour without a calendar).
  This is all the code does today.
- **Resource clock** (new): schedule hours are wall-clock hours from the plan
  start, and each deliverable advances only while its own calendar is open.

## 2. Calendar objects

### 2.1 One calendar

```json
{"hours_per_day": 8, "day_start": "09:00",
 "workdays": ["mon", "tue", "wed", "thu", "fri"], "utc_offset_minutes": -300}
```

| Field | Type | Default | Rule |
|---|---|---|---|
| `hours_per_day` | number | `8` | finite, `0 < h <= 24` |
| `day_start` | string | `"00:00"` | local `HH:MM`, 24-hour, exactly two digits each: `^([01][0-9]\|2[0-3]):[0-5][0-9]$` |
| `workdays` | array of strings | `["mon".."fri"]` | non-empty; each one of `mon tue wed thu fri sat sun`; duplicates are ignored |
| `utc_offset_minutes` | integer | `0` | `-1440..=1440`; a fixed offset (no daylight saving) |

Every field is optional, so `{}` is the default 8-hour Monday-to-Friday
calendar opening at 00:00 UTC, as today. Unknown fields are rejected. A
24/7 calendar is `{"hours_per_day": 24, "workdays": ["mon","tue","wed","thu","fri","sat","sun"]}`.

`day_start` is new. Absent, it is `00:00`, so every calendar accepted today
keeps its meaning. Output always echoes it (`"day_start": "00:00"` included);
a stored calendar without it reads as `00:00`.

Daylight saving is out of scope: a team that changes offset re-baselines with
the new `utc_offset_minutes`.

### 2.2 Windows

Local time is UTC plus `utc_offset_minutes`. For every local date `d` whose
weekday is in `workdays`, the calendar has one window

```text
[d + day_start, d + day_start + hours_per_day)   (local time)
```

- A window may cross local midnight. It belongs to the weekday it opens on:
  `{"day_start": "22:00", "hours_per_day": 8, "workdays": ["mon"]}` is open
  Monday 22:00 to Tuesday 06:00 only.
- Windows of consecutive days can touch (24-hour days) but never overlap,
  since `hours_per_day <= 24`. Touching windows are one continuous open span:
  work never pauses at a boundary between them.
- Windows repeat weekly, so the next window is always less than 8 days away.
  Implementations may skip whole weeks arithmetically (as `elapsed_hours`
  does today) but must give the same result as walking window by window.

### 2.3 Per-resource calendars

```json
{
  "default": {"hours_per_day": 24, "workdays": ["mon","tue","wed","thu","fri","sat","sun"]},
  "owner":   {"hours_per_day": 8, "day_start": "09:00",
              "workdays": ["mon","tue","wed","thu","fri"], "utc_offset_minutes": -300}
}
```

- An object with a `default` key is a calendar map. Every key is a resource
  name and every value is a calendar object (section 2.1). `default` is
  required in a map and is the fallback for every resource without its own
  entry.
- Keys are compared exactly (case-sensitive), like `capacities` keys. A key
  must be non-empty. At most 256 entries, `default` included.
- Keys are the values of the `resource_key` metadata field (default
  `metadata.owner`), the same names `plan.schedule` levels against, or pool
  names from `metadata.resources`. A deliverable whose resource is
  `unassigned` (no `metadata.<resource_key>`) uses `unassigned`'s entry if
  present, else `default`.
- Keys that match no deliverable are allowed and ignored.

### 2.4 The `calendar` argument: one rule for both forms

Every tool that takes a calendar takes it as one argument, `calendar`, whose
value is either form. The form is decided by a single rule:

> **If the object has a `default` key it is a calendar map; otherwise it is a
> single calendar.**

This is unambiguous because a single calendar rejects unknown fields, so it
can never contain `default`. Consequences:

- Every `calendar` value accepted today is a single calendar and behaves
  exactly as today (section 6.1).
- `{"owner": {...}}` without `default` is parsed as the single form, so it
  is an unknown field `owner` and is rejected exactly as an unknown calendar
  field is today: `INVALID_PARAMS` at argument parsing (section 7). The
  message adds that a per-resource calendar map needs a `default` entry.
- On `plan.baseline`, `"calendar": null` clears the calendar: the baseline
  counts wall-clock hours even when the previous baseline had a calendar.
  An omitted `calendar` reuses the previous baseline's (section 4.5).
- Wherever a calendar map is required for the resource clock, a single
  calendar `C` passed to `plan.schedule` or `plan.simulate` means
  `{"default": C}`. Only `plan.baseline` keeps the single form on the legacy
  plan clock (section 6.1).

### 2.5 Calendar of a deliverable

Resolve each deliverable to a set of calendars, then to windows:

1. `metadata.wall_clock_only: true`: no calendar. It is always open and its
   `duration_hours` runs in wall-clock hours, as today.
2. Otherwise take the pools it claims, exactly as levelling does: the keys of
   `metadata.resources`, else the single `metadata.<resource_key>` value, else
   `unassigned`.
3. Map each pool to its entry, else `default`. Deduplicate and sort the
   resulting keys. These are the deliverable's **calendar keys** (for
   example `["owner"]`, `["default"]`).
4. With one key, the deliverable's windows are that calendar's. With several,
   they are the **intersection**: work advances only while every claimed
   pool's calendar is open. If the intersection has no open time in a week,
   the call fails with `INVALID_VALUE` naming the deliverable and the keys.

Calendar choice never changes capacity checks: `INVALID_CAPACITIES` is
computed exactly as today.

## 3. Placement on the resource clock

This section applies to `plan.schedule` and `plan.simulate` whenever a
`calendar` is given, and to a `plan.baseline` with a calendar map. Without a
calendar they behave exactly as today.

### 3.1 Time axis

- Hours are wall-clock hours from `start`. `plan.schedule` and
  `plan.simulate` take a new optional `start` (RFC 3339, default the server
  clock's now); `plan.baseline` already has one. `start` is ignored when no
  calendar is given.
- `lag_hours` on an edge is wall-clock hours (a wait, not work).
- `duration_hours` on a `wall_clock_only` deliverable is wall-clock hours.
- Every other work length `L` is working hours on the deliverable's windows.

### 3.2 Window functions

For a deliverable's windows `W` (section 2.5):

- `open_at(W, t)`: the earliest instant `>= t` at which `W` is open. `t`
  itself when `W` is open at `t`. An instant exactly at a window's close is
  not open; exactly at its open is.
- `advance(W, s, L)`: for `L > 0`, the earliest `f >= s` with
  `working_hours(W, s, f) = L`. Work pauses at each window close and resumes
  at the next open, so `f` is never a window open: work that ends exactly at
  a close finishes at the close.
- `working_hours(W, a, b)`: the length of `[a, b)` that lies inside `W`.
- `retreat(W, f, L)` (backward pass): the latest `s <= f` with
  `working_hours(W, s, f) = L`.

An always-open `W` (wall-clock only) makes all four plain arithmetic.

### 3.3 The placement rule

For each deliverable, in the order levelling already chooses:

1. `ready` = the latest prerequisite `finish + lag_hours`, or 0. On the
   levelled schedule, `free` = the time the needed unit of every claimed pool
   becomes free, exactly as today; `unconstrained = max(ready, free)`. Without
   levelling (CPM, Monte Carlo) `unconstrained = ready`.
2. A milestone (`L = 0`) is placed at `start = finish = unconstrained`. It
   is an instant and is never moved into a window.
3. A `wall_clock_only` deliverable starts at `unconstrained` and finishes
   `duration_hours` later.
4. Other work starts at `start = open_at(W, unconstrained)` and finishes at
   `finish = advance(W, start, L)`.

So work that becomes ready outside its window starts when the next window
opens; it does not wait for a window long enough to hold all of it. A
deliverable longer than the time left in its window runs to the close,
pauses, and continues at each following open until `L` working hours are
done.

- The **calendar wait** of a deliverable is
  `calendar_wait_hours = start - unconstrained` (wall-clock hours, 0 when
  `unconstrained` is inside a window).
- A levelled unit is held from `start` to `finish`, closed gaps included: no
  other deliverable runs on it in between. (On a single shared calendar the
  gaps are closed for everyone anyway.)
- Levelling's choice of the next deliverable compares the candidate
  `start` after step 4, so a candidate that must wait for its window loses to
  one that can start now. Ties are broken as today.

### 3.4 What `plan.schedule` reports with a calendar

Additive fields; every existing field keeps its name and type.

- `rows[].start`, `rows[].finish`, `makespan`: wall-clock hours from `start`.
- `rows[].start_at`, `rows[].finish_at`: the same instants in RFC 3339 UTC.
- `rows[].calendars`: the calendar keys (section 2.5); `[]` for
  `wall_clock_only`.
- `rows[].calendar_wait_hours`: section 3.3.
- `start`: the plan start used, RFC 3339.
- `cpm_makespan`: the unlevelled makespan placed on the same calendars (the
  CPM forward pass of section 3.6), so the difference is the cost of
  capacity alone.
- `load[].available_hours`: `capacity` times the working hours of that pool's
  calendar in `[0, makespan)`. `utilisation = busy_hours / available_hours`
  (0 when `available_hours` is 0). Without a calendar `available_hours` is
  `capacity * makespan`, as `utilisation` is computed today.
- `project_buffer_hours` and `feeding_buffers[].buffer_hours` stay
  percentages of summed work lengths `L`, not of wall-clock spans.

### 3.5 `waited_on: "calendar"`

`WaitKind` gains `calendar` (wire `"calendar"`). Walking the driving chain
back from the last finish, for the current link with `start` `s` and its
`unconstrained` `u`:

1. If `s - u > 1e-4` the link reports `waited_on: "calendar"`,
   `waited_on_calendar: "<key>"` (its calendar keys joined with `+`, the way
   `resource` joins pools) and `calendar_wait_hours: s - u`.
2. Otherwise it reports `start`, `dependency` or `resource` as today.
3. Either way the walk continues from `u`, not `s`, with today's rules: to
   the prerequisite whose `finish + lag` equals `u`; else to the holder of a
   saturated pool whose `finish` equals `u`, in which case a calendar link
   also reports that pool in `waited_on_resource`; else it stops.

So the first link waited on `start`, or on `calendar` when the plan start
falls outside its window. `waited_on_calendar` and `calendar_wait_hours` are
omitted from links that did not wait on a calendar.

### 3.6 CPM and Monte Carlo with a calendar

- The calendar CPM forward pass is section 3.3 without levelling. It gives
  `cpm_makespan` above and the deterministic makespan of Monte Carlo.
- `plan.simulate` takes `calendar` and `start` at the top level, next to
  `schedule` and `monte_carlo`, and applies them to both. Calendar lookup uses
  `schedule.resource_key` when `schedule` is given, else `owner`. (A top-level
  `resource_key` is not added; Monte Carlo without `schedule` always uses
  `owner`.)
- Each Monte Carlo iteration places sampled lengths with the forward pass, so
  `p50`, `p80`, `p95`, `mean` and `deterministic_makespan` are wall-clock
  hours. Criticality uses a backward pass with `retreat`: latest finish is the
  minimum over successors of their latest start minus lag (the makespan for
  the finish node), latest start is `retreat(W, latest_finish, L)`, and float
  is `latest_start - earliest_start` in wall-clock hours. Float can be
  positive on the driving chain across a calendar wait: in the worked example
  (section 8) `impl` could finish as late as Monday 09:00 without moving
  `review`, so it has 57 hours of float although the chain runs through it.
- `plan.status`, `plan.lint` and the stored CPM result stay calendar-free.

## 4. Levelled baselines (`plan.baseline`)

### 4.1 Arguments

New optional arguments, beside the existing `start`, `calendar`, `reason`,
`backfill_from` and plan selector:

| Argument | Type | Meaning |
|---|---|---|
| `capacities` | object of resource name to integer `>= 0` | as `plan.schedule` |
| `resource_key` | non-empty string | as `plan.schedule`, default `"owner"`; also selects calendar entries (section 2.5) |
| `project_buffer_pct` | number `0..=100` | as `plan.schedule`, default `25`; recorded and reported, never added to rows or PV |
| `schedule` | `"cpm"` or `"levelled"` | optional; forces the kind (below) |

```text
plan.baseline {"plan": "web", "capacities": {"agent": 3, "owner": 1},
               "reason": "freeze the 3-agent levelled schedule"}
```

### 4.2 Which schedule is frozen

Resolve each of `capacities`, `resource_key` and `project_buffer_pct` from
the argument, else from the previous baseline of this plan (if it recorded
one), else its default (`capacities` has none). Then:

- `schedule` omitted: **levelled** when capacities resolved, else **cpm**.
- `schedule: "levelled"`: capacities must resolve, else `INVALID_VALUE`
  ("a levelled baseline needs capacities").
- `schedule: "cpm"`: freezes CPM and records no capacities. Passing
  `capacities` with it is `INVALID_VALUE`. This is the only way back from a
  levelled baseline to CPM.
- `resource_key` or `project_buffer_pct` given while the result is cpm and
  no calendar map is in use: `INVALID_VALUE` (they would have no effect).

A **cpm** baseline freezes each row's `es`/`ef` from the CPM result, exactly
as today (on the resource clock: the calendar CPM forward pass, section
3.6). A **levelled** baseline freezes each row's `start`/`finish` from the
same computation `plan.schedule` runs for the same graph, `capacities`,
`resource_key`, `project_buffer_pct`, and (when a calendar map is in use)
`calendar` and `start`. The acceptance test is equality: every row's `es`
equals `rows[].start` and `ef` equals `rows[].finish` of that `plan.schedule`
call. Milestones and `wall_clock_only` rows are frozen like any other row.
Budgets, cost rates and BAC do not depend on the kind.

With a single calendar or none (the plan clock), a levelled baseline uses
`plan.schedule` **without** a calendar and reads its hours as plan-clock
hours, exactly as a CPM baseline reads CPM hours today.

The first baseline taken automatically by execution (`first_execution`) is
always cpm with no calendar, as today.

### 4.3 What is recorded and reported

The baseline records `schedule` (`"cpm"` or `"levelled"`), and for levelled
baselines `capacities`, `resource_key`, `project_buffer_pct` and
`project_buffer_hours` (from the levelled result). A cpm baseline that uses a
calendar map records `resource_key` (needed to resolve calendars); every
other field is `null`.

- `plan.baseline` returns these fields beside today's `calendar`, `bac`,
  `finish_hours` and so on; `finish_hours` is the frozen finish of the chosen
  schedule (the levelled makespan for a levelled baseline).
- `plan.ev` reports `schedule` beside `baseline_number`, and `capacities`
  for a levelled baseline (omitted for cpm: the report's nulls are reserved
  for metrics explained in `undefined`).
- `plan.status`'s `baseline` summary becomes
  `{baseline_number, as_of, schedule}`.
- The `plan.ev.baselined` audit payload adds `schedule` and `capacities`,
  for automatic first baselines too (always `"cpm"` and `null` there).

### 4.4 `critical_float_consumed_hours`

`max(0, current finish - baseline finish_hours)`, where the current finish
is computed the way the baseline was: CPM for cpm, levelled with the recorded
capacities, `resource_key` and calendars for levelled. If the current graph
no longer levels against the recorded inputs (a new resource without
capacity, or an invalid `metadata.resources` added since), it falls back to
the CPM finish on the same calendars instead of failing the report.

### 4.5 Re-baseline reuse

A re-baseline without an argument reuses the previous baseline's value for
it: `calendar` (as today), `capacities`, `resource_key` and
`project_buffer_pct` (section 4.2). `start` and `backfill_from` are never
reused. A newly selected variant has no previous baseline, so its baseline 1
uses only what is passed.

Two explicit resets exist: `"calendar": null` returns to wall-clock hours,
and `"schedule": "cpm"` returns to a CPM baseline (dropping the recorded
capacities, `resource_key` and `project_buffer_pct`). Each reset is itself
reused by the next re-baseline: after `"schedule": "cpm"`, a re-baseline
without `capacities` stays CPM.

## 5. PV accrual

### 5.1 Plan clock (single calendar or none): unchanged

`t = elapsed_hours(start, as_of, calendar)`; a row with `ef <= es` is worth
its budget once `t >= es`, else `budget * clamp((t - es) / (ef - es), 0, 1)`.
The only change is that `elapsed_hours` honours `day_start`: a workday's
window opens at `day_start` local, not at midnight. With `day_start` absent
(`00:00`) the result is identical to today. This applies to cpm and levelled
baselines alike.

### 5.2 Resource clock (calendar map)

Each baseline row stores `es` and `ef` in wall-clock hours from `start` and
its `calendars` keys. With `tau = max(0, as_of - start)` in wall-clock hours
and `W` the row's windows (section 2.5, recomputed from the stored calendar
map and keys; `[]` is always open):

```text
ef <= es:  pv = budget if tau >= es else 0
otherwise: pv = budget * working_hours(W, es, clamp(tau, es, ef))
                       / working_hours(W, es, ef)
```

So PV accrues only while the row's calendar is open: it stays flat overnight
and over weekends for a human row and climbs around the clock for a 24/7 row.
If `working_hours(W, es, ef)` is 0 (a placement this contract cannot
produce, for example a row edited by hand), the row falls back to the linear
wall-clock formula of section 5.1 with `t = tau`.

Property for tests: if every resource resolves to the same calendar `C`, the
graph has no lags and no `wall_clock_only` rows, a map baseline and a
single-`C` baseline taken at the same `start` give the same PV at every
`as_of` (to f32 rounding).

### 5.3 Calendar waits in EV

Each resource-clock row also stores its planned `calendar_wait_hours`
(section 3.3). PV already expects a planned wait, so a human step that
waits for 09:00 as planned causes no schedule variance.

`plan.ev` reports the planned waits as frozen in the baseline:

- `rows[].calendar_wait_hours`: the row's stored value (0 for plan-clock
  baselines and rows that never waited);
- `calendar_wait_hours`: the total, defined as the sum over the frozen
  schedule's **driving chain** (section 3.5), the same total `plan.schedule`
  reports for the same inputs. Waits off the driving chain do not delay the
  finish, so they are reported per row but not counted in the total. The
  baseline stores the total with its schedule (`baselines.schedule`,
  `calendar_wait_hours`) and `plan.baseline` returns it too.

Both are plan facts, independent of `as_of`, statuses and actuals. `plan.ev`
does not report a share of schedule variance caused by calendar waits.
Measuring an unplanned wait (work finished late and missed its window)
needs the actual start of each deliverable, which actuals do not record
today; it is future work.

## 6. Compatibility and storage

### 6.1 Rules that keep today's behaviour

- No `calendar`: wall-clock plan clock, as today.
- A single `calendar` on `plan.baseline`: the plan clock, stored and read
  exactly as today (`day_start` aside, which defaults to `00:00`). It is
  **not** converted to `{"default": C}`, because lags and the meaning of
  stored `es`/`ef` differ on the resource clock.
- No `capacities` and no previous levelled baseline: a cpm baseline, as
  today.
- Baselines stored before this change read unchanged: their `calendar`
  column is a single calendar or `NULL`, their rows have no `calendars`, and
  their kind is `cpm`.
- `plan.schedule` and `plan.simulate` without `calendar` return exactly
  today's output (no new fields).
- A calendar map on `plan.baseline` freezes the resource clock: a cpm
  baseline uses the calendar CPM forward pass (levelling with unbounded
  capacity, so no deliverable waits for a unit), a levelled one is
  `plan.schedule` on the same calendar and `start`.

### 6.2 Storage (`baselines` table)

- `calendar` (existing TEXT column): JSON of a single calendar, a calendar
  map, or `NULL`. On read, an object with a `default` key decodes as a map
  (section 2.4). A map selects the resource clock for the whole baseline.
- `schedule` (new nullable TEXT column, schema v6,
  `migrate_v6_baseline_schedule`): JSON
  `{"kind": "levelled", "capacities": {...}, "resource_key": "owner",
  "project_buffer_pct": 25.0, "project_buffer_hours": 6.5}` or
  `{"kind": "cpm", "resource_key": "owner", "calendar_wait_hours": 57.0}`
  (a cpm baseline on a calendar map; resource-clock baselines of either kind
  add `calendar_wait_hours`, the driving-chain total). `NULL` means
  `{"kind": "cpm"}` and is what every plain CPM baseline stores, so CPM
  baselines are stored exactly as before v6.
- `rows` (existing JSON): each row gains optional `calendars` (array of keys)
  and `calendar_wait_hours` (number), written only on the resource clock and
  defaulting to absent and 0 on read. `finish_hours` is still derived on read
  as the largest `ef`.

A stored value that does not decode is `BACKEND_ERROR` naming the plan and
baseline, as today.

## 7. Errors

The single-calendar form keeps today's codes exactly, so every existing
client and test sees the same refusal. `INVALID_VALUE` is used only for
what is new: the map form, `day_start`, and levelled-baseline argument
conflicts.

| Condition | Code |
|---|---|
| Single calendar: unknown field (including a map without `default`) or wrong JSON type | `INVALID_PARAMS` (argument parsing), as today |
| Single calendar: `hours_per_day`, `workdays` or `utc_offset_minutes` out of range | `INVALID_GRAPH`, as today |
| `day_start` not `HH:MM` in `00:00..23:59` (for example `"9:00"`, `"24:00"`, `"09:00:00"`), in either form | `INVALID_VALUE` |
| Calendar map: any entry with an unknown field, a wrong type or an out-of-range value | `INVALID_VALUE` |
| Calendar map: an entry that is not an object, an empty key, or more than 256 entries | `INVALID_VALUE` |
| A multi-pool deliverable whose calendars never overlap | `INVALID_VALUE` |
| `plan.baseline`: `schedule: "levelled"` without resolvable capacities; `capacities` with `schedule: "cpm"`; `resource_key` or `project_buffer_pct` with no effect; empty `resource_key`; `project_buffer_pct` outside `0..=100` | `INVALID_VALUE` |
| `plan.baseline`: a resource carrying work has no or too few units | `INVALID_CAPACITIES`, with the same `missing` list and message `plan.schedule` gives for the same graph and inputs |
| A capacity that is negative or not an integer, or `schedule` not `"cpm"`/`"levelled"` | `INVALID_PARAMS` (argument parsing), as `plan.schedule` |

Reasons name the path, for example
`calendar.owner.day_start must be HH:MM (00:00..23:59), got "9:00"`.

So that `day_start` gives `INVALID_VALUE` while the rest of a single
calendar keeps its parse-time codes, `day_start` deserializes as any string
and the planner validates its format. The same rules apply to the single
form on `plan.schedule` and `plan.simulate`. `plan.schedule`'s own
`project_buffer_pct` error stays `INVALID_GRAPH`.

## 8. Worked example: 24/7 agents and a 09:00-17:00 owner

Agents and CI run around the clock. The human `owner` works Monday to Friday,
09:00 to 17:00 at UTC-5.

```json
"calendar": {
  "default": {"hours_per_day": 24, "workdays": ["mon","tue","wed","thu","fri","sat","sun"]},
  "owner":   {"hours_per_day": 8, "day_start": "09:00",
              "workdays": ["mon","tue","wed","thu","fri"], "utc_offset_minutes": -300}
}
```

The plan has three deliverables in a chain, with effort as budget (cost rate
1) and `capacities: {"agent": 3, "owner": 1}`:

| id | `metadata.owner` | calendar keys | `L` (h) | prerequisite |
|---|---|---|---|---|
| `impl` | `agent` | `["default"]` | 10 | none |
| `review` | `owner` | `["owner"]` | 2 | `impl` |
| `release` | `agent` | `["default"]` | 1 | `review` |

`start` is Friday 2026-11-13 14:00 local (`2026-11-13T19:00:00Z`).

Placement (hours are wall-clock from `start`):

| id | ready | start | finish | calendar wait | start (local) | finish (local) |
|---|---|---|---|---|---|---|
| `impl` | 0 | 0 | 10 | 0 | Fri 14:00 | Sat 00:00 |
| `review` | 10 | 67 | 69 | 57 | Mon 09:00 | Mon 11:00 |
| `release` | 69 | 69 | 70 | 0 | Mon 11:00 | Mon 12:00 |

`review` becomes ready at Saturday 00:00, outside the owner's windows, and
starts at the next window, Monday 09:00 (`2026-11-16T14:00:00Z`). The
makespan is 70 wall-clock hours (Monday 12:00 local); without calendars it is
13. The driving chain, in execution order:

```json
[{"id": "impl", "waited_on": "start"},
 {"id": "review", "waited_on": "calendar", "waited_on_calendar": "owner",
  "calendar_wait_hours": 57.0},
 {"id": "release", "waited_on": "dependency"}]
```

PV against a baseline taken with this map (BAC 13):

| `as_of` (local) | `tau` | `impl` | `review` | `release` | PV |
|---|---|---|---|---|---|
| Fri 19:00 | 5 | 5 | 0 | 0 | 5 |
| Sat 12:00 | 22 | 10 | 0 | 0 | 10 |
| Mon 10:00 | 68 | 10 | 1 | 0 | 11 |
| Mon 12:00 | 70 | 10 | 2 | 1 | 13 |

If execution matches the plan, on Saturday at noon `impl` is complete and
`review` waits for Monday: EV 10, PV 10, SPI 1.0. A wall-clock CPM baseline
of the same plan (`impl` 0-10, `review` 10-12, `release` 12-13) has PV 13 by
then and reads SPI 0.77 all weekend; an 8-hour Monday-to-Friday single
calendar instead plans the agent work into office hours that are not when it
runs.

### Spanning windows, overnight, weekend and day start

With `review` at `L = 10` instead: it starts Monday 09:00 (t 67), runs to
17:00 (t 75), pauses overnight and finishes Tuesday 11:00 (t 93). Its PV is
8/10 of its budget at Monday 17:00 and still 8/10 at Monday 22:00, then
climbs again from Tuesday 09:00.

On the owner calendar:

| becomes ready (local) | `L` | starts | finishes | case |
|---|---|---|---|---|
| Tue 22:00 | 2 | Wed 09:00 | Wed 11:00 | overnight |
| Fri 18:00 | 2 | Mon 09:00 | Mon 11:00 | weekend |
| Wed 07:30 | 2 | Wed 09:00 | Wed 11:00 | before `day_start` |
| Wed 16:00 | 2 | Wed 16:00 | Thu 10:00 | inside a window, spans the night |
| Wed 17:00 | 2 | Thu 09:00 | Thu 11:00 | exactly at close: not open |

## 9. Decisions on the contract's open questions

1. **Ownership.** `src/server.rs` schemas and argument structs belong to
   `tool-wiring`, `src/simulate.rs` to `calendar-schedule`, and the README
   and the `cpm-ev` and `deliverable-cpm` skills to `skills-docs`. The
   levelled-baseline work owns the EV engine and planner, and appends the
   `schedule` column and the v6 migration to `src/ev_store.rs` and
   `src/plan_store.rs`.
2. **Dropping a calendar.** `"calendar": null` on `plan.baseline` clears it
   back to wall-clock hours; omitted reuses the previous calendar (sections
   2.4 and 4.5).
3. **Calendar-wait variance.** Narrowed to planned waits, reported per row
   and in total (section 5.3). Unplanned waits are future work.
4. **Error codes.** The single-calendar form keeps today's codes;
   `INVALID_VALUE` covers only the map form, `day_start` and levelled-baseline
   conflicts (section 7).

## 10. Library API for `plan.baseline`

`tool-wiring` maps the `plan.baseline` arguments one to one onto
`cpm_planner::earned_value::BaselineRequest`:

| Argument | Field | Builder |
|---|---|---|
| `start` | `start: Option<DateTime<Utc>>` | `with_start` |
| `calendar` | `calendar_input: BaselineCalendar` | `with_calendar_input` |
| `reason` | `reason: Option<String>` | `with_reason` |
| `backfill_from` | `backfill_from: Option<DateTime<Utc>>` | `with_backfill_from` |
| `capacities` | `capacities: Option<BTreeMap<String, u32>>` | `with_capacities` |
| `resource_key` | `resource_key: Option<String>` | `with_resource_key` |
| `project_buffer_pct` | `project_buffer_pct: Option<f32>` | `with_project_buffer_pct` |
| `schedule` | `schedule: Option<ScheduleKind>` | `with_schedule` |

- `earned_value::Calendar` is `calendar::Calendar` (one type, with
  `day_start`). `Baseline::calendar` and `BaselineOutcome::calendar` are
  `Option<calendar::CalendarSpec>`, which serializes as the single object or
  the flat map, so the wire shape of a single calendar is unchanged apart
  from the echoed `day_start`.
- `BaselineCalendar` is `Inherit` (absent), `WallClock` (`null`),
  `Single(Calendar)` or `PerResource(map)`; the planner validates a map with
  `CalendarSpec::parse` (every error `INVALID_VALUE`). It implements `Deserialize`, so
  an argument struct field `#[serde(default)] calendar: BaselineCalendar`
  tells absent from `null`; a single calendar with an unknown field or a
  wrong type stays a deserialization error (`INVALID_PARAMS`), as today.
  `BaselineCalendar::from_json(Option<Value>)` does the same from a raw
  value.
- The older `calendar: Option<Calendar>` field (and `with_calendar`) remains
  as a shorthand for `Single`; `calendar_input` wins when it is not
  `Inherit`. `BaselineRequest::effective_calendar` gives the resolved choice.
- `ScheduleKind` serializes as `"cpm"` / `"levelled"`.
- `BaselineOutcome` adds `schedule`, `capacities`, `resource_key`,
  `project_buffer_pct`, `project_buffer_hours` and `calendar_wait_hours`;
  `EvReport` adds `schedule`, `capacities` (omitted for cpm) and
  `calendar_wait_hours`, and each `EvRow` adds `calendar_wait_hours`;
  `PlanBaseline` (the `plan.status` summary) adds `schedule`.
- Pure builders: `build_baseline` (CPM; a map goes to the resource clock
  with `resource_key` `owner`), `build_levelled_baseline` (levelled), and
  `build_baseline_on_calendar(graph, start, calendar, number,
  resource_key, levelling)` for the resource clock. The recorded inputs are
  `Baseline::schedule` (`BaselineSchedule`), whose `levelling_request()`
  rebuilds the `ScheduleRequest`. Rows carry `calendars` and
  `calendar_wait_hours` on the resource clock.
