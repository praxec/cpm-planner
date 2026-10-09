# cpm-planner Backlog Roadmap (Toolchain → Jev → Install/Skill)

> **For agentic workers:** This is the *roadmap* (phase-level plan). Each phase is its own branch +
> PR into `dev`. Immediately before executing a phase, expand it into a step-level TDD plan
> (`docs/superpowers/plans/2026-10-09-pN-<name>.md`) via superpowers:writing-plans — the code it
> touches changes phase to phase, so step-level code written now would be stale by P4.
> REQUIRED SUB-SKILL per phase: superpowers:subagent-driven-development or superpowers:executing-plans.

**Goal:** Clear the open GitHub backlog (#12–#28), add plan-as-code portfolios (named plans +
variants as files, compared by scorecard), add an OpenRouter integration (generative LLM + Jev judgments via rig), ship per-client MCP install instructions, and land the `deliverable-cpm` planning skill
— on a pinned Rust 1.99.0 toolchain.

**Architecture:** Keep the existing layering (`algorithm.rs` kernel → `planner.rs` `BasicCpmPlanner`
→ `server.rs` rmcp façade → `plan_store.rs` SQLite). New read-only analyses (lint, schedule,
simulate, EV, review) are pure modules over `PlanGraph` + `CriticalPathResult`, exposed as new
`plan.*` tools. All `Deliverable` schema changes land in one phase so `hash_graph` and every
test fixture are touched once.

**Tech Stack:** Rust 1.99.0 (edition 2024), tokio, rmcp 1.8, rusqlite (bundled), serde.
OpenRouter via rig 0.44 (MSRV 1.95): `rig-core` `providers::openrouter` (generative) + `rig-typesafeai` (Jev).

**Plan-as-code (decided 2026-10-09):** plan *definitions* live in the repo at `.cpm-planner/plans/<name>/<variant>.json`
(git-diffable, PR-reviewable); the SQLite DB holds only *runtime* state (leases, locks, statuses, counters,
EV actuals, audit) keyed by (project, plan name, variant, content-hash revision).

**Spec:** GitHub issues #12–#28 (open backlog as of 2026-10-09) + user request in this session.

## Global Constraints

- Toolchain pinned: `rust-toolchain.toml` `channel = "1.99.0"`; `Cargo.toml` `rust-version = "1.99"`; CI + Dockerfile use `1.99.0` exactly.
- OpenRouter integration (generative + Jev) is compiled in by default; at runtime it activates only when `OPENROUTER_API_KEY` (or `CPM_OPENROUTER_KEY_FILE`) is set. Without a key, LLM tools return `review_unavailable` — never a silent fallback. Core planning never requires the network.
- Plan files are the source of truth for graph *definition*; the server only reads/writes paths inside the project root.
- Plan directory: `<repo root>/.cpm-planner/` (`plans/<name>/<variant>.json`). Repo root = nearest ancestor of the server cwd (or `CPM_PROJECT_ROOT`) containing `.cpm-planner/` or `.git`; created on first `plan.export`/`plan.fork`.
- Every new `Deliverable` / prerequisite field MUST be added to `planner.rs::hash_graph` (else distinct graphs dedupe to one plan).
- Wire compatibility: plain-string `prerequisites` and plain-path `owned_files` keep working forever (object forms are additive).
- `PlanStatus.deliverables` rows are positional tuples — extend only by appending or via parallel arrays.
- Persisted `cached_result` must be recomputed when CPM math changes (versioned, see P1).
- Every new tool: constant + `PLAN_TOOL_NAMES` + schema in `plan_tool_definitions` + `instructions()` text + README tool table + `tests/server_integration.rs` roundtrip.
- Deterministic analysis is never delegated to the model; Jev output is advisory, never mutates.
- Branching: one feature branch per phase off `dev` (`chore/p0-rust-1.99`, `feat/p1-cpm-correctness`, …) → PR into `dev`; `dev` → `main` cuts 0.1.0 after P7b.
- One PR per phase into `dev`; `cargo fmt --check`, `cargo test` green on 1.99.0; clippy clean on touched code.

## Review Focus

