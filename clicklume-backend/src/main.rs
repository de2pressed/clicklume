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
const LEGACY_SOCKET_PATH: &str = "/tmp/autoclick.sock";

fn main() -> Result<()> {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    log::info!("Starting clicklume-backend v1.0");

    // No root required. /dev/uinput has ACL granting user access; we only
    // emit synthetic input — never read physical devices. See README.

    let _instance_lock = lock_instance()?;
    anyhow::ensure!(
        UnixStream::connect(SOCKET_PATH).is_err(),
        "A backend already owns the socket"
    );
    unsafe {
        libc::umask(0o077);
    }

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
    let jitter_ms = Arc::new(AtomicU32::new(cfg.jitter_ms.min(100)));
    let repeat_count = Arc::new(AtomicU32::new(cfg.repeat_count.min(1_000_000)));

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
    let auto_stop_state = clicker_state.clone();
    let auto_stop_gui_stream = gui_stream.clone();
    let clicker =
        clicker::Clicker::new(clicker_state.clone()).with_auto_stop_callback(Arc::new(move || {
            send_status_from_state_to_gui(&auto_stop_gui_stream, &auto_stop_state);
        }));
    clicker.start()?;

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
    let hotkeys_available = Arc::new(AtomicBool::new(hotkey_reader.has_devices()));

    // Spawn a drain thread: pulls HotkeyAction events, mutates shared state,
    // pushes Status + HotkeyPressed notifications to the GUI listener.
    let hotkey_drain_state = clicker_state.clone();
    let hotkey_drain_shutdown = shutdown.clone();
    let hotkey_drain_gui_stream = gui_stream.clone();
    let hotkey_cfg = cfg.clone();
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
                            let new_state = !hotkey_drain_state
                                .enabled
                                .fetch_xor(true, Ordering::Relaxed);
                            log::info!(
                                "Autoclick {} via in-process hotkey",
                                if new_state { "STARTED" } else { "STOPPED" }
                            );
                        }
                        hotkeys::HotkeyAction::Increase => {
                            let new_cps = change_cps(&hotkey_drain_state.cps, &hotkey_cfg, true);
                            log::info!("CPS increased to {} via in-process hotkey", new_cps);
                        }
                        hotkeys::HotkeyAction::Decrease => {
                            let new_cps = change_cps(&hotkey_drain_state.cps, &hotkey_cfg, false);
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

    // Remove existing socket files
    let _ = std::fs::remove_file(SOCKET_PATH);
    let _ = std::fs::remove_file(LEGACY_SOCKET_PATH);

    // Create Unix socket listener
    let socket_listener = UnixListener::bind(SOCKET_PATH)?;
    socket_listener.set_nonblocking(true)?;
    log::info!("Listening on Unix socket: {}", SOCKET_PATH);

    // Make socket accessible to non-root users
    std::fs::set_permissions(
        SOCKET_PATH,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;

    // Create legacy symlink for backward compatibility with existing tools / GNOME extensions
    let _ = std::os::unix::fs::symlink(SOCKET_PATH, LEGACY_SOCKET_PATH);

    // Shared slot for the long-lived GUI notification socket. Cleared by the
    // listener holder thread on EOF so a dead socket never blocks backend
    // pushes (which would otherwise spin or leak FDs).

    // Handle socket connections — one thread per accept so the GUI's
    // long-lived notification listener can't block one-shot CLI invocations.
    // Loop exits when shutdown flag is set (via Quit IPC or signal).
    while !shutdown.load(Ordering::Relaxed) {
        let available = hotkey_reader.has_devices();
        if hotkeys_available.swap(available, Ordering::Relaxed) != available {
            send_hotkey_availability(&gui_stream, available);
        }
        match socket_listener.accept() {
            Ok((mut stream, _addr)) => {
                log::info!("Connection received");
                let clicker_state_clone = clicker_state.clone();
                let gui_stream_clone = gui_stream.clone();
                let shutdown_clone = shutdown.clone();
                let connection_cfg = cfg.clone();
                let connection_hotkeys_available = hotkeys_available.clone();
                std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                    match read_command(&mut stream) {
                        Ok(buffer) => {
                            if let Err(e) = handle_connection_with_buffer(
                                &stream,
                                &buffer,
                                &clicker_state_clone,
                                gui_stream_clone,
                                &shutdown_clone,
                                &connection_cfg,
                                &connection_hotkeys_available,
                            ) {
                                log::debug!("Error handling connection: {}", e);
                            }
                        }
                        Err(e) => log::debug!("Command read failed: {}", e),
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

    // Remove socket files
    let _ = std::fs::remove_file(SOCKET_PATH);
    let _ = std::fs::remove_file(LEGACY_SOCKET_PATH);

    Ok(())
}

fn socket_identity(stream: &UnixStream) -> std::io::Result<u64> {
    use std::os::fd::AsRawFd;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(stream.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() }.st_ino)
}

fn send_hotkey_availability(gui_stream: &Arc<Mutex<Option<UnixStream>>>, available: bool) {
    let mut json = serde_json::to_vec(&BackendToGui::HotkeyAvailability(available)).unwrap();
    json.push(b'\n');
    if let Some(stream) = gui_stream.lock().unwrap().as_mut() {
        let _ = stream.write_all(&json);
    }
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
        repeat_count: state.repeat_count.load(Ordering::Relaxed),
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
                                log::debug!("GUI listener write failed: {}; clearing slot", e);
                                *guard = None;
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

/// Same as `send_status_from_state_to_gui` but writes to the requester's stream
/// and returns a Result so the IPC handler can propagate errors.
fn send_status_full(stream: &mut UnixStream, state: &ClickerState) -> Result<()> {
    let msg = BackendToGui::Status {
        enabled: state.enabled.load(Ordering::Relaxed),
        cps: state.cps.load(Ordering::Relaxed),
        button: state.button.lock().unwrap().clone(),
        mode: state.mode.lock().unwrap().as_str().to_string(),
        randomize: state.randomize.load(Ordering::Relaxed),
        jitter_ms: state.jitter_ms.load(Ordering::Relaxed),
        repeat_count: state.repeat_count.load(Ordering::Relaxed),
    };
    let mut json = serde_json::to_vec(&msg)?;
    json.push(b'\n');
    match stream.write_all(&json) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            // Fire-and-forget one-shot commands: requester closed its end before
            // we got the full payload out. Not an error from the backend's
            // perspective — the long-lived GUI listener receives its own
            // dedicated send_status_from_state_to_gui() write.
            log::debug!("Requester closed before full Status reply");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn handle_connection_with_buffer(
    stream: &UnixStream,
    buffer: &[u8],
    state: &ClickerState,
    gui_stream_for_hotkey: Arc<Mutex<Option<UnixStream>>>,
    shutdown: &Arc<AtomicBool>,
    cfg: &config::Config,
    hotkeys_available: &AtomicBool,
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
            state.enabled.store(true, Ordering::Relaxed);
            log::info!("Autoclick STARTED via GUI");
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::Stop => {
            state.enabled.store(false, Ordering::Relaxed);
            log::info!("Autoclick STOPPED via GUI");
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::Toggle => {
            let new_state = !state.enabled.fetch_xor(true, Ordering::Relaxed);
            log::info!(
                "Autoclick {} via IPC toggle",
                if new_state { "STARTED" } else { "STOPPED" }
            );
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::SetCps(new_cps) => {
            let new_cps = cfg.clamp_cps(new_cps);
            state.cps.store(new_cps, Ordering::Relaxed);
            log::info!("CPS set to {} via GUI", new_cps);
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::IncreaseCps => {
            let new_cps = change_cps(&state.cps, cfg, true);
            log::info!("CPS increased to {} via IPC", new_cps);
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::DecreaseCps => {
            let new_cps = change_cps(&state.cps, cfg, false);
            log::info!("CPS decreased to {} via IPC", new_cps);
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::SetButton(new_button) => {
            let new_button = match new_button.to_lowercase().as_str() {
                "right" => "right".to_string(),
                "middle" => "middle".to_string(),
                _ => "left".to_string(),
            };
            *state.button.lock().unwrap() = new_button.clone();
            log::info!("Button set to {} via GUI", new_button);
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::SetMode(new_mode) => {
            *state.mode.lock().unwrap() = ClickMode::from_str(&new_mode);
            log::info!("Click mode set to {} via GUI", new_mode);
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::SetRandomize(enabled_random) => {
            state.randomize.store(enabled_random, Ordering::Relaxed);
            log::info!("Randomize set to {} via GUI", enabled_random);
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::SetJitterMs(jitter) => {
            state.jitter_ms.store(jitter.min(100), Ordering::Relaxed);
            log::info!("Jitter set to {}ms via GUI", jitter);
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::SetRepeatCount(count) => {
            state
                .repeat_count
                .store(count.min(1_000_000), Ordering::Relaxed);
            log::info!("Repeat count set to {} via GUI", count);
            send_status_full(&mut stream, state)?;
            send_status_from_state_to_gui(&gui_stream_for_hotkey, state);
        }
        GuiToBackend::GetStatus => {
            log::debug!("Status requested via IPC");
            send_status_full(&mut stream, state)?;
        }
        GuiToBackend::SubscribeGui => {
            log::info!("GUI notification subscription received");
            let _ = stream.set_read_timeout(None);
            let subscription_id = socket_identity(&stream)?;
            let cloned_for_writes = stream.try_clone()?;
            {
                let mut guard = gui_stream_for_hotkey.lock().unwrap();
                if guard.is_some() {
                    return Err(anyhow::anyhow!("A GUI is already subscribed"));
                }
                *guard = Some(cloned_for_writes);
            }
            send_hotkey_availability(
                &gui_stream_for_hotkey,
                hotkeys_available.load(Ordering::Relaxed),
            );
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
                if guard.as_ref().and_then(|s| socket_identity(s).ok()) == Some(subscription_id) {
                    let _ = guard.take();
                }
            }
            log::info!("GUI listener disconnected; cleared slot");
            return Ok(());
        }
        GuiToBackend::Quit => {
            log::info!("Quit requested via GUI");
            // send_status may fail with broken pipe if the GUI already
            // closed its end after sending Quit — that's fine, the GUI is
            // exiting too. Ignore the result and proceed with shutdown.
            let _ = send_status_full(&mut stream, state);
            shutdown.store(true, Ordering::Relaxed);
            let _ = std::fs::remove_file(SOCKET_PATH);
            return Ok(());
        }
    }

    Ok(())
}

/// Serialize backend startup so a second process cannot unlink a live socket.
fn lock_instance() -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let uid = unsafe { libc::geteuid() };
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/tmp".into());
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(base.join(format!("clicklume-{uid}.lock")))?;
    use std::os::unix::fs::MetadataExt;
    anyhow::ensure!(
        file.metadata()?.uid() == uid,
        "Backend lock belongs to another user"
    );
    anyhow::ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another backend is already running"
    );
    Ok(file)
}

/// Unix streams may split even tiny JSON commands across multiple reads.
fn read_command(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut buffer = Vec::new();
    loop {
        anyhow::ensure!(buffer.len() < 4096, "Command exceeds 4096 bytes");
        anyhow::ensure!(std::time::Instant::now() < deadline, "Command timed out");
        let mut chunk = [0; 256];
        let n = stream.read(&mut chunk)?;
        anyhow::ensure!(n > 0, "Connection closed before a complete command");
        buffer.extend_from_slice(&chunk[..n]);
        match serde_json::from_slice::<GuiToBackend>(&buffer) {
            Ok(_) => return Ok(buffer),
            Err(e) if e.is_eof() => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

fn change_cps(cps: &AtomicU32, cfg: &config::Config, increase: bool) -> u32 {
    let next = |current: u32| {
        let step = config::Config::get_cps_step(if increase {
            current
        } else {
            current.saturating_sub(1)
        });
        cfg.clamp_cps(if increase {
            current.saturating_add(step)
        } else {
            current.saturating_sub(step)
        })
    };
    let previous = cps
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(next(current))
        })
        .unwrap();
    next(previous)
}

#[cfg(test)]
mod ipc_tests {
    use super::*;
    #[test]
    fn concurrent_cps_updates_respect_bounds_and_decrease_once() {
        let cfg = config::Config {
            max_cps: 100,
            ..Default::default()
        };
        let cps = Arc::new(AtomicU32::new(20));
        assert_eq!(change_cps(&cps, &cfg, false), 19);
        cps.store(1, Ordering::Relaxed);
        let workers: Vec<_> = (0..10)
            .map(|_| {
                let cps = cps.clone();
                let cfg = cfg.clone();
                std::thread::spawn(move || change_cps(&cps, &cfg, true))
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(cps.load(Ordering::Relaxed), 11);
        cps.store(100, Ordering::Relaxed);
        assert_eq!(change_cps(&cps, &cfg, true), 100);
    }

    #[test]
    fn fragmented_command_is_read_completely() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        let sender = std::thread::spawn(move || {
            writer.write_all(b"{\"SetCps\":").unwrap();
            std::thread::sleep(Duration::from_millis(20));
            writer.write_all(b"42}").unwrap();
        });
        assert!(matches!(
            serde_json::from_slice::<GuiToBackend>(&read_command(&mut reader).unwrap()).unwrap(),
            GuiToBackend::SetCps(42)
        ));
        sender.join().unwrap();
    }
}
