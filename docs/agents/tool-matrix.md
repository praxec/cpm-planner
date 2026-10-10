# Agent tool matrix for `cpm-planner skills install --target <tool>`

Research date: **2026-10-10**. Every claim below was checked on that date against official vendor docs, specs or changelogs. "Checked" means fetched on 2026-10-10. Where a vendor URL redirected, the redirect target (also vendor-owned) is cited.

Two headline findings:

1. **Agent Skills (`<dir>/<name>/SKILL.md`, agentskills.io) is now the common format.** Claude Code, Codex, Cursor, VS Code Copilot, Gemini CLI, Zed, Roo, Cline, Amp, Windsurf/Devin and Junie all load it. Only the directory differs. Slash commands such as `.cursor/commands`, `~/.codex/prompts` and Copilot prompt files are now legacy, or are being migrated into skills.
2. **`.agents/skills/` (project) and `~/.agents/skills/` (user) are the closest thing to a shared location.** Codex, Cursor, Copilot, Gemini CLI, Zed, Roo, Amp and Windsurf read them. **Claude Code does not.** It reads only `.claude/skills/` and `~/.claude/skills/`.

## Shared skill file (used by every SKILL.md target)

The following satisfies the strictest constraints, from the agentskills.io spec and VS Code. `name` is 1–64 characters of `[a-z0-9-]`, with no leading, trailing or double hyphen, and must equal the directory name. `description` is 1–1024 characters. Claude Code truncates `description` plus `when_to_use` at 1,536 characters, so 1024 is safe everywhere.

`<skills-root>/cpm-plan/SKILL.md`:

```markdown
---
name: cpm-plan
description: Build a critical-path (CPM) project plan using the cpm-planner MCP server. Use when the user wants to plan a project, break work into dependent tasks, estimate durations, or find the critical path.
---

# cpm-plan

Use the `cpm-planner` MCP tools to turn the user's goal into a CPM plan.

1. Clarify scope, then draft tasks with durations and prerequisites.
2. Call the cpm-planner planning/analysis tools; fix any lint-gate failures.
3. Report the critical path, total duration and float, then offer /cpm-improve.

User arguments: $ARGUMENTS
```

Notes on the template:
- `$ARGUMENTS` is substituted by Claude Code. Other tools treat it as literal text, which is harmless but reads oddly. The installer could drop that line for non-Claude targets.
- Do not use Claude-only fields such as `disable-model-invocation` or `when_to_use` in the shared copy unless the target is known to honour them. Cursor, VS Code, Zed and Claude all document `disable-model-invocation`. Agentskills.io does not.
- Supporting files (`references/`, `scripts/`, `assets/`) are allowed by the spec and by every SKILL.md tool below.

Sources: https://agentskills.io/specification (2026-10-10); https://code.visualstudio.com/docs/copilot/customization/agent-skills (2026-10-10); https://code.claude.com/docs/en/skills (2026-10-10)

---

## 1. Claude Code

**Paths**
- User: `~/.claude/skills/<name>/SKILL.md`
- Project: `.claude/skills/<name>/SKILL.md`. Nested `<subdir>/.claude/skills/` also loads, and symlinked skill folders are allowed.
- Plugin: `<plugin>/skills/<name>/SKILL.md`, invoked as `/plugin-name:skill-name`.
- Legacy commands: `.claude/commands/<name>.md` and `~/.claude/commands/`. "Custom commands have been merged into skills… both create `/deploy`… Your existing `.claude/commands/` files keep working." If a skill and a command share a name, **the skill wins**.
- **No `.agents/skills` support is documented.** The skills page does not list it. The memory page says Claude does not read "anything under a `.agents/` directory", but that statement is about instruction files.

