# P5 — Earned Value (#16) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A selected plan can be baselined, deliverables report progress, actual effort and evidence as they execute, and `plan.ev` reports PV, EV, AC, SV, CV, SPI, CPI, EAC, ETC, VAC, TCPI, per-deliverable rows, critical-float consumption and alerts; snapshots record the trend and export as JSON or Markdown.

**Architecture:** A pure `src/earned_value.rs` computes EV from (frozen baseline, current graph, statuses, actuals, as-of time, calendar). The store gains schema v4 tables `baselines`, `ev_actuals`, `ev_snapshots`, plus a `leased_hours` accumulator updated whenever a lease ends. New tools: `plan.baseline`, `plan.ev`, `plan.snapshot`; `plan.mark_status` gains `earned_pct`, `actual_effort_hours`, `evidence`. Execution-side tools honour P4P's `VARIANT_NOT_SELECTED` gating.

**Tech Stack:** Rust 1.99.0, chrono, rusqlite, serde, rmcp.

**Spec:** roadmap § P5 (+ EV clock decision: wall-clock default, opt-in working calendar) + issue #16.

## Global Constraints

- Cost unit = hours; `metadata.cost_rate` (finite ≥ 0, default 1.0) multiplies a deliverable's budget AND its actual cost.
- Budget per deliverable = `schedule::effort_basis(d)` × rate (effort basis: explicit effort > estimate.likely > milestone 0 > estimator; never duration). BAC = Σ budgets (synthetic endpoints excluded).
- PV(t) = Σ budget_i × clamp((t − ES_i) / (EF_i − ES_i), 0, 1) over the **frozen baseline** ES/EF (zero-length: 0 before ES, budget at/after ES). t = elapsed hours since baseline `start`: wall-clock by default; with `calendar {hours_per_day (0 < h ≤ 24, default 8), workdays: ["mon".."sun"] (default mon–fri), utc_offset_minutes (default 0)}` only working time counts (a workday contributes `hours_per_day` hours starting at 00:00 local).
- Earning rules (`Deliverable.earning_rule`, new field, hashed): `zero_hundred` (default; earned% = 100 iff Complete), `fifty_fifty` (50 when InProgress or any earned_pct reported, 100 when Complete), `weighted` (reported `earned_pct`, 100 when Complete). EV = Σ budget_i × earned%_i / 100.
- AC = Σ rate_i × (reported `actual_effort_hours` if present else `leased_hours`).
- Ratios: SV = EV − PV, CV = EV − AC, SPI = EV/PV, CPI = EV/AC, EAC = BAC/CPI, ETC = EAC − AC, VAC = BAC − EAC, TCPI = (BAC − EV)/(BAC − AC). Any division by 0 (or non-finite result) → the field is `null` and `undefined: [{field, reason}]` explains (e.g. `"PV is 0 before any work was planned"`). Never NaN/inf in output.
- Re-baseline requires a non-empty `reason`; baselines are numbered; `plan.ev` uses the latest; all baseline operations audited (`plan.ev.baselined`).
- `earned_pct`: integer 0..=100; only accepted with status `in_progress` (or alongside `complete`, where it is ignored); `actual_effort_hours` finite 0..=1e6; `evidence` ≤ 2048 chars, appended (list) per deliverable.
- Alerts: `SPI_BELOW_0_9` / `CPI_BELOW_0_9` when the metric is < 0.9 on the latest two snapshots.
- Execution tools gate on the selected variant (reuse P4P helper).
- Schema v4 in the single migration transaction; `user_version` 4.
- CRLF files keep CRLF; new files LF; fmt/test/clippy -D warnings green; tool wiring rules as in prior phases.

## Review Focus
1. `plan.ev` before baseline → clear `NOT_BASELINED:` error.
2. As-of before baseline start → PV 0, SPI null with reason (no NaN).
3. A deliverable completed without any lease and without reported hours → AC contribution 0, CPI unaffected (documented).
4. Calendar with weekends — PV flat over Saturday/Sunday.
5. Re-baseline mid-plan keeps actuals and EV, resets PV curve to the new baseline.

---

