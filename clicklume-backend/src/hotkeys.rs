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
use evdev::{Device, KeyCode};
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use std::collections::HashSet;
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

fn is_excluded_device_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // Exclude pointer / mouse devices
    lower.contains("mouse")
        || lower.contains("touchpad")
        || lower.contains("trackpoint")
        || lower.contains("trackball")
        // Exclude audio / consumer headsets
        || lower.contains("avrcp")
        || lower.contains("headset")
        || lower.contains("audio")
        // Exclude system switches / video bus
        || lower.contains("power button")
        || lower.contains("sleep button")
        || lower.contains("lid switch")
        || lower.contains("video bus")
        // Exclude our own virtual device or other virtual mice
        || lower.contains("clicklume")
        || lower.contains("virtual")
}

/// A device is useful to us if:
/// 1. It is not an excluded device type (mouse, touchpad, audio, virtual).
/// 2. It is a genuine typing keyboard (supports KEY_A, KEY_ENTER, KEY_SPACE).
/// 3. It can emit at least one of the configured hotkey keycodes.
fn supports_any_hotkey(path: &Path, hotkeys: &[KeyCode; 4]) -> bool {
    let name = device_name_for(path);

    // 1. Zero-touch pre-filter: Do NOT open devices that are clearly mice, touchpads,
    // headsets, system buttons, or virtual devices. This avoids device contention with
    // game engines (e.g. Roblox / Sober) and audio stacks.
    if is_excluded_device_name(&name) {
        log::debug!(
            "Skipping {} ({}) — excluded device type (mouse/touchpad/audio/virtual)",
            path.display(),
            name
        );
        return false;
    }

    // 2. Open device read-only to inspect capabilities
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
            name
        );
        return false;
    };

    // 3. True Physical Keyboard Validation:
    // A genuine typing keyboard MUST support basic alphanumeric and formatting keys
    // such as KEY_A, KEY_ENTER, and KEY_SPACE.
    // Mice with secondary multimedia endpoints, AVRCP headsets, and macro controls
    // do NOT support these core keys.
    let is_typing_keyboard = keys.contains(KeyCode::KEY_A)
        && keys.contains(KeyCode::KEY_ENTER)
        && keys.contains(KeyCode::KEY_SPACE);

    if !is_typing_keyboard {
        log::debug!(
            "Skipping {} ({}) — lacks core keyboard keys (KEY_A / KEY_ENTER / KEY_SPACE)",
            path.display(),
            name
        );
        return false;
    }

    // 4. Check if the keyboard supports any of the configured hotkeys
    let supported = hotkeys.iter().any(|key| keys.contains(*key));
    if !supported {
        log::debug!(
            "Skipping {} ({}) — does not support configured hotkeys",
            path.display(),
            name
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

                // Event-driven hotplug: watch /dev/input for newly created device nodes
                let inotify = match Inotify::init(InitFlags::IN_CLOEXEC | InitFlags::IN_NONBLOCK) {
                    Ok(inotify) => {
                        match inotify.add_watch("/dev/input", AddWatchFlags::IN_CREATE) {
                            Ok(_) => {
                                log::info!("Inotify watch active on /dev/input (event-driven hotplug)");
                                Some(inotify)
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to add inotify watch on /dev/input: {}; falling back to slow poll",
                                    e
                                );
                                None
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to initialize inotify: {}; falling back to slow poll",
                            e
                        );
                        None
                    }
                };

                while running.load(Ordering::Relaxed) {
                    if let Some(ref inotify) = inotify {
                        let mut poll_fds = [PollFd::new(inotify, PollFlags::POLLIN)];
                        match poll(&mut poll_fds, 1000) {
                            Ok(n) if n > 0 => {
                                if let Ok(events) = inotify.read_events() {
                                    if !events.is_empty() {
                                        log::debug!(
                                            "Inotify detected {} new event(s) in /dev/input",
                                            events.len()
                                        );
                                        // 300ms settling debounce so kernel drivers, BlueZ, and
                                        // game engines (SDL2) finish binding before ClickLume probes
                                        thread::sleep(Duration::from_millis(300));
                                        let _ = inotify.read_events();

                                        let mut detached = Vec::new();
                                        rescan_and_spawn(
                                            &hotkeys,
                                            tx.clone(),
                                            running.clone(),
                                            active_paths.clone(),
                                            &mut detached,
                                        );
                                        for handle in detached {
                                            std::mem::forget(handle);
                                        }
                                    }
                                }
                            }
                            Ok(_) => {
                                // 1-second poll timeout: loop and re-check running.load()
                                continue;
                            }
                            Err(e) => {
                                if e != nix::errno::Errno::EINTR {
                                    log::warn!("Inotify poll error: {}", e);
                                    thread::sleep(Duration::from_secs(1));
                                }
                            }
                        }
                    } else {
                        // Fallback slow poll if inotify is unavailable
                        thread::sleep(Duration::from_secs(5));
                        let mut detached = Vec::new();
                        rescan_and_spawn(
                            &hotkeys,
                            tx.clone(),
                            running.clone(),
                            active_paths.clone(),
                            &mut detached,
                        );
                        for handle in detached {
                            std::mem::forget(handle);
                        }
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
    mut dev: WatchedDevice,
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
        match dev.dev.fetch_events() {
            Ok(events) => {
                for event in events {
                    // EV_KEY == 1; value 1 == initial press. Ignore release(0) and repeat(2).
                    if event.event_type().0 == 1 && event.value() == 1 {
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
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::Interrupted => {
                continue;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_excluded_devices() {
        // Excluded: mice, touchpads, virtual devices, headsets, system buttons
        assert!(is_excluded_device_name("Lenovo Bluetooth Mouse"));
        assert!(is_excluded_device_name("Lenovo Bluetooth Mouse Keyboard"));
        assert!(is_excluded_device_name("Evision USB DEVICE Mouse"));
        assert!(is_excluded_device_name("ASUF1207:00 2808:0219 Touchpad"));
        assert!(is_excluded_device_name("HBTS001 (AVRCP)"));
        assert!(is_excluded_device_name("clicklume-virtual-mouse"));
        assert!(is_excluded_device_name("Power Button"));
        assert!(is_excluded_device_name("Sleep Button"));
        assert!(is_excluded_device_name("Lid Switch"));
        assert!(is_excluded_device_name("Video Bus"));

        // Allowed: real physical and Bluetooth keyboards
        assert!(!is_excluded_device_name("AT Translated Set 2 keyboard"));
        assert!(!is_excluded_device_name("Evision USB DEVICE Keyboard"));
        assert!(!is_excluded_device_name("Logitech MX Keys"));
        assert!(!is_excluded_device_name("Keychron K2 Pro"));
        assert!(!is_excluded_device_name("Apple Magic Keyboard"));
    }

    #[test]
    fn test_live_keyboard_discovery_excludes_mice() {
        let hotkeys = [
            KeyCode::KEY_F6,
            KeyCode::KEY_F7,
            KeyCode::KEY_F8,
            KeyCode::KEY_F9,
        ];
        let skip = HashSet::new();
        let found = find_keyboard_devices_for_hotkeys(&hotkeys, &skip);
        for p in &found {
            let name = device_name_for(p);
            println!("Discovered valid keyboard: {} ({})", p.display(), name);
            assert!(
                !is_excluded_device_name(&name),
                "Excluded device should not be discovered: {}",
                name
            );
        }
    }
}

