## Summary

<!-- What does this change and why? -->

## Linked issue

<!-- Closes #123 -->

## Checklist

- [ ] Tests added or updated for the behavior change
- [ ] All 9 gates pass (see CONTRIBUTING.md):
  - [ ] `cargo fmt --all --check`
  - [ ] `cargo clippy --all-targets -- -D warnings`
  - [ ] `cargo nextest run --all-targets`
  - [ ] `cargo test --doc`
  - [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`
  - [ ] `cargo publish --dry-run --locked`
  - [ ] `cargo deny check`
  - [ ] `scripts/check-version-sync.sh`
  - [ ] `node --test "npm/test/*.test.mjs"`
- [ ] `CHANGELOG.md` has an entry under `[Unreleased]`
- [ ] Docs updated (README, `docs/`, skills) where behavior or tool schemas changed
- [ ] PR targets `dev`
