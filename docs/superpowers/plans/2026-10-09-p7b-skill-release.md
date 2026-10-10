# P7b — `deliverable-cpm` Skill (plan-as-code) + 0.1.0 Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Agents planning with cpm-planner follow a documented plan-as-code method (files under `.cpm-planner/plans/`, native lint/schedule/simulate/compare/review/EV tools, an explicit improvement loop) packaged as an installable skill; the 0.1.0 release ships with complete docs and the dogfooded roadmap EV report.

**Architecture:** Rework PR #25's branch (`skill/deliverable-cpm`) onto current `dev`: rewrite `skills/deliverable-cpm/SKILL.md`, delete the Python stand-ins, add `skills/deliverable-cpm/examples/` (a small plan file + a worked improvement-loop transcript sketch). Release prep bumps to 0.1.0 across Cargo/server.json, finalises CHANGELOG, and refreshes README/instructions().

**Spec:** roadmap § P7b (plan-as-code, PR #25 decision, dogfood decision).

## Global Constraints
- The skill never tells agents to keep untracked scratch graphs; the only plan files live at `<repo root>/.cpm-planner/plans/<name>/<variant>.json`.
- Every tool named in the skill exists with the exact argument names in `src/server.rs` (verified by a test that greps SKILL.md tool names against `PLAN_TOOL_NAMES`).
- Skill frontmatter: `name: deliverable-cpm`, a `description` under 1024 chars starting with "Use when…".
- Version 0.1.0 consistent (Cargo.toml, Cargo.lock, server.json — CI version-sync passes).
- README has one tools table covering every tool; CHANGELOG `[0.1.0] - <release date>` replaces `[Unreleased]` with compare links.

### Task 1: Rewrite the skill
Sections: (1) nodes are artifacts (`metadata.artifact`), (2) edges are consumption (`{id, consumes, kind}`; artifact vs interface; `metadata.contract`), (3) author the file → `plan.lint {path}` until clean, (4) `plan.sync {path}`, verify `critical_path` runs `__start__ … __finish__`, milestones, (5) level resources with `plan.schedule {capacities}`, (6) the improvement loop: `plan.review` (Jev, optional) → reason → `plan.fork {edits}` → `plan.simulate`/`plan.compare` (makespan, P80, criticality risk, effort, peak load) → explain trade-off → `plan.select`, (7) execution: `plan.acquire_cohort` (`ids`/`filter`, `ttl_seconds`, `blocked`, `needs_operator`), `plan.heartbeat`, `plan.mark_status` (`earned_pct`, `actual_effort_hours`, `evidence`), `plan.accept` for owner/manual work, (8) baseline + EV: `plan.baseline`, `plan.ev`, `plan.snapshot`, alerts, re-baseline rules, (9) revising: edit the file → `plan.sync` (carry-over rules summary). Delete `scripts/graph_lint.py`, `scripts/resource_schedule.py`. Add `examples/small-plan.json` (valid, lint-clean) + `examples/improvement-loop.md`. Test `tests/skill_docs.rs`: every `plan.*` token in SKILL.md is in `PLAN_TOOL_NAMES`; `examples/small-plan.json` parses and lints clean.
Install docs in README: Claude Code (`~/.claude/skills/deliverable-cpm/` or project `.claude/skills/`), Codex (`.agents/skills/`), copying from a release or the repo.

### Task 2: Release 0.1.0 prep
Version bump; CHANGELOG `[0.1.0]` with sections consolidated from `[Unreleased]` (Added/Changed/Fixed/Breaking for library API), compare link `[0.1.0]: …/compare/v0.0.1...v0.1.0`; README refresh (tool table, env vars, plan-as-code quickstart, EV quickstart); `instructions()` final pass (tool count, workflow paragraph); `server.json` description ≤ 100 chars.

### Task 3: Dogfood report
Using the release-candidate binary: `plan.sync` the roadmap file `.cpm-planner/plans/backlog-roadmap/main.json` (migrate the existing unnamed plan by syncing the file as name `backlog-roadmap`, variant `main`; accept completed phases with merged-PR evidence; record actual hours from git history per phase); `plan.baseline` at the roadmap's creation time (2026-10-09T15:00Z) with reason "initial baseline"; `plan.snapshot {format: markdown}` → `docs/ev/backlog-roadmap.md`; `plan.simulate {monte_carlo}` P80 for the remaining work; include both in the 0.1.0 release notes section of CHANGELOG.
