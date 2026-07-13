# GitHub launch plan

Approved by the owner for publication under the ClickLume name.

## Repository

- Owner: `de2pressed`
- Name: `clicklume`
- URL: `https://github.com/de2pressed/clicklume`
- Visibility: Public
- Default branch: `main`
- License: MIT

Description:

> Native Rust autoclicker for Ubuntu 24.04 GNOME Wayland — global hotkeys,
> repeat modes, random intervals, and a compact monochrome light/dark GUI. No
> X11 and no root at runtime.

Topics:

`wayland`, `ubuntu`, `gnome`, `autoclicker`, `auto-clicker`, `rust`, `uinput`,
`evdev`, `linux-automation`, `global-hotkeys`

## Before creation

1. Owner visually approves the final compact GUI.
2. Capture one dark and one light screenshot at the default 560x455 size.
3. Add screenshots under `assets/screenshots/` and to the README.
4. Confirm whether the public author label should remain `de2pressed` or use a
   different display name.
5. Review the `input`-group security disclosure in README and SECURITY.

## Creation sequence after approval

1. Create an empty public `de2pressed/clicklume` repository without GitHub's
   generated README, license, or `.gitignore`.
2. Review the local public-file list. Internal `AGENTS.md`, `agent-docs/`,
   `.hermes/`, build output, and release artifacts are ignored.
3. Create the initial commit on `main`:
   `Initial ClickLume public preview`.
4. Add `https://github.com/de2pressed/clicklume.git` as `origin` and push.
5. Apply the description and topics above.

## Repository settings

- Enable Issues and private security advisories.
- Keep Discussions and Wiki disabled for the preview unless community demand
  appears.
- Protect `main`: require pull requests, require the `CI / rust` and
  `CI / shell` checks, dismiss stale approvals, and block force pushes.
- Enable Dependabot alerts and dependency graph.
- Allow GitHub Actions to create releases through the workflow's scoped
  `contents: write` permission.

## First release

1. Confirm CI passes on `main`.
2. Tag the reviewed commit as `v0.1.0` and push the tag.
3. The release workflow builds and publishes:
   - `clicklume_0.1.0_amd64.deb`
   - `clicklume-0.1.0-linux-x86_64.tar.gz`
   - `SHA256SUMS`
4. Install the uploaded `.deb` on a clean Ubuntu 24.04 GNOME Wayland account.
5. Re-run toggle, forced-death recovery, clean Quit, and logout/login hotkey
   checks before marking the release non-draft.

Release title:

> ClickLume v0.1.0 — Ubuntu GNOME Wayland preview

The release notes must state the narrow support target, one-time input-group
permission, no-root runtime model, and absence of coordinate picking or macro
recording.

## SEO and presentation

- Keep “Ubuntu 24.04”, “GNOME Wayland”, and “autoclicker” in the first README
  paragraph.
- Use descriptive screenshot alt text containing “ClickLume autoclicker on
  GNOME Wayland”.
- Keep repository and binary naming consistent: ClickLume / `clicklume`.
- Link troubleshooting headings directly from issue-form guidance.
- Do not claim generic Linux or all-Wayland compatibility until tested.

## Later milestones

- `v0.2.0`: clean-machine installer/uninstaller coverage and diagnostics bundle.
- `v0.3.0`: accessibility audit, keyboard navigation, and multi-display testing.
- `v1.0.0`: only after repeatable testing across multiple Ubuntu 24.04 GNOME
  systems, reboot/relogin recovery, and a stable installation contract.
