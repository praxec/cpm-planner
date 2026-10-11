# Agent install guide

This guide installs cpm-planner for one AI coding tool, end to end. It is
written so an agent can follow it step by step, and a person can too.

There are four steps:

1. Install the program.
2. Register it as an MCP server.
3. Install the agent skills.
4. Verify.

Per-tool skill paths, invocations and MCP snippets come from the
[agent tool matrix](agents/tool-matrix.md); other commands come from each
tool's docs, cited where they are used.
The `skills` subcommand and the `npx` launcher need **cpm-planner 0.2.0 or
later**. The `npx` commands work once `@matthew-cochran/cpm` is published to
npm (from 0.2.0); until then use the installed binary.

## Contents

- [Which tool are you?](#which-tool-are-you)
- [Step 1: install the program](#step-1-install-the-program)
- [Claude Code](#claude-code)
- [OpenAI Codex CLI](#openai-codex-cli)
- [Cursor](#cursor)
- [GitHub Copilot in VS Code](#github-copilot-in-vs-code)
- [Gemini CLI](#gemini-cli)
- [Other tools: Zed, Windsurf, Roo, Amp and AGENTS.md readers](#other-tools-zed-windsurf-roo-amp-and-agentsmd-readers)
- [Claude Desktop](#claude-desktop)
- [Verify](#verify)
- [Uninstall](#uninstall)
- [Troubleshooting](#troubleshooting)

## Which tool are you?

Find your tool, then follow its section.

| You are | Skills target | MCP config key | You type | Section |
|---|---|---|---|---|
| Claude Code | `--target claude` | `mcpServers` | `/cpm-plan` | [Claude Code](#claude-code) |
| OpenAI Codex CLI | `--target codex` | `[mcp_servers.cpm-planner]` (TOML) | `$cpm-plan` | [Codex](#openai-codex-cli) |
| Cursor | `--target cursor` | `mcpServers` | `/cpm-plan` | [Cursor](#cursor) |
| GitHub Copilot in VS Code | `--target copilot` | `servers` | `/cpm-plan` | [Copilot](#github-copilot-in-vs-code) |
| Gemini CLI | `--target gemini` | `mcpServers` | `/cpm-plan` (a command) | [Gemini CLI](#gemini-cli) |
| Zed | `--target codex` (writes `~/.agents/skills/`) | `context_servers` | `/cpm-plan` | [Other tools](#other-tools-zed-windsurf-roo-amp-and-agentsmd-readers) |
| Windsurf / Devin Desktop | `--target codex` (writes `~/.agents/skills/`) | `mcpServers` | `@cpm-plan` | [Other tools](#other-tools-zed-windsurf-roo-amp-and-agentsmd-readers) |
| Roo Code, Amp, another AGENTS.md reader | `--target codex` (user) or `--target agents-md` (project) | see your tool | depends on the tool | [Other tools](#other-tools-zed-windsurf-roo-amp-and-agentsmd-readers) |
| Claude Desktop | none (MCP only) | `mcpServers` | none | [Claude Desktop](#claude-desktop) |

The other skills work the same way: `/cpm-improve`, `/cpm-run`, `/cpm-ev`,
`/cpm-revise` (`$cpm-improve` and so on in Codex). Agents also load a skill by
themselves when your request matches its description.

**Scope.** `--user` installs the skills for all your projects. `--project <dir>`
installs them into one repository, so they can be committed and shared.

**One target per tool.** Cursor and Copilot read both `.claude/skills/` and
`.agents/skills/`. If you also install `--target claude`, `--target codex`,
`--target gemini` (which writes `.agents/skills/` too) or `--target all`, they
may list each skill twice. For Cursor and Copilot, install
only their own target.

## Step 1: install the program

Pick one of the two forms. Every section below shows both.

**A. The binary (recommended).** The installers check the SHA-256 against the
release's `checksums.sha256`, exit non-zero on a mismatch and install nothing.
If the installer fails, stop.

```sh
# Linux / macOS  ->  ~/.local/bin/cpm-planner
curl -fsSL https://github.com/praxec/cpm-planner/releases/latest/download/install.sh | sh
```

```powershell
# Windows (PowerShell)  ->  %LOCALAPPDATA%\Programs\cpm-planner\cpm-planner.exe
irm https://github.com/praxec/cpm-planner/releases/latest/download/install.ps1 | iex
```

To pin a release, pass a tag such as `v0.2.0`:

```sh
curl -fsSL https://github.com/praxec/cpm-planner/releases/latest/download/install.sh | sh -s -- --version v0.2.0
```

```powershell
$env:PRAXEC_VERSION = 'v0.2.0'; irm https://github.com/praxec/cpm-planner/releases/latest/download/install.ps1 | iex
```

The default install directories are those of `scripts/install.sh` and
`scripts/install.ps1`. The installer prints the absolute path it installed to. **Write it down**: GUI
clients need it (see [PATH for GUI clients](#path-for-gui-clients)). If the
directory is not on your `PATH`, the installer prints the line to add, or pass
`--add-to-path` (`-AddToPath` on Windows). More options are in the
[README install section](../README.md#install).

**B. npx, nothing to install.** With Node 18 or later, `npx -y @matthew-cochran/cpm`
downloads the release binary on first run, verifies it and caches it. It passes
all arguments through, so `cpm-planner <args>` becomes
`npx -y @matthew-cochran/cpm <args>` everywhere in this guide:

```sh
npx -y @matthew-cochran/cpm --version
npx -y @matthew-cochran/cpm skills install --target claude --user
```

When you install skills through npx, the printed "for this binary" hint points
into the npx cache. Register the `npx` form instead.

## Claude Code

1. Install the program ([step 1](#step-1-install-the-program)).
2. Register the MCP server for all your projects. Use one of these:

   ```sh
   # binary
   claude mcp add --transport stdio --scope user cpm-planner -- cpm-planner
   # npx
   claude mcp add --transport stdio --scope user cpm-planner -- npx -y @matthew-cochran/cpm
   ```

   For one repository, use `--scope project`, which writes a shared `.mcp.json`:

   ```sh
   # binary
   claude mcp add --transport stdio --scope project cpm-planner -- cpm-planner
   # npx
   claude mcp add --transport stdio --scope project cpm-planner -- npx -y @matthew-cochran/cpm
   ```

   The resulting `.mcp.json` (npx form):

   ```json
   { "mcpServers": { "cpm-planner": { "type": "stdio", "command": "npx", "args": ["-y", "@matthew-cochran/cpm"] } } }
   ```

3. Install the skills:

   ```sh
   cpm-planner skills install --target claude --user
   ```

   This writes `~/.claude/skills/cpm-*/SKILL.md` and `deliverable-cpm`. For one
   repository, run `cpm-planner skills install --target claude --project .`,
   which writes `.claude/skills/`.
4. Start a new Claude Code session.
5. [Verify](#verify). Check registration with `claude mcp list` or
   `claude mcp get cpm-planner`
   ([Claude Code MCP docs](https://code.claude.com/docs/en/mcp)). Then type
   `/cpm-plan`; it should autocomplete.

Project `.mcp.json` servers ask for approval the first time.

## OpenAI Codex CLI

1. Install the program ([step 1](#step-1-install-the-program)).
2. Register the MCP server. Use one of these:

   ```sh
   # binary
   codex mcp add cpm-planner -- cpm-planner
   # npx
   codex mcp add cpm-planner -- npx -y @matthew-cochran/cpm
   ```

   Or edit `~/.codex/config.toml` (or `.codex/config.toml` in a trusted project):

   ```toml
   [mcp_servers.cpm-planner]
   command = "npx"
   args = ["-y", "@matthew-cochran/cpm"]
   # binary form: command = "cpm-planner" and args = []
   ```

3. Install the skills:

   ```sh
   cpm-planner skills install --target codex --user
   ```

   This writes `~/.agents/skills/`. For one repository, run
   `cpm-planner skills install --target codex --project .`, which writes
   `.agents/skills/`.
4. Start a new Codex session.
5. [Verify](#verify). Check registration with `codex mcp list`. Then type
   `$cpm-plan`, or run `/skills` and pick it. Codex documents no `/cpm-plan`.

## Cursor

1. Install the program ([step 1](#step-1-install-the-program)).
2. Register the MCP server in `~/.cursor/mcp.json` (all projects) or
   `.cursor/mcp.json` (one project). Use one of these:

   ```json
   { "mcpServers": { "cpm-planner": { "type": "stdio", "command": "npx", "args": ["-y", "@matthew-cochran/cpm"] } } }
   ```

   ```json
   { "mcpServers": { "cpm-planner": { "type": "stdio", "command": "/absolute/path/to/cpm-planner", "args": [] } } }
   ```

   Cursor is a GUI app, so use the absolute path the installer printed.
3. Install the skills:

   ```sh
   cpm-planner skills install --target cursor --user
   ```

   This writes `~/.cursor/skills/`. For one repository, run
   `cpm-planner skills install --target cursor --project .`, which writes
   `.cursor/skills/`. Do not also install `--target all` for Cursor (see
   [One target per tool](#which-tool-are-you)).
4. Restart Cursor.
5. [Verify](#verify). In Agent chat, type `/cpm-plan` (or `@cpm-plan`).

## GitHub Copilot in VS Code

1. Install the program ([step 1](#step-1-install-the-program)).
2. Register the MCP server. Note the top-level key is `servers`, not
   `mcpServers`. For your user profile, use one of these:

   ```sh
   # npx
   code --add-mcp '{"name":"cpm-planner","command":"npx","args":["-y","@matthew-cochran/cpm"]}'
   # binary
   code --add-mcp '{"name":"cpm-planner","command":"/absolute/path/to/cpm-planner","args":[]}'
   ```

   Or run "MCP: Open User Configuration" in VS Code and add the JSON below.
   For one workspace, put the same JSON in `.vscode/mcp.json`:

   ```json
   { "servers": { "cpm-planner": { "type": "stdio", "command": "npx", "args": ["-y", "@matthew-cochran/cpm"] } } }
   ```

   ```json
   { "servers": { "cpm-planner": { "type": "stdio", "command": "/absolute/path/to/cpm-planner", "args": [] } } }
   ```

   In PowerShell, use the JSON form: `code --add-mcp` loses its quotes there
   (see [Windows quoting](#windows-quoting)).
3. Install the skills:

   ```sh
   cpm-planner skills install --target copilot --user
   ```

   This writes `~/.copilot/skills/`. For one repository, run
   `cpm-planner skills install --target copilot --project .`, which writes
   `.github/skills/`. Do not also install `--target all` for Copilot (see
   [One target per tool](#which-tool-are-you)).
4. Reload the VS Code window.
5. [Verify](#verify). In Copilot Chat, type `/cpm-plan`. `/skills` opens the
   skills menu.

## Gemini CLI

Confidence for Gemini is **medium**. Its paths are documented, but Google is
moving Gemini CLI to Antigravity CLI, and since 2026-06-18 Gemini CLI serves
only Code Assist Standard/Enterprise users and paid Gemini API-key users.

1. Install the program ([step 1](#step-1-install-the-program)).
2. Register the MCP server in `~/.gemini/settings.json` (all projects) or
   `.gemini/settings.json` (one project). Use one of these:

   ```json
   { "mcpServers": { "cpm-planner": { "command": "npx", "args": ["-y", "@matthew-cochran/cpm"] } } }
   ```

   ```json
   { "mcpServers": { "cpm-planner": { "command": "/absolute/path/to/cpm-planner", "args": [] } } }
   ```

   The command form works for the binary:
   `gemini mcp add -s user cpm-planner cpm-planner`. Its npx form,
   `gemini mcp add -s user cpm-planner npx -y @matthew-cochran/cpm`, is
   untested: the parser may read `-y` as its own option. Prefer the JSON for npx.
3. Install the skills:

   ```sh
   cpm-planner skills install --target gemini --user
   ```

   This writes `~/.agents/skills/` and one command per skill in
   `~/.gemini/commands/cpm-*.toml`. For one repository, run
   `cpm-planner skills install --target gemini --project .`.
4. Start a new Gemini CLI session.
5. [Verify](#verify). Type `/cpm-plan`; it comes from the TOML command. The
   skills themselves are model-activated through `activate_skill`, with a
   confirmation prompt. `/skills list` shows them.

**Antigravity CLI** is not a separate target yet. Its workspace skills live in
`.agents/skills/`, which `cpm-planner skills install --target codex --project .`
writes, and Antigravity turns them into slash commands with an undocumented
syntax. Its global skills path is disputed, so there is no user-level install
for it.

## Other tools: Zed, Windsurf, Roo, Amp and AGENTS.md readers

These tools read `~/.agents/skills/` (user) or `.agents/skills/` (project), so
the `codex` target reaches them:

```sh
cpm-planner skills install --target codex --user
```

| Tool | MCP registration | You type |
|---|---|---|
| Zed | `settings.json`: `{"context_servers": {"cpm-planner": {"command": "npx", "args": ["-y", "@matthew-cochran/cpm"], "env": {}}}}` (binary: `"command": "/absolute/path/to/cpm-planner", "args": []`) | `/cpm-plan` or `@cpm-plan` |
| Windsurf / Devin Desktop | `~/.config/devin/mcp_config.json` (`%APPDATA%\devin\` on Windows), key `mcpServers` | `@cpm-plan` |
| Roo Code | not verified; see your tool's docs | not documented |
| Amp | `amp.mcpServers` in Amp settings | not documented; the model loads skills |

Zed reads project `.agents/skills/` only in trusted worktrees.

For a repository shared by people who use different tools, the `agents-md`
target writes a managed block into `./AGENTS.md` plus `.agents/skills/`:

```sh
cpm-planner skills install --target agents-md --project .
```

The block sits between `<!-- cpm-planner:begin -->` and
`<!-- cpm-planner:end -->`; the rest of `AGENTS.md` is not touched. `agents-md`
is project only: with `--user` it is a usage error (exit 2).

To install for Claude Code and every `.agents/skills/` reader in one go, use
`cpm-planner skills install --target all --project .`. It writes
`.claude/skills/` and `.agents/skills/` only.

## Claude Desktop

Claude Desktop takes the MCP server but has no skills target.

1. Install the binary ([step 1](#step-1-install-the-program), form A).
2. Edit `claude_desktop_config.json` (Settings, Developer, Edit Config; paths
   from the [README's Claude Desktop section](../README.md#claude-desktop)):
   - macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
   - Windows: `%APPDATA%\Claude\claude_desktop_config.json`

   ```json
   { "mcpServers": { "cpm-planner": { "command": "/absolute/path/to/cpm-planner", "args": [] } } }
   ```

   ```json
   { "mcpServers": { "cpm-planner": { "command": "npx", "args": ["-y", "@matthew-cochran/cpm"] } } }
   ```

3. Restart Claude Desktop, then check that `plan.status` is among the tools.

## Verify

Run all four checks. If one fails, see [Troubleshooting](#troubleshooting).

1. **The program runs.** `cpm-planner --version` (or
   `npx -y @matthew-cochran/cpm --version`) prints `cpm-planner <version>`.
2. **The MCP server is connected.** `plan.status`, `plan.submit` and `plan.get`
   are among your available tools. As an end-to-end check, call `plan.submit`
   with this two-deliverable plan, then `plan.get` with the returned plan id,
   and confirm both deliverables come back:

   ```json
   {"graph":{"deliverables":[{"id":"a","owned_files":["a.txt"],"prerequisites":[],"estimated_effort_hours":1},{"id":"b","owned_files":["b.txt"],"prerequisites":["a"],"estimated_effort_hours":1}]}}
   ```

3. **The invocation resolves.** `/cpm-plan` (Claude Code, Cursor, Copilot,
   Gemini) or `$cpm-plan` (Codex) is offered by your tool. Restart the tool or
   open a new session if it is not.
4. **The skills are installed.** `cpm-planner skills list` prints one line per
   skills directory, user and current project, with the targets and file
   counts. `0 modified, 0 missing` means the files are as installed. Run from
   your home directory, the same directory is listed twice, once as `(user)`
   and once as `(project)`, because the current project is your home then.

Without any MCP client, `node scripts/mcp-smoke.mjs /absolute/path/to/cpm-planner`
from a checkout of this repository runs the MCP handshake and fails if
`plan.submit`, `plan.status` or `plan.get` is missing.

## Uninstall

1. Remove the skills with the same target and scope you installed:

   ```sh
   cpm-planner skills uninstall --target claude --user
   ```

   It removes only files that still match what it wrote, and keeps a shared
   directory such as `.agents/skills/` until the last target using it is
   uninstalled. Add `--dry-run` to preview. `cpm-planner skills list` should
   then no longer list that directory.
2. Remove the MCP registration: for Claude Code,
   `claude mcp remove cpm-planner --scope user`
   ([Claude Code MCP docs](https://code.claude.com/docs/en/mcp)); for the
   others, delete the `cpm-planner` entry from the config file you edited.
3. Remove the program:
   - binary: delete `~/.local/bin/cpm-planner`, or on Windows the
     `%LOCALAPPDATA%\Programs\cpm-planner\` directory (the installers'
     default install directories, from `scripts/install.sh` and
     `scripts/install.ps1`);
   - npx: delete the launcher cache, `~/.cache/cpm-planner/` (or
     `$XDG_CACHE_HOME/cpm-planner/`), or `%LOCALAPPDATA%\cpm-planner\` on
     Windows (see the [npm launcher README](../npm/README.md#what-it-does)).
4. Optional: plan state lives in the SQLite file `CPM_PLANNER_DB` names
   (default `~/.local/share/praxec/cpm-planner.db`). Delete it only if you
   want to lose your plans.

## Troubleshooting

### PATH for GUI clients

GUI apps (Cursor, VS Code, Claude Desktop, Zed) do not inherit your shell
`PATH`, so a bare `cpm-planner` or `npx` may not be found. Use absolute paths:

- binary: the path the installer printed, e.g. `/home/you/.local/bin/cpm-planner`
  or `C:\\Users\\you\\AppData\\Local\\Programs\\cpm-planner\\cpm-planner.exe`
  (backslashes doubled inside JSON);
- npx: the output of `command -v npx` (macOS/Linux) or `where npx` (Windows).

If `cpm-planner` is not found in a terminal either, the install directory is
not on `PATH`: re-run the installer with `--add-to-path` (`-AddToPath`), or add
the line it printed.

### Windows quoting

- The commands `skills install` prints paste into both cmd and PowerShell:
  paths use `\`, and a path with spaces is in double quotes.
- In PowerShell, a path containing `$` or a backtick needs single quotes.
- PowerShell strips the `\"` escapes from `code --add-mcp '{...}'`. Use the JSON
  form in "MCP: Open User Configuration" instead.
- Inside JSON, write each backslash twice.
- If a client cannot start `npx` on Windows, register the binary form with the
  absolute `.exe` path instead: the installer's default is
  `%LOCALAPPDATA%\Programs\cpm-planner\cpm-planner.exe` (`scripts/install.ps1`),
  and the [README's Claude Desktop section](../README.md#claude-desktop) shows
  it in JSON.

### A stale npm cache

The launcher caches the binary per version, in `~/.cache/cpm-planner/<version>/`
(`%LOCALAPPDATA%\cpm-planner\<version>\` on Windows). It checks the cached
binary's SHA-256 on every start and refuses a directory that other users can
write (see the [npm launcher README](../npm/README.md#what-it-does)). To force
a fresh download, delete that version directory, or point the
cache somewhere new:

```sh
CPM_PLANNER_CACHE_DIR="$HOME/.cache/cpm-planner-fresh" npx -y @matthew-cochran/cpm --version
```

In an MCP config, put `CPM_PLANNER_CACHE_DIR` in the server's `env` block. To
get a specific release, name the package version: `npx -y @matthew-cochran/cpm@0.2.0`.

### Offline use

The launcher needs GitHub only for the first download of each version. On a
machine without access, install the binary another way (the installer on a
connected machine, or an archive from the
[releases page](https://github.com/praxec/cpm-planner/releases)), then either:

- register the binary directly as the MCP command; or
- keep the npx form and set `CPM_PLANNER_BINARY` to the binary's path, so the
  launcher runs it without downloading. It must be a native executable
  (`cpm-planner.exe` on Windows), not a `.cmd` or `.bat` file.

`skills install` itself never uses the network.

### Other problems

- `skills install` prints `modified, skipped` or `foreign, skipped`: you edited
  that file, or cpm-planner did not write it. It is kept. `--force` overwrites it.
- `--project <dir>: not an existing directory`: create the directory first, or
  run from the repository root with `--project .`.
- The skill appears twice in Cursor or Copilot: see
  [One target per tool](#which-tool-are-you).
- For bugs, see [SUPPORT.md](../SUPPORT.md).
