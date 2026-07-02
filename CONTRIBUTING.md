# Contributing to cpm-planner

Thanks for your interest. cpm-planner is a small, focused crate — a CPM planning
kernel plus an MCP server façade.

## Development

```sh
cargo build
cargo test
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

CI runs build + test on Linux/macOS/Windows, plus `rustfmt` and `clippy` (warnings
are denied). Please make sure those pass locally before opening a pull request.

## Guidelines

- Keep the crate's two concerns clean: the pure CPM kernel (`algorithm`, `task`)
  has no I/O; the MCP/lock-coordination layer (`planner`, `server`) sits on top.
- Add a test for any behavior change. The kernel is deterministic, so tests
  should be too.
- Conventional, focused commits.

## Reporting issues

Open an issue with a minimal reproduction — for scheduling bugs, the task graph
(or `plan.submit` payload) that produces the wrong result is ideal.
