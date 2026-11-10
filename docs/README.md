# cpm-planner documentation

Start with the [project README](../README.md) for installation and the full
MCP tool reference, and the [agent install guide](AGENT-INSTALL.md) for MCP
client registration and the agent skills.

## Guides

- [Agent install guide](AGENT-INSTALL.md): install cpm-planner, register
  the MCP server, install the skills and verify, per AI coding tool.
- [Architecture](architecture.md): the modules in `src/`, the request flow from
  submit to review, the SQLite schema versions, the concurrency model and the
  security boundaries.
- [Plan overview and explain](overview.md): `plan.overview`, one snapshot of
  status, definition, earned value and evidence, and `plan.explain`, why a
  deliverable is blocked and what completing it unlocks.
- [Offline plan optimization](optimize.md): `plan.optimize`, deterministic
  edit-list proposals (merges, false dependencies, gate consolidation and
  bypass) checked by simulation, run before the optional AI review.
- [Evidence](evidence.md): evidence records with a scope and an identity,
  which scope closes which deliverable, stale or current verdicts, and the
  gate-scope lint findings.
- [Calendars and levelled baselines](calendars.md): per-resource working
  calendars on `plan.schedule`, `plan.simulate` and `plan.baseline`, levelled
  baselines, and how PV accrues inside working windows.
- [Threads](threads.md): `metadata.thread` continuity threads, how leasing
  prefers the agent that advanced a thread, and how to size deliverables per
  thread.
- [Releasing](releasing.md): the maintainer runbook for version bumps, the
  release PR, tagging, verifying the published assets, and the manual
  crates.io and npm steps.
- [Agent tool matrix](agents/tool-matrix.md): where each AI coding tool loads
  skills and commands from, with sources.
- [Roadmap earned value](ev/backlog-roadmap.md): how the project tracked its
  own roadmap with the earned-value tools.

## Project files

- [CONTRIBUTING.md](../CONTRIBUTING.md): toolchain, gates, branch model.
- [SECURITY.md](../SECURITY.md): supported versions and private reporting.
- [SUPPORT.md](../SUPPORT.md): where to ask questions.
- [CHANGELOG.md](../CHANGELOG.md): release history.
- [Agent skill](../skills/deliverable-cpm/SKILL.md): the plan-as-code method
  for agents.

## Design history

`docs/superpowers/` holds the design specs and implementation plans the
project was built from, phase by phase. It is a historical record, not user
documentation, and it may describe intermediate designs that later changed.
It stays at this path because the planning tooling reads and writes it there.
It is not included in the published crate.
