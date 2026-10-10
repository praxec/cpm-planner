# cpm-planner documentation

Start with the [project README](../README.md) for installation, client
registration and the full MCP tool reference.

## Guides

- [Agent install guide](AGENT-INSTALL.md): install cpm-planner, register
  the MCP server, install the skills and verify, per AI coding tool.
- [Architecture](architecture.md): the modules in `src/`, the request flow from
  submit to review, the SQLite schema versions, the concurrency model and the
  security boundaries.
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
