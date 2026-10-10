# AGENTS.md

Instructions for coding agents working on this repository. People should read
[CONTRIBUTING.md](CONTRIBUTING.md), which has the full detail; this file is
the short version.

This file is about contributing to cpm-planner itself. The
`<!-- cpm-planner:begin -->` / `<!-- cpm-planner:end -->` block that
`cpm-planner skills install --target agents-md` writes belongs in *users'*
repositories; do not add it here.

## Build, test and gates

The toolchain is pinned in `rust-toolchain.toml`. Run every gate before you
push; CI runs the same ones. Besides Rust you need `cargo-deny`, `bash` and
`jq` (for `scripts/check-version-sync.sh`), and Node 21+ (for the
`npm/test/*.test.mjs` glob).

```sh
cargo build
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo publish --dry-run --locked
cargo deny check
scripts/check-version-sync.sh
node --test "npm/test/*.test.mjs"   # Node 21+ for the glob; the package supports 18+
```

The binary is `cpm-planner`. With no arguments it is the MCP stdio server;
`cargo run -q -- --help` lists the subcommands.

## Branch model

- Branch from `dev` and open the pull request into `dev`.
- `dev` goes to `main` only through a release PR; tags `vX.Y.Z` are pushed on
  `main` (see [docs/releasing.md](docs/releasing.md)).
- Never push to `main` or `dev` directly, and never tag or publish.

## Conventions

- Conventional Commits: `feat(scope): ...`, `fix(scope): ...`, `docs: ...`.
- Every behaviour change has a test. Tests have declarative names and one
  behavioural assertion each, and default tests never use the network.
- Text files are LF (`.gitattributes`). `tests/repo_hygiene.rs` checks this,
  that relative links in `README.md`, `AGENTS.md` and `docs/` resolve, and
  that the `main`-branch links in `llms.txt` name existing files and headings.
- Add a line under `[Unreleased]` in [CHANGELOG.md](CHANGELOG.md).
- Update the README and `docs/` when behaviour they describe changes.

## Skills

The agent skills live in `skills/<name>/SKILL.md`, the one source of truth.
`build.rs` embeds them in the binary, and `cpm-planner skills install` writes
them out per tool; never commit generated copies. `tests/skill_docs.rs`
checks their front matter and that every tool they name exists;
`tests/skills_install.rs` covers the installer. Per-tool paths come from
[docs/agents/tool-matrix.md](docs/agents/tool-matrix.md), which cites the
vendor docs: change a path there first, with its source.

## Secrets

Never commit API keys, tokens or `.env` files, and never print a key in test
output or logs. `OPENROUTER_API_KEY` comes from the environment only.

## The live test

The OpenRouter review test is ignored by default and costs money. Run it only
when asked:

```sh
OPENROUTER_API_KEY=... cargo test --test server_review -- --ignored
```
