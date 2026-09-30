//! App state + lifecycle (backend spawn, listener thread, signal
//! handler, hotkey handling, persistent settings wiring).

#![allow(function_casts_as_integer)] // required by libc signal-handler registration

use crate::ipc;
use crate::state::{self, PersistentState};
use common::{BackendToGui, GuiToBackend};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

extern "C" {
    fn shutdown(fd: RawFd, how: libc::c_int) -> libc::c_int;
}
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
    std::fs::write(path, text)
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
    let (tx, rx) = mpsc::channel::<()>();
    let mut pipefds = [0i32; 2];
    let r = unsafe { libc::pipe(pipefds.as_mut_ptr()) };
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
        use std::os::fd::{FromRawFd, IntoRawFd};
        let mut f = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let mut buf = [0u8; 16];
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(()).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
                Err(_) => break,
            }
        }
        let _ = f.into_raw_fd();
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
fn apply_autostart(enable: bool) {
    let verb = if enable { "enable" } else { "disable" };
    match std::process::Command::new(USER_SYSTEMCTL)
        .args(["--user", verb, UNIT_NAME])
        .output()
    {
        Ok(out) if out.status.success() => {
            log::info!("Autostart {}: applied", verb);
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

fn sync_persisted_backend_settings() {
    let settings = state::load();
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
    tx: Sender<GuiEvent>,
    ctx: egui::Context,
    backend_pid: Arc<AtomicU32>,
    shutting_down: Arc<AtomicBool>,
) {
    let mut backoff_ms: u64 = 100;
    let mut supervised_child: Option<std::process::Child> = None;
    loop {
        if let Ok(mut stream) = UnixStream::connect(ipc::SOCKET_PATH) {
            let subscribe = serde_json::to_vec(&GuiToBackend::SubscribeGui)
                .expect("SubscribeGui must serialize");
            if let Err(e) = stream.write_all(&subscribe) {
                log::warn!("GUI listener failed to subscribe: {}", e);
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(2000);
                continue;
            }

            log::info!("GUI listener subscribed to backend");
            backoff_ms = 100;
            let mut buffer = [0u8; 4096];
            let mut pending = Vec::new();
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        log::info!("GUI listener EOF");
                        break;
                    }
                    Ok(n) => {
                        pending.extend_from_slice(&buffer[..n]);
                        while let Some(newline_idx) = pending.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = pending.drain(..=newline_idx).collect();
                            let line = &line[..line.len() - 1];
                            match serde_json::from_slice::<BackendToGui>(line) {
                                Ok(msg) => {
                                    log::info!("GUI listener parsed: {:?}", msg);
                                    match &msg {
                                        BackendToGui::HotkeyPressed(action) if action == "quit" => {
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
                                            *cps,
                                            button.clone(),
                                            mode.clone(),
                                            *randomize,
                                            *jitter_ms,
                                            *repeat_count,
                                        ),
                                        _ => {}
                                    }
                                    let _ = tx.send(GuiEvent::Backend(msg));
                                    ctx.request_repaint();
                                }
                                Err(e) => log::warn!("GUI listener parse error: {}", e),
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("GUI listener read error: {}", e);
                        break;
                    }
                }
            }
            // EOF can arrive just before a cleanly quitting child becomes
            // waitable. A release build may consume the repaint while the
            // socket still looks alive, then go idle. Briefly wait/reap here
            // so the disconnect event reaches the UI at a deterministic point.
            if let Some(mut child) = supervised_child.take() {
                let _ = child.wait();
            } else {
                let pid = backend_pid.load(Ordering::Acquire) as libc::pid_t;
                let mut status = 0;
                if pid > 0 && pid != u32::MAX as libc::pid_t {
                    for _ in 0..60 {
                        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                        if result == pid || result == -1 {
                            break;
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            backend_pid.store(0, Ordering::Release);
            let _ = tx.send(GuiEvent::Disconnected);
            ctx.request_repaint();
            if !shutting_down.load(Ordering::Acquire) {
                log::info!("Backend listener supervising restart");
                supervised_child = spawn_supervised_backend(&backend_pid);
            }
        }
        // A failed reconnect means the backend is still absent. Wake the UI
        // on every backoff step so lifecycle polling can reap and respawn the
        // child even when the release event loop is otherwise completely idle.
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

    hotkey_rx: Option<Receiver<GuiEvent>>,
    sig_rx: Option<Receiver<()>>,
    backend_child: Option<std::process::Child>,
    backend_pid: Arc<AtomicU32>,
    shutting_down: Arc<AtomicBool>,
    last_socket_check: Instant,

    persistent: PersistentState,
    save_tx: Sender<PersistentState>,

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
        let (tx, rx) = mpsc::channel::<GuiEvent>();
        let listener_ctx = cc.egui_ctx.clone();
        let backend_pid = Arc::new(AtomicU32::new(0));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let listener_backend_pid = Arc::clone(&backend_pid);
        let listener_shutting_down = Arc::clone(&shutting_down);
        thread::spawn(move || {
            listen_for_notifications(
                tx,
                listener_ctx,
                listener_backend_pid,
                listener_shutting_down,
            )
        });

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
            hotkey_rx: Some(rx),
            sig_rx: Some(sig_rx),
            backend_child: None,
            backend_pid,
            shutting_down,
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

        // Apply the persisted autostart preference to the systemd user service.
        // (Re-)enable / disable here so the change persists across reboots.
        apply_autostart(app.autostart);

        // Try connecting to an existing backend first; otherwise spawn one.
        if UnixStream::connect(ipc::SOCKET_PATH).is_ok() {
            app.backend_present = true;
        } else {
            let _ = std::fs::remove_file(ipc::SOCKET_PATH);
            app.spawn_backend();
        }

        // Push initial settings only after a backend is actually connectable.
        // Sending these before spawning the backend silently drops the user's
        // persisted UI state and leaves the backend on config defaults.
        if app.backend_present {
            app.sync_backend_settings();
        }

        app
    }

    fn save_persistent(&mut self) {
        self.persistent.cps = self.cps;
        self.persistent.button = self.button.clone();
        self.persistent.mode = self.mode.clone();
        self.persistent.randomize = self.randomize;
        self.persistent.jitter_ms = self.jitter_ms;
        self.persistent.repeat_count = self.repeat_count;
        self.persistent.autostart = self.autostart;
        self.persistent.dark_mode = self.dark_mode;
        let snap = self.persistent.clone();
        let _ = self.save_tx.send(snap);
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
        self.randomize = on;
        if self.send_command(GuiToBackend::SetRandomize(on), "set randomization") {
            self.save_persistent();
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
        self.autostart = on;
        apply_autostart(on);
        self.save_persistent();
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
            let child_alive = match self.backend_child.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => {
                        log::info!("Backend stopped for hotkey reload: {}", status);
                        self.backend_child = None;
                        self.backend_pid.store(0, Ordering::Release);
                        false
                    }
                    Ok(None) => true,
                    Err(e) => {
                        log::warn!("Could not wait for backend during hotkey reload: {}", e);
                        false
                    }
                },
                None => false,
            };
            if !socket_alive && !child_alive {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        if UnixStream::connect(ipc::SOCKET_PATH).is_ok() || self.backend_child.is_some() {
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
        self.spawn_backend();
        if self.backend_present {
            self.sync_backend_settings();
        }
        true
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        log::info!("GUI exiting — sending Quit to backend");
        self.quit();
        if let Some(mut child) = self.backend_child.take() {
            let waiter = thread::spawn(move || {
                let _ = child.wait();
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            while !waiter.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        }
        // A service stop can terminate the backend just after it handled
        // Quit, leaving only the pathname behind. Remove that stale marker so
        // the next launch never mistakes it for a live backend.
        let _ = std::fs::remove_file(ipc::SOCKET_PATH);
        thread::sleep(Duration::from_millis(500));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        crate::ui::render(self, ui);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Backend presence poll (1Hz). Use a connect-side shutdown so the
        // probe fd closes immediately — otherwise we leak short-lived
        // listener-handling threads on the backend every second.
        if self.last_socket_check.elapsed() >= SOCKET_POLL_INTERVAL {
            self.last_socket_check = Instant::now();
            self.backend_present = if let Ok(stream) = UnixStream::connect(ipc::SOCKET_PATH) {
                // SHUT_RDWR immediately closes the connection from our side
                // the moment we release the fd, so the backend's per-accept
                // thread sees EOF and tears down without us keeping an open
                // "is the socket alive?" socket open for a full second.
                let _ = std::os::unix::io::AsRawFd::as_raw_fd(&stream);
                unsafe {
                    shutdown(stream.as_raw_fd(), libc::SHUT_RDWR);
                }
                true
            } else {
                false
            };

            if let Some(child) = self.backend_child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        log::info!("Backend child exited (status={}); clearing handle", status);
                        self.backend_child = None;
                        self.backend_pid.store(0, Ordering::Release);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!("try_wait on backend child failed: {}", e);
                        self.backend_child = None;
                    }
                }
            }

            if !self.backend_present && !self.shutdown_requested {
                if self.enabled {
                    log::warn!("Backend socket became unreachable, stopping autoclicker");
                    self.enabled = false;
                }
                // A SIGKILL'd child can briefly report Ok(None) from
                // try_wait even though its socket has already disappeared.
                // Never overwrite that Child handle: doing so loses the only
                // way to reap it and leaves a zombie behind. Wait for the next
                // poll to observe/reap the exit, then respawn.
                if self.backend_child.is_none() {
                    log::info!("Backend absent — auto-respawning");
                    self.spawn_backend();
                    if self.backend_present {
                        self.sync_backend_settings();
                    }
                } else {
                    log::info!("Backend socket absent; waiting to reap child before respawn");
                }
            }
        }

        // Drain backend notifications.
        let mut actions = Vec::new();
        if let Some(ref mut rx) = self.hotkey_rx {
            while let Ok(msg) = rx.try_recv() {
                actions.push(msg);
            }
        }
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
                    log::info!(
                        "GUI received Status: enabled={} cps={} button={} mode={} randomize={} jitter_ms={} repeat_count={}",
                        enabled, cps, button, mode, randomize, jitter_ms, repeat_count
                    );
                    let settings_changed = self.cps != cps
                        || self.button != button
                        || self.mode != mode
                        || self.randomize != randomize
                        || self.jitter_ms != jitter_ms
                        || self.repeat_count != repeat_count;
                    self.enabled = enabled;
                    self.cps = cps;
                    self.button = button;
                    self.mode = mode;
                    self.randomize = randomize;
                    self.jitter_ms = jitter_ms;
                    self.repeat_count = repeat_count;
                    if settings_changed {
                        // Backend-owned global hotkeys bypass the GUI setters,
                        // so persist the resulting status here as well.
                        self.save_persistent();
                    }
                }
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

        if !backend_hotkey_recent {
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

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}
