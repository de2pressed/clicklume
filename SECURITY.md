# Security policy

## Supported versions

Until ClickLume reaches 1.0, security fixes are provided for the latest release
only.

## Reporting a vulnerability

Do not open a public issue for vulnerabilities involving input-device access,
command execution, installer privilege boundaries, or IPC authorization.

Use the private security-advisory form at:

https://github.com/de2pressed/clicklume/security/advisories/new

Include the affected version, Ubuntu/GNOME versions, reproduction steps, and
impact. Do not include passwords, authentication tokens, or unrelated input
data. An initial response is targeted within seven days.

## Permission model

ClickLume runs as the desktop user. Its installer performs two privileged setup
operations: installing udev rules and optionally adding a user to the `input`
group. The GUI and backend must never run as root.
