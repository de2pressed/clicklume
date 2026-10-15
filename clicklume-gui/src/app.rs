//! App state + lifecycle (backend spawn, listener thread, signal
//! handler, hotkey handling, persistent settings wiring).

#![allow(function_casts_as_integer)] // required by libc signal-handler registration

use crate::ipc;
use crate::state::{self, PersistentState};
use common::{BackendToGui, GuiToBackend};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SOCKET_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SOCKET_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const CPS_DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeySettings {
    #[serde(default = "default_toggle_hotkey")]
    pub toggle: String,
    #[serde(default = "default_increase_hotkey")]
    pub increase: String,
    #[serde(default = "default_decrease_hotkey")]
    pub decrease: String,
    #[serde(default = "default_quit_hotkey")]
    pub quit: String,
}

fn default_toggle_hotkey() -> String {
    "KEY_F6".to_string()
}

fn default_increase_hotkey() -> String {
    "KEY_F7".to_string()
}

fn default_decrease_hotkey() -> String {
    "KEY_F8".to_string()
}

fn default_quit_hotkey() -> String {
    "KEY_F9".to_string()
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            toggle: default_toggle_hotkey(),
            increase: default_increase_hotkey(),
            decrease: default_decrease_hotkey(),
            quit: default_quit_hotkey(),
        }
    }
}

fn parse_egui_key(name: &str) -> Option<egui::Key> {
    let normalized = name.trim().to_uppercase();
    let stripped = normalized.strip_prefix("KEY_").unwrap_or(&normalized);
    match stripped {
        "F1" => Some(egui::Key::F1),
        "F2" => Some(egui::Key::F2),
        "F3" => Some(egui::Key::F3),
        "F4" => Some(egui::Key::F4),
        "F5" => Some(egui::Key::F5),
        "F6" => Some(egui::Key::F6),
        "F7" => Some(egui::Key::F7),
        "F8" => Some(egui::Key::F8),
        "F9" => Some(egui::Key::F9),
        "F10" => Some(egui::Key::F10),
        "F11" => Some(egui::Key::F11),
        "F12" => Some(egui::Key::F12),
        _ => None,
    }
}

fn config_path() -> std::path::PathBuf {
    config_dir().join("config.toml")
}

fn config_dir() -> PathBuf {
    config_base().join("clicklume")
}

fn legacy_config_path() -> PathBuf {
    config_base().join("autoclick").join("config.toml")
}

fn config_base() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn backend_binary() -> std::io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("CLICKLUME_BACKEND") {
        return Ok(PathBuf::from(path));
    }
    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "GUI executable has no parent directory",
        )
    })?;
    Ok(directory.join("clicklume-backend"))
}

fn load_hotkeys() -> HotkeySettings {
    let path = config_path();
    let source = if path.exists() {
        path
    } else {
        legacy_config_path()
    };
    match std::fs::read_to_string(source) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => HotkeySettings::default(),
    }
}

fn save_hotkeys(settings: &HotkeySettings) -> std::io::Result<()> {
    let path = config_path();
    let mut value = std::fs::read_to_string(&path)
        .or_else(|_| std::fs::read_to_string(legacy_config_path()))
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .unwrap_or_else(|| toml::Value::Table(Default::default()));

    if let toml::Value::Table(table) = &mut value {
        table.insert(
            "toggle".into(),
            toml::Value::String(settings.toggle.clone()),
        );
        table.insert(
            "increase".into(),
            toml::Value::String(settings.increase.clone()),
        );
        table.insert(
            "decrease".into(),
            toml::Value::String(settings.decrease.clone()),
        );
        table.insert("quit".into(), toml::Value::String(settings.quit.clone()));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(&value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    common::write_atomic(&path, text.as_bytes())
}

// ---------------------------------------------------------------------------
// Self-pipe signal handler (SIGTERM/SIGINT -> graceful Quit)
// ---------------------------------------------------------------------------

static SIG_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn sigterm_handler(_sig: i32) {
    let fd = SIG_PIPE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        let b: u8 = 1;
        unsafe {
            libc::write(fd, &b as *const u8 as *const libc::c_void, 1);
        }
    }
}

fn install_signal_handler(ctx: egui::Context) -> Receiver<()> {
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    let mut pipefds = [0i32; 2];
    let r = unsafe { libc::pipe2(pipefds.as_mut_ptr(), libc::O_CLOEXEC) };
    if r != 0 {
        log::warn!("Failed to create self-pipe for signal handling");
        return rx;
    }
    unsafe {
        let flags = libc::fcntl(pipefds[1], libc::F_GETFL);
        libc::fcntl(pipefds[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
        SIG_PIPE_WRITE.store(pipefds[1], Ordering::Relaxed);
        libc::signal(
            libc::SIGTERM,
            sigterm_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            sigterm_handler as *const () as libc::sighandler_t,
        );
    }
    let read_fd = pipefds[0];
    thread::spawn(move || {
        use std::os::fd::FromRawFd;
        let mut f = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let mut buf = [0u8; 16];
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if matches!(tx.try_send(()), Err(mpsc::TrySendError::Disconnected(_))) {
                        break;
                    }
                    ctx.request_repaint();
                }
                Err(_) => break,
            }
        }
    });
    rx
}

