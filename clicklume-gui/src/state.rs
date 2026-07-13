//! Persistent GUI state, loaded from and saved to
//! `~/.config/clicklume/state.toml`.
//!
//! We use the workspace's existing `toml` + `serde` dependencies — no
//! new crates required. Saves are debounced (500ms) to avoid hammering
//! disk while the user drags the CPS slider.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

const STATE_DIR: &str = "clicklume";
const LEGACY_STATE_DIR: &str = "autoclick";
const STATE_FILE: &str = "state.toml";
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
static STATE_FILE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentState {
    pub cps: u32,
    pub button: String,
    pub mode: String,
    pub randomize: bool,
    pub jitter_ms: u32,
    pub always_on_top: bool,
    /// Number of click actions before stopping. Zero means unlimited.
    #[serde(default)]
    pub repeat_count: u32,
    /// Start the GUI service automatically on user login (writes to
    /// `~/.config/systemd/user/clicklume.service` enabled flag).
    #[serde(default)]
    pub autostart: bool,
    /// False selects the bright glass theme; true selects the dark theme.
    #[serde(default = "default_dark_mode")]
    pub dark_mode: bool,
    pub window_pos: Option<[f32; 2]>,
}

fn default_dark_mode() -> bool {
    true
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            cps: 12,
            button: "left".to_string(),
            mode: "single".to_string(),
            randomize: false,
            jitter_ms: 10,
            always_on_top: false,
            repeat_count: 0,
            autostart: false,
            dark_mode: true,
            window_pos: None,
        }
    }
}

fn config_base() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn state_path() -> PathBuf {
    config_base().join(STATE_DIR).join(STATE_FILE)
}

fn legacy_state_path() -> PathBuf {
    config_base().join(LEGACY_STATE_DIR).join(STATE_FILE)
}

fn read_state(path: &Path) -> Option<PersistentState> {
    match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<PersistentState>(&text) {
            Ok(state) => Some(state),
            Err(e) => {
                log::warn!("{} parse error ({}); using defaults", path.display(), e);
                None
            }
        },
        Err(e) => {
            log::warn!("{} read error ({}); using defaults", path.display(), e);
            None
        }
    }
}

pub fn load() -> PersistentState {
    let path = state_path();
    if path.exists() {
        return read_state(&path).unwrap_or_default();
    }

    let legacy = legacy_state_path();
    if let Some(state) = legacy.exists().then(|| read_state(&legacy)).flatten() {
        log::info!(
            "Migrating preferences from {} to {}",
            legacy.display(),
            path.display()
        );
        save_to_disk(&path, &state);
        return state;
    }

    PersistentState::default()
}

/// Spawn a background save worker. Returns a Sender that you can push
/// new `PersistentState` snapshots to; the worker debounces writes.
pub fn spawn_saver() -> Sender<PersistentState> {
    let (tx, rx) = mpsc::channel::<PersistentState>();
    thread::spawn(move || {
        let path = state_path();
        let mut latest: Option<PersistentState> = None;
        let mut last_change = Instant::now();
        loop {
            match rx.recv_timeout(SAVE_DEBOUNCE) {
                Ok(snapshot) => {
                    latest = Some(snapshot);
                    last_change = Instant::now();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(snap) = latest.take() {
                        if last_change.elapsed() >= SAVE_DEBOUNCE {
                            save_to_disk(&path, &snap);
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(snap) = latest.take() {
                        save_to_disk(&path, &snap);
                    }
                    break;
                }
            }
        }
    });
    tx
}

fn save_to_disk(path: &Path, snapshot: &PersistentState) {
    let _guard = STATE_FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    save_to_disk_locked(path, snapshot);
}

fn save_to_disk_locked(path: &Path, snapshot: &PersistentState) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match toml::to_string_pretty(snapshot) {
        Ok(text) => {
            if let Err(e) = std::fs::write(path, text) {
                log::warn!("Failed to write {}: {}", path.display(), e);
            }
        }
        Err(e) => log::warn!("Failed to serialize state: {}", e),
    }
}

/// Persist settings reported by the backend without waiting for the GUI event
/// loop. This matters for global F7/F8 presses while the Wayland window is
/// minimized or otherwise idle: the listener thread receives the status even
/// when GNOME delays the next app logic pass.
pub fn merge_backend_settings(
    cps: u32,
    button: String,
    mode: String,
    randomize: bool,
    jitter_ms: u32,
) {
    let path = state_path();
    let _guard = STATE_FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut snapshot = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| toml::from_str::<PersistentState>(&text).ok())
        .unwrap_or_default();
    snapshot.cps = cps.clamp(1, 1000);
    snapshot.button = button;
    snapshot.mode = mode;
    snapshot.randomize = randomize;
    snapshot.jitter_ms = jitter_ms.min(100);
    save_to_disk_locked(&path, &snapshot);
}

#[cfg(test)]
mod tests {
    use super::PersistentState;

    #[test]
    fn legacy_state_defaults_to_dark_mode() {
        let state: PersistentState = toml::from_str(
            r#"
cps = 12
button = "left"
mode = "single"
randomize = false
jitter_ms = 10
always_on_top = false
autostart = false
window_pos = [10.0, 20.0]
"#,
        )
        .expect("legacy state should remain compatible");
        assert!(state.dark_mode);
        assert_eq!(state.repeat_count, 0);
    }
}
