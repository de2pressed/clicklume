//! clicklume-backend - The autoclicker backend service
//! Listens on a Unix socket for commands from the GUI

mod clicker;
mod config;
mod hotkeys;

use anyhow::Result;
use clicker::{ClickMode, ClickerState};
use common::{BackendToGui, GuiToBackend};
use evdev::KeyCode;
#[allow(unused_imports)]
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Socket path for communication
const SOCKET_PATH: &str = "/tmp/clicklume.sock";

fn main() -> Result<()> {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    log::info!("Starting clicklume-backend v1.0");

    // No root required. /dev/uinput has ACL granting user access; we only
    // emit synthetic input — never read physical devices. See README.

    // Load configuration
    let cfg = config::Config::load()?;
    log::info!("Configuration loaded successfully");

    // Shared shutdown flag: the Quit handler sets it, the accept loop observes
    // it between iterations. This lets main() return normally (running Drop on
    // Clicker/clicker thread + uinput device) instead of process::exit().
    let shutdown = Arc::new(AtomicBool::new(false));

    // Create shared state
    let enabled = Arc::new(AtomicBool::new(false));
    let cps = Arc::new(AtomicU32::new(cfg.clamp_cps(cfg.default_cps)));
    let button = Arc::new(Mutex::new(cfg.button.clone()));
    let mode = Arc::new(Mutex::new(ClickMode::from_str(&cfg.mode)));
    let randomize = Arc::new(AtomicBool::new(cfg.randomize));
    let jitter_ms = Arc::new(AtomicU32::new(cfg.jitter_ms));
    let repeat_count = Arc::new(AtomicU32::new(cfg.repeat_count));

    // Create clicker state
    let clicker_state = ClickerState {
        enabled: enabled.clone(),
        cps: cps.clone(),
        button: button.clone(),
        mode: mode.clone(),
        randomize: randomize.clone(),
        jitter_ms: jitter_ms.clone(),
        repeat_count: repeat_count.clone(),
    };

    // (gui_stream declared at the very top so the hotkey drain and accept loop
    // share the same Arc.)
    let gui_stream: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));

    // Create and start clicker
    let clicker = clicker::Clicker::new(clicker_state.clone());
    clicker.start();

    // Start the in-process hotkey reader (replaces gsd-media-keys). The reader
    // passively reads configured keyboard devices and pushes HotkeyAction events on a
    // channel. A drain thread below converts those to state mutations and
    // pushes Status notifications to the GUI.
    let toggle_kc = hotkeys::parse_keycode(&cfg.toggle).unwrap_or(KeyCode::KEY_F6);
    let increase_kc = hotkeys::parse_keycode(&cfg.increase).unwrap_or(KeyCode::KEY_F7);
    let decrease_kc = hotkeys::parse_keycode(&cfg.decrease).unwrap_or(KeyCode::KEY_F8);
    let quit_kc = hotkeys::parse_keycode(&cfg.quit).unwrap_or(KeyCode::KEY_F9);
    log::info!(
        "Hotkey bindings: toggle={:?} increase={:?} decrease={:?} quit={:?}",
        toggle_kc,
        increase_kc,
        decrease_kc,
        quit_kc
    );
    let (hotkey_reader, hotkey_rx) =
        match hotkeys::HotkeyReader::start(toggle_kc, increase_kc, decrease_kc, quit_kc) {
            Ok(pair) => pair,
            Err(e) => {
                log::error!(
                "Failed to start in-process hotkey reader: {}. Falling back to IPC-only hotkeys.",
                e
            );
                // Return a dummy reader + disconnected channel so the rest of
                // the code can stay uniform. We never receive anything on it.
                return Err(e);
            }
        };
    // Keep hotkey_reader alive for the whole backend lifetime.
    let _hotkey_reader_keepalive = hotkey_reader;

    // Spawn a drain thread: pulls HotkeyAction events, mutates shared state,
    // pushes Status + HotkeyPressed notifications to the GUI listener.
    let hotkey_drain_state = clicker_state.clone();
    let hotkey_drain_shutdown = shutdown.clone();
    let hotkey_drain_gui_stream = gui_stream.clone();
    std::thread::spawn(move || {
        log::info!("Hotkey drain thread started");
        while !hotkey_drain_shutdown.load(Ordering::Relaxed) {
            match hotkey_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(action) => {
                    let action_str = match action {
                        hotkeys::HotkeyAction::Toggle => "toggle",
                        hotkeys::HotkeyAction::Increase => "increase",
                        hotkeys::HotkeyAction::Decrease => "decrease",
                        hotkeys::HotkeyAction::Quit => "quit",
                    };
                    // Apply the action to shared state. We duplicate the
                    // logic that IPC handlers do so the GUI sees consistent
                    // state via the push below.
                    match action {
                        hotkeys::HotkeyAction::Toggle => {
                            let new_state = !hotkey_drain_state.enabled.load(Ordering::Relaxed);
                            hotkey_drain_state
                                .enabled
                                .store(new_state, Ordering::Relaxed);
                            log::info!(
                                "Autoclick {} via in-process hotkey",
                                if new_state { "STARTED" } else { "STOPPED" }
                            );
                        }
                        hotkeys::HotkeyAction::Increase => {
                            let current = hotkey_drain_state.cps.load(Ordering::Relaxed);
                            let step = config::Config::get_cps_step(current);
                            let new_cps = (current + step).min(1000);
                            hotkey_drain_state.cps.store(new_cps, Ordering::Relaxed);
                            log::info!("CPS increased to {} via in-process hotkey", new_cps);
                        }
                        hotkeys::HotkeyAction::Decrease => {
                            let current = hotkey_drain_state
                                .cps
                                .load(Ordering::Relaxed)
                                .saturating_sub(1);
                            let step = config::Config::get_cps_step(current);
                            let new_cps = current.saturating_sub(step).max(1);
                            hotkey_drain_state.cps.store(new_cps, Ordering::Relaxed);
                            log::info!("CPS decreased to {} via in-process hotkey", new_cps);
                        }
                        hotkeys::HotkeyAction::Quit => {
                            log::info!("Quit requested via in-process hotkey");
                            hotkey_drain_shutdown.store(true, Ordering::Relaxed);
                        }
                    }
                    // Mirror to GUI listener so the window updates without
                    // the user touching it. The IPC path already does this
                    // on every command, so we replicate it here.
                    send_status_from_state_to_gui(&hotkey_drain_gui_stream, &hotkey_drain_state);
                    send_hotkey_notification(&hotkey_drain_gui_stream, action_str);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    log::info!("Hotkey channel disconnected; drain thread exiting");
                    break;
                }
            }
        }
    });

    // Remove existing socket file
    let _ = std::fs::remove_file(SOCKET_PATH);

    // Create Unix socket listener
    let socket_listener = UnixListener::bind(SOCKET_PATH)?;
    socket_listener.set_nonblocking(true)?;
    log::info!("Listening on Unix socket: {}", SOCKET_PATH);

    // Make socket accessible to non-root users
    std::fs::set_permissions(
        SOCKET_PATH,
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    )?;

    // Shared slot for the long-lived GUI notification socket. Cleared by the
    // listener holder thread on EOF so a dead socket never blocks backend
    // pushes (which would otherwise spin or leak FDs).

    // Handle socket connections — one thread per accept so the GUI's
    // long-lived notification listener can't block one-shot CLI invocations.
    // Loop exits when shutdown flag is set (via Quit IPC or signal).
    while !shutdown.load(Ordering::Relaxed) {
        match socket_listener.accept() {
            Ok((mut stream, _addr)) => {
                log::info!("Connection received");
                let enabled = enabled.clone();
                let cps = cps.clone();
                let button = button.clone();
                let mode = mode.clone();
                let randomize = randomize.clone();
                let jitter_ms = jitter_ms.clone();
                let repeat_count = repeat_count.clone();
                let gui_stream_clone = gui_stream.clone();
                let shutdown_clone = shutdown.clone();
                std::thread::spawn(move || {
                    // Detect listener vs command: set a short read timeout.
                    // If the read times out (no data arrived), this is the
                    // GUI's long-lived notification listener — register it.
                    // Otherwise the read returns the command bytes and we
                    // handle the command normally.
                    use std::io::Read;
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
                    let mut buf = [0u8; 1024];
                    match stream.read(&mut buf) {
                        Ok(0) => {
                            log::debug!("Connection closed before sending a command; treating as health probe");
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            log::debug!("Connection sent no command within probe window; closing");
                        }
                        Err(e) => {
                            log::warn!("Connection probe read failed: {}", e);
                        }
                        Ok(n) => {
                            // Data arrived — parse as command. First, rewrite
                            // the buffer so handle_connection can re-read it
                            // (we already consumed the bytes via read above).
                            // Simpler: call a variant that accepts the pre-read
                            // buffer directly.
                            if let Err(e) = handle_connection_with_buffer(
                                &stream,
                                &buf[..n],
                                &enabled,
                                &cps,
                                &button,
                                &mode,
                                &randomize,
                                &jitter_ms,
                                &repeat_count,
                                gui_stream_clone,
                                &shutdown_clone,
                            ) {
                                log::error!("Error handling connection: {}", e);
                            }
                        }
                    }
                    log::info!("Connection closed");
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                log::error!("Connection error: {}", e);
            }
        }
    }

    // Cleanup
    log::info!("Shutting down...");
    clicker.stop();

    // Remove socket file
    let _ = std::fs::remove_file(SOCKET_PATH);

    Ok(())
}

fn send_hotkey_notification(gui_stream: &Arc<Mutex<Option<UnixStream>>>, action: &str) {
    let msg = BackendToGui::HotkeyPressed(action.to_string());
    if let Ok(mut json) = serde_json::to_vec(&msg) {
        json.push(b'\n');
        if let Ok(mut guard) = gui_stream.lock() {
            if let Some(ref mut stream) = *guard {
                let _ = stream.write_all(&json);
            }
        }
    }
}

/// Push a full Status update from a ClickerState to the registered GUI listener.
fn send_status_from_state_to_gui(
    gui_stream: &Arc<Mutex<Option<UnixStream>>>,
    state: &ClickerState,
) {
    let msg = BackendToGui::Status {
        enabled: state.enabled.load(Ordering::Relaxed),
        cps: state.cps.load(Ordering::Relaxed),
        button: state.button.lock().unwrap().clone(),
        mode: state.mode.lock().unwrap().as_str().to_string(),
        randomize: state.randomize.load(Ordering::Relaxed),
        jitter_ms: state.jitter_ms.load(Ordering::Relaxed),
    };
    match serde_json::to_vec(&msg) {
        Ok(mut json) => {
            json.push(b'\n');
            match gui_stream.lock() {
                Ok(mut guard) => {
                    if let Some(ref mut stream) = *guard {
                        match stream.write_all(&json) {
                            Ok(_) => log::info!(
                                "Pushed full Status to GUI listener ({} bytes)",
                                json.len()
                            ),
                            Err(e) => {
                                if e.kind() == std::io::ErrorKind::BrokenPipe
                                    || e.kind() == std::io::ErrorKind::ConnectionReset
                                {
                                    log::debug!(
                                        "GUI listener push: connection gone ({}); clearing slot",
                                        e
                                    );
                                    *guard = None;
                                } else {
                                    log::warn!("Failed to push full Status to GUI listener: {}", e);
                                }
                            }
                        }
                    } else {
                        log::info!("No GUI listener registered; skipping full Status push");
                    }
                }
                Err(e) => log::error!("Failed to lock gui_stream mutex: {}", e),
            }
        }
        Err(e) => log::error!("Failed to serialize full Status for GUI listener: {}", e),
    }
}

/// Status push that includes mode/randomize/jitter. Used by SetMode /
/// SetRandomize / SetJitterMs handlers so the GUI's persistent state stays
/// in sync after every change.
#[allow(clippy::too_many_arguments)]
fn send_status_full_to_gui(
    gui_stream: &Arc<Mutex<Option<UnixStream>>>,
    enabled: &Arc<AtomicBool>,
    cps: &Arc<AtomicU32>,
    button: &Arc<Mutex<String>>,
    mode: &Arc<Mutex<ClickMode>>,
    randomize: &Arc<AtomicBool>,
    jitter_ms: &Arc<AtomicU32>,
) {
    let msg = BackendToGui::Status {
        enabled: enabled.load(Ordering::Relaxed),
        cps: cps.load(Ordering::Relaxed),
        button: button.lock().unwrap().clone(),
        mode: mode.lock().unwrap().as_str().to_string(),
        randomize: randomize.load(Ordering::Relaxed),
        jitter_ms: jitter_ms.load(Ordering::Relaxed),
    };
    match serde_json::to_vec(&msg) {
        Ok(mut json) => {
            json.push(b'\n');
            if let Ok(mut guard) = gui_stream.lock() {
                if let Some(ref mut stream) = *guard {
                    let _ = stream.write_all(&json);
                }
            }
        }
        Err(e) => log::error!("Failed to serialize full Status: {}", e),
    }
}

/// Same as `send_status_full_to_gui` but writes to the requester's stream
/// and returns a Result so the IPC handler can propagate errors.
#[allow(clippy::too_many_arguments)]
fn send_status_full(
    stream: &mut UnixStream,
    enabled: &Arc<AtomicBool>,
    cps: &Arc<AtomicU32>,
    button: &Arc<Mutex<String>>,
    mode: &Arc<Mutex<ClickMode>>,
    randomize: &Arc<AtomicBool>,
    jitter_ms: &Arc<AtomicU32>,
) -> Result<()> {
    let msg = BackendToGui::Status {
        enabled: enabled.load(Ordering::Relaxed),
        cps: cps.load(Ordering::Relaxed),
        button: button.lock().unwrap().clone(),
        mode: mode.lock().unwrap().as_str().to_string(),
        randomize: randomize.load(Ordering::Relaxed),
        jitter_ms: jitter_ms.load(Ordering::Relaxed),
    };
    let mut json = serde_json::to_vec(&msg)?;
    json.push(b'\n');
    match stream.write_all(&json) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            // Fire-and-forget one-shot commands: requester closed its end before
            // we got the full payload out. Not an error from the backend's
            // perspective — the long-lived GUI listener receives its own
            // dedicated send_status_full_to_gui() write.
            log::debug!("Requester closed before full Status reply");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_connection_with_buffer(
    stream: &UnixStream,
    buffer: &[u8],
    enabled: &Arc<AtomicBool>,
    cps: &Arc<AtomicU32>,
    button: &Arc<Mutex<String>>,
    mode: &Arc<Mutex<ClickMode>>,
    randomize: &Arc<AtomicBool>,
    jitter_ms: &Arc<AtomicU32>,
    repeat_count: &Arc<AtomicU32>,
    gui_stream_for_hotkey: Arc<Mutex<Option<UnixStream>>>,
    shutdown: &Arc<AtomicBool>,
) -> Result<()> {
    let mut stream = stream.try_clone()?;

    // Parse message
    let msg: GuiToBackend = match serde_json::from_slice(buffer) {
        Ok(msg) => msg,
        Err(e) => {
            log::error!("Failed to parse message: {}", e);
            return Ok(());
        }
    };

    log::debug!("Received message: {:?}", msg);

    // Handle message
    match msg {
        GuiToBackend::Start => {
            enabled.store(true, Ordering::Relaxed);
            log::info!("Autoclick STARTED via GUI");
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::Stop => {
            enabled.store(false, Ordering::Relaxed);
            log::info!("Autoclick STOPPED via GUI");
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::Toggle => {
            let new_state = !enabled.load(Ordering::Relaxed);
            enabled.store(new_state, Ordering::Relaxed);
            log::info!(
                "Autoclick {} via IPC toggle",
                if new_state { "STARTED" } else { "STOPPED" }
            );
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::SetCps(new_cps) => {
            let new_cps = new_cps.clamp(1, 1000);
            cps.store(new_cps, Ordering::Relaxed);
            log::info!("CPS set to {} via GUI", new_cps);
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::IncreaseCps => {
            let current = cps.load(Ordering::Relaxed);
            let step = config::Config::get_cps_step(current);
            let new_cps = (current + step).min(1000);
            cps.store(new_cps, Ordering::Relaxed);
            log::info!("CPS increased to {} via IPC", new_cps);
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::DecreaseCps => {
            let current = cps.load(Ordering::Relaxed);
            let step = config::Config::get_cps_step(current.saturating_sub(1));
            let new_cps = current.saturating_sub(step).max(1);
            cps.store(new_cps, Ordering::Relaxed);
            log::info!("CPS decreased to {} via IPC", new_cps);
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::SetButton(new_button) => {
            let new_button = match new_button.to_lowercase().as_str() {
                "right" => "right".to_string(),
                "middle" => "middle".to_string(),
                _ => "left".to_string(),
            };
            *button.lock().unwrap() = new_button.clone();
            log::info!("Button set to {} via GUI", new_button);
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::SetMode(new_mode) => {
            *mode.lock().unwrap() = ClickMode::from_str(&new_mode);
            log::info!("Click mode set to {} via GUI", new_mode);
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::SetRandomize(enabled_random) => {
            randomize.store(enabled_random, Ordering::Relaxed);
            log::info!("Randomize set to {} via GUI", enabled_random);
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::SetJitterMs(jitter) => {
            jitter_ms.store(jitter.min(100), Ordering::Relaxed);
            log::info!("Jitter set to {}ms via GUI", jitter);
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::SetRepeatCount(count) => {
            repeat_count.store(count.min(1_000_000), Ordering::Relaxed);
            log::info!("Repeat count set to {} via GUI", count);
            send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            )?;
            send_status_full_to_gui(
                &gui_stream_for_hotkey,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
        }
        GuiToBackend::SubscribeGui => {
            log::info!("GUI notification subscription received");
            let _ = stream.set_read_timeout(None);
            let cloned_for_writes = stream.try_clone()?;
            *gui_stream_for_hotkey.lock().unwrap() = Some(cloned_for_writes);
            log::info!("Registered GUI notification listener");

            // Keep this handler alive until the GUI disconnects. Backend status
            // pushes use the cloned stream stored above; this original stream is
            // just the lifetime guard for the subscription.
            let mut sink = [0u8; 1024];
            while let Ok(n) = stream.read(&mut sink) {
                if n == 0 {
                    break;
                }
            }
            if let Ok(mut guard) = gui_stream_for_hotkey.lock() {
                let _ = guard.take();
            }
            log::info!("GUI listener disconnected; cleared slot");
            return Ok(());
        }
        GuiToBackend::Quit => {
            log::info!("Quit requested via GUI");
            // send_status may fail with broken pipe if the GUI already
            // closed its end after sending Quit — that's fine, the GUI is
            // exiting too. Ignore the result and proceed with shutdown.
            let _ = send_status_full(
                &mut stream,
                enabled,
                cps,
                button,
                mode,
                randomize,
                jitter_ms,
            );
            // Quit is handled on a per-connection thread. Exiting immediately
            // avoids a window where the socket is gone but the process is
            // still winding down, which can strand the GUI's release event
            // loop between its presence and reaping checks. The kernel closes
            // all uinput/socket descriptors on process exit, so the virtual
            // device is still released deterministically.
            shutdown.store(true, Ordering::Relaxed);
            let _ = std::fs::remove_file(SOCKET_PATH);
            std::process::exit(0);
        }
    }

    Ok(())
}