// ---------------------------------------------------------------------------
// systemd --user autostart (WantedBy=graphical-session.target)
// ---------------------------------------------------------------------------

const USER_SYSTEMCTL: &str = "systemctl";
const UNIT_NAME: &str = "clicklume.service";

/// Enable or disable the ClickLume user service. Equivalent to
///   systemctl --user (enable|disable) clicklume.service
/// but without depending on XDG_RUNTIME_DIR being set; spawns systemctl
/// with --user and lets systemd log via dbus. Errors are logged but
/// otherwise ignored — the GUI must still run even if the service file
/// is missing (e.g., fresh install).
fn apply_autostart(enable: bool) -> bool {
    let verb = if enable { "enable" } else { "disable" };
    match std::process::Command::new(USER_SYSTEMCTL)
        .args(["--user", verb, UNIT_NAME])
        .output()
    {
        Ok(out) if out.status.success() => {
            log::info!("Autostart {}: applied", verb);
            return true;
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            log::warn!(
                "Autostart {}: systemctl exited with {:?}: {}",
                verb,
                out.status.code(),
                stderr.trim()
            );
        }
        Err(e) => log::warn!("Autostart {}: failed to spawn systemctl: {}", verb, e),
    }
    false
    // Re-evaluate WantedBy= so a disabled service actually stops being
    // pulled in next login. `disable` does this by symlink removal;
    // `enable` recreates it.
}

// ---------------------------------------------------------------------------
// Backend notification listener thread
// ---------------------------------------------------------------------------

enum GuiEvent {
    Backend(BackendToGui),
    Disconnected,
}

type GuiInbox = Arc<Mutex<VecDeque<GuiEvent>>>;

fn queue_event(inbox: &GuiInbox, event: GuiEvent) {
    let mut queue = inbox.lock().unwrap();
    // One latest event per protocol variant (at most six slots), even when
    // Wayland defers a minimized window's event loop for hours.
    queue.retain(|old| match (old, &event) {
        (GuiEvent::Backend(a), GuiEvent::Backend(b)) => {
            std::mem::discriminant(a) != std::mem::discriminant(b)
        }
        (GuiEvent::Disconnected, GuiEvent::Disconnected) => false,
        _ => true,
    });
    queue.push_back(event);
}

fn read_notification(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
) -> std::io::Result<Option<BackendToGui>> {
    const MAX_NOTIFICATION_BYTES: u64 = 4096;
    line.clear();
    let count = reader
        .take(MAX_NOTIFICATION_BYTES + 1)
        .read_until(b'\n', line)?;
    if count == 0 {
        return Ok(None);
    }
    if count as u64 > MAX_NOTIFICATION_BYTES || !line.ends_with(b"\n") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Oversized or incomplete backend notification",
        ));
    }
    serde_json::from_slice(line)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn sync_persisted_backend_settings() {
    let settings = state::current_snapshot();
    let commands = [
        GuiToBackend::SetMode(settings.mode),
        GuiToBackend::SetButton(settings.button),
        GuiToBackend::SetCps(settings.cps),
        GuiToBackend::SetRandomize(settings.randomize),
        GuiToBackend::SetJitterMs(settings.jitter_ms),
        GuiToBackend::SetRepeatCount(settings.repeat_count),
    ];
    for command in commands {
        if let Err(error) = ipc::send(command) {
            log::warn!(
                "Could not restore settings after backend restart: {}",
                error
            );
            break;
        }
    }
}

fn spawn_supervised_backend(backend_pid: &AtomicU32) -> Option<std::process::Child> {
    if backend_pid
        .compare_exchange(0, u32::MAX, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }
    let binary = match backend_binary() {
        Ok(path) => path,
        Err(error) => {
            log::error!("Could not resolve backend for listener restart: {}", error);
            backend_pid.store(0, Ordering::Release);
            return None;
        }
    };
    match std::process::Command::new(&binary)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .process_group(0)
        .spawn()
    {
        Ok(mut child) => {
            backend_pid.store(child.id(), Ordering::Release);
            let deadline = Instant::now() + SOCKET_WAIT_TIMEOUT;
            while Instant::now() < deadline {
                if UnixStream::connect(ipc::SOCKET_PATH).is_ok() {
                    sync_persisted_backend_settings();
                    return Some(child);
                }
                thread::sleep(Duration::from_millis(50));
            }
            log::error!("Listener-spawned backend did not open its socket in time");
            let _ = child.kill();
            let _ = child.wait();
            backend_pid.store(0, Ordering::Release);
            None
        }
        Err(error) => {
            log::error!("Listener could not restart backend: {}", error);
            backend_pid.store(0, Ordering::Release);
            None
        }
    }
}

