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

### Prebuilt binary (recommended)

Prebuilt packages are published for Linux, macOS and Windows on x86_64 and ARM64. The installers resolve your OS and CPU, download the matching archive, **verify its SHA-256 against the release's `checksums.sha256` before installing**, and atomically replace the binary in a user-local directory. A checksum mismatch aborts with a non-zero exit and installs nothing. Re-running the installer upgrades in place.

```sh
# Linux / macOS  ->  ~/.local/bin/cpm-planner
curl -fsSL https://github.com/praxec/cpm-planner/releases/latest/download/install.sh | sh

# Windows (PowerShell)  ->  %LOCALAPPDATA%\Programs\cpm-planner\cpm-planner.exe
irm https://github.com/praxec/cpm-planner/releases/latest/download/install.ps1 | iex
```

Pin a release with `sh -s -- --version v0.0.3` (`-Version v0.0.3` on Windows). Make sure the install directory is on your `PATH`, or use the absolute path in the client configs below.

Or download an archive directly and check it against `checksums.sha256` (a machine-readable `release-manifest.json` is published too):

| Platform | Asset |
|----------|-------|
| Linux x86_64 | `cpm-planner-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `cpm-planner-aarch64-unknown-linux-gnu.tar.gz` |
| macOS x86_64 | `cpm-planner-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `cpm-planner-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `cpm-planner-x86_64-pc-windows-msvc.zip` |
| Windows ARM64 | `cpm-planner-aarch64-pc-windows-msvc.zip` |

All assets are at <https://github.com/praxec/cpm-planner/releases/latest>.

### From source

```sh
cargo install cpm-planner
```

### Docker

```sh
docker pull ghcr.io/praxec/cpm-planner
```

## Register as an MCP server

cpm-planner speaks MCP over stdio. Register the `cpm-planner` command with your client. Every snippet below has an optional `env` block; drop it to use the defaults (see [Environment variables](#environment-variables)). If the binary is not on your `PATH`, use its absolute path as the command.

### Claude Code

```sh
claude mcp add cpm-planner --scope user -- cpm-planner
# with environment variables (put another option between --env and the name)
claude mcp add --env CPM_PLANNER_DB=/path/to/cpm-planner.db --env CPM_MAX_TTL_SECS=28800 --transport stdio --scope user cpm-planner -- cpm-planner
```

`--scope user` registers it for all your projects; `--scope project` writes a shared `.mcp.json` in the project root; the default `local` scope is private to the current project. The `.mcp.json` equivalent:

```json
{
  "mcpServers": {
    "cpm-planner": {
      "command": "cpm-planner",
      "args": [],
      "env": { "CPM_PLANNER_DB": "/path/to/cpm-planner.db" }
    }
  }
}
```

### Claude Desktop

Edit `claude_desktop_config.json` (Settings, Developer, Edit Config) and restart Claude Desktop:

- macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
- Windows: `%APPDATA%\Claude\claude_desktop_config.json`

```json
{
  "mcpServers": {
    "cpm-planner": {
      "command": "cpm-planner",
      "args": [],
      "env": { "CPM_PLANNER_DB": "/path/to/cpm-planner.db" }
    }
  }
}
```

On Windows use an absolute command path with escaped backslashes, e.g. `C:\\Users\\you\\AppData\\Local\\Programs\\cpm-planner\\cpm-planner.exe`.

### Cursor

`~/.cursor/mcp.json` (all projects) or `.cursor/mcp.json` (one project):

```json
{
  "mcpServers": {
    "cpm-planner": {
      "type": "stdio",
      "command": "cpm-planner",
      "args": [],
      "env": { "CPM_PLANNER_DB": "/path/to/cpm-planner.db" }
    }
  }
}
```

### VS Code

`.vscode/mcp.json` in the workspace. Note the top-level key is `servers`, not `mcpServers`:

```json
{
  "servers": {
    "cpm-planner": {
      "type": "stdio",
      "command": "cpm-planner",
      "args": [],
      "env": { "CPM_PLANNER_DB": "/path/to/cpm-planner.db" }
    }
  }
}
```

### Codex CLI

```sh
codex mcp add cpm-planner --env CPM_PLANNER_DB=/path/to/cpm-planner.db -- cpm-planner
```

or in `~/.codex/config.toml`:

```toml
[mcp_servers.cpm-planner]
command = "cpm-planner"
args = []

