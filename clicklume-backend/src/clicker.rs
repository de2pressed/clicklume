//! Clicker module for generating mouse clicks via uinput
//!
//! Creates a virtual mouse device that emits click events.
//! Supports single / double / hold click modes, optional inter-click
//! jitter (randomization), and a configurable mouse button.
//!
//! AGENTS.md rule #4: do NOT change the EV_REL / REL_X / REL_Y axis
//! declaration — Mutter/libinput requires it to classify the virtual
//! device as a pointer. See `agent-docs/gotchas/buttons-only-mouse-no-relative-axes.md`.

use anyhow::Result;
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, KeyCode, RelativeAxisCode};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Click mode. `Single` emits one press+release per cycle, `Double` emits
/// two presses separated by a short delay (typical double-click), and
/// `Hold` emits a single press and keeps it down for the full cycle
/// interval (useful for sustained "fire" behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickMode {
    Single,
    Double,
    Hold,
}

impl ClickMode {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "double" => ClickMode::Double,
            "hold" => ClickMode::Hold,
            _ => ClickMode::Single,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ClickMode::Single => "single",
            ClickMode::Double => "double",
            ClickMode::Hold => "hold",
        }
    }
}

/// Shared state between threads
#[derive(Debug, Clone)]
pub struct ClickerState {
    /// Whether autoclicking is currently enabled
    pub enabled: Arc<AtomicBool>,
    /// Current CPS (clicks per second)
    pub cps: Arc<AtomicU32>,
    /// Mouse button to click
    pub button: Arc<Mutex<String>>,
    /// Click mode (single / double / hold)
    pub mode: Arc<Mutex<ClickMode>>,
    /// Whether to randomize inter-click interval
    pub randomize: Arc<AtomicBool>,
    /// Jitter range in milliseconds (added/subtracted from interval)
    pub jitter_ms: Arc<AtomicU32>,
    /// Number of click actions before stopping. Zero means unlimited.
    pub repeat_count: Arc<AtomicU32>,
}

impl ClickerState {
    #[allow(dead_code)]
    pub fn new(cps: u32, button: String) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            cps: Arc::new(AtomicU32::new(cps)),
            button: Arc::new(Mutex::new(button)),
            mode: Arc::new(Mutex::new(ClickMode::Single)),
            randomize: Arc::new(AtomicBool::new(false)),
            jitter_ms: Arc::new(AtomicU32::new(0)),
            repeat_count: Arc::new(AtomicU32::new(0)),
        }
    }
}

/// The click worker that generates mouse clicks
pub struct Clicker {
    state: ClickerState,
    running: Arc<AtomicBool>,
}

