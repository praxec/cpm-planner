# cpm-planner

[![CI](https://github.com/praxec/cpm-planner/actions/workflows/ci.yml/badge.svg)](https://github.com/praxec/cpm-planner/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/cpm-planner.svg)](https://crates.io/crates/cpm-planner)
[![docs.rs](https://docs.rs/cpm-planner/badge.svg)](https://docs.rs/cpm-planner)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

cpm-planner is a Critical Path Method (CPM) planner exposed as an MCP server. You
submit a task graph; it computes the schedule — earliest/latest start and finish,
slack, the critical path, and the bottleneck tasks that actually gate completion —
and it coordinates **lock-aware cohort scheduling** so multiple workers can run
disjoint deliverables in parallel without colliding. Any MCP client (Claude Code,
Cursor, a custom orchestrator, or an [praxec](https://github.com/praxec/praxec)
workflow) drives it over the standard protocol.

It is a standalone tool: it has no dependency on praxec and is consumed
purely over MCP.

## Install

From crates.io:

```sh
cargo install cpm-planner
```

Or download a pre-built binary for your platform from the
[latest release](https://github.com/praxec/cpm-planner/releases/latest)
(verify against the release's `checksums.sha256`):

| Platform | Download |
|----------|----------|
| Linux x86_64 | [`.tar.gz`](https://github.com/praxec/cpm-planner/releases/latest/download/cpm-planner-x86_64-unknown-linux-gnu.tar.gz) |
| Linux ARM64 | [`.tar.gz`](https://github.com/praxec/cpm-planner/releases/latest/download/cpm-planner-aarch64-unknown-linux-gnu.tar.gz) |
| macOS x86_64 | [`.tar.gz`](https://github.com/praxec/cpm-planner/releases/latest/download/cpm-planner-x86_64-apple-darwin.tar.gz) |
| macOS Apple Silicon | [`.tar.gz`](https://github.com/praxec/cpm-planner/releases/latest/download/cpm-planner-aarch64-apple-darwin.tar.gz) |
| Windows x86_64 | [`.zip`](https://github.com/praxec/cpm-planner/releases/latest/download/cpm-planner-x86_64-pc-windows-msvc.zip) |

It speaks MCP over stdio (the standard transport). Wire it into your editor like
any other MCP server:

```jsonc
{ "command": "cpm-planner", "args": [] }
```

## MCP tools

| Tool | Does |
|------|------|
| `plan.submit` | Submit a task graph; returns a plan id (idempotent on the graph + caller). |
| `plan.acquire_cohort` | Atomically acquire up to N ready deliverables with mutually disjoint file sets. |
| `plan.heartbeat` | Refresh the TTL on a held lock. |
| `plan.mark_status` | Mark a deliverable complete/failed; releases its lock. |
| `plan.status` | Read-only snapshot of the plan and its locks. |
| `plan.force_release` | Operator escape hatch: release a lock regardless of holder/TTL. |

## Use as a library

The CPM kernel is also a plain Rust library, independent of MCP:

```rust
use cpm_planner::{CpmAlgorithm, Task, TaskKind};

let mut tasks = vec![
    Task::new("design", "Design", TaskKind::Custom { description: "design".into() }, 4.0),
    Task::new("build", "Build", TaskKind::Custom { description: "build".into() }, 8.0)
        .depends_on("design"),
    Task::new("test", "Test", TaskKind::Custom { description: "test".into() }, 2.0)
        .depends_on("build"),
];

let result = CpmAlgorithm::calculate(&mut tasks);
println!("critical path: {:?}", result.critical_path); // ["design", "build", "test"]
// also: result.bottlenecks, result.optimal_duration_parallel,
// and per-task .float (slack) / .is_critical on each Task.
```

See the [API docs](https://docs.rs/cpm-planner).

## Use with an MCP client (e.g. praxec)

cpm-planner is fully standalone — it speaks plain MCP and has no code dependency
on any particular client. As one example, you can wire it into an
[praxec](https://github.com/praxec/praxec) workflow as an MCP
connection (protocol only, no shared code):

```yaml
connections:
  planner:
    kind: mcp
    command: cpm-planner
```

## License

[Apache-2.0](LICENSE).