### Task 1: Pure EV engine (`src/earned_value.rs`)
Types `Calendar`, `EarningRule`, `Baseline { number, start, calendar, rows: Vec<BaselineRow{id, es, ef, budget}>, bac }`, `Actuals { earned_pct: Option<u8>, actual_hours: Option<f32>, leased_hours: f32, evidence: Vec<String> }`, `EvReport { as_of, baseline_number, bac, pv, ev, ac, sv, cv, spi, cpi, eac, etc, vac, tcpi (all Option<f32> where division), undefined: Vec<Undefined>, rows: Vec<EvRow{id, budget, pv, earned_pct, ev, ac, status}>, critical_float_consumed_hours: f32, alerts: Vec<String> }`; `fn elapsed_hours(start, as_of, calendar) -> f32`; `fn build_baseline(graph, cpm, start, calendar, number) -> Baseline`; `fn compute_ev(baseline, graph, statuses, actuals, as_of, previous_snapshots: &[EvSummary]) -> EvReport`. Critical-float consumed = max(0, current forecast finish − baseline finish) where current forecast finish = __finish__ EF of the current cached CPM measured from baseline start with completed work fixed (document the approximation: current CPM EF − baseline __finish__ EF, floored at 0).
Tests (textbook fixtures, one assertion each): wall-clock elapsed; calendar skips weekends; calendar partial day; PV linear interpolation; zero-length PV step; each earning rule; AC prefers reported hours over leased; cost_rate scales budget and AC; every ratio formula on a hand-computed fixture (BAC 100, PV 50, EV 40, AC 48 → SPI 0.8, CPI 0.8333, EAC 120, ETC 72, VAC −20, TCPI 60/52); each zero-division case yields null + reason; alerts need two consecutive snapshots below 0.9.

### Task 2: Schema v4 + actuals capture
Tables: `baselines(plan_id, number, start_us, calendar TEXT, rows TEXT, bac REAL, reason TEXT, created_at_us, PRIMARY KEY(plan_id, number))`, `ev_actuals(plan_id, deliverable_id, earned_pct INTEGER, actual_hours REAL, leased_hours REAL NOT NULL DEFAULT 0, evidence TEXT NOT NULL DEFAULT '[]', updated_at_us, PRIMARY KEY(plan_id, deliverable_id))`, `ev_snapshots(plan_id, taken_at_us, as_of_us, summary TEXT, PRIMARY KEY(plan_id, taken_at_us))`. Lease end (complete, failed, force_release, reap, accept override, revise forced release) adds `(end − acquired_at)` hours to `leased_hours` in the same transaction. `MarkStatusRequest` gains `earned_pct`, `actual_effort_hours`, `evidence` (+ server args with validation per Global Constraints). `Deliverable.earning_rule` (hashed; `CPM_VERSION` unchanged).
Tests: leased hours accumulate across two leases (fake clock); reaped lease adds its leased time; earned_pct rejected with status ready; actual hours persisted; evidence appended; migration v3→v4 keeps data; earning_rule changes plan identity.

### Task 3: `plan.baseline`, `plan.ev`, `plan.snapshot` (planner + tools)
`plan.baseline { plan_id, start?, calendar?, reason? }` (reason required when a baseline exists) → baseline number; `plan.ev { plan_id, as_of? }` → EvReport (`NOT_BASELINED:` new error prefix); `plan.snapshot { plan_id, as_of?, format?: "json"|"markdown" }` → appends snapshot, returns summary + rendered export (Markdown table: date, PV, EV, AC, SPI, CPI, EAC). Gating: baseline/snapshot are execution-side (selected variant only); ev is read-only (any variant, but only meaningful when baselined). Docs, README, CHANGELOG, instructions(), server tests.
Tests: baseline then ev roundtrip; ev before baseline → NOT_BASELINED; re-baseline without reason rejected; re-baseline keeps actuals; snapshot markdown contains header row; two low snapshots raise SPI alert; baseline on draft variant → VARIANT_NOT_SELECTED; NaN never serialised (fuzz-ish: random small plans, assert JSON has no null where value is finite and no "NaN").

### Task 4: Dogfood
Baseline the roadmap plan (`plan_0bf93f68…`) at its creation date with `plan.baseline` once P5 is merged; record actual hours for completed phases from git history (first to last commit per phase) via `plan.mark_status`/`plan.accept`; write the first `plan.snapshot` Markdown into `docs/ev/backlog-roadmap.md`.