[mcp_servers.cpm-planner.env]
CPM_PLANNER_DB = "/path/to/cpm-planner.db"
```

### Docker

Use the container as the stdio command (`-i` keeps stdin open; the volume keeps plan state across runs):

```sh
docker run -i --rm -v cpm-planner-data:/data -e CPM_PLANNER_DB=/data/cpm-planner.db ghcr.io/praxec/cpm-planner
```

As a client config, put `docker` in `command` and the words after `docker` in `args`, for example in Claude Code:

```sh
claude mcp add cpm-planner --scope user -- docker run -i --rm -v cpm-planner-data:/data -e CPM_PLANNER_DB=/data/cpm-planner.db ghcr.io/praxec/cpm-planner
```

### praxec

See [Use with an MCP client](#use-with-an-mcp-client-eg-praxec) below for the YAML connection and [Using it with Praxec](#using-it-with-praxec) for the `packs/setup.sh` one-command setup.

### Optional environment variables

Any client's `env` block (or `-e`/`--env` flag) accepts:

- `CPM_PLANNER_DB`: SQLite path for plan state.
- `CPM_MAX_TTL_SECS`: ceiling for lease TTLs.
- `CPM_PROJECT_ROOT`: planned (P4P), not yet read by the server.
- `OPENROUTER_API_KEY`: optional, will enable `plan.review` (planned, P6); not yet used.

### Agent self-install

An agent can install and verify cpm-planner itself:

1. Detect the OS and CPU architecture (`uname -sm`, or `$env:PROCESSOR_ARCHITECTURE` on Windows).
2. Run the installer for that OS (`install.sh` or `install.ps1` above). It verifies the SHA-256 and exits non-zero on any mismatch; stop if it does.
3. Register the server with your own client using the matching section above (for Claude Code: `claude mcp add cpm-planner --scope user -- cpm-planner`), then restart or reload the client if it requires that.
4. Call `tools/list` and confirm `plan.status` (and `plan.submit`, `plan.get`) are present.
5. Submit a two-deliverable smoke plan with `plan.submit` (for example `a` and `b`, with `b` depending on `a`, each with a small effort and disjoint `owned_files`), then read it back with `plan.get` using the returned plan id and confirm both deliverables round-trip.

To check a binary without any client, run `node scripts/mcp-smoke.mjs /path/to/cpm-planner` from a checkout; it performs the MCP `initialize` and `tools/list` handshake and fails if the core tools are missing.

## MCP tools

| Tool | Does |
|------|------|
| `plan.submit` | Submit a task graph; returns a plan id (idempotent on the graph + caller). A prerequisite is a deliverable id string or an object `{id, consumes?, kind?: artifact\|interface, lag_hours?}`; both forms round-trip through `plan.get`. A deliverable may set `milestone: true` (an acceptance point, zero-length unless given an estimate or duration, reported in `plan.status` `milestones`). A deliverable's optional `duration_hours` (calendar time) replaces effort as its scheduled length and a prerequisite's `lag_hours` delays the dependent's start; effort stays the cost basis. |
| `plan.acquire_cohort` | Atomically acquire up to N ready deliverables with mutually disjoint file sets (an `owned_files` entry may be `{path, mode: "append"}`; append claims on one path may be co-leased and appear in `shared_paths`; a file may be owned by several deliverables only if they are ordered by prerequisites, or all claims are append); optional `ttl_seconds` sets the lease TTL (clamped to the server maximum); optional `ids` and `filter.metadata` target deliverables; `metadata.kind = "manual"` deliverables are never leased; the response lists unleased candidates in `blocked` (with `blocked_count`, `needs_operator`). |
| `plan.heartbeat` | Refresh the TTL on a held lock; optional `ttl_seconds` sets the new TTL (clamped to the server maximum). |
| `plan.mark_status` | Mark a deliverable complete/failed; releases its lock. Without a lock, Complete requires complete prerequisites (`PREREQUISITES_INCOMPLETE`) and every lockless mark is audited. |
| `plan.status` | Read-only snapshot of the plan and its locks. `critical_path` always runs `__start__` to `__finish__` (synthetic zero-effort endpoints; their `schedule` rows carry `synthetic: true`, and `__start__`/`__finish__` are reserved deliverable ids rejected with `INVALID_GRAPH`). `plan_complete` is true once every deliverable is Complete; the `plan.completed` audit event fires once when that happens. `milestones` has one row per deliverable with `milestone: true` (or legacy `metadata.milestone == true`): `id`, `critical_path` (longest chain from `__start__` to it), `hours` (its earliest finish) and `complete`. |
| `plan.get` | Return the stored plan graph for a plan_id (read back what was submitted). |
| `plan.force_release` | Operator escape hatch: release a lock regardless of holder/TTL; `reset_counters: true` also clears lapse/failure counters. |
| `plan.accept` | Manager/owner acceptance: complete a deliverable without holding its lease (audited, with evidence). |

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

## Environment variables

| Variable | Default | Meaning |
|----------|---------|---------|
| `CPM_PLANNER_DB` | OS data dir (`~/.local/share/praxec/cpm-planner.db`) | SQLite path for durable, cross-process planner state; `:memory:` gives ephemeral state. |
| `CPM_MAX_TTL_SECS` | `28800` (8h) | Server-side ceiling for `ttl_seconds` on `plan.acquire_cohort` and `plan.heartbeat`; larger requested values are clamped. Must be a positive integer — any other value aborts startup. |

Leases default to 5 minutes. For long-running work pass `ttl_seconds` (≤ the server maximum) on acquire/heartbeat, and heartbeat at least every `ttl/3`.

## License

[Apache-2.0](LICENSE).