**File format**
- Use the shared SKILL.md above.
- All frontmatter fields are optional. If `name` is absent it defaults to the directory name.
- Documented fields: `name`, `description`, `when_to_use`, `argument-hint`, `arguments`, `disable-model-invocation`, `user-invocable`, `allowed-tools`, `model`, `effort`, `context`, `agent`, `hooks`, `paths`, `shell`, plus others.
- Documented limits:
  - `description` plus `when_to_use` is truncated at 1,536 characters.
  - The docs recommend keeping SKILL.md under 500 lines.
  - Reserved names: `synced`, and `anthropic-skills` or anything starting with `anthropic-skills:`.
  - No charset or length limit for `name` is stated.
- Arguments: `$ARGUMENTS`, `$ARGUMENTS[N]` or `$N`, and named `$name` declared through `arguments`.

**Invocation**
- Type `/cpm-plan`. Claude also auto-loads the skill when the description matches.
- `/cpm-plan some args` passes the args to `$ARGUMENTS`.
- The directory name also invokes the skill when `name` differs.

**MCP registration**

Use `-s` / `--scope` with `local` (the default, stored in `~/.claude.json`), `project` (writes `.mcp.json`) or `user`. `--` separates Claude's options from the server's command.

```bash
# binary
claude mcp add --transport stdio --scope user cpm-planner -- cpm-planner
# npx
claude mcp add --transport stdio --scope user cpm-planner -- npx -y @matthew-cochran/cpm
# project-shared (.mcp.json)
claude mcp add --transport stdio --scope project cpm-planner -- npx -y @matthew-cochran/cpm
```

`.mcp.json` equivalent:

```json
{ "mcpServers": { "cpm-planner": { "type": "stdio", "command": "npx", "args": ["-y", "@matthew-cochran/cpm"] } } }
```

The JSON form also works: `claude mcp add-json cpm-planner '{"type":"stdio","command":"cpm-planner","args":[]}' --scope user`.

**AGENTS.md**
- Claude Code reads it natively since v2.1.277, but **only when no `CLAUDE.md`, `.claude/CLAUDE.md` or `CLAUDE.local.md` exists** in the working directory or any directory above it.
- Otherwise, import it with `@AGENTS.md` from CLAUDE.md.
- It does not read `AGENTS.override.md`.

**Sources (all checked 2026-10-10)**
- https://code.claude.com/docs/en/skills
- https://code.claude.com/docs/en/mcp
- https://code.claude.com/docs/en/memory

**Caveats**
- `--env` placed immediately before the server name swallows the name, so put `--transport stdio` between them.
- Project `.mcp.json` servers prompt for approval.
- I did not find a Windows `cmd /c npx` note in the portion of the MCP page I read, because the page was truncated. This is unverified.

---

## 2. OpenAI Codex CLI

The `developers.openai.com/codex/*` URLs now 308-redirect to `learn.chatgpt.com/docs/*`.

**Paths (skills)**
- Project ("Repo"): `$CWD/.agents/skills`, plus every parent directory up to the repo root.
- User: **`$HOME/.agents/skills`**. The docs do not list `~/.codex/skills`. Cursor still reads `.codex/skills` for compatibility, which suggests it was an older location.
- Admin: `/etc/codex/skills`.
- System: bundled skills.
- Symlinks are followed.
- Custom prompts (deprecated): `~/.codex/prompts/<name>.md`. These are top-level only, user only, and cannot be set per project.

**File format**
- Use the shared SKILL.md above. Codex requires `name` and `description`.
- Optional `agents/openai.yaml` inside the skill folder supports `interface`, `policy.allow_implicit_invocation` and `dependencies.tools`.
- The initial skill list is capped at 2% of the context window, or 8,000 characters when the window is unknown. Keep descriptions short.

Legacy prompt (optional) at `~/.codex/prompts/cpm-plan.md`:

```markdown
---
description: Build a CPM plan with cpm-planner
argument-hint: GOAL="<project goal>"
---
Use the cpm-planner MCP tools to build a critical-path plan for: $GOAL
```

**Invocation**
- Skills: type `$cpm-plan`, or run `/skills` and pick one. Codex also invokes skills implicitly when the description matches.
- **There is no documented `/cpm-plan` for skills.**
- Deprecated prompts: `/prompts:cpm-plan GOAL="..."`.

