## Summary

<!-- What does this change and why? -->

## Linked issue

<!-- Closes #123 -->

## Checklist

- [ ] Tests added or updated for the behavior change
- [ ] All 10 gates pass (see CONTRIBUTING.md):
  - [ ] `cargo fmt --all --check`
  - [ ] `cargo clippy --workspace --all-targets -- -D warnings`
  - [ ] `cargo nextest run --workspace --all-targets`
  - [ ] `cargo test --workspace --doc`
  - [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`
  - [ ] `cargo publish --workspace --dry-run --locked`
  - [ ] `cargo deny check`
  - [ ] `scripts/check-version-sync.sh`
  - [ ] `scripts/check-core-deps.sh`
  - [ ] `node --test "npm/test/*.test.mjs"`
- [ ] `CHANGELOG.md` has an entry under `[Unreleased]`
- [ ] Docs updated (README, `docs/`, skills) where behavior or tool schemas changed
- [ ] PR targets `dev`
