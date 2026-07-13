## Summary

Describe the user-visible change and why it is needed.

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo build --release --locked`
- [ ] Wayland smoke test completed when runtime behavior changed

## Safety

- [ ] No runtime root requirement was added
- [ ] The virtual mouse still declares REL_X and REL_Y
- [ ] No new dependency was added without discussion
