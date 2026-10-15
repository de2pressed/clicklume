# Changelog

All notable changes to ClickLume will be documented here. The project follows
Semantic Versioning after the first public preview.

## [Unreleased]

### Performance and reliability

- Replace click-worker polling and periodic idle GUI rendering with blocking waits and event-driven updates
- Bound IPC workers, connection queues, notification frames and background queues
- Include queue time in IPC read deadlines and disconnect stalled subscribers
- Reap backend children in one supervisor, recover before the first GUI subscription, and handle termination signals without waiting for a native repaint
- Join hotplug readers, reconcile late device permissions and avoid stale reader handles
- Debounce settings writes, preserve newer listener snapshots and restore staged preferences after recovery
- Remove duplicate input synchronization events and per-click string copies
- Add resource, overload, cancellation and repeated backend-recovery regression checks

### Added

- Compact monochrome translucent GUI with persistent light and dark modes
- Portable sibling discovery for the click backend
- User installer and uninstaller
- Debian and release-archive packaging scripts
- GitHub Actions for formatting, linting, tests, builds, and tagged releases
- Security, contribution, issue, and pull-request templates

### Fixed

- Disable obsolete ClickLume GNOME shortcuts during upgrades so a global hotkey cannot start and immediately stop clicking through duplicate handlers
- Apply repeat-mode changes immediately and flush pending settings before Start, avoiding a stale one-click limit in “Until stopped” mode
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