fn listen_for_notifications(
    inbox: GuiInbox,
    ctx: egui::Context,
    backend_pid: Arc<AtomicU32>,
    shutting_down: Arc<AtomicBool>,
    global_hotkeys_available: Arc<AtomicBool>,
    save_tx: SyncSender<()>,
    mut supervised_child: Option<std::process::Child>,
) {
    let mut backoff_ms: u64 = 100;
    loop {
        // This thread owns and reaps every backend Child, including the first
        // startup. Recovery never depends on a native window repaint.
        if let Some(child) = supervised_child.as_mut() {
            let exited = match child.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(error) => {
                    log::warn!("Could not reap backend: {}", error);
                    error.raw_os_error() == Some(libc::ECHILD)
                }
            };
            if exited {
                backend_pid.store(0, Ordering::Release);
                supervised_child = None;
            }
        }
        if let Ok(mut stream) = UnixStream::connect(ipc::SOCKET_PATH) {
            let subscribe = serde_json::to_vec(&GuiToBackend::SubscribeGui)
                .expect("SubscribeGui must serialize");
            let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
            if let Err(e) = stream.write_all(&subscribe) {
                log::warn!("GUI listener failed to subscribe: {}", e);
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(2000);
                continue;
            }

            log::info!("GUI listener subscribed to backend");
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            loop {
                match read_notification(&mut reader, &mut line) {
                    Ok(None) => {
                        log::info!("GUI listener EOF");
                        break;
                    }
                    Ok(Some(msg)) => {
                        backoff_ms = 100;
                        log::debug!("GUI listener parsed: {:?}", msg);
                        match &msg {
                            BackendToGui::HotkeyAvailability(available) => {
                                global_hotkeys_available.store(*available, Ordering::Release);
                            }
                            BackendToGui::HotkeyPressed(action) if action == "quit" => {
                                shutting_down.store(true, Ordering::Release);
                                state::flush();
                                // GNOME can defer an idle/minimized Wayland window's
                                // next native event-loop pass indefinitely, so a
                                // ViewportCommand::Close is not reliable here. The
                                // backend has already persisted its final Status and
                                // started shutdown. Exit successfully so systemd's
                                // Restart=on-failure does not reopen the application.
                                // Do not run process-wide exit handlers from this
                                // background thread: graphics teardown races the
                                // renderer and can fault. POSIX _exit is atomic and
                                // the kernel owns all remaining resource cleanup.
                                unsafe { libc::_exit(0) };
                            }
                            BackendToGui::Status {
                                cps,
                                button,
                                mode,
                                randomize,
                                jitter_ms,
                                repeat_count,
                                ..
                            } => state::merge_backend_settings(
                                &save_tx,
                                *cps,
                                button.clone(),
                                mode.clone(),
                                *randomize,
                                *jitter_ms,
                                *repeat_count,
                            ),
                            _ => {}
                        }
                        queue_event(&inbox, GuiEvent::Backend(msg));
                        ctx.request_repaint();
                    }
                    Err(e) => {
                        log::warn!("GUI listener read error: {}", e);
                        break;
                    }
                }
            }
            global_hotkeys_available.store(false, Ordering::Release);
            queue_event(&inbox, GuiEvent::Disconnected);
            ctx.request_repaint();
        } else if supervised_child.is_none() && !shutting_down.load(Ordering::Acquire) {
            supervised_child = spawn_supervised_backend(&backend_pid);
        }
        // Retry only while disconnected, with bounded exponential backoff.
        ctx.request_repaint();
        thread::sleep(Duration::from_millis(backoff_ms));
        backoff_ms = (backoff_ms * 2).min(2000);
    }
}

// ---------------------------------------------------------------------------
// App struct
// ---------------------------------------------------------------------------

pub struct App {
    pub enabled: bool,
    pub cps: u32,
    pub button: String,
    pub mode: String,
    pub randomize: bool,
    pub jitter_ms: u32,
    pub repeat_count: u32,
    /// Whether the ClickLume user service is enabled on login.
    pub autostart: bool,
    pub dark_mode: bool,
    pub theme_refresh_needed: bool,
    pub backend_present: bool,
    pub hotkeys: HotkeySettings,
    pub hotkey_draft: HotkeySettings,
    pub show_hotkey_settings: bool,
    pub last_error: Option<String>,

    inbox: GuiInbox,
    sig_rx: Option<Receiver<()>>,
    backend_child: Option<std::process::Child>,
    backend_pid: Arc<AtomicU32>,
    shutting_down: Arc<AtomicBool>,
    global_hotkeys_available: Arc<AtomicBool>,
    last_socket_check: Instant,