**AGENTS.md discovery**
- Global: `~/.codex` (or `$CODEX_HOME`). Codex reads `AGENTS.override.md`, falling back to `AGENTS.md`, and uses the first non-empty file.
- Project: walks from the Git root down to the cwd. In each directory it takes the first of `AGENTS.override.md`, `AGENTS.md`, then `project_doc_fallback_filenames`, with at most one file per directory.
- Files are concatenated root first, so files nearer the cwd come later and win.
- The cap is `project_doc_max_bytes`, 32 KiB by default.

**MCP registration**

```bash
codex mcp add cpm-planner -- cpm-planner
codex mcp add cpm-planner -- npx -y @matthew-cochran/cpm
codex mcp list
```

The same in `~/.codex/config.toml`, or in project `.codex/config.toml` for **trusted projects only**:

```toml
[mcp_servers.cpm-planner]
command = "npx"
args = ["-y", "@matthew-cochran/cpm"]
# binary form: command = "cpm-planner"
```

**Sources (all checked 2026-10-10)**
- https://learn.chatgpt.com/docs/build-skills (redirect from https://developers.openai.com/codex/skills)
- https://learn.chatgpt.com/docs/custom-prompts
- https://learn.chatgpt.com/docs/agent-configuration/agents-md
- https://learn.chatgpt.com/docs/extend/mcp?surface=cli

**Caveats**
- No `--scope` flag for `codex mcp add` is documented, and the docs do not say which file it writes. To scope a server to a project, the installer should write `.codex/config.toml` directly.
- A hyphenated TOML table key (`mcp_servers.cpm-planner`) is valid as a bare key, but `cpm_planner` is the conservative choice.
- Invocation uses `$`, not `/`, so users get `$cpm-plan`.

---

## 3. Cursor

**Paths**
- Skills, project: `.agents/skills/` and `.cursor/skills/`.
- Skills, user: `~/.agents/skills/` and `~/.cursor/skills/`.
- Cursor also loads `.claude/skills/`, `.codex/skills/` and their `~/` equivalents for compatibility, plus nested project folders such as `apps/web/.cursor/skills/`.
- Personal skills in `~/.cursor/skills/` sync to Cloud Agents only when sync is enabled. `~/.agents/skills/` does not sync.
- Legacy commands: `.cursor/commands/<name>.md`, per the Cursor 1.6 changelog. A global `~/.cursor/commands` is mentioned only in community forum answers, not in official docs.
- `/migrate-to-skills` (Cursor 2.4+) converts commands and dynamic rules into skills.

**File format**
- Use the shared SKILL.md above.
- Required: `name` (lowercase letters, digits and hyphens; **must match the folder**) and `description`.
- Optional: `paths`, `disable-model-invocation`, `icon`, `color`, `metadata`. Legacy `globs` is still accepted.
- Rules: `.cursor/rules/*.mdc` with frontmatter `description`, `globs`, `alwaysApply`. A plain `.md` there is skipped. Rules suit context, not commands.

Example rule:

```markdown
---
description: Use cpm-planner MCP tools for project scheduling questions
globs:
alwaysApply: false
---
When the user asks about schedules, dependencies or critical path, use the cpm-planner MCP tools.
```

**Invocation**
- Type `/cpm-plan` in Agent chat, or `@cpm-plan` to attach it as context.
- A skill invoked this way applies to one message. Cursor also auto-applies skills.

**MCP registration**

Write `.cursor/mcp.json` (project) or `~/.cursor/mcp.json` (global). No CLI command or deeplink is documented on the MCP page.

```json
{ "mcpServers": { "cpm-planner": { "type": "stdio", "command": "cpm-planner", "args": [] } } }
```

```json
{ "mcpServers": { "cpm-planner": { "type": "stdio", "command": "npx", "args": ["-y", "@matthew-cochran/cpm"] } } }
```

**AGENTS.md**
- Supported at the project root and nested in any subdirectory. More specific files take precedence.