1. **Plans persisted before P1** carrying the buggy `cached_result` — expect status/critical path to be correct after upgrade without re-submit (P1 recompute-on-load test).
2. **Mixed old/new wire forms in one graph** (string + object prerequisites, path + `{path, mode}` files) — must parse, hash stably, and round-trip through status (P3 tests).
3. **Lapse-limited / manual deliverables in a cohort** — one bad deliverable must never fail `acquire_cohort` for the whole plan (P2 test).
4. **Jev unavailable / no key / 4xx / timeout** — `plan.review` returns `review_unavailable` with deterministic lint still attached, never a silent fallback or a hang (P6 test with mock server).
5. **EV on a plan with zero-effort or missing-estimate deliverables and before baseline start** — no NaN/∞ in SPI/CPI (return `null` + reason) (P5 test).

---

## Phase map & dependencies

```
P0 toolchain ─┬─ P1 CPM correctness (#18,#15) ─┬─ P3 graph schema ─ P4 analysis + scorecard ─ P4P portfolio ─┬─ P5 EV (#16)
              │                                  │  (#21,#22,#27,#28,#12)  (#20,#19,#23a)        (#23b + new)  └─ P6 Jev (#26)
              ├─ P2 lease ergonomics (#17,#14,#24,#13)   (parallel with P1)                                        │
              └─ P7a install docs (parallel any time)                        P7b skill + docs refresh ◄─────────────┘
```

| Phase | Issues | Size | Can run in parallel with |
|---|---|---|---|
| P0 Toolchain 1.99.0 pin | — | S | — |
| P1 CPM correctness + schedule exposure | #18, #15 | S–M | P2, P7a |
| P2 Lease ergonomics | #17, #14, #24, #13 | M | P1, P7a |
| P3 Graph schema batch | #21, #22, #27, #28, #12 | L | — (touches everything) |
| P4 Analysis tools + plan scorecard | #20, #19, #23a | L | — |
| P4P Plan portfolio (projects, names, variants, revisions, compare, select) | #23b + new issue | L | — |
| P5 Earned value | #16 | L | P6 |
| P6 Optional Jev review | #26 | L | P5 |
| P7a MCP install instructions | — | S | anything |
| P7b `deliverable-cpm` skill + docs refresh | — | M | last |

---

## P0 — Pin Rust 1.99.0

**Files:** Create `rust-toolchain.toml`; Modify `Cargo.toml` (`rust-version`), `.github/workflows/ci.yml` (2× `dtolnay/rust-toolchain@stable` → `@1.99.0`), `.github/workflows/release.yml`, `Dockerfile` (`rust:1-slim` → `rust:1.99.0-slim`), `Cargo.lock`, `CONTRIBUTING.md`/README MSRV mention.

- [ ] `rustup toolchain install 1.99.0 --component rustfmt clippy`
- [ ] Add `rust-toolchain.toml`: `[toolchain] channel = "1.99.0"`, `components = ["rustfmt", "clippy"]`, `profile = "minimal"`
- [ ] Bump `rust-version = "1.99"`, pin CI/release/Docker as above
- [ ] Bump `rmcp` requirement to `1.8` (lock already 1.8.0).
- [ ] `cargo update`; `cargo build --all-targets && cargo test`; `cargo fmt --all`; fix new clippy lints (cherry-pick the async-trait clippy fixes from `origin/codex/prebuilt-installers` 528bbf1/79a31d6 if they still apply)
- [ ] Commit `chore(toolchain): pin Rust 1.99.0`; PR → dev

## P1 — CPM correctness (#18) + schedule exposure (#15)

**Files:** `src/algorithm.rs` (`build_result`, `forward_pass`), `src/planner.rs` (`priority_key`, `status`), `src/plan.rs` (`PlanStatus`), `src/plan_store.rs` (schema versioning + recompute), `src/server.rs`, tests `tests/algorithm.rs`, `tests/persistence.rs`, `tests/server_integration.rs`.