    persistent: PersistentState,
    save_tx: SyncSender<()>,

    cps_last_change: Option<Instant>,
    cps_last_sent: u32,
    jitter_last_change: Option<Instant>,
    jitter_last_sent: u32,
    repeat_last_change: Option<Instant>,
    repeat_last_sent: u32,

    /// Set once the user or the global quit hotkey requests application
    /// shutdown. This prevents the lifecycle poll from treating the expected
    /// backend exit as a crash and respawning it before the viewport closes.
    shutdown_requested: bool,
    /// When a backend-driven hotkey is being applied, we briefly suppress the
    /// egui window-focused fallback path so the user sees one toggle, not two.
    hotkey_action_pending: Option<Instant>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let inbox = Arc::new(Mutex::new(VecDeque::new()));
        let listener_ctx = cc.egui_ctx.clone();
        let backend_pid = Arc::new(AtomicU32::new(0));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let listener_backend_pid = Arc::clone(&backend_pid);
        let listener_shutting_down = Arc::clone(&shutting_down);
        let global_hotkeys_available = Arc::new(AtomicBool::new(false));
        let listener_hotkeys_available = global_hotkeys_available.clone();

        let sig_rx = install_signal_handler(cc.egui_ctx.clone());

        let persistent = state::load();
        let save_tx = state::spawn_saver();
        crate::theme::apply(&cc.egui_ctx, persistent.dark_mode);

