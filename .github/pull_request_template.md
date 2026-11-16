## Summary

<!-- What does this change and why? Link the issue. -->

## Verification

<!-- How was this verified? Tests added/run, hardware tested (which OS/devices), or why not applicable. -->

## Checklist

- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets`, `cargo test --workspace` pass locally
- [ ] Real-time code paths stay allocation-free and lock-free (see `docs/architecture/overview.md`)
- [ ] `unsafe` code, FFI, driver, installer, updater or CI-permission changes are called out for careful review
- [ ] Docs, feature matrix and CHANGELOG updated where behaviour changed