**Sources (all checked 2026-10-10)**
- https://cursor.com/docs/context/skills
- https://cursor.com/help/customization/skills
- https://cursor.com/docs/context/rules
- https://cursor.com/docs/context/mcp
- https://cursor.com/changelog/1-6

**Caveats**
- `https://cursor.com/docs/context/commands` returned skills and migration content with no commands reference. I found no current official page that documents `.cursor/commands` or `~/.cursor/commands`. Treat commands as legacy and write skills.
- The MCP field table marks `type: "stdio"` as required, but the examples omit it. Include it.
- Because Cursor reads `.claude/skills`, `.codex/skills` and `.agents/skills`, installing the same skill for several targets can produce duplicate entries. Dedup behaviour is undocumented.

---

## 4. Gemini CLI (and its successor, Antigravity CLI)

**Important:**
- Google announced on 2026-05-19 that Gemini CLI is being transitioned into Antigravity CLI.
- On 2026-06-18, Gemini CLI stopped serving Google AI Pro/Ultra subscribers and free Code Assist individual users.
- Gemini CLI remains available for Code Assist Standard/Enterprise users and for paid Gemini API-key users.
- Antigravity keeps "Agent Skills, Hooks, Subagents, and Extensions (now as Antigravity plugins)", but "there won't be 1:1 feature parity".

**Paths (Gemini CLI)**
- Custom commands: user `~/.gemini/commands/`, project `<root>/.gemini/commands/`. The project command wins on a name clash.
- Skills: user `~/.gemini/skills/` or `~/.agents/skills/`, workspace `.gemini/skills/` or `.agents/skills/`. `.agents/` wins over `.gemini/` within a tier.
- Context: global `~/.gemini/GEMINI.md`, plus `GEMINI.md` in workspace dirs and their parents, plus a just-in-time scan of accessed directories and their ancestors.
- AGENTS.md is read only if `context.fileName` in settings.json lists it, e.g. `["AGENTS.md","GEMINI.md"]`.
- Extensions: `~/.gemini/extensions/<ext>/gemini-extension.json`, with `commands/*.toml`, `skills/<name>/SKILL.md`, `mcpServers` and `contextFileName`.

**Paths (Antigravity CLI, per the migration guide)**
- Skills: global `~/.gemini/antigravity-cli/skills/`, workspace `.agents/skills/`. The guide says workspace skills must be in `.agents/skills/` "to recognize them as active slash commands".
- MCP: `~/.gemini/config/mcp_config.json` (global) and `.agents/mcp_config.json` (workspace).
- TOML commands are converted to skills by `agy plugin import gemini`.

**File format**

Command file `.gemini/commands/cpm-plan.toml` gives `/cpm-plan`. Using `.gemini/commands/cpm/plan.toml` instead gives `/cpm:plan`.

```toml
description = "Build a critical-path (CPM) plan with the cpm-planner MCP server"
prompt = """
Use the cpm-planner MCP tools to build a critical-path plan.
Clarify scope, draft tasks with durations and prerequisites, run the analysis,
and report the critical path, total duration and float.

Request: {{args}}
"""
```

`prompt` is required and `description` is optional. If the prompt has no `{{args}}`, the arguments are appended after two newlines.

Skill: the shared SKILL.md above, at `~/.agents/skills/cpm-plan/SKILL.md`. Gemini does not document frontmatter beyond name and description.

**Invocation**
- Commands: `/cpm-plan <args>`, or `/cpm:plan` if namespaced.
- Skills are **model-triggered** through the `activate_skill` tool, with a confirmation prompt. `/skills list|enable|disable|reload` manages them. There is no documented `/<skill-name>` in Gemini CLI.
- In Antigravity, `.agents/skills` become slash commands, but the exact syntax is undocumented.
- Extension commands have the lowest precedence. On a conflict they are prefixed with the extension name, e.g. `/cpm.plan`.

**MCP registration**

`gemini mcp add [options] <name> <command> [args...]`. Scope `-s user|project` defaults to **project**.