- [ ] **#18 regression tests first**: reproduce both reported shapes (parallel co-critical siblings P0a/P1a; unrelated zero-float tasks) — assert `critical_path` is a real prerequisite chain and `critical_path_duration == max(EF)`.
- [ ] Fix `build_result`: duration = max EF; path = walk back from max-EF sink choosing the predecessor with `EF == successor.ES`; add `critical_ids` (all zero-float) as a separate field.
- [ ] Fix `optimal_duration_parallel` (= max EF, not sum of batch maxima) and `forward_pass` duplicate-prerequisite wedge (dedupe prerequisites in `validate_graph` or count unique preds) — tests for each.
- [ ] Introduce `PRAGMA user_version` migration framework in `plan_store.rs` (subsume `migrate_counter_columns` as v1); add `cpm_version` constant; on load, recompute `cached_result` if stored version < current. Test: write an old-format row, reopen, assert corrected path.
- [ ] **`plan.get { plan_id }`** returns the stored graph (deliverables, estimates, files, metadata) — agents can no longer lose the definition after submit.
- [ ] **#15**: add parallel `schedule: [{id, es, ef, ls, lf, float, critical}]` and `ready: [ids sorted by float asc]` to `PlanStatus` (append-only); document in `instructions()`.
- [ ] PR → dev, closes #18 #15.

## P2 — Lease ergonomics (#17, #14, #24, #13)

**Files:** `src/planner.rs` (`acquire_cohort`, `mark_status`, `heartbeat`, `force_release`), `src/ports.rs` (trait signatures → args structs), `src/plan.rs` (`Cohort`), `src/server.rs`, `src/audit.rs`, `tests/locks.rs`, `tests/server_integration.rs`.

- [ ] Refactor `Planner` trait methods to take request structs (`AcquireRequest { max, ids, filter, ttl_seconds }`, etc.) so later phases add fields without signature churn.
- [ ] **#17**: lapse-limited deliverables are skipped and reported in new `Cohort.blocked: [{id, reason}]`; never fail the whole call. `force_release` (with `reset_counters: true`) clears lapse/attempt counts; error message names that exact call.
- [ ] **#14**: `acquire_cohort` accepts `ids: [..]` (raises `MISSING_PREREQUISITE`/not-ready per id in `blocked`) and `filter: {metadata: {key: value}}`. Deliverables with `metadata.executor`/`kind == "manual"` are excluded from unfiltered acquires.
- [ ] **#24**: new `plan.accept { plan_id, deliverable_id, accepted_by, evidence, override_lock? }` — validates prerequisites complete, emits `accepted` audit event, may override a live foreign lock only with `override_lock: true` (audited). Lockless `mark_status complete` now also checks prerequisites + emits audit.
- [ ] **#13**: `ttl_seconds` on acquire/heartbeat (clamped to server max `CPM_MAX_TTL_SECS`, default 8h); default TTL derived from `estimated_effort_hours` when absent? → **default stays 5 min**, explicit opt-in only. Document heartbeat cadence.
- [ ] PR → dev, closes #17 #14 #24 #13.

## P3 — Graph schema batch (#21, #22, #27, #28, #12)

**Files:** `src/plan.rs` (`Deliverable`, new `Prerequisite`, `OwnedFile`), `src/planner.rs` (`validate_graph`, `hash_graph`, `deliverable_to_task`), `src/locks.rs` (`file_to_deliverable` exclusive/append semantics, `reap_expired`), `src/plan_store.rs` (index rebuild), `src/algorithm.rs` (lag, duration), `src/task.rs`, all tests.

