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

### Prebuilt binary (no Rust, Cargo, or Git required)

```sh
# Linux / macOS
curl -fsSL https://github.com/praxec/cpm-planner/releases/latest/download/install.sh | sh

# Windows (PowerShell)
irm https://github.com/praxec/cpm-planner/releases/latest/download/install.ps1 | iex
```

The installer resolves your OS and CPU architecture, downloads the matching
release asset, verifies its SHA-256 against the release's `checksums.sha256`,
extracts it safely, and atomically installs the binary into a user-local
managed directory (`$HOME/.local/bin` on Linux/macOS,
`%LOCALAPPDATA%\Programs\cpm-planner` on Windows). It never compiles from
source and fails loudly on an unsupported OS/architecture.

Pin a specific release instead of the current latest stable:

```sh
curl -fsSL https://github.com/praxec/cpm-planner/releases/latest/download/install.sh \
  | sh -s -- --version v0.0.3
```

The installer scripts are published as release assets alongside the binaries.

Or download the archive directly. Every release publishes a `checksums.sha256`
and a machine-readable `release-manifest.json` listing each target, asset,
digest, version, and source SHA:

| OS | Arch | Target triple | Asset | Build support | Runtime smoke tested |
|----|------|---------------|-------|---------------|----------------------|
| Linux | x86_64 | `x86_64-unknown-linux-gnu` | `cpm-planner-x86_64-unknown-linux-gnu.tar.gz` | native CI | native CI |
| Linux | arm64 | `aarch64-unknown-linux-gnu` | `cpm-planner-aarch64-unknown-linux-gnu.tar.gz` | native CI | native CI |
| macOS | x86_64 | `x86_64-apple-darwin` | `cpm-planner-x86_64-apple-darwin.tar.gz` | native CI | native CI |
| macOS | Apple Silicon | `aarch64-apple-darwin` | `cpm-planner-aarch64-apple-darwin.tar.gz` | native CI | native CI |
| Windows | x86_64 | `x86_64-pc-windows-msvc` | `cpm-planner-x86_64-pc-windows-msvc.zip` | native CI | native CI |
| Windows | arm64 | `aarch64-pc-windows-msvc` | `cpm-planner-aarch64-pc-windows-msvc.zip` | native CI | native CI |

"Native CI" means each asset is built and its MCP `initialize`/`tools/list`
handshake smoke-tested on that platform's own runner. No other CPU/OS
combination is claimed; unsupported platforms are rejected rather than
silently cross-compiled.

### Updates

Re-run the installer to update. Only the binary in the managed install
directory is replaced (atomically); application state and configuration live
outside that directory and are never overwritten. The SQLite plan store lives
at `~/.local/share/praxec/cpm-planner.db` (override with `CPM_PLANNER_DB`).

### From source

```sh
cargo install cpm-planner
```

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

## Using it with Praxec

This is an MCP tool used by [Praxec](https://github.com/praxec/praxec) packs. The easiest way to
get it — and a workflow pack that uses it — up and running is the one-command setup:

```bash
curl -fsSL https://raw.githubusercontent.com/praxec/packs/main/setup.sh | bash
```

See the [pack registry](https://github.com/praxec/packs) for this tool's provider coordinates
(container image / release binary) and which packs depend on it.

## License

[Apache-2.0](LICENSE).
