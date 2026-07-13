# Contributing to ClickLume

Thank you for helping improve ClickLume. The initial support target is Ubuntu
24.04 LTS with GNOME 46 on Wayland.

## Before changing code

- Keep runtime unprivileged; never make the GUI or backend run as root.
- Do not remove `REL_X` or `REL_Y` from the virtual mouse capabilities.
- Discuss new dependencies before adding them.
- Keep user-facing controls connected to real backend behavior.
- Do not claim compositor support that has not been tested.

## Development checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```

Changes to clicking, IPC, hotkeys, lifecycle management, installation, or the
GUI must also complete the end-to-end Wayland smoke test.

## Pull requests

Keep pull requests focused. Explain the user problem, the chosen behavior, and
how it was verified. Include before/after screenshots for visual changes and
call out permission or lifecycle implications explicitly.

By contributing, you agree that your work is licensed under the MIT License.
