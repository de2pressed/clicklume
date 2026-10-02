# Changelog

All notable changes to ClickLume will be documented here. The project follows
Semantic Versioning after the first public preview.

## [Unreleased]

### Added

- Compact monochrome translucent GUI with persistent light and dark modes
- Portable sibling discovery for the click backend
- User installer and uninstaller
- Debian and release-archive packaging scripts
- GitHub Actions for formatting, linting, tests, builds, and tagged releases
- Security, contribution, issue, and pull-request templates

### Fixed

- Correct F11/F12 evdev codes and avoid duplicate focused/global hotkey actions
- Honor configured CPS bounds and share atomic increase/decrease behavior
- Accumulate fragmented IPC commands, validate replies, and protect socket ownership
- Reject duplicate backend startup and restrict IPC socket access to the desktop user
- Surface uinput startup failures and release held buttons promptly on Stop/Quit
- Preserve newer backend settings during debounced GUI saves and write TOML atomically
- Accept partial configuration files and align GUI controls with the backend rate range
- Stop installation after failed builds and preserve login-start preferences on upgrades
- Apply uinput ACLs despite late udev-rule ordering and refresh the uinput device
- Report failed autostart changes and close inherited signal-pipe descriptors

- Backend death no longer loses the child handle or leaves a zombie on respawn
- systemd stop now signals the GUI before terminating the service cgroup

## [0.1.0] - Unreleased

Initial public preview for Ubuntu 24.04 GNOME 46 Wayland.
