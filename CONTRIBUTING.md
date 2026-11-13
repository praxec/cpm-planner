# Contributing to cpm-planner

Thanks for your interest. cpm-planner is a CPM planning kernel plus an MCP
server facade. By participating you agree to the
[Code of Conduct](CODE_OF_CONDUCT.md). Security issues go through
[SECURITY.md](SECURITY.md), not public issues. Questions: [SUPPORT.md](SUPPORT.md).

## Toolchain

The Rust toolchain is pinned in `rust-toolchain.toml` (1.99.1, with rustfmt and
clippy); `rust-version` in `Cargo.toml` is the MSRV. `rustup` picks the pin up
automatically. Files use LF line endings (`.gitattributes`, enforced by
`tests/repo_hygiene.rs`).

## Gates

Run these before opening a pull request; CI runs them too.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --all-targets
cargo test --workspace --doc
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo publish --workspace --dry-run --locked
scripts/check-core-deps.sh
cargo deny check
scripts/check-version-sync.sh
node --test "npm/test/*.test.mjs"
```

Prerequisites beyond the Rust toolchain:

- `cargo nextest` is [cargo-nextest](https://nexte.st/)
  (`cargo install cargo-nextest --locked`). Nextest runs the Rust test binaries;
  `.config/nextest.toml` keeps the suite running after a failure so one cycle reports every
  defect. `cargo test --doc` remains a separate gate because nextest does not run doctests.
- `cargo deny` is [cargo-deny](https://github.com/EmbarkStudios/cargo-deny)
  (`cargo install cargo-deny`).
- `scripts/check-version-sync.sh` needs `bash` and
  [`jq`](https://jqlang.org/) on `PATH` (on Windows, Git Bash).
- The npm launcher tests need Node 21 or later, because `node --test` takes a
  glob only from Node 21; the published package itself supports Node 18 and
  later.

## Architecture map

The repository is a Cargo workspace of two crates. The pure kernel,
`cpm-planner-core` in `crates/cpm-planner-core/`, has no I/O and no SQLite,
async runtime, MCP or LLM dependency (`scripts/check-core-deps.sh` enforces
that in CI). `cpm-planner` (the root package) re-exports the kernel's modules
at their old paths and adds the planner, store and server layers on top.

Kernel, in `crates/cpm-planner-core/src/`:

- `plan.rs`: the graph model (deliverables, prerequisites, owned files,
  statuses), ids, limits and `PlannerError`.
- `validate.rs`: `validate_graph`, the structural checks before scheduling.
- `algorithm.rs`: the CPM forward/backward pass.
- `task.rs`: core CPM task and result types.
- `estimator.rs`: effort estimation per task kind.
- `graph.rs`: hidden helpers over the deliverable graph, shared with `cpm-planner`.
- `schedule.rs`: turns a validated plan graph into a critical-path result.
- `calendar.rs`: working calendars and their window arithmetic.
- `drag.rs`: Devaux DRAG and graph diameter.
- `risk.rs`: activity-risk and criticality-risk calculations.
- `network_health.rs`: iDesign network-health metrics.
- `monte_carlo.rs`: seeded Monte Carlo schedule risk.
- `resource_schedule.rs`: resource-constrained leveling, driving chain, buffers.
- `earned_value.rs`: pure earned-value engine (PV/EV/AC, SPI/CPI/EAC).

Planner, store and server, in `src/`:

- `src/lint.rs`: `plan.lint`, static checks over a plan graph.
- `src/metrics.rs`: the plan scorecard combining DRAG, risk, health and lint.
- `src/edits.rs`: pure structured edits over a plan graph.
- `src/revise.rs`: pure plan revision carrying progress over.
- `src/compare.rs`: Pareto front and weighted rank of plan variants.
- `src/simulate.rs`: read-only simulation of a graph that is never persisted.
- `src/review.rs`: `plan.review`, lint plus one batched judgment call.
- `src/plan.rs`: re-exports the core graph model and adds the execution
  request and response types of the `Planner` trait.
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
- `src/skills.rs`: the `cpm-planner skills install|uninstall|list` subcommand.
- `src/bin/server.rs`: the `cpm-planner` binary entry point.
- `build.rs`: embeds `skills/` into the binary at build time.
- `npm/`: the `@matthew-cochran/cpm` npx launcher (Node, no dependencies),
  tested by `npm/test/`.
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
  OPENROUTER_API_KEY=... cargo nextest run --test server_review \
    -E 'test(review_against_openrouter_live)' --ignored
  ```

## Reporting issues

Use the issue forms. For scheduling bugs, include the `plan.submit` payload or
plan file that produces the wrong result.