impl Clicker {
    /// Create a new clicker with the given state
    pub fn new(state: ClickerState) -> Self {
        Self {
            state,
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Get the KeyCode for a button name
    fn get_button_code(button: &str) -> KeyCode {
        match button.to_lowercase().as_str() {
            "left" => KeyCode::BTN_LEFT,
            "right" => KeyCode::BTN_RIGHT,
            "middle" => KeyCode::BTN_MIDDLE,
            _ => KeyCode::BTN_LEFT,
        }
    }

    /// Create a virtual mouse device
    fn create_virtual_mouse() -> Result<VirtualDevice> {
        let keys =
            AttributeSet::from_iter([KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE]);

        // CRITICAL: declare REL_X + REL_Y axes. Without these, libinput does
        // NOT grant LIBINPUT_DEVICE_CAP_POINTER (its evdev_configure_device()
        // requires REL_X+REL_Y for relative-axis pointer classification).
        // Mutter consumes input only via libinput, so a buttons-only device's
        // BTN_LEFT events never reach the cursor. We never emit motion events,
        // just declaring the axes is enough to flip libinput's classification.
        let rel_axes = AttributeSet::from_iter([RelativeAxisCode::REL_X, RelativeAxisCode::REL_Y]);

        let device = VirtualDevice::builder()?
            .name("clicklume-virtual-mouse")
            .with_keys(&keys)?
            .with_relative_axes(&rel_axes)?
            .build()?;

        log::info!("Created virtual mouse device with EV_REL + REL_X/REL_Y");
        Ok(device)
    }

    /// Send a mouse button event
    fn send_button_event(device: &mut VirtualDevice, button: KeyCode, pressed: bool) -> Result<()> {
        use evdev::{InputEvent, SynchronizationCode};

        let value = if pressed { 1 } else { 0 };

        let event = InputEvent::new_now(EventType::KEY.0, button.0, value);
        device.emit(&[event])?;

        let sync_event = InputEvent::new_now(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_REPORT.0,
            0,
        );
        device.emit(&[sync_event])?;

        Ok(())
    }

    /// Compute the next inter-click sleep duration given current settings.
    /// If randomization is enabled, adds a uniform random offset in
    /// `[-jitter_ms, +jitter_ms]` (clamped to non-negative).
    fn next_interval(&self) -> Duration {
        let cps = self.state.cps.load(Ordering::Relaxed);
        let base_ns = if cps > 0 {
            1_000_000_000u64 / cps as u64
        } else {
            10_000_000u64 // 10ms fallback when CPS=0
        };
        let jitter_ms = self.state.jitter_ms.load(Ordering::Relaxed);
        let randomize = self.state.randomize.load(Ordering::Relaxed);
        let ns = if randomize && jitter_ms > 0 {
            // cheap xorshift PRNG — no rand crate dependency
            let mut state = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0xDEAD_BEEF);
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let r = state as i64;
            let jitter_range = (jitter_ms as i64) * 1_000_000; // ms -> ns
            let offset = (r % (2 * jitter_range + 1)) - jitter_range;
            (base_ns as i64 + offset).max(1) as u64
        } else {
            base_ns
        };
        Duration::from_nanos(ns)
    }

    /// Perform one click "event" (single press, double press, or hold) for
    /// the current mode. Returns true if any button events were emitted.
    fn emit_click_event(&self, device: &mut VirtualDevice, btn_code: KeyCode) -> Result<()> {
        let mode = *self.state.mode.lock().unwrap();
        match mode {
            ClickMode::Single => {
                Self::send_button_event(device, btn_code, true)?;
                thread::sleep(Duration::from_micros(100));
                Self::send_button_event(device, btn_code, false)?;
            }
            ClickMode::Double => {
                Self::send_button_event(device, btn_code, true)?;
                thread::sleep(Duration::from_micros(80));
                Self::send_button_event(device, btn_code, false)?;
                thread::sleep(Duration::from_millis(50));
                Self::send_button_event(device, btn_code, true)?;
                thread::sleep(Duration::from_micros(80));
                Self::send_button_event(device, btn_code, false)?;
            }
            ClickMode::Hold => {
                // Press and release over the full interval — sustained
                // press for half the interval, release for the second half
                Self::send_button_event(device, btn_code, true)?;
                let cps = self.state.cps.load(Ordering::Relaxed).max(1);
                let hold = Duration::from_millis(500_u64.saturating_div(cps as u64));
                thread::sleep(hold);
                Self::send_button_event(device, btn_code, false)?;
            }
        }
        Ok(())
    }

    /// Start the click worker thread
    pub fn start(&self) {
        let running = self.running.clone();
        let enabled = self.state.enabled.clone();
        let cps = self.state.cps.clone();
        let button = self.state.button.clone();
        let mode = self.state.mode.clone();
        let randomize = self.state.randomize.clone();
        let jitter_ms = self.state.jitter_ms.clone();
        let repeat_count = self.state.repeat_count.clone();

        thread::spawn(move || {
            log::info!("Click worker thread started");

            let mut device = match Self::create_virtual_mouse() {
                Ok(d) => d,
                Err(e) => {
                    log::error!("Failed to create virtual mouse: {}", e);
                    return;
                }
            };

            let clicker = Clicker::new(ClickerState {
                enabled: enabled.clone(),
                cps: cps.clone(),
                button: button.clone(),
                mode: mode.clone(),
                randomize: randomize.clone(),
                jitter_ms: jitter_ms.clone(),
                repeat_count: repeat_count.clone(),
            });

            let mut completed_actions = 0u32;
            let mut was_enabled = false;

            while running.load(Ordering::Relaxed) {
                if !enabled.load(Ordering::Relaxed) {
                    completed_actions = 0;
                    was_enabled = false;
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }

                if !was_enabled {
                    completed_actions = 0;
                    was_enabled = true;
                }

                let current_button = button.lock().unwrap().clone();
                let btn_code = Self::get_button_code(&current_button);

                let interval = clicker.next_interval();
                let start = std::time::Instant::now();

                if let Err(e) = clicker.emit_click_event(&mut device, btn_code) {
                    log::debug!("Click emission failed: {}", e);
                }

                let elapsed = start.elapsed();
                if elapsed < interval {
                    let remaining = interval - elapsed;
                    if remaining.as_millis() > 0 {
                        thread::sleep(remaining);
                    } else {
                        std::hint::spin_loop();
                    }
                }

                let limit = repeat_count.load(Ordering::Relaxed);
                if limit > 0 {
                    completed_actions = completed_actions.saturating_add(1);
                    if completed_actions >= limit {
                        enabled.store(false, Ordering::Relaxed);
                        was_enabled = false;
                    }
                }
            }

            log::info!("Click worker thread terminated");
        });
    }

    /// Stop the clicker
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Check if clicker is enabled
    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        self.state.enabled.load(Ordering::Relaxed)
    }

    /// Get current CPS
    #[allow(dead_code)]
    pub fn get_cps(&self) -> u32 {
        self.state.cps.load(Ordering::Relaxed)
    }

    /// Get current button
    #[allow(dead_code)]
    pub fn get_button(&self) -> String {
        self.state.button.lock().unwrap().clone()
    }
}