```bash
gemini mcp add -s user cpm-planner cpm-planner
gemini mcp add -s user cpm-planner npx -y @matthew-cochran/cpm
```

The second form, with `-y` among the args, is untested. Commander-style parsers may treat `-y` as an option, so `--` before `npx` may be needed. This is not documented.

`~/.gemini/settings.json` or `.gemini/settings.json`:

```json
{ "mcpServers": { "cpm-planner": { "command": "npx", "args": ["-y", "@matthew-cochran/cpm"] } } }
```

The docs warn against underscores in server names, so `cpm-planner` is fine.

Extension alternative: `gemini-extension.json` is the better packaging for Gemini, because a single `gemini extensions install <github-url>` delivers the MCP server, commands, skills and context together. It also maps to Antigravity plugins through `agy plugin import gemini`.

```json
{
  "name": "cpm-planner",
  "version": "1.0.0",
  "description": "CPM planning skills and MCP server",
  "mcpServers": { "cpm-planner": { "command": "npx", "args": ["-y", "@matthew-cochran/cpm"] } },
  "contextFileName": "GEMINI.md"
}
```

**Sources (all checked 2026-10-10)**
- https://geminicli.com/docs/cli/custom-commands/
- https://geminicli.com/docs/cli/skills/
- https://geminicli.com/docs/tools/mcp-server/
- https://geminicli.com/docs/cli/gemini-md/
- https://geminicli.com/docs/extensions/
- https://geminicli.com/docs/extensions/reference/
- https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/
- https://antigravity.google/docs/cli/gcli-migration/

**Caveats**
- The Gemini CLI audience is shrinking.
- Antigravity's global skills path is disputed. The official guide says `~/.gemini/antigravity-cli/skills/`. Community issues (non-official, not relied on) report `~/.gemini/config/skills/`.
- I did not verify whether the Antigravity `mcp_config.json` accepts `command`/`args` keys, because the guide shows only `serverUrl`.
- Treat Antigravity as a separate, future target.

---

## 5. GitHub Copilot in VS Code

**Paths**
- Skills, project: `.github/skills/`, `.claude/skills/`, `.agents/skills/`.
- Skills, user: `~/.copilot/skills/`, `~/.claude/skills/`, `~/.agents/skills/`.
- The `chat.agentSkillsLocations` setting is deprecated.
- Prompt files, project: `.github/prompts/<name>.prompt.md`.
- Prompt files, user: the VS Code profile's user data folder. **No literal path is documented.** Create one with "Chat: New Prompt File".
- Instructions: `.github/copilot-instructions.md`, plus `.github/instructions/*.instructions.md` with `applyTo: "<glob>"`.

**File format**
- Use the shared SKILL.md above.
- `name` is required, `[a-z0-9-]`, at most 64 characters and equal to the directory name. `description` is required, at most 1024 characters.
- Optional: `argument-hint`, `user-invocable`, `disable-model-invocation`.

Prompt file `.github/prompts/cpm-plan.prompt.md`. The frontmatter key is now `agent`, and `mode` is not documented.

```markdown
---
name: cpm-plan
description: Build a critical-path (CPM) plan with the cpm-planner MCP server
argument-hint: project goal
agent: agent
tools: ['cpm-planner/*']
---
Use the cpm-planner MCP tools to build a CPM plan for: ${input:goal:project goal}
Report the critical path, total duration and float.
```

**Invocation**
- Type `/cpm-plan` for both skills and prompt files, optionally with trailing text.
- `/skills` opens the Configure Skills menu.

**AGENTS.md**
- Supported at the repo root, controlled by `chat.useAgentsMdFile` for the Local agent.
- Nested AGENTS.md is experimental, controlled by `chat.useNestedAgentsMdFiles`, and **off by default**.
- CLAUDE.md is read when `chat.useClaudeMdFile` is set.

**MCP registration**

`.vscode/mcp.json` uses the top-level key **`servers`**, not `mcpServers`:

```json
{ "servers": { "cpm-planner": { "type": "stdio", "command": "cpm-planner", "args": [] } } }
```

