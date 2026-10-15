# ClickLume resource and reliability audit — 2026-10-15

Scope: backend click timing, IPC and subscriptions, hotkeys and hotplug,
GUI lifecycle and rendering, persistence, CLI, installer, packaging and user
service. Changes introduce no dependencies and preserve the virtual mouse's
REL_X/REL_Y declaration and unprivileged runtime.

## Changes

| Area | Finding | Change |
| --- | --- | --- |
| Click worker | Woke every 10ms while disabled and every 2ms while waiting/holding | Condition-variable waits; Start/Stop/Quit explicitly wake the worker. Zero-duration waits return immediately. |
| Mouse emission | `VirtualDevice::emit` already appends SYN_REPORT; the extra emit produced three SYN_REPORTs per transition | One KEY plus one SYN_REPORT per transition, instead of one KEY plus three SYN_REPORTs. |
| Click hot path | Cloned/lowercased a button string each action and reseeded jitter from wall-clock time each interval | Compare the borrowed button name and keep a worker-local xorshift state. |
| IPC server | Created a new OS thread on every connection; accepted clients had no overall concurrency limit | Four persistent workers and a queue of 16 connections. Excess connections close immediately; command deadlines include queue time and expired queued work is discarded. |
| Backend idle work | Accept loop woke at 40Hz and hotkey drain at 5Hz | Socket readiness uses poll; hotkey actions use a blocking, bounded channel. |
| Subscribers | Clearing a failed writer left its cloned subscription reader blocked indefinitely | Explicitly shut down failed subscriptions; terminate unsolicited subscriber input; close the active subscription before joining workers at shutdown. |
| Hotplug | Reader threads could remain blocked after Stop; hotplug threads were detached and could not be joined | Nonblocking device reads behind readiness waits, retained/reaped reader handles, bounded hotkey queue, and joined shutdown. |
| Device discovery | IN_CREATE could precede settled permissions or reuse of a removed event node | Watch attribute changes too and reconcile every five seconds, preserving mouse/touchpad exclusions and passive reads. |
| GUI notifications | Unbounded event channel and unbounded partial-line buffer when native logic was delayed | Coalesce by message variant into at most six slots, retain latest status, and reject frames larger than 4096 bytes. |
| GUI lifecycle | Two threads could reap/replace the same backend; early startup death could miss subscription-based recovery | Background supervisor owns and reaps every backend child after startup, and retries independently of window repaints. |
| GUI rendering | Periodic repaint for lifecycle work kept a healthy idle window rendering | Healthy idle windows render on input/status events; pending settings schedule a debounce deadline and missing-backend recovery retains a timer. |
| Settings | Saver woke every 500ms while clean; each backend status synchronously serialized and fsynced settings | One-slot save notification queue; block while clean, debounce changes, and cap continuous-edit save delay at two seconds. Final quit flushes staged settings. |
| Settings ordering | Deferred GUI status processing could persist an older snapshot over the listener's newer settings | The listener stages backend settings; the GUI updates its comparison baseline without saving that status again. Recovery restores staged preferences. |
| CLI/shutdown | CLI writes had no timeout; idle GNOME windows could defer GUI-thread signal handling | Bound CLI writes, reap children in one supervisor, and handle SIGINT/SIGTERM in the self-pipe worker without relying on repaints. |

## Resource boundaries

- IPC: four workers, 16 queued server-side connections, one GUI subscription.
- Commands: at most 4096 bytes, one-second read deadline including queue time, 200ms response-write timeout.
- Notifications: at most 4096 bytes each; at most six pending GUI event categories.
- Hotkeys: 64 queued actions; excess input is dropped rather than accumulating delayed actions.
- Saves: one pending notification; snapshots are staged separately. Signals use a bounded nonblocking self-pipe.
- Recovery retries: exponential backoff capped at two seconds, plus a bounded startup wait.

The application necessarily has event and click loops. Each persistent loop
blocks on input, a condition variable, or a bounded timeout. Recovery retries
and socket errors have backoff; there is no intentional busy-wait loop.

## Verification

Passed: 19 Rust tests, strict workspace Clippy, formatting, shell syntax and
mocked installer regressions, native Wayland release smoke, and backend/GUI
resource checks. Native smoke covered startup, fragmented IPC, configured CPS
bounds, subscription ownership, duplicate-backend rejection, mode-0600 socket,
REL axes, five SIGKILL recoveries with old-process reaping, one clean IPC Quit
recovery, graceful GUI shutdown, and idle-window SIGTERM/backend reaping.

GUI measurements on this desktop (autoclicking disabled):

| Metric | Existing installed release, 30s | Updated release, 180s |
| --- | ---: | ---: |
| CPU, percent of one core | 8.374% | 0.677% |
| Voluntary context switches/second, all threads | 363.80 | 118.29 |
| Resident memory, start/end | 193104 / 200040 KiB | 168764 / 168508 KiB |
| Threads, start/end | 18 / 18 | 18 / 18 |
| File descriptors, start/end | 71 / 71 | 71 / 71 |

After 5,100 status/settings/Stop requests, updated GUI RSS remained 168508 KiB,
with 18 threads and 71 FDs. The saver persisted the final CPS value (1000) and
100 unchanged Stop statuses did not change the preferences file's mtime.
Graphics/driver memory and startup cache behavior vary; this sample establishes
bounded observed usage, not a universal RAM saving.

Backend measurements on this desktop (autoclicking disabled):

| Metric | Existing installed release, 30s | Updated release, 60s |
| --- | ---: | ---: |
| CPU, percent of one core | 0.333% | 0.017% |
| Voluntary context switches/second, all threads | 144.05 | 10.52 |
| Resident memory, start/end | 3116 / 3116 KiB | 3616 / 3616 KiB |
| Threads, start/end | 8 / 8 | 12 / 12 |
| File descriptors, start/end | 11 / 11 | 11 / 11 |

The four persistent IPC workers add a small fixed memory/thread cost in
exchange for removing thread creation per request and bounding overload.
The updated backend handled 96 slow clients without adding threads; server
FDs peaked at 31 (20 above idle). After 5,000 status/settings requests, RSS
remained 3616 KiB and thread count remained 12. The two extra final FDs were
the deliberately retained subscription, which clean Quit closed successfully.

These are short process-level samples, not a controlled hardware benchmark;
physical keyboard activity and native graphics/driver activity can affect
results. The older installed binary was measured without changing it.

Automated verification emits no real mouse clicks or physical hotkeys. Mock
click sinks exercise balanced press/release in all modes and 3,000 immediate
cancellations. Native tests check startup, IPC, overload, backend recovery and
cleanup using the real Wayland session and uinput device creation.

A short soak cannot establish days of crash-free operation. Physical F-key
behavior, actual click delivery, sleep/reboot endurance and multi-day use still
need interactive/endurance verification.

## Reproduce

Close ClickLume normally, then run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bash -n install.sh uninstall.sh scripts/*.sh
python3 tests/install-regression.py
cargo build --workspace --release --locked
python3 tests/live-smoke.py
python3 tests/resource-smoke.py --seconds 60
python3 tests/resource-smoke.py --seconds 60 --gui
```

For a longer idle soak, increase `--seconds`. `--measure-only --bin-dir PATH`
measures an older build without applying the new resource-bound assertions.


## Delivery state

The user installation was updated after the audit. Installed GUI, backend and
CLI SHA-256 hashes match the tested release binaries. The app-menu launcher
was verified to run those installed binaries with clicking disabled. Existing
preferences and disabled autostart were preserved; system permission setup was
skipped. Source, regression checks and this report are published through the
repository's pull-request workflow.