- [ ] `Prerequisite` untagged enum: `"id"` | `{ id, consumes?, kind?: "artifact"|"interface", lag_hours? }` with `ids()` accessor; migrate every `.prerequisites` use (**#21**, lag part of **#27**).
- [ ] `Deliverable.milestone: bool` (also honour legacy `metadata.milestone`); per-milestone critical path + hours in `PlanStatus.milestones` (**#22**).
- [ ] `Deliverable.duration_hours: Option<f32>` (calendar) used by CPM when present; effort remains for EV; lag added per edge in forward/backward pass (**#27**).
- [ ] `OwnedFile` untagged enum: path | `{ path, mode: "exclusive"|"append" }`; lock index tracks exclusive holder vs append set; `Cohort.shared_paths` (**#28**).
- [ ] Overlap validation: reject only between deliverables *not* ordered by reachability (shared transitive-closure helper `reachability(&graph)` in new `src/graph.rs`), append-vs-append allowed (**#12**).
- [ ] `hash_graph` covers all new fields; test: graphs differing only in each new field hash differently; mixed old/new forms round-trip.
- [ ] PR → dev, closes #21 #22 #27 #28 #12.

## P4 — Analysis tools (#20, #19, #23)

**Files:** Create `src/lint.rs`, `src/resource_schedule.rs`, `src/simulate.rs`; Modify `src/server.rs`, `src/plan_store.rs` (revisions), `src/planner.rs` (`priority_key`); tests `tests/lint.rs`, `tests/resource_schedule.rs`, `tests/revise.rs`. Wire in the existing unused `drag.rs`/`risk.rs`/`network_health.rs` metrics where they fit (lint/simulate output).

- [ ] **#20** `plan.lint { graph }` (no persistence): cycles with the actual loop, transitive-redundant edges, nodes reaching no milestone, edges without `consumes`, interface edges not targeting a contract node, unordered file overlaps. Port `skills/deliverable-cpm/scripts/graph_lint.py` test cases as Rust fixtures.
- [ ] **#19** `plan.schedule { plan_id|graph, capacities, resource_key = "metadata.owner", project_buffer_pct = 25 }`: list scheduling by longest remaining tail → makespan, per-deliverable start/finish, per-resource load, driving chain (dep vs resource waits), feeding buffers. `priority_key` switches to longest-remaining-tail. Port `resource_schedule.py` cases.
- [ ] **#23a** `plan.simulate { graph, capacities? }` = validate + CPM (+ schedule) without persisting; returns the scorecard below.
- [ ] **Scorecard** `src/metrics.rs` `PlanScorecard` (wires the currently-unused `risk.rs`, `drag.rs`, `network_health.rs`): makespan (CPM + resource-constrained), critical-path length & #critical ids, total float / near-critical count (float < 10% of makespan), criticality risk + band (`risk::criticality_risk`), activity risk, DRAG per critical node, diameter, cyclomatic complexity + project efficiency, merge-bias count (nodes with ≥3 prerequisites), total effort, parallelism (effort ÷ makespan), peak resource load, lint finding counts by severity. **Monte Carlo schedule risk (decided: include)** in `src/monte_carlo.rs`: optional `Deliverable.estimate: {optimistic, likely, pessimistic}` (+ `hash_graph`); single-point estimates = zero variance; seeded PRNG (`seed` arg, default fixed) → PERT-beta samples, N=2000 (arg `iterations`, max 50k) over the existing forward pass → P50/P80/P95 makespan, criticality index per deliverable (share of runs on the critical path), sensitivity ranking (correlation of each deliverable's duration with makespan). Tests: zero-variance graph ⇒ P50=P80=P95=CPM makespan; same seed ⇒ identical output; merge-bias fixture (5 parallel branches) ⇒ P80 > single-chain P80 at equal nominal makespan.
- [ ] PR → dev, closes #20 #19 #23 (a).

## P4P — Plan portfolio: projects, named plans, variants, revisions, compare, select (#23b + new issue)

Today: anonymous `plan_<uuid>`, one global DB, no listing, identical graphs dedupe to one plan — so a repo
can't hold several named plans, and alternatives can't be compared.

**Model** (plan-as-code)
- **Files** — `.cpm-planner/plans/<name>/<variant>.json` is the definition; committed with code; variants are sibling files. Listing = directory scan + DB runtime state.
- **Project** — a scope (default: git repo root's remote URL or path, overridable with `project` arg / `CPM_PROJECT`). Holds many plans.
- **Plan** (named, e.g. `auth-rewrite`) — a line of work in a project. Holds variants.
- **Variant** (e.g. `baseline`, `crash-ci`, `split-api`) — an alternative graph for the same plan; forked from another variant's revision. Exactly one variant per plan is **selected** (executable); others are drafts for comparison.
- **Revision** — immutable snapshot of a variant's graph over time (#23b). Execution state, locks, EV baselines attach to the selected variant's head revision. `plan_id` stays as an opaque handle to (project, plan, variant) so all existing tools keep working.

**Files:** `src/plan_store.rs` (tables `projects`, `plan_lines`, `variants`, `revisions`; migrate legacy plans into project `_legacy`, plan = old id), `src/portfolio.rs` (fork/diff/compare/select logic), `src/server.rs`, `tests/portfolio.rs`.

- [ ] `plan.sync { path, graph? }`: read (or accept inline when the server's cwd isn't the project) the plan file, validate, record a new revision if the content hash changed (carry-over rules below), return `plan_id`. `plan.submit` stays as the inline path, gaining optional `project`, `name`, `variant` (default `main`); dedup scoped to (project, name, variant), never across names.
- [ ] `plan.lint` / `plan.simulate` accept `{ path }` as well as `{ graph }` (dry runs on files).
- [ ] `plan.export { plan_id, path }` writes the stored graph to `.cpm-planner/plans/…` (for plans born in the DB).
- [ ] Drift: `plan.status` reports `definition_drift: true` when the file's hash ≠ the synced revision.
- [ ] Path safety: canonicalize, reject anything outside the project root (test with `../`, symlink, absolute paths).
- [ ] `plan.list { project? }` → plans, variants, selected flag, head revision, status summary, scorecard headline.
- [ ] `plan.fork { plan_id, variant, apply?: [proposal] }` → writes a new sibling variant file (optionally applying lint/review proposals: remove edge, split node, crash estimate) and syncs it as a draft.
- [ ] `plan.revise { plan_id, graph }` (#23b): new revision; carry statuses/counters for unchanged ids; re-open dependents whose prerequisites changed (and interface consumers of revised contracts); refuse removal of a deliverable with a live lock unless `force`; return diff `{added, removed, changed, reopened}`.
- [ ] `plan.compare { plan_ids: [..] | plan: name, capacities?, weights? }` → scorecards side-by-side, structural diff vs first, Pareto front over (makespan, P80, criticality risk, effort, peak load), weighted rank + one-line rationale per variant.
- [ ] **Design vs execution (decided):** any number of variants may be designed (lint/simulate/compare/fork/revise); exactly one per plan is executable. `acquire_cohort`, `heartbeat`, `mark_status`, `plan.accept`, `plan.baseline` reject non-selected variants with `VARIANT_NOT_SELECTED`.
- [ ] `plan.select { plan_id }` → makes a variant executable; carries progress from the previously selected variant via the revise carry-over rules; refuses if the old variant holds live locks unless `force`. Audited.
- [ ] `plan.archive { plan_id }` hides drafts from `plan.list`.
- [ ] **Improvement loop** (documented in skill + `instructions()`): `lint → review (Jev, optional) → fork variants from proposals → simulate/compare → select → execute → EV actuals → calibrate estimates for the next revision`.
- [ ] PR → dev; open a GitHub issue for this phase first so it's tracked like the rest.

## P5 — Earned value (#16)

**Files:** Create `src/earned_value.rs`; Modify `src/plan_store.rs` (tables `baselines`, `ev_actuals`, `ev_snapshots`), `src/planner.rs` (`mark_status`), `src/server.rs`; tests `tests/earned_value.rs`.

Definitions (hours are the cost unit; optional `metadata.cost_rate` multiplies):
- BAC = Σ budget_i, budget_i = `estimated_effort_hours` (or estimator default).
- PV(t) = Σ budget_i × clamp((t − ES_i)/(EF_i − ES_i), 0, 1) on the frozen baseline schedule, t in **wall-clock hours** from baseline start by default; opt-in working calendar `calendar: {hours_per_day: 8, workdays: "Mon-Fri"}` on `plan.baseline`.
- EV = Σ budget_i × earned%_i by rule: `0/100` (default), `50/50`, `weighted` (reported `earned_pct`).
- AC = Σ actual hours (`actual_effort_hours` on complete; else summed lease durations).
- SV = EV−PV, CV = EV−AC, SPI = EV/PV, CPI = EV/AC, EAC = BAC/CPI, ETC = EAC−AC, VAC = BAC−EAC, TCPI = (BAC−EV)/(BAC−AC); undefined ratios → `null` + reason.

- [ ] `plan.baseline { plan_id, start, calendar?, rebaseline_reason? }` freezes schedule + PV curve; re-baseline audited.
- [ ] `mark_status` accepts `earned_pct`, `actual_effort_hours`, `evidence` (update `deny_unknown_fields` arg struct + schema); lease durations accumulate as AC proxy.
- [ ] `plan.ev { plan_id, as_of? }` → totals + per-deliverable rows + critical-path float consumed + `alerts` (SPI or CPI < 0.9 on two consecutive snapshots).
- [ ] `plan.snapshot { plan_id }` appends dated snapshot; `format: "json"|"markdown"` export.
- [ ] Textbook-number tests (hand-computed fixture), zero-budget/no-baseline edge cases.
- [ ] PR → dev, closes #16.

## P6 — OpenRouter integration: generative LLM + Jev (#26)

**Files:** Create `src/llm/mod.rs` (config + `JudgmentModel` / `Generator` traits), `src/llm/openrouter.rs` (rig `providers::openrouter` client), `src/llm/jev.rs` (rig-typesafeai client), `src/review.rs`; Modify `Cargo.toml`, `src/bin/server.rs` (config), `src/server.rs`; tests `tests/review.rs` (fakes + mock HTTP).

Research facts (2026-10-09):
- Jev on OpenRouter = `typesafe/jev-1.13` (alias `~typesafe/jev-latest`), a **decisions-only** model (choice / noul / score with probabilities; no chat). 32K prompt; $0.042/M input, output free.
- **rig 0.44.0 (2026-10-07) ships `rig-typesafeai`** (root `rig` feature `typesafeai`): typed `Query` structs, `Choice` / `Noul` / `Score` / `DynamicScore` builders, `Jev::evaluation().evaluate(&state, &query)`. `JevConfig::new(token).model(..).with_endpoint(..)` — defaults to `https://api.typesafe.ai/v1/systemone` + `JEV_TOKEN`, but endpoint, model and bearer token are configurable.
- OpenRouter documents `POST /api/v1/systemone` as the drop-in for TypeSafe SDKs → planned config: `JevConfig::new(OPENROUTER_API_KEY).model("typesafe/jev-1.13").with_endpoint("https://openrouter.ai/api/v1/systemone")`. **Not yet verified live** — Task 1 is the spike.
- rig-core `providers::openrouter` (`OPENROUTER_API_KEY`) covers generative chat models + structured output (`output_schema`).

- [ ] **Spike (gate):** with a real key, `cargo run --example jev_openrouter_smoke` calls Jev through OpenRouter via rig-typesafeai with one Noul question; record request/response shape + `usage.cost`. If OpenRouter's systemone path rejects rig's wire format, fall back to a thin custom `rig` `Operation` against `/api/alpha/decisions` (same `Query` types) and file an upstream rig issue.
- [ ] Deps: `rig = { version = "0.44", default-features = false, features = ["typesafeai", "reqwest", "rustls"] }` (trim to `rig-core` + `rig-typesafeai` if the facade pulls the agent runtime unnecessarily). Check binary size/compile-time delta and record it in the PR.
- [ ] Config: `OPENROUTER_API_KEY` | `CPM_OPENROUTER_KEY_FILE`; `CPM_JEV_MODEL` (default `typesafe/jev-1.13`); `CPM_LLM_MODEL` (generative, default chosen in spike — cheap structured-output-capable model); `CPM_LLM_TIMEOUT_SECS` (default 30).
- [ ] **Decided:** reasoning about improvements lives in the skill (the calling agent is the generative model). The server provides deterministic lint + Jev's calibrated scores + simulate/compare as the objective check. rig's OpenRouter client is the standard integration; the server-side generative path is built as a client but only wired to a tool when a headless use (CI review, nightly health report) is needed.
- [ ] Division of labour: **Jev** answers typed judgments — `false_dependency` (Noul per edge), `missing_dependency` (Noul over deterministically pre-filtered pairs), `split_candidate` / `interface_split` (Score), crash technique (Choice). **Generative LLM** produces content Jev can't: concrete split proposals (new deliverable ids, file partition, `consumes` text), crash-option descriptions, and the human-readable review summary — always as `output_schema` structured output, validated by `plan.lint` before being returned.
- [ ] `plan.review { plan_id|path|graph, capacities? }`: lint first (deterministic findings never delegated) → Jev judgments with probabilities per edge/node → deterministic proposals where mechanical (remove edge flagged false with p ≥ threshold, crash = estimate change) each verified with `plan.simulate` (hours saved) and ranked by hours saved ÷ cost → returned as `findings` + `proposals` that `plan.fork { apply }` can consume. Splits are returned as *candidates* for the agent to design. Records model, question hash and `usage.cost`. Never mutates.
- [ ] No key / outage / 4xx / timeout → `review_unavailable { reason }` + lint findings; tests via fakes and a local mock server (mockito/httpmock, same as rig's own tests).
- [ ] PR → dev, closes #26.

## P7a — MCP install instructions (parallel, any time)

**PR #11 (decided 2026-10-09):** land #29 → #30 → P2 first, then rebase PR #11 (`codex/prebuilt-installers`) onto `dev` as the base of P7a: drop its `src/` clippy workarounds (superseded by P0) and its full-file `src/ports.rs` rewrite, pin its workflows to `dtolnay/rust-toolchain@1.99.0`, fix the failing `test (macos-latest)` job, sync `server.json`/version, then merge.

**Files:** `README.md` (new "Install as an MCP server" section), `server.json` (version sync + CI check), optionally `scripts/install.sh`/`install.ps1` salvaged from `origin/codex/prebuilt-installers`.

- [ ] Per-client snippets: Claude Code (`claude mcp add cpm-planner -- cpm-planner`, with `-e CPM_PLANNER_DB=…`, `--scope user|project`), Claude Desktop (`claude_desktop_config.json`), Cursor (`.cursor/mcp.json`), VS Code (`.vscode/mcp.json`), Codex (`~/.codex/config.toml` `[mcp_servers.cpm-planner]`), Docker (`docker run -i --rm -v …:/data ghcr.io/praxec/cpm-planner`), praxec.
- [ ] OpenRouter env vars in each snippet (`OPENROUTER_API_KEY`, commented, marked optional).
- [ ] "Agent self-install" block: copy-paste instructions an agent can follow (install binary → register → verify via `plan.status` tool listing).
- [ ] `server.json` version = Cargo version, CI step asserts equality; MCP smoke test (`scripts/mcp-smoke.mjs` from codex branch) in CI.
- [ ] PR → dev.

## P7b — `deliverable-cpm` skill + docs refresh (last)

**PR #25 (decided 2026-10-09):** held open, not merged as-is (it teaches the scratch-`graph.json` workflow and the #18 caveat). P7b rewrites it on that branch per the steps below, then merges.

**Files:** `skills/deliverable-cpm/SKILL.md` (from `origin/skill/deliverable-cpm` b40f267), README, CHANGELOG, `instructions()`.

- [ ] **Plan-improvement loop** section: `plan.review` → reason about flagged items → write candidate variants (`plan.fork`) → `plan.simulate`/`plan.compare` (P80, criticality risk, effort, peak load) → explain trade-off → `plan.select`.
- [ ] Rebase skill branch; make the workflow plan-as-code (author `.cpm-planner/plans/<name>/<variant>.json` → `plan.lint {path}` → `plan.sync` → execute; never keep an untracked scratch `graph.json`); replace Python stand-ins (`graph_lint.py`, `resource_schedule.py`) and metadata conventions with native `plan.lint` / `plan.schedule` / `plan.simulate` / `plan.revise` / `plan.baseline` / `plan.ev` / `plan.review` and first-class `milestone`, `consumes`, `kind`, `duration_hours`; delete the scripts.
- [ ] Skill install instructions for Claude Code (`~/.claude/skills/` or plugin), plus README section on the planning method.
- [ ] **Dogfood the roadmap (decided 2026-10-09):** the roadmap itself is a cpm-planner plan at `.cpm-planner/plans/backlog-roadmap/main.json` — one deliverable per phase/task, prerequisites from the phase map, effort = size points (S=1, S–M=1.5, M=2, L=4), `metadata.owner` = junior|subagent|owner. Lifecycle: created right after P1 merges (submitted inline until `plan.sync` exists, then synced from the file in P4P); baselined with `plan.baseline` as soon as P5 ships; each completed phase marked with `actual_effort_hours` + PR link as evidence. P7b publishes the real `plan.ev` report (PV, EV, AC, SPI, CPI, EAC) and `plan.compare`/Monte Carlo P80 in the 0.1.0 release notes, and the skill uses it as its worked example.
- [ ] README tool table, CHANGELOG 0.0.2→0.1.0, `server.json`, bump version to 0.1.0.
- [ ] PR → dev.

---

## Decisions (2026-10-09)

1. OpenRouter via rig (`providers::openrouter`) + rig-typesafeai (Jev) is the standard integration, key-activated. Key provided at P6.
2. One feature branch + PR per phase into `dev`; cut 0.1.0 after P7b.
3. Reuse `origin/codex/prebuilt-installers` (P0 clippy fixes, P7a scripts/smoke test) and `skill/deliverable-cpm` (P7b). Stale local branches deleted.
4. EV clock: wall-clock default, opt-in working calendar.
5. #26: improvement reasoning is a skill; server = lint + Jev scores + simulate/compare.
6. Plan-as-code in `<repo root>/.cpm-planner/plans/`; runtime state in DB.
7. Many variants designed, one executed.
8. Monte Carlo included in P4 scorecard.
