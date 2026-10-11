# P8 — Open-source readiness + agent install (`cpm-*` skills) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** cpm-planner is a properly set up open-source repository — community health files, supply-chain checks, protected branches, accurate packaging and docs — and any agentic coding tool can install the server and the `cpm-*` skills from a release binary by following one document; nothing is published until this lands.

**Architecture:** Part A hardens the repo (line endings, health files, CI supply-chain jobs, packaging metadata, docs index, GitHub settings). Part B splits the `deliverable-cpm` skill into a `cpm-*` family generated from one source, embeds it in the binary, and adds `cpm-planner skills install` that writes each tool's native format, plus `docs/AGENT-INSTALL.md` / `llms.txt`.

**Tech Stack:** Rust 1.99 (edition 2024), `include_str!`/`include_dir`-style embedding (no new runtime deps unless justified), GitHub Actions, `cargo-deny`, Dependabot.

**Spec:** user requests 2026-10-10 — "instructions for LLMs to wire this up and install the skills (cpm skills we can call with `/cpm-*`) … for any agentic coding tool (codex, claude code, cursor, agents.md, etc)" and "don't publish until we have a properly set up and documented repo with everything an open-source repo should have".

## Global Constraints
- No tag, release, crates.io, npm or MCP-registry publish in this plan; npm `@matthew-cochran/cpm` is published manually by the user per the runbook. Release PR #43 stays unmerged; publishing is a separate, user-approved step after the final review.
- Next published version is **0.2.0** (new `skills` subcommand = feature); the unpublished `[0.1.1]` CHANGELOG section folds into `[0.2.0]` (Task 10).
- One source of truth for skill content: `skills/`. Generated per-tool files are produced by code, never hand-edited copies in the repo.
- Every per-tool path and invocation syntax is verified against that tool's official documentation (URL recorded in `docs/agents/tool-matrix.md`) — never from memory.
- `skills install` never overwrites a file it did not write or that the user has modified (hash manifest); `--force` is the only override. It never needs network access and never runs with elevated privileges.
- Repository text files are LF (`.gitattributes`), except files that must be CRLF (`*.ps1` stays as-is if PowerShell requires; `*.bat`/`*.cmd` CRLF).
- Existing gates stay green: `cargo fmt --all --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`, `scripts/check-version-sync.sh`.
- Tests: declarative names, one behavioural assertion each, no network in default tests.
- GitHub settings changes (Task 5) are made by the controller via `gh api`, are reversible, and are listed in the PR body.

## Review Focus
1. Re-running `skills install` after the user edited a generated file → the edit is kept, the run reports it as "modified, skipped", exit code 0 (test in Task 8).
2. `skills install --target agents-md` on a repo whose `AGENTS.md` already has user content → only the marked block between `<!-- cpm-planner:begin -->`/`<!-- cpm-planner:end -->` changes; content outside is byte-identical (test in Task 8).
3. Windows paths (`%USERPROFILE%`, backslashes) and a HOME with spaces → files land in the right place (CI smoke on windows-latest in Task 9; unit test with a spaced temp dir in Task 8).
4. A newer binary installing over skills written by an older binary → files are updated only where the manifest hash still matches what the old binary wrote (test in Task 8).
5. `cargo install cpm-planner` from crates.io currently yields 0.0.1 with a stale repository URL → README must not advertise it until a matching version is published; release runbook covers `cargo publish` (Task 4, Task 10).

---

