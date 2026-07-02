# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.0.1] - 2026-06-17

### Added

- Initial release.
- Critical Path Method (CPM) planner as a reusable kernel: earliest/latest
  start, slack, the critical path, and bottleneck tasks that gate completion.
- A stdio MCP server exposing the planner over the standard protocol.
- Lock-aware parallel execution coordination so multiple workers can run
  file-disjoint deliverables concurrently without colliding.
- Six-tool MCP surface: `plan.submit`, `plan.acquire_cohort`, `plan.heartbeat`,
  `plan.mark_status`, `plan.status`, and `plan.force_release`.

[0.0.1]: https://github.com/praxec/cpm-planner/releases/tag/v0.0.1