```json
{ "servers": { "cpm-planner": { "type": "stdio", "command": "npx", "args": ["-y", "@matthew-cochran/cpm"] } } }
```

User profile, from the CLI:

```bash
code --add-mcp "{\"name\":\"cpm-planner\",\"command\":\"npx\",\"args\":[\"-y\",\"@matthew-cochran/cpm\"]}"
```

User-level `mcp.json` is opened with "MCP: Open User Configuration". It lives in the profile folder.

**Sources (all checked 2026-10-10)**
- https://code.visualstudio.com/docs/copilot/customization/agent-skills
- https://code.visualstudio.com/docs/copilot/customization/prompt-files
- https://code.visualstudio.com/docs/copilot/customization/custom-instructions
- https://code.visualstudio.com/docs/copilot/customization/mcp-servers

**Caveats**
- Skills cover both scopes with known paths, so prefer them. User prompt files have no documented path, so the installer should not write them.
- Copilot also reads `.claude/skills` and `.agents/skills`, so installing a skill for several targets can produce duplicates.
- The docs give no explicit statement that skills are on by default.

---

## 6. Generic AGENTS.md (agents.md)

**Format**
- Plain Markdown with no required fields: "AGENTS.md is just standard Markdown".

**Nesting**
- "Agents automatically read the nearest file in the directory tree, so the closest one takes precedence."
- "The closest AGENTS.md to the edited file wins; explicit user chat prompts override everything."

**Readers listed on agents.md**
- Codex, Jules, Factory, Aider, goose, opencode, Zed, Warp, VS Code, Devin, UiPath, Junie, Amp, Cursor, RooCode, Gemini CLI, Kilo Code, Phoenix, Semgrep, Copilot coding agent, Ona, Windsurf, Augment.
- Claude Code is not on that list, but its own docs say it reads AGENTS.md when no CLAUDE.md exists (v2.1.277+).

**Actual behaviour differs per tool**
- Codex concatenates root to cwd and caps at 32 KiB.
- Cursor and Windsurf apply nested files by directory.
- VS Code reads nested files only behind an experimental setting.
- Gemini reads AGENTS.md only when it is listed in `context.fileName`.
- Junie prefers `.junie/AGENTS.md`.

**There is no standard user-level AGENTS.md**
- Codex uses `~/.codex/AGENTS.md`.
- Zed uses `~/.config/zed/AGENTS.md`.
- Junie uses `~/.junie/AGENTS.md`.
- Cline reads `~/.agents/AGENTS.md`.

**What to write**
- A delimited managed block in project `AGENTS.md`, for example `<!-- cpm-planner:begin --> … <!-- cpm-planner:end -->`. It should say that the cpm-planner MCP server is available and when to use the five workflows. AGENTS.md cannot create a slash command.

Sources (checked 2026-10-10): https://agents.md/, plus the per-tool pages cited in each section.

---

## 7. Other tools (brief)

