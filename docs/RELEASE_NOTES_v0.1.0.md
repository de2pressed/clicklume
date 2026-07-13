ClickLume v0.1.0 is the first public preview of a native Rust autoclicker for
Ubuntu 24.04 LTS with GNOME 46 on Wayland.

Highlights:

- Compact monochrome GUI with persistent light and dark themes.
- Configurable F1–F12 global hotkeys, with F6–F9 defaults.
- Single, double, and hold click modes with unlimited or fixed repeat counts.
- Random interval offset and 1–1000 CPS control.
- Automatic backend recovery after a crash or clean IPC restart.
- Debian package and portable release archive.

Runtime is unprivileged. Installation performs one-time setup for `/dev/uinput`
and membership in the Linux `input` group so global hotkeys can read keyboard
events. Log out and back in after the group is added.

This preview intentionally supports Ubuntu 24.04 GNOME Wayland only. It does
not claim support for X11 or other Wayland compositors. GNOME 46 does not expose
global pointer coordinates or unrestricted input capture to ordinary clients,
so coordinate picking and macro recording are not included.
