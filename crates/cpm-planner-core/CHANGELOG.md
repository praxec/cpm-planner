# Changelog

All notable changes to `cpm-planner-core` are documented in the
[cpm-planner changelog](https://github.com/praxec/cpm-planner/blob/main/CHANGELOG.md):
both crates are released together and always share one version.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-11-13

### Added

- First release. The pure scheduling kernel of cpm-planner, split out of the `cpm-planner` crate
  with no SQLite, tokio, rmcp, rig or other I/O dependency: the plan graph model and
  `PlannerError` (`plan`), `validate` (`validate_graph`), `algorithm`, `task`, `schedule`
  (`compute_cpm`), `estimator`, `calendar`, `resource_schedule`, `monte_carlo`, `drag`, `risk`,
  `network_health` and `earned_value`. Earlier history of these modules is in the cpm-planner
  changelog under 0.7.1 and before, when they were part of `cpm-planner`.

[0.8.0]: https://github.com/praxec/cpm-planner/releases/tag/v0.8.0
