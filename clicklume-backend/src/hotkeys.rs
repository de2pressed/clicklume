//! Passive in-process hotkey reader for autoclicker.
//!
//! GNOME 46 / Wayland does not provide a reliable GlobalShortcuts portal, and
//! gsd-media-keys has been observed failing to grab F-key accelerators. This
//! module owns the hotkey path by reading physical keyboard evdev devices
//! directly.
//!
//! Important: this reader is PASSIVE. It opens `/dev/input/eventN` read-only
//! and never calls EVIOCGRAB. EVIOCGRAB is an exclusive kernel grab; using it
//! makes the keyboard/touchpad stop working everywhere else. Read-only passive
//! evdev lets both GNOME/libinput and this backend receive the same events.

use anyhow::{Context, Result};
use evdev::{Device, InputEvent, KeyCode};
use std::collections::HashSet;
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyAction {
    Toggle,
    Increase,
    Decrease,
    Quit,
}

pub fn keycode_to_action(
    code: KeyCode,
    toggle: KeyCode,
    increase: KeyCode,
    decrease: KeyCode,
    quit: KeyCode,
) -> Option<HotkeyAction> {
    if code == toggle {
        Some(HotkeyAction::Toggle)
    } else if code == increase {
        Some(HotkeyAction::Increase)
    } else if code == decrease {
        Some(HotkeyAction::Decrease)
    } else if code == quit {
        Some(HotkeyAction::Quit)
    } else {
        None
    }
}

/// Parse a "KEY_F6" / "F6" style string into a KeyCode.
pub fn parse_keycode(name: &str) -> Option<KeyCode> {
    let normalized = name.trim().to_uppercase();
    let stripped = normalized.strip_prefix("KEY_").unwrap_or(&normalized);
    if let Some(num_str) = stripped.strip_prefix('F') {
        if let Ok(n) = num_str.parse::<u8>() {
            if (1..=12).contains(&n) {
                // Linux input-event-codes.h: KEY_F1=59, KEY_F2=60, ..., KEY_F12=70.
                let code = 58 + n as u16;
                return Some(KeyCode(code));
            } else if (13..=24).contains(&n) {
                // Linux input-event-codes.h: KEY_F13=183, ..., KEY_F24=194.
                let code = 183 + (n - 13) as u16;
                return Some(KeyCode(code));
            }
        }
    }
    None
}

fn event_node_sort_key(path: &Path) -> u32 {
    path.file_name()
        .and_then(|s| s.to_str())
        .and_then(|name| name.strip_prefix("event"))
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(u32::MAX)
}

fn is_event_node(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|name| name.starts_with("event"))
        .unwrap_or(false)
}

fn device_name_for(path: &Path) -> String {
    let event_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    std::fs::read_to_string(
        PathBuf::from("/sys/class/input")
            .join(event_name)
            .join("device/name"),
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|_| "unknown".to_string())
}

/// A device is useful to us if the kernel says it can emit at least one of the
/// configured hotkey keycodes. This avoids fragile sysfs KEY bitmap parsing and
/// prevents false positives like Bluetooth mice, power buttons, sleep buttons,
/// video bus devices, and headset AVRCP controls.
fn supports_any_hotkey(path: &Path, hotkeys: &[KeyCode; 4]) -> bool {
    let dev = match Device::open(path) {
        Ok(dev) => dev,
        Err(e) => {
            log::debug!(
                "Skipping {}: cannot open for capability probe: {}",
                path.display(),
                e
            );
            return false;
        }
    };

    let Some(keys) = dev.supported_keys() else {
        log::debug!(
            "Skipping {} ({}) — no EV_KEY capability",
            path.display(),
            device_name_for(path)
        );
        return false;
    };

    let supported = hotkeys.iter().any(|key| keys.contains(*key));
    if !supported {
        log::debug!(
            "Skipping {} ({}) — does not support configured hotkeys",
            path.display(),
            device_name_for(path)
        );
    }
    supported
}

pub fn find_keyboard_devices_for_hotkeys(
    hotkeys: &[KeyCode; 4],
    skip_paths: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir("/dev/input") {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!("Failed to read /dev/input: {}", e);
            return Vec::new();
        }
    };

    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_event_node(path))
        .filter(|path| !skip_paths.contains(path))
        .filter(|path| supports_any_hotkey(path, hotkeys))
        .collect();

    found.sort_by_key(|p| event_node_sort_key(p));
    found.dedup();

    if !found.is_empty() {
        log::info!("Discovered {} new hotkey-capable input device(s):", found.len());
        for p in &found {
            log::info!("  {} ({})", p.display(), device_name_for(p));
        }
    }
    found
}

struct WatchedDevice {
    dev: Device,
    path: PathBuf,
    name: String,
}

impl WatchedDevice {
    fn open(path: PathBuf) -> Result<Self> {
        let name = device_name_for(&path);
        let dev = Device::open(&path).with_context(|| format!("open({})", path.display()))?;
        log::info!("Watching hotkeys on {} ({})", path.display(), name);
        Ok(Self { dev, path, name })
    }