### Task 1: Line endings and editor config
**Files:** Create `.gitattributes`, `.editorconfig`; renormalize the 10 CRLF and 2 mixed files.
- [ ] `.gitattributes`: `* text=auto eol=lf`; `*.ps1 text eol=crlf` only if `scripts/install.ps1` tests require it (check `installer-tests.yml`); binaries (`*.png`, `*.ico`, `*.db`) `binary`.
- [ ] `.editorconfig`: utf-8, lf, final newline, trim trailing whitespace (except `*.md`), indent 4 for `*.rs`, 2 for `*.yml/*.json/*.toml`.
- [ ] `git add --renormalize .` in ONE commit containing only line-ending changes (`git diff --ignore-all-space --stat` must be empty apart from the two new files' commit).
- [ ] Test: `tests/repo_hygiene.rs::tracked_text_files_are_lf` — walks `git ls-files` (skip binaries/`*.ps1` if CRLF ruled) and asserts no `\r\n`.
- [ ] Update CONTRIBUTING (Task 2) to drop any CRLF-preservation advice. Commit `chore(repo): normalize line endings to LF; add .gitattributes and .editorconfig`.

### Task 2: Community health files
**Files:** Create `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1, verbatim, enforcement contact = GitHub private reporting link + maintainer email from `Cargo.toml` authors), `SUPPORT.md`, `.github/ISSUE_TEMPLATE/{bug_report.yml,feature_request.yml,config.yml}`, `.github/pull_request_template.md`, `.github/CODEOWNERS` (`* @matt-cochran`). Rewrite `CONTRIBUTING.md`; update `SECURITY.md`.
- [ ] Bug form asks: version (`cpm-planner --version`), OS/arch, MCP client, the `plan.submit`/plan file or tool call that reproduces, expected vs actual; `config.yml` disables blank issues and links security reports to advisories and questions to SUPPORT.
- [ ] PR template: summary, linked issue, checklist (tests, fmt/clippy/doc, CHANGELOG `[Unreleased]`, docs updated).
- [ ] CONTRIBUTING: architecture map of `src/` modules (one line each, from the actual tree), branch model (feature → PR into `dev`; `dev` → `main` release PR; tags `vX.Y.Z` on `main`), conventional commits, test discipline, how to run the ignored live OpenRouter test (`OPENROUTER_API_KEY=… cargo test --test server_review -- --ignored`), MSRV/toolchain pin, how skills are generated (Task 7).
- [ ] SECURITY: supported-versions table (latest minor only), scope rewritten to current surface — plan-file I/O confined to the project root (no-follow handles), optional outbound HTTPS to OpenRouter with key hygiene guarantees, SQLite store, `skills install` writes under user/project dirs only; private reporting link; expected response time.
- [ ] Test: `tests/repo_hygiene.rs::issue_forms_are_valid_yaml` (parse the three YAML files with `serde_yaml` dev-dep or a minimal check if a dep is unjustified). Commit `docs(community): code of conduct, support, issue/PR templates, CODEOWNERS; refresh CONTRIBUTING and SECURITY`.

### Task 3: Supply-chain CI
**Files:** Create `.github/dependabot.yml`, `deny.toml`; modify `.github/workflows/ci.yml`.
- [ ] Dependabot: `cargo` and `github-actions`, weekly, grouped minor/patch, target branch `dev`.
- [ ] `deny.toml`: licenses allowlist derived from the current tree (`cargo deny list` output recorded in report), advisories deny, bans for duplicate-major warn, sources crates.io only.
- [ ] CI jobs: `cargo-deny` (pinned action version), `docs` (`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`), `package` (`cargo package --locked --list` + `cargo publish --dry-run --locked`), all with `timeout-minutes`.
- [ ] Fix anything `cargo deny check` flags (or document a justified exception in `deny.toml` with a comment). Commit `ci: cargo-deny, docs and package checks; dependabot`.

### Task 4: Packaging and documentation
**Files:** `Cargo.toml` (description, `homepage`, `documentation = "https://docs.rs/cpm-planner"`, keywords ≤5, categories, `include`/`exclude` so `docs/superpowers/` and CI scripts are not packaged), `[package.metadata.docs.rs]`, `src/lib.rs` crate-level `//!` overview with a runnable example, `README.md` (badges: CI, release, license; table of contents; one-paragraph "what it is"; remove or caveat `cargo install` until crates.io has a matching version), `docs/README.md` (index of docs), `docs/architecture.md` (modules, data flow submit→schedule→lease→EV, store schema versions), `docs/releasing.md` (runbook: version bump, CHANGELOG, release PR `dev`→`main` merge commit, tag, verify assets/installer/GHCR, `cargo publish`, MCP registry, rollback notes).
- [ ] `docs/superpowers/` stays in place (SDD tooling paths depend on it); `docs/README.md` explains it holds design/plan history.
- [ ] Test: `tests/repo_hygiene.rs::readme_links_resolve` — every relative link in README.md and docs/*.md points to an existing file/anchor.
- [ ] `cargo publish --dry-run` passes and the package file list contains no `docs/superpowers`. Commit `docs: crate docs, architecture, releasing runbook, docs index; packaging metadata`.

### Task 5: Repository settings (controller-run)
Via `gh api` (reversible; record before/after in the PR body):
- [ ] Description = Cargo `description`; homepage = README anchor or docs.rs; topics: `mcp`, `mcp-server`, `critical-path`, `project-planning`, `earned-value`, `agents`, `rust`.
- [ ] Branch protection on `main` and `dev`: required status checks = the CI job names (test ×3 OS, fmt + clippy, cargo-deny, docs, package, installer); no force-push; no deletion; `main` also requires the gitflow guard.
- [ ] Private vulnerability reporting enabled; Discussions off unless the user asks.
- [ ] crates.io: note (no action) that the 0.0.1 listing points at the old repository — fixed by the next `cargo publish`.

### Task 6: Tool-convention research
**Files:** Create `docs/agents/tool-matrix.md`.
- [ ] For Claude Code (skills, slash invocation, user vs project dirs), Codex CLI (skills dirs, invocation, AGENTS.md), Cursor (commands dir and format, rules), Gemini CLI (custom commands TOML, namespacing, GEMINI.md), GitHub Copilot in VS Code (prompt files / instructions), and generic AGENTS.md: record the exact path(s), file format, front-matter fields, how the user invokes it, and the MCP registration snippet — each with the official doc URL and date checked.
- [ ] Ruling recorded for each tool: which `cpm-*` name/syntax users type (e.g. `/cpm-plan`, `/cpm:plan`).
- [ ] No code. Commit `docs(agents): per-tool skill/command conventions with sources`.

### Task 7: `cpm-*` skill family
**Files:** `skills/cpm-plan/SKILL.md`, `skills/cpm-improve/SKILL.md`, `skills/cpm-run/SKILL.md`, `skills/cpm-ev/SKILL.md`, `skills/cpm-revise/SKILL.md`; keep `skills/deliverable-cpm/` as the shared method reference (each `cpm-*` links to it); modify `tests/skill_docs.rs`.
- [ ] Each skill: front matter `name: cpm-<x>`, `description` starting "Use when…" ≤1024 chars with trigger keywords; body ≤150 lines: when to use, the exact tool sequence with argument shapes taken from `src/server.rs`, stop conditions, pitfalls (the five documented limitations where relevant).
- [ ] Tests: every `plan.*` token in every skill is in `PLAN_TOOL_NAMES`; every skill's front matter name matches its directory; descriptions start with "Use when" and fit the limit; every `cpm-*` links to `deliverable-cpm`.
- [ ] Commit `feat(skills): cpm-plan, cpm-improve, cpm-run, cpm-ev, cpm-revise`.

### Task 8: `cpm-planner skills install`
**Files:** Create `src/skills.rs` (embedding + renderers + installer), modify `src/bin/server.rs` (subcommand parsing: no args = MCP server as today; `skills install|uninstall|list`, `--version`, `--help`), tests `tests/skills_install.rs`.
- [ ] Embed `skills/**` at compile time (`include_str!` table generated by `build.rs` or a const list kept in sync by a test).
- [ ] `skills install --target <claude|codex|cursor|gemini|copilot|agents-md|all> [--project <dir> | --user] [--dry-run] [--force]`; renderers per Task 6's matrix; `agents-md` writes/updates a marked block; manifest `.cpm-planner-skills.json` (path → sha256 of what we wrote, binary version) in each target root.
- [ ] `skills list` shows installed targets/versions; `skills uninstall` removes only files whose hash matches the manifest.
- [ ] Output: one line per file (`created|updated|unchanged|modified, skipped`), summary, exit 0; exit 2 on usage errors.
- [ ] Tests (temp dirs, no network): install creates expected files per target; second run reports all `unchanged`; user-edited file is skipped; `--force` overwrites; AGENTS.md outside-block content preserved byte-for-byte; upgrade path from an older manifest; spaced HOME path; `--dry-run` writes nothing; uninstall leaves user-modified files.
- [ ] Docs: README "Agent skill" section rewritten around `skills install`. Commit `feat(cli): skills install/uninstall/list for Claude Code, Codex, Cursor, Gemini, Copilot, AGENTS.md`.

### Task 9: Agent install guide + CI smoke
**Files:** Create `docs/AGENT-INSTALL.md`, `llms.txt` (repo root, llmstxt.org format pointing at AGENT-INSTALL and key docs), `AGENTS.md` (this repo's own contributor-agent instructions: build/test commands, branch model, conventions); modify `README.md` (top-level "For AI agents" link), `.github/workflows/installer-tests.yml`.
- [ ] AGENT-INSTALL: a decision table "which tool are you?" → exact numbered steps per tool: install binary (installer one-liner with pinned version), register MCP (snippet from Task 6), `cpm-planner skills install --target <tool> --user`, verify (`plan.status` visible; `/cpm-plan` resolves; `cpm-planner skills list`).
- [ ] CI smoke (ubuntu/macos/windows): build, `skills install --target all --project $RUNNER_TEMP/proj`, assert expected files, rerun → all unchanged; `--user` with HOME/USERPROFILE redirected to a temp dir.
- [ ] Test: `tests/repo_hygiene.rs::agent_install_names_only_existing_targets` (every `--target` value in AGENT-INSTALL.md is accepted by the CLI parser). Commit `docs(agents): AGENT-INSTALL guide, llms.txt, AGENTS.md; CI skills smoke`.

### Task 11: npm installer `@matthew-cochran/cpm` (manual publish)
**Files:** Create `npm/package.json`, `npm/bin/cpm-planner.js`, `npm/lib/install.js`, `npm/README.md`, `npm/test/*.test.mjs`; modify `.github/workflows/release.yml` (pack + attach tarball), `.github/workflows/ci.yml` (npm tests + `npm pack --dry-run`), `scripts/check-version-sync.sh` (also checks `npm/package.json`), `docs/releasing.md`, `docs/AGENT-INSTALL.md`, README.
- [ ] Package: name `@matthew-cochran/cpm`, version = crate version, `bin: { "cpm-planner": "bin/cpm-planner.js", "cpm": "bin/cpm-planner.js" }`, `engines.node >= 18`, no runtime dependencies, `files` whitelist, `license Apache-2.0`, repository/homepage, `publishConfig.access public`.
- [ ] Launcher: maps `process.platform`/`process.arch` to the six release targets (x86_64/aarch64 × linux-gnu/apple-darwin/pc-windows-msvc); on first run downloads `https://github.com/praxec/cpm-planner/releases/download/v<version>/<asset>` and `checksums.sha256` over HTTPS only, verifies SHA-256, extracts (tar.gz via `tar`, zip via PowerShell `Expand-Archive` on Windows) into a per-version cache (`$XDG_CACHE_HOME` / `~/.cache` / `%LOCALAPPDATA%` `cpm-planner/<version>/`), then spawns the binary with stdio inherited and forwards args/exit code/signals. Honours `CPM_PLANNER_BINARY` (use a local binary, skip download) and `CPM_PLANNER_DOWNLOAD_BASE` (mirror; https unless `PRAXEC_ALLOW_INSECURE=1`, matching install.sh). Never writes to stdout before the child starts (stdout is the MCP channel); progress/errors go to stderr.
- [ ] Optional `postinstall` pre-fetch that never fails the install (offline/CI-safe: errors are warnings).
- [ ] Tests (node:test, no network): target mapping for all six + unsupported platform error; checksum mismatch aborts and deletes the partial file; cache hit skips download (local HTTP fixture server); `CPM_PLANNER_BINARY` bypass; stdout untouched before spawn.
- [ ] Release workflow: after binaries, `npm version` is NOT run (versions come from the repo); `npm pack` in `npm/` and upload `matthew-cochran-cpm-<version>.tgz` to the draft release. No `npm publish` in CI.
- [ ] Runbook (`docs/releasing.md`): manual publish = download the tarball from the release, `npm publish ./matthew-cochran-cpm-<version>.tgz --access public` (2FA), verify with `npx -y @matthew-cochran/cpm --version` and the MCP smoke. AGENT-INSTALL/README gain the `npx -y @matthew-cochran/cpm` MCP command for every client.
- [ ] Commit `feat(npm): @matthew-cochran/cpm launcher that downloads and verifies the release binary`.

### Task 10: Release readiness (no publish)
- [ ] CHANGELOG: fold `[0.1.1]` into `[Unreleased]` → `[0.2.0] - <date set at release>` placeholder kept as `[Unreleased]` until the user approves; version bump to 0.2.0 across Cargo/lock/server.json; README pins → `v0.2.0`.
- [ ] Close #43 as superseded by the 0.2.0 release PR (controller, with a comment).
- [ ] Final whole-branch review on the most capable model, including a fresh-clone walkthrough: a new contributor follows CONTRIBUTING to build/test; a new user follows AGENT-INSTALL for two tools in temp HOMEs.
- [ ] Open the `dev`→`main` release PR for 0.2.0 but do not merge or tag; hand to the user.
