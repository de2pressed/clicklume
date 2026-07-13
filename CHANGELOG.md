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

- Backend death no longer loses the child handle or leaves a zombie on respawn
- systemd stop now signals the GUI before terminating the service cgroup

## [0.1.0] - Unreleased

Initial public preview for Ubuntu 24.04 GNOME 46 Wayland.