| Tool | Skills / commands | AGENTS.md | MCP config | Source (checked 2026-10-10) |
|---|---|---|---|---|
| **Windsurf**, now branded Devin Desktop / Cascade | Project skills: `.devin/skills/` (preferred), `.windsurf/skills/` (legacy), `.agents/skills/`, and `.claude/skills/` if Claude config reading is on. Global skills: `~/.codeium/windsurf/skills/`, `~/.config/devin/skills/`, `~/.agents/skills/`. Skills are invoked with **`@skill-name`**. `/name` is for Workflows (`.devin/workflows/`, `.windsurf/workflows/`), which are being deprecated in favour of skills. | Yes, all AGENTS.md files in the workspace. The root file is always on; nested files apply to their own directory. | `~/.config/devin/mcp_config.json` (`%APPDATA%\devin\` on Windows), key `mcpServers`. | https://docs.devin.ai/desktop/cascade/skills (redirect from docs.windsurf.com); https://docs.windsurf.com/windsurf/cascade/mcp; https://docs.windsurf.com/windsurf/cascade/agents-md |
| **Zed** | Skills in `~/.agents/skills/` and `<worktree>/.agents/skills/` (trusted worktrees only, no nesting). Invoked with `/cpm-plan` or `@skill`. Rules have been folded into skills and instructions. | Yes, as the primary instruction file. Personal file: `~/.config/zed/AGENTS.md`. | `settings.json` → `"context_servers": {"cpm-planner": {"command": "npx", "args": ["-y", "@matthew-cochran/cpm"], "env": {}}}` | https://zed.dev/docs/ai/skills; https://zed.dev/docs/ai/mcp; https://zed.dev/docs/ai/instructions |
| **Cline** | Project skills: `.cline/skills/`, `.clinerules/skills/`, `.claude/skills/`. Global skills: `~/.cline/skills/`. Invoked as `/skill-name` or automatically, and enabled by default. Global wins on a name clash. | Listed as a supported rule type. Also reads `~/.agents/AGENTS.md`. | `~/.cline/data/settings/cline_mcp_settings.json`. I did not verify a project-level file. | https://docs.cline.bot/customization/skills; https://docs.cline.bot/customization/overview |
| **Roo Code** | Skills in `~/.roo/skills/`, `~/.agents/skills/`, `.roo/skills/`, `.agents/skills/`, plus mode-specific `skills-{mode}/`. Slash commands are a separate feature; I did not verify their location. | Not mentioned on the skills page. agents.md lists RooCode as a reader. | I did not verify. Older docs used `.roo/mcp.json`; that is unconfirmed today. | https://roocodeinc.github.io/Roo-Code/features/skills (redirect from docs.roocode.com; page "Last updated May 15, 2026") |
| **Amp** | Skills in `~/.config/agents/skills/`, `~/.agents/skills/`, `~/.config/amp/skills/`, `.agents/skills/` (and parents), `.claude/skills/`, `~/.claude/skills/`, and others. The first matching `name` wins. User invocation syntax is not documented; skills load from the model's choice. | Yes: cwd, parents and subtrees. Falls back to `AGENT.md` or `CLAUDE.md`. | `amp.mcpServers` in Amp settings. Skills can also bundle `mcp.json`. | https://ampcode.com/docs/customize/skills; https://ampcode.com/docs/customize/mcp |
| **JetBrains Junie** | Skills in `.junie/skills/<name>/SKILL.md`. The CLI also takes `--skill-location`. I did not verify user-level skills or slash invocation. | Yes. `.junie/AGENTS.md` is used exclusively if it exists; otherwise root `AGENTS.md` plus `.junie/rules`. Global file: `~/.junie/AGENTS.md`. | `.junie/mcp/mcp.json` (project), `~/.junie/mcp/mcp.json` (user). The IDE and CLI share the format. | https://junie.jetbrains.com/docs/guidelines-and-memory.html; https://junie.jetbrains.com/docs/junie-cli-mcp-configuration.html |

Notes:
- The JetBrains AI Assistant (as distinct from Junie) was not separately verified.
- Every tool in the table can consume the shared SKILL.md. Writing to `.agents/skills` / `~/.agents/skills` reaches Windsurf, Zed, Roo and Amp without dedicated targets. Cline and Junie are not reached this way.

---

## Recommendation table

Five skills are written per target: `cpm-plan`, `cpm-improve`, `cpm-run`, `cpm-ev`, `cpm-revise`. Every SKILL.md target writes `<root>/<skill>/SKILL.md` from the shared template.

| Target id | User scope writes | Project scope writes | Invocation users get | Confidence |
|---|---|---|---|---|
| `claude` | `~/.claude/skills/cpm-*/SKILL.md`. Optionally print or run `claude mcp add --transport stdio --scope user cpm-planner -- <cmd>`. | `.claude/skills/cpm-*/SKILL.md`, and `.mcp.json` `mcpServers.cpm-planner` | `/cpm-plan`, `/cpm-improve`, `/cpm-run`, `/cpm-ev`, `/cpm-revise` | **High.** Paths, invocation and the skills-over-commands precedence are explicit in the docs. |
| `codex` | `~/.agents/skills/cpm-*/SKILL.md`, and `~/.codex/config.toml` `[mcp_servers.cpm-planner]` (or `codex mcp add`) | `.agents/skills/cpm-*/SKILL.md`, and `.codex/config.toml` `[mcp_servers.cpm-planner]` (trusted projects only) | `$cpm-plan`, `$cpm-improve`, … (or the `/skills` picker). No `/cpm-plan`. | **Medium.** Skill paths and `$` syntax are documented. The user path is `~/.agents`, not `~/.codex/skills`. `codex mcp add` has no documented scope or target file. Skipping `~/.codex/prompts` is fine because it is deprecated. |
| `cursor` | `~/.cursor/skills/cpm-*/SKILL.md` (or `~/.agents/skills`), and `~/.cursor/mcp.json` | `.cursor/skills/cpm-*/SKILL.md`, and `.cursor/mcp.json` | `/cpm-plan`, … (also `@cpm-plan`) | **High** for skills and MCP, which are documented. **Low** for `.cursor/commands` / `~/.cursor/commands`, which have no current official docs, so do not write them. |
| `gemini` | `~/.gemini/commands/cpm-*.toml`, `~/.agents/skills/cpm-*/SKILL.md`, and `~/.gemini/settings.json` `mcpServers.cpm-planner` (or `gemini mcp add -s user`) | `.gemini/commands/cpm-*.toml`, `.agents/skills/cpm-*/SKILL.md`, and `.gemini/settings.json` | `/cpm-plan`, … from the TOML commands. Use `cpm/plan.toml` to get `/cpm:plan`. Skills are model-activated. | **Medium.** Gemini CLI paths are well documented, but the product is now restricted to enterprise and API-key users. Antigravity paths (global skills, `mcp_config.json` stdio keys) are **low** and conflicting, so ship a `gemini-extension.json` as the forward path. |
| `copilot` | `~/.copilot/skills/cpm-*/SKILL.md`. MCP through `code --add-mcp '{…}'`. No user prompt files, because no path is documented. | `.github/skills/cpm-*/SKILL.md` (optionally `.github/prompts/cpm-*.prompt.md`), and `.vscode/mcp.json` `servers.cpm-planner` | `/cpm-plan`, … | **High.** Skills paths, limits and the `servers` key are explicit. Medium for prompt-file `agent` versus legacy `mode` on older VS Code builds. |
| `agents-md` | `~/.agents/skills/cpm-*/SKILL.md`, read by Codex, Cursor, Copilot, Gemini, Zed, Roo, Amp and Windsurf. No universal user AGENTS.md exists, so write none. | Managed block in `./AGENTS.md`, and `.agents/skills/cpm-*/SKILL.md` | Tool-dependent: `/cpm-plan` (Cursor, Copilot, Zed), `$cpm-plan` (Codex), `@cpm-plan` (Windsurf), auto-activation elsewhere. AGENTS.md alone gives no command. | **Medium.** Reader support for `.agents/skills` is well documented per tool, but Claude Code does not read it. AGENTS.md semantics vary per tool, for example Gemini needs `context.fileName` and Claude skips it when a CLAUDE.md exists. |

**Cross-target guidance**

- **Duplicates.** Cursor reads `.claude`, `.codex` and `.agents`. Copilot reads `.claude`, `.agents` and `.github`. Installing several targets into one repo may list the skill twice, and no tool documents dedup. Two mitigations:
  - Have `skills install --target all` write `.claude/skills` plus `.agents/skills` only.
  - Detect the overlap and warn.
- **MCP snippet.** Default to `npx -y @matthew-cochran/cpm` for portability, and use `cpm-planner` when the binary is on PATH. The JSON key is `mcpServers` everywhere except VS Code (`servers`) and Zed (`context_servers`).
- **Name rule.** Keep `name` equal to the directory and `[a-z0-9-]{1,64}`. All five planned names comply.
