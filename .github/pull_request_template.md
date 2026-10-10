## Summary

<!-- What does this change and why? -->

## Linked issue

<!-- Closes #123 -->

## Checklist

- [ ] Tests added or updated for the behavior change
- [ ] All 8 gates pass (see CONTRIBUTING.md):
  - [ ] `cargo fmt --all --check`
  - [ ] `cargo clippy --all-targets -- -D warnings`
  - [ ] `cargo test`
  - [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`
  - [ ] `cargo publish --dry-run --locked`
  - [ ] `cargo deny check`
  - [ ] `scripts/check-version-sync.sh`
  - [ ] `node --test "npm/test/*.test.mjs"`
- [ ] `CHANGELOG.md` has an entry under `[Unreleased]`
- [ ] Docs updated (README, `docs/`, skills) where behavior or tool schemas changed
- [ ] PR targets `dev`