        if let Some(pos) = persistent.window_pos {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::Pos2::new(
                    pos[0], pos[1],
                )));
        }

        let mut app = Self {
            enabled: false,
            cps: persistent.cps.clamp(1, 1000),
            button: persistent.button.clone(),
            mode: persistent.mode.clone(),
            randomize: persistent.randomize,
            jitter_ms: persistent.jitter_ms,
            repeat_count: persistent.repeat_count.min(1_000_000),
            autostart: persistent.autostart,
            dark_mode: persistent.dark_mode,
            theme_refresh_needed: false,
            backend_present: false,
            hotkeys: load_hotkeys(),
            hotkey_draft: HotkeySettings::default(),
            show_hotkey_settings: false,
            last_error: None,
            inbox: inbox.clone(),
            sig_rx: Some(sig_rx),
            backend_child: None,
            backend_pid,
            shutting_down,
            global_hotkeys_available,
            last_socket_check: Instant::now(),
            persistent: persistent.clone(),
            save_tx,
            cps_last_change: None,
            cps_last_sent: 0,
            jitter_last_change: None,
            jitter_last_sent: persistent.jitter_ms,
            repeat_last_change: None,
            repeat_last_sent: persistent.repeat_count,
            shutdown_requested: false,
            hotkey_action_pending: None,
        };

        app.hotkey_draft = app.hotkeys.clone();

        // Read the actual service state; opening the GUI must not change it.
        if let Ok(output) = std::process::Command::new(USER_SYSTEMCTL)
            .args(["--user", "is-enabled", UNIT_NAME])
            .output()
        {
            let status = String::from_utf8_lossy(&output.stdout);
            if matches!(status.trim(), "enabled" | "disabled") {
                app.autostart = status.trim() == "enabled";
            }
        }

        // Try connecting to an existing backend first; otherwise spawn one.
        if UnixStream::connect(ipc::SOCKET_PATH).is_ok() {
            app.backend_present = true;
        } else {
            app.spawn_backend();
        }

        // Push initial settings only after a backend is actually connectable.
        // Sending these before spawning the backend silently drops the user's
        // persisted UI state and leaves the backend on config defaults.
        if app.backend_present {
            app.sync_backend_settings();
        }

        // Initialize persistence and startup settings before the listener can
        // stage backend snapshots; startup must not overwrite a newer update.
        let listener_save_tx = app.save_tx.clone();
        let initial_child = app.backend_child.take();
        thread::spawn(move || {
            listen_for_notifications(
                inbox,
                listener_ctx,
                listener_backend_pid,
                listener_shutting_down,
                listener_hotkeys_available,
                listener_save_tx,
                initial_child,
            )
        });

        app
    }

    fn save_persistent(&mut self) {
        let previous = self.persistent.clone();
        self.persistent.cps = self.cps;
        self.persistent.button = self.button.clone();
        self.persistent.mode = self.mode.clone();
        self.persistent.randomize = self.randomize;
        self.persistent.jitter_ms = self.jitter_ms;
        self.persistent.repeat_count = self.repeat_count;
        self.persistent.autostart = self.autostart;
        self.persistent.dark_mode = self.dark_mode;
        let snap = self.persistent.clone();
        state::queue_save(&self.save_tx, &previous, snap);
    }

    fn cps_step(cps: u32) -> u32 {
        match cps {
            1..=20 => 1,
            21..=50 => 5,
            51..=100 => 10,
            101..=500 => 25,
            _ => 50,
        }
    }

    fn handle_hotkey(&mut self, action: &str) {
        match action {
            "toggle" => {
                self.hotkey_action_pending = Some(Instant::now());
                self.toggle();
            }
            "increase" => {
                self.hotkey_action_pending = Some(Instant::now());
                self.set_cps(self.cps.saturating_add(Self::cps_step(self.cps)).min(1000));
            }
            "decrease" => {
                self.hotkey_action_pending = Some(Instant::now());
                let step = Self::cps_step(self.cps.saturating_sub(1));
                self.set_cps(self.cps.saturating_sub(step).max(1));
            }
            "quit" => self.quit(),
            _ => {}
        }
    }

    fn flush_pending_settings(&mut self) {
        if self.cps == self.cps_last_sent {
            self.cps_last_change = None;
        }
        if self.jitter_ms == self.jitter_last_sent {
            self.jitter_last_change = None;
        }
        if self.repeat_count == self.repeat_last_sent {
            self.repeat_last_change = None;
        }
        if !self.backend_present {
            return;
        }
        if let Some(t) = self.cps_last_change {
            if t.elapsed() >= CPS_DEBOUNCE
                && self.cps != self.cps_last_sent
                && self.send_command(GuiToBackend::SetCps(self.cps), "set click rate")
            {
                self.cps_last_sent = self.cps;
                self.cps_last_change = None;
            }
        }
        if let Some(t) = self.jitter_last_change {
            if t.elapsed() >= CPS_DEBOUNCE
                && self.jitter_ms != self.jitter_last_sent
                && self.send_command(GuiToBackend::SetJitterMs(self.jitter_ms), "set jitter")
            {
                self.jitter_last_sent = self.jitter_ms;
                self.jitter_last_change = None;
            }
        }
        if let Some(t) = self.repeat_last_change {
            if t.elapsed() >= CPS_DEBOUNCE
                && self.repeat_count != self.repeat_last_sent
                && self.send_command(
                    GuiToBackend::SetRepeatCount(self.repeat_count),
                    "set repeat count",
                )
            {
                self.repeat_last_sent = self.repeat_count;
                self.repeat_last_change = None;
            }
        }
    }

    fn send_command(&mut self, command: GuiToBackend, action: &str) -> bool {
        match ipc::send(command) {
            Ok(()) => {
                self.last_error = None;
                true
            }
            Err(e) => {
                self.backend_present = false;
                self.last_error = Some(format!("Could not {}: {}", action, e));
                log::warn!("IPC command failed while trying to {}: {}", action, e);
                false
            }
        }
    }

    fn sync_backend_settings(&mut self) {
        let commands = [
            (GuiToBackend::SetMode(self.mode.clone()), "apply click mode"),
            (
                GuiToBackend::SetButton(self.button.clone()),
                "apply mouse button",
            ),
            (GuiToBackend::SetCps(self.cps), "apply click rate"),
            (
                GuiToBackend::SetRandomize(self.randomize),
                "apply randomization",
            ),
            (GuiToBackend::SetJitterMs(self.jitter_ms), "apply jitter"),
            (
                GuiToBackend::SetRepeatCount(self.repeat_count),
                "apply repeat count",
            ),
        ];
        for (command, action) in commands {
            if !self.send_command(command, action) {
                return;
            }
        }
        self.cps_last_sent = self.cps;
        self.cps_last_change = None;
        self.jitter_last_sent = self.jitter_ms;
        self.jitter_last_change = None;
        self.repeat_last_sent = self.repeat_count;
        self.repeat_last_change = None;
    }

    fn spawn_backend(&mut self) {
        if self
            .backend_pid
            .compare_exchange(0, u32::MAX, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            log::debug!("Backend spawn already claimed by lifecycle supervisor");
            return;
        }
        let backend_bin = match backend_binary() {
            Ok(path) => path,
            Err(e) => {
                log::error!("Could not resolve backend binary: {}", e);
                self.last_error = Some(format!("Could not locate click backend: {}", e));
                self.backend_pid.store(0, Ordering::Release);
                return;
            }
        };
        if !backend_bin.is_file() {
            log::error!("Backend binary not found at {}", backend_bin.display());
            self.last_error = Some(format!(
                "Click backend is missing: {}",
                backend_bin.display()
            ));
            self.backend_pid.store(0, Ordering::Release);
            return;
        }
        log::info!("Spawning backend: {}", backend_bin.display());
        // The backend inherits supplementary groups from the GUI process.
        // On Ubuntu 24.04 / logind 255+, the systemd --user instance must
        // already have `input` in its group list for /dev/input/eventN to
        // be openable. If `input` was added to the user via `usermod -aG
        // input` AFTER the session started, the user MUST log out and back
        // in so systemd --user picks up the new group — otherwise passive
        // hotkey reads will fail with EACCES. See
        // `agent-docs/gotchas/systemd-user-group-after-usermod.md`.
        let result = std::process::Command::new(&backend_bin)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .process_group(0)
            .spawn();
        match result {
            Ok(child) => {
                self.backend_pid.store(child.id(), Ordering::Release);
                self.backend_child = Some(child);
                let deadline = Instant::now() + SOCKET_WAIT_TIMEOUT;
                while Instant::now() < deadline {
                    if UnixStream::connect(ipc::SOCKET_PATH).is_ok() {
                        self.backend_present = true;
                        log::info!("Backend socket ready");
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                if UnixStream::connect(ipc::SOCKET_PATH).is_ok() {
                    self.backend_present = true;
                    log::info!("Backend socket connectable");
                } else {
                    log::warn!("Backend socket did not become connectable in time");
                    if let Some(mut child) = self.backend_child.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    self.backend_pid.store(0, Ordering::Release);
                    self.backend_present = false;
                }
            }
            Err(e) => {
                self.backend_pid.store(0, Ordering::Release);
                log::error!("Backend spawn failed: {}", e);
            }
        }
    }

    // -------------------------------------------------------------------------
    // Public mutators used by ui.rs
    // -------------------------------------------------------------------------

    pub fn toggle(&mut self) {
        if self.send_command(GuiToBackend::Toggle, "toggle autoclicking") {
            self.enabled = !self.enabled;
        }
    }

    pub fn start(&mut self) {
        if !self.enabled && self.send_command(GuiToBackend::Start, "start autoclicking") {
            self.enabled = true;
        }
    }

    pub fn stop(&mut self) {
        if self.enabled && self.send_command(GuiToBackend::Stop, "stop autoclicking") {
            self.enabled = false;
        }
    }

    fn send_backend_quit(&self) {
        if let Err(e) = ipc::send_blocking(GuiToBackend::Quit) {
            log::debug!("Backend already unavailable during quit: {}", e);
        }
    }

    pub fn quit(&mut self) {
        self.shutdown_requested = true;
        self.shutting_down.store(true, Ordering::Release);
        self.send_backend_quit();
    }

    pub fn set_cps(&mut self, v: u32) {
        let v = v.clamp(1, 1000);
        if v == self.cps {
            return;
        }
        self.cps = v;
        self.cps_last_change = Some(Instant::now());
        self.save_persistent();
    }

    pub fn set_button(&mut self, b: String) {
        if b == self.button {
            return;
        }
        let previous = self.button.clone();
        self.button = b.clone();
        if self.send_command(GuiToBackend::SetButton(b), "set mouse button") {
            self.save_persistent();
        } else {
            self.button = previous;
        }
    }

    pub fn set_mode(&mut self, m: String) {
        if m == self.mode {
            return;
        }
        let previous = self.mode.clone();
        self.mode = m.clone();
        if self.send_command(GuiToBackend::SetMode(m), "set click mode") {
            self.save_persistent();
        } else {
            self.mode = previous;
        }
    }

    pub fn set_randomize(&mut self, on: bool) {
        if on == self.randomize {
            return;
        }
        let previous = self.randomize;
        self.randomize = on;
        if self.send_command(GuiToBackend::SetRandomize(on), "set randomization") {
            self.save_persistent();
        } else {
            self.randomize = previous;
        }
    }

    pub fn set_jitter_ms(&mut self, v: u32) {
        let v = v.clamp(0, 100);
        if v == self.jitter_ms {
            return;
        }
        self.jitter_ms = v;
        self.jitter_last_change = Some(Instant::now());
        self.save_persistent();
    }

    pub fn set_repeat_count(&mut self, count: u32) {
        let count = count.min(1_000_000);
        if count == self.repeat_count {
            return;
        }
        self.repeat_count = count;
        self.repeat_last_change = Some(Instant::now());
        self.save_persistent();
    }

    pub fn set_autostart(&mut self, on: bool) {
        if on == self.autostart {
            return;
        }
        if apply_autostart(on) {
            self.autostart = on;
            self.save_persistent();
        } else {
            self.last_error = Some(
                "Could not change Start on login; check the user service installation.".into(),
            );
        }
    }

    pub fn set_dark_mode(&mut self, on: bool) {
        if on == self.dark_mode {
            return;
        }
        self.dark_mode = on;
        self.theme_refresh_needed = true;
        self.save_persistent();
    }

    pub fn open_hotkey_settings(&mut self) {
        self.hotkey_draft = self.hotkeys.clone();
        self.show_hotkey_settings = true;
    }

    pub fn cancel_hotkey_settings(&mut self) {
        self.hotkey_draft = self.hotkeys.clone();
        self.show_hotkey_settings = false;
    }

    pub fn apply_hotkey_settings(&mut self) -> bool {
        let values = [
            &self.hotkey_draft.toggle,
            &self.hotkey_draft.increase,
            &self.hotkey_draft.decrease,
            &self.hotkey_draft.quit,
        ];
        if (0..values.len()).any(|i| ((i + 1)..values.len()).any(|j| values[i] == values[j])) {
            self.last_error = Some("Each action needs a different F-key.".to_string());
            return false;
        }

        if let Err(e) = save_hotkeys(&self.hotkey_draft) {
            self.last_error = Some(format!("Could not save hotkeys: {}", e));
            return false;
        }

        self.hotkeys = self.hotkey_draft.clone();
        self.show_hotkey_settings = false;
        self.shutting_down.store(true, Ordering::Release);
        self.send_backend_quit();

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let socket_alive = UnixStream::connect(ipc::SOCKET_PATH).is_ok();
            let child_alive = self.backend_pid.load(Ordering::Acquire) != 0;
            if !socket_alive && !child_alive {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        if UnixStream::connect(ipc::SOCKET_PATH).is_ok()
            || self.backend_pid.load(Ordering::Acquire) != 0
        {
            self.shutting_down.store(false, Ordering::Release);
            self.backend_present = false;
            self.last_error = Some(
                "The backend did not stop in time; hotkeys will apply after the next restart."
                    .to_string(),
            );
            return false;
        }

        self.backend_present = false;
        self.shutting_down.store(false, Ordering::Release);
        // This Quit only reloads the child backend; the application itself is
        // staying open and should resume normal crash recovery afterwards.
        self.shutdown_requested = false;
        // The listener resumes supervision and restores the staged settings.
        true
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        log::info!("GUI exiting — sending Quit to backend");
        self.quit();
        state::flush();
        let deadline = Instant::now() + SOCKET_WAIT_TIMEOUT;
        while self.backend_pid.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        crate::ui::render(self, ui);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Check presence at most once a second when logic is already awake.
        // Healthy idle windows rely on listener-driven lifecycle supervision.
        if self.last_socket_check.elapsed() >= SOCKET_POLL_INTERVAL {
            self.last_socket_check = Instant::now();
            self.backend_present = if let Ok(stream) = UnixStream::connect(ipc::SOCKET_PATH) {
                // SHUT_RDWR immediately closes the connection from our side
                // the moment we release the fd, so the backend's per-accept
                // thread sees EOF and tears down without us keeping an open
                // "is the socket alive?" socket open for a full second.
                let _ = stream.shutdown(std::net::Shutdown::Both);
                true
            } else {
                false
            };

            if !self.backend_present {
                self.enabled = false;
            }
        }

        // Drain backend notifications.
        let mut actions = Vec::new();
        actions.extend(self.inbox.lock().unwrap().drain(..));
        for event in actions {
            let msg = match event {
                GuiEvent::Backend(msg) => msg,
                GuiEvent::Disconnected => {
                    self.backend_present = false;
                    ctx.request_repaint_after(Duration::from_millis(100));
                    continue;
                }
            };
            match msg {
                BackendToGui::HotkeyPressed(action) => {
                    // Backend already applies the action itself and broadcasts
                    // a fresh Status. We must NOT call handle_hotkey() here —
                    // doing so would send a second Toggle/Increase/Decrease
                    // IPC round-trip and create the ping-pong loop shown in
                    // the previous session's journal (every press was logged
                    // twice: "via in-process hotkey" AND "via IPC toggle").
                    log::info!("Backend hotkey notification: {}", action);
                    self.hotkey_action_pending = Some(Instant::now());
                    if action == "quit" {
                        // The backend has already begun its own shutdown. Mark
                        // this as intentional before asking the viewport to
                        // close, otherwise the 1 Hz monitor can respawn it.
                        self.shutdown_requested = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        return;
                    }
                }
                BackendToGui::Status {
                    enabled,
                    cps,
                    button,
                    mode,
                    randomize,
                    jitter_ms,
                    repeat_count,
                } => {
                    self.backend_present = true;
                    log::debug!(
                        "GUI received Status: enabled={} cps={} button={} mode={} randomize={} jitter_ms={} repeat_count={}",
                        enabled, cps, button, mode, randomize, jitter_ms, repeat_count
                    );
                    self.enabled = enabled;
                    if self.cps_last_change.is_none() {
                        self.cps = cps;
                        self.cps_last_sent = cps;
                    }
                    self.button = button;
                    self.mode = mode;
                    self.randomize = randomize;
                    if self.jitter_last_change.is_none() {
                        self.jitter_ms = jitter_ms;
                        self.jitter_last_sent = jitter_ms;
                    }
                    if self.repeat_last_change.is_none() {
                        self.repeat_count = repeat_count;
                        self.repeat_last_sent = repeat_count;
                    }
                    // The listener already staged this status. Updating only
                    // our comparison baseline avoids writing a delayed GUI
                    // event over a newer listener snapshot or saving twice.
                    self.persistent.cps = cps;
                    self.persistent.button = self.button.clone();
                    self.persistent.mode = self.mode.clone();
                    self.persistent.randomize = randomize;
                    self.persistent.jitter_ms = jitter_ms;
                    self.persistent.repeat_count = repeat_count;
                }
                BackendToGui::HotkeyAvailability(_) => {}
                BackendToGui::Ready => {
                    self.backend_present = true;
                }
                BackendToGui::Error(e) => {
                    log::warn!("Backend error: {}", e);
                }
            }
        }

        // SIGTERM/SIGINT — graceful Quit + close.
        let mut got_signal = false;
        if let Some(ref mut rx) = self.sig_rx {
            if rx.try_recv().is_ok() {
                got_signal = true;
            }
        }
        if got_signal {
            log::info!("Received termination signal, sending Quit to backend");
            self.quit();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // Flush debounced settings update if deadline passed.
        self.flush_pending_settings();

        // Window-focused fallback hotkeys (work even when the in-process evdev
        // reader is unavailable, e.g. when the user is not in the `input`
        // group). Skip briefly after a backend-driven hotkey, otherwise the
        // egui path triggers a second toggle while the backend is still
        // broadcasting Status, producing visible double-toggles.
        let backend_hotkey_recent = self
            .hotkey_action_pending
            .map(|t| t.elapsed() < Duration::from_millis(600))
            .unwrap_or(false);

        if !self.global_hotkeys_available.load(Ordering::Acquire) && !backend_hotkey_recent {
            if let Some(key) = parse_egui_key(&self.hotkeys.toggle) {
                if ctx.input(|i| i.key_pressed(key)) {
                    self.handle_hotkey("toggle");
                }
            }
            if let Some(key) = parse_egui_key(&self.hotkeys.increase) {
                if ctx.input(|i| i.key_pressed(key)) {
                    self.handle_hotkey("increase");
                }
            }
            if let Some(key) = parse_egui_key(&self.hotkeys.decrease) {
                if ctx.input(|i| i.key_pressed(key)) {
                    self.handle_hotkey("decrease");
                }
            }
            if let Some(key) = parse_egui_key(&self.hotkeys.quit) {
                if ctx.input(|i| i.key_pressed(key)) {
                    self.handle_hotkey("quit");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
            }
        }

        // A healthy idle window needs no timer-driven rendering. The listener
        // supervises backend exits independently and wakes us for new status;
        // input and the signal pipe also request their own repaint. Keep a
        // fallback timer only while recovering a missing backend.
        if !self.backend_present {
            ctx.request_repaint_after(SOCKET_POLL_INTERVAL);
        }
        if self.backend_present {
            for changed in [
                self.cps_last_change,
                self.jitter_last_change,
                self.repeat_last_change,
            ]
            .into_iter()
            .flatten()
            {
                ctx.request_repaint_after(
                    CPS_DEBOUNCE
                        .saturating_sub(changed.elapsed())
                        .max(Duration::from_millis(1)),
                );
            }
        }
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;

    #[test]
    fn deferred_gui_updates_are_bounded_and_keep_latest_status() {
        let inbox = Arc::new(Mutex::new(VecDeque::new()));
        for cps in 1..=100_000 {
            queue_event(
                &inbox,
                GuiEvent::Backend(BackendToGui::Status {
                    enabled: false,
                    cps,
                    button: "left".into(),
                    mode: "single".into(),
                    randomize: false,
                    jitter_ms: 0,
                    repeat_count: 0,
                }),
            );
            queue_event(
                &inbox,
                GuiEvent::Backend(BackendToGui::HotkeyPressed("increase".into())),
            );
        }
        let queue = inbox.lock().unwrap();
        assert_eq!(queue.len(), 2);
        assert!(matches!(
            queue.front(),
            Some(GuiEvent::Backend(BackendToGui::Status { cps: 100_000, .. }))
        ));
    }

    #[test]
    fn oversized_and_unterminated_notifications_are_rejected() {
        let mut line = Vec::new();
        let mut oversized = std::io::Cursor::new(vec![b'x'; 100_000]);
        assert!(read_notification(&mut oversized, &mut line).is_err());
        assert_eq!(line.len(), 4097);
        let mut incomplete = std::io::Cursor::new(b"\"Ready\"");
        assert!(read_notification(&mut incomplete, &mut line).is_err());
        let mut valid = std::io::Cursor::new(b"\"Ready\"\n\"Ready\"\n");
        assert!(matches!(
            read_notification(&mut valid, &mut line),
            Ok(Some(BackendToGui::Ready))
        ));
        assert!(matches!(
            read_notification(&mut valid, &mut line),
            Ok(Some(BackendToGui::Ready))
        ));
        assert!(read_notification(&mut valid, &mut line).unwrap().is_none());
    }
}
