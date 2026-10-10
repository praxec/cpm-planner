#!/usr/bin/env bash
# Smoke test for `cpm-planner skills install|list|uninstall` against a real
# binary, in throwaway directories. Used by .github/workflows/installer-tests.yml
# on Linux, macOS and Windows (Git Bash).
#
# Usage: scripts/skills-smoke.sh <path-to-cpm-planner> [<scratch-dir>]
# The scratch dir defaults to $RUNNER_TEMP, else a new mktemp -d directory.
set -euo pipefail

exe="${1:?usage: skills-smoke.sh <path-to-cpm-planner> [<scratch-dir>]}"
scratch="${2:-${RUNNER_TEMP:-$(mktemp -d)}}"
scratch="$(cd "$scratch" && pwd)"

# A path as the native binary should see it (C:\... on Windows).
native() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s' "$1"; fi
}

fail() { echo "FAIL: $*" >&2; exit 1; }
skills="cpm-ev cpm-improve cpm-plan cpm-revise cpm-run deliverable-cpm"
commands="cpm-ev cpm-improve cpm-plan cpm-revise cpm-run"

# Every skill's SKILL.md exists under each given skills directory.
expect_skills() {
  for dir in "$@"; do
    for s in $skills; do
      [ -f "$dir/$s/SKILL.md" ] || fail "missing $dir/$s/SKILL.md"
    done
  done
}

# No file the installer writes is left under the given directory.
expect_no_skill_files() {
  left="$(find "$1" \( -name SKILL.md -o -name 'cpm-*.toml' \) -print)"
  [ -z "$left" ] || fail "files left after uninstall: $left"
}

# Lines that report one file: "<status>  <path>".
file_lines() { grep -E '^(created|updated|unchanged|modified, skipped|foreign, skipped|removed)  ' "$1" || true; }

echo "== project scope"
proj="$scratch/proj"
rm -rf "$proj" && mkdir -p "$proj"
"$exe" skills install --target all --project "$(native "$proj")" > "$scratch/first.log"
cat "$scratch/first.log"
expect_skills "$proj/.claude/skills" "$proj/.agents/skills"

echo "== rerun is a no-op"
"$exe" skills install --target all --project "$(native "$proj")" > "$scratch/second.log"
total="$(file_lines "$scratch/second.log" | wc -l | tr -d ' ')"
changed="$(file_lines "$scratch/second.log" | grep -v '^unchanged  ' || true)"
[ "$total" -gt 0 ] || fail "rerun printed no file lines"
[ -z "$changed" ] || fail "rerun changed files: $changed"
echo "all $total file lines unchanged"

echo "== project list"
"$exe" skills list --project "$(native "$proj")" | tee "$scratch/list-project.log"
[ "$(grep -c '(project)' "$scratch/list-project.log" || true)" -eq 2 ] || fail "expected 2 project installs"

echo "== user scope (HOME and USERPROFILE redirected, path with a space)"
uhome="$scratch/home with space"
rm -rf "$uhome" && mkdir -p "$uhome"
user_env=(env "HOME=$(native "$uhome")" "USERPROFILE=$(native "$uhome")")
for t in claude codex cursor copilot gemini; do
  "${user_env[@]}" "$exe" skills install --target "$t" --user > "$scratch/user-$t.log"
done
expect_skills "$uhome/.claude/skills" "$uhome/.agents/skills" "$uhome/.cursor/skills" "$uhome/.copilot/skills"
for c in $commands; do
  [ -f "$uhome/.gemini/commands/$c.toml" ] || fail "missing $uhome/.gemini/commands/$c.toml"
done

echo "== user list"
"${user_env[@]}" "$exe" skills list --user | tee "$scratch/list-user.log"
[ "$(grep -c '(user)' "$scratch/list-user.log" || true)" -eq 5 ] || fail "expected 5 user installs"

if [ "${RUNNER_OS:-}" = "Windows" ] || [ "${OS:-}" = "Windows_NT" ]; then
  echo "== Windows: USERPROFILE alone (no HOME)"
  wprofile="$scratch/profile only"
  rm -rf "$wprofile" && mkdir -p "$wprofile"
  env -u HOME "USERPROFILE=$(native "$wprofile")" "$exe" skills install --target claude --user > "$scratch/profile.log"
  expect_skills "$wprofile/.claude/skills"
fi

echo "== uninstall"
"$exe" skills uninstall --target all --project "$(native "$proj")"
expect_no_skill_files "$proj"
for t in claude codex cursor copilot gemini; do
  "${user_env[@]}" "$exe" skills uninstall --target "$t" --user > "$scratch/uninstall-$t.log"
done
expect_no_skill_files "$uhome"

echo "skills smoke: OK"
