# Contributing to cpm-planner

Thanks for your interest. cpm-planner is a CPM planning kernel plus an MCP
server facade. By participating you agree to the
[Code of Conduct](CODE_OF_CONDUCT.md). Security issues go through
[SECURITY.md](SECURITY.md), not public issues. Questions: [SUPPORT.md](SUPPORT.md).

## Toolchain

The Rust toolchain is pinned in `rust-toolchain.toml` (1.99.0, with rustfmt and
clippy); `rust-version` in `Cargo.toml` is the MSRV. `rustup` picks the pin up
automatically. Files use LF line endings (`.gitattributes`, enforced by
`tests/repo_hygiene.rs`).

## Gates

Run these before opening a pull request; CI runs them too.

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
scripts/check-version-sync.sh
```

## Architecture map

The pure kernel has no I/O; the planner, store, and server layers sit on top.

- `src/algorithm.rs`: the CPM forward/backward pass.
- `src/task.rs`: core CPM task and result types.
- `src/estimator.rs`: effort estimation per task kind.
- `src/graph.rs`: crate-private helpers over the deliverable graph.
- `src/schedule.rs`: turns a validated plan graph into a critical-path result.
- `src/lint.rs`: `plan.lint`, static checks over a plan graph.
- `src/drag.rs`: Devaux DRAG and graph diameter.
- `src/risk.rs`: activity-risk and criticality-risk calculations.
- `src/network_health.rs`: iDesign network-health metrics.
- `src/metrics.rs`: the plan scorecard combining DRAG, risk, health and lint.
- `src/monte_carlo.rs`: seeded Monte Carlo schedule risk.
- `src/resource_schedule.rs`: resource-constrained leveling, driving chain, buffers.
- `src/earned_value.rs`: pure earned-value engine (PV/EV/AC, SPI/CPI/EAC).
- `src/edits.rs`: pure structured edits over a plan graph.
- `src/revise.rs`: pure plan revision carrying progress over.
- `src/compare.rs`: Pareto front and weighted rank of plan variants.
- `src/simulate.rs`: read-only simulation of a graph that is never persisted.
- `src/review.rs`: `plan.review`, lint plus one batched judgment call.
- `src/plan.rs`: wire data model for the `Planner` trait.
- `src/ports.rs`: the lock-aware `Planner` trait.
- `src/planner.rs`: `BasicCpmPlanner`, the shipped `Planner` implementation.
- `src/planner/ev.rs`: earned-value operations of the planner.
- `src/locks.rs`: lock store for deliverable leases.
- `src/audit.rs`: audit events for lock-lifecycle transitions.
- `src/plan_store.rs`: SQLite persistence for planner state.
- `src/ev_store.rs`: SQLite storage for baselines, actuals and EV snapshots.
- `src/portfolio.rs`: plan lines, variants and revision history in the store.
- `src/project.rs`: project-root discovery and confined plan-file paths.
- `src/llm/mod.rs`: OpenRouter/Jev configuration and the judgment-model seam.
- `src/llm/jev.rs`: judgment model over Jev via `rig-typesafeai`.
- `src/llm/openrouter.rs`: rig-core OpenRouter chat client for future headless use.
- `src/server.rs`: the MCP tool surface (`PlanServer`).
- `src/bin/server.rs`: the `cpm-planner` binary entry point.
- `src/lease_hours_tests.rs`, `src/mark_actuals_tests.rs`: in-crate test modules.

Integration tests live in `tests/`.

## Skills

Agent skills live in `skills/`. `tests/skill_docs.rs` validates the tool names
they reference against the server's tool list.

## Branch model

- Feature branches open pull requests into `dev`.
- Releases go `dev` to `main` through a release PR merged with a merge commit.
  `gitflow-guard.yml` only lets `main` accept merges from `dev`.
- A tag `vX.Y.Z` on `main` runs `.github/workflows/release.yml`. The workflow
  triggers on any `v*` tag, so tags are pushed only on `main` by convention;
  see `docs/releasing.md`.

## Commits and pull requests

Use [Conventional Commits](https://www.conventionalcommits.org/)
(`feat(scope): ...`, `fix(scope): ...`, `docs: ...`, `chore: ...`). Keep commits
focused. Add an entry under `[Unreleased]` in `CHANGELOG.md` and fill in the PR
template.

## Test discipline

- Every behavior change comes with a test. The kernel is deterministic, so
  tests are too.
- Use declarative test names with one behavioral assertion each.
- Default tests must not touch the network.
- The live OpenRouter test is opt-in and costs money:

  ```sh
  OPENROUTER_API_KEY=... cargo test --test server_review -- --ignored
  ```

## Reporting issues

Use the issue forms. For scheduling bugs, include the `plan.submit` payload or
plan file that produces the wrong result.