    fn read_event(&self) -> Result<Option<InputEvent>> {
        let mut buf = [0u8; 24];
        let raw = self.dev.as_fd().as_raw_fd();
        match nix::unistd::read(raw, &mut buf) {
            Ok(0) => Ok(None),
            Ok(n) if n >= 24 => {
                let type_ = u16::from_ne_bytes(buf[16..18].try_into().unwrap());
                let code = u16::from_ne_bytes(buf[18..20].try_into().unwrap());
                let value = i32::from_ne_bytes(buf[20..24].try_into().unwrap());
                Ok(Some(InputEvent::new_now(type_, code, value)))
            }
            Ok(_) => Ok(None),
            Err(nix::Error::EINTR) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("read failed: {}", e)),
        }
    }
}

pub struct HotkeyReader {
    running: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl HotkeyReader {
    pub fn start(
        toggle: KeyCode,
        increase: KeyCode,
        decrease: KeyCode,
        quit: KeyCode,
    ) -> Result<(Self, Receiver<HotkeyAction>)> {
        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let active_paths = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let hotkeys = [toggle, increase, decrease, quit];

        let mut reader = HotkeyReader {
            running: running.clone(),
            threads: Vec::new(),
        };

        rescan_and_spawn(
            &hotkeys,
            tx.clone(),
            running.clone(),
            active_paths.clone(),
            &mut reader.threads,
        );

        let monitor_handle = thread::Builder::new()
            .name("clicklume-hotkey-monitor".into())
            .spawn(move || {
                log::info!("Hotkey monitor thread started");
                while running.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(2));
                    let mut detached = Vec::new();
                    rescan_and_spawn(
                        &hotkeys,
                        tx.clone(),
                        running.clone(),
                        active_paths.clone(),
                        &mut detached,
                    );
                    // We intentionally detach hotplug reader handles here. They exit on
                    // EOF/read error or process shutdown; keeping them in this monitor
                    // stack would not buy us anything and would complicate ownership.
                    for handle in detached {
                        std::mem::forget(handle);
                    }
                }
                log::info!("Hotkey monitor thread stopped");
            })
            .context("spawn hotkey monitor thread")?;
        reader.threads.push(monitor_handle);

        Ok((reader, rx))
    }

    #[allow(dead_code)]
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Drop for HotkeyReader {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

fn rescan_and_spawn(
    hotkeys: &[KeyCode; 4],
    tx: Sender<HotkeyAction>,
    running: Arc<AtomicBool>,
    active_paths: Arc<Mutex<HashSet<PathBuf>>>,
    handles: &mut Vec<thread::JoinHandle<()>>,
) {
    let currently_active = active_paths.lock().unwrap().clone();
    let devices = find_keyboard_devices_for_hotkeys(hotkeys, &currently_active);
    if devices.is_empty() {
        if currently_active.is_empty() {
            log::warn!("No hotkey-capable keyboard devices found. Hotkeys disabled until one appears.");
        }
        return;
    }

    for path in devices {
        let mut active = active_paths.lock().unwrap();
        if active.contains(&path) {
            continue;
        }
        active.insert(path.clone());
        drop(active);

        let tx = tx.clone();
        let running = running.clone();
        let active_paths = active_paths.clone();
        let hotkeys = *hotkeys;
        let path_for_thread = path.clone();
        let active_paths_for_thread = active_paths.clone();
        match thread::Builder::new()
            .name(format!("clicklume-hotkey-{}", path.display()))
            .spawn(move || {
                match WatchedDevice::open(path_for_thread.clone()) {
                    Ok(dev) => reader_thread(dev, tx, running, hotkeys),
                    Err(e) => log::warn!("Failed to watch {}: {}", path_for_thread.display(), e),
                }
                active_paths_for_thread
                    .lock()
                    .unwrap()
                    .remove(&path_for_thread);
            }) {
            Ok(handle) => handles.push(handle),
            Err(e) => {
                active_paths.lock().unwrap().remove(&path);
                log::error!(
                    "Failed to spawn reader thread for {}: {}",
                    path.display(),
                    e
                );
            }
        }
    }
}

fn reader_thread(
    dev: WatchedDevice,
    tx: Sender<HotkeyAction>,
    running: Arc<AtomicBool>,
    hotkeys: [KeyCode; 4],
) {
    log::info!(
        "Reader thread started for {} ({})",
        dev.path.display(),
        dev.name
    );
    while running.load(Ordering::Relaxed) {
        match dev.read_event() {
            Ok(Some(event)) => {
                // EV_KEY == 1; value 1 == initial press. Ignore release(0) and repeat(2).
                if event.event_type().0 != 1 || event.value() != 1 {
                    continue;
                }
                let code = KeyCode(event.code());
                if let Some(action) =
                    keycode_to_action(code, hotkeys[0], hotkeys[1], hotkeys[2], hotkeys[3])
                {
                    log::info!(
                        "Hotkey action {:?} from {} ({})",
                        action,
                        dev.path.display(),
                        dev.name
                    );
                    if tx.send(action).is_err() {
                        log::warn!("Hotkey receiver dropped; exiting reader thread");
                        return;
                    }
                }
            }
            Ok(None) => {
                log::info!(
                    "EOF/short read on {} ({}); reader exiting",
                    dev.path.display(),
                    dev.name
                );
                return;
            }
            Err(e) => {
                log::warn!(
                    "Read error on {} ({}): {}; reader exiting",
                    dev.path.display(),
                    dev.name,
                    e
                );
                return;
            }
        }
    }
    log::info!(
        "Reader thread for {} ({}) stopped",
        dev.path.display(),
        dev.name
    );
}
