#!/usr/bin/env bash
# Fail when cpm-planner-core's normal or build dependency graph, on any
# target, contains a crate the pure kernel must not pull in: SQLite (whose
# `links = "sqlite3"` would clash with a dependent's own SQLite, such as
# sqlx's), an async runtime, MCP, the LLM clients, filesystem capability
# crates or a logging backend.
# Usage: scripts/check-core-deps.sh
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
forbidden='^(rusqlite|libsqlite3-sys|sqlx.*|tokio|tokio-.*|rmcp|rig-.*|reqwest|hyper|cap-std|cap-fs-ext|tracing-subscriber|execution-policy|uuid|sha2|url)$'

crates="$(cd "$root" && cargo tree --locked -p cpm-planner-core -e normal,build --target all \
  --prefix none --format '{p}' | awk '{ print $1 }' | sort -u)"
[ -n "$crates" ] || { echo "cargo tree printed no crates for cpm-planner-core" >&2; exit 2; }

bad="$(printf '%s\n' "$crates" | grep -E "$forbidden" || true)"
if [ -n "$bad" ]; then
  echo "cpm-planner-core must not depend on:" >&2
  printf '  %s\n' $bad >&2
  exit 1
fi
echo "core deps ok: $(printf '%s\n' "$crates" | wc -l | tr -d ' ') crates, none forbidden"
