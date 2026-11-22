## What and why

<!-- What does this change, and why is it needed? Link the issue if there is one. -->

## How it was tested

<!-- Commands run, new or updated tests, manual checks. -->

## Checklist

- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass
- [ ] New user-facing strings exist in both `locales/en` and `locales/tr`
- [ ] Migrations are additive (expand/contract, docs/UPDATES.md §5); released migrations are unchanged
- [ ] `CHANGELOG.md` (`Unreleased`) is updated if users or operators notice the change
- [ ] No secrets, credentials or personal data in code, tests, logs or fixtures
