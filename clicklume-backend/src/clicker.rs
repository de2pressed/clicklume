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
use std::sync::{Arc, Condvar, Mutex};
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
    /// Shared by command writers and the worker to avoid polling while idle or held.
    pub wake: Arc<(Mutex<()>, Condvar)>,
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
            wake: Arc::new((Mutex::new(()), Condvar::new())),
        }
    }

    pub fn notify(&self) {
        // Synchronize with the waiter's predicate check so a Stop/Start cannot
        // arrive between checking the atomics and going to sleep.
        let _guard = self.wake.0.lock().unwrap();
        self.wake.1.notify_all();
    }
}

/// The click worker that generates mouse clicks
pub struct Clicker {
    state: ClickerState,
    running: Arc<AtomicBool>,
    on_auto_stop: Option<Arc<dyn Fn() + Send + Sync>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Clicker {
    /// Create a new clicker with the given state
    pub fn new(state: ClickerState) -> Self {
        Self {
            state,
            running: Arc::new(AtomicBool::new(true)),
            on_auto_stop: None,
            worker: Mutex::new(None),
        }
    }

    /// Attach a callback that fires when repeat count reaches its limit and autoclicking stops
    pub fn with_auto_stop_callback(mut self, cb: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.on_auto_stop = Some(cb);
        self
    }

    /// Get the KeyCode for a button name
    fn get_button_code(button: &str) -> KeyCode {
        if button.eq_ignore_ascii_case("right") {
            KeyCode::BTN_RIGHT
        } else if button.eq_ignore_ascii_case("middle") {
            KeyCode::BTN_MIDDLE
        } else {
            KeyCode::BTN_LEFT
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
        use evdev::InputEvent;

        let value = if pressed { 1 } else { 0 };

        let event = InputEvent::new_now(EventType::KEY.0, button.0, value);
        // VirtualDevice::emit already appends SYN_REPORT. An explicit sync
        // emitted a second batch (and three syncs) for every button transition.
        device.emit(&[event])?;

        Ok(())
    }

    /// Compute the next inter-click sleep duration given current settings.
    /// If randomization is enabled, adds a uniform random offset in
    /// `[-jitter_ms, +jitter_ms]` (clamped to non-negative).
    fn next_interval(&self, rng: &mut u64) -> Duration {
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
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            let jitter_range = (jitter_ms as i64) * 1_000_000; // ms -> ns
            let offset = (*rng % (2 * jitter_range + 1) as u64) as i64 - jitter_range;
            (base_ns as i64 + offset).max(1) as u64
        } else {
            base_ns
        };
        Duration::from_nanos(ns)
    }

    /// Perform one click "event" (single press, double press, or hold) for
    /// the current mode. The sink is injectable so cancellation and release
    /// behavior can be tested without sending real desktop input.
    fn emit_click_event(
        &self,
        emit: &mut impl FnMut(KeyCode, bool) -> Result<()>,
        btn_code: KeyCode,
        interval: Duration,
    ) -> Result<()> {
        let mode = *self.state.mode.lock().unwrap();
        match mode {
            ClickMode::Single => {
                // Ensure a realistic hold duration. In Linux/Wayland (libinput),
                // button presses held for <10ms trigger the hardware debounce filter
                // (DEBOUNCE_TIMEOUT_BOUNCE), causing the release to be suppressed and locking
                // the mouse button in the down state.
                let hold =
                    (interval / 2).clamp(Duration::from_millis(5), Duration::from_millis(25));
                emit(btn_code, true)?;
                self.wait_while_enabled(hold);
                emit(btn_code, false)?;
            }
            ClickMode::Double => {
                let hold =
                    (interval / 4).clamp(Duration::from_millis(5), Duration::from_millis(20));
                emit(btn_code, true)?;
                self.wait_while_enabled(hold);
                emit(btn_code, false)?;
                if !self.wait_while_enabled(Duration::from_millis(25)) {
                    return Ok(());
                }
                emit(btn_code, true)?;
                self.wait_while_enabled(hold);
                emit(btn_code, false)?;
            }
            ClickMode::Hold => {
                // Press and release over the full interval — sustained
                // press for the full interval before releasing
                emit(btn_code, true)?;
                let hold = interval;
                self.wait_while_enabled(hold);
                emit(btn_code, false)?;
            }
        }
        Ok(())
    }

    /// Sleep until the deadline or Stop/Quit, without 2ms polling slices.
    fn wait_while_enabled(&self, duration: Duration) -> bool {
        if duration.is_zero() {
            return self.running.load(Ordering::Relaxed)
                && self.state.enabled.load(Ordering::Relaxed);
        }
        let guard = self.state.wake.0.lock().unwrap();
        let _wait = self
            .state
            .wake
            .1
            .wait_timeout_while(guard, duration, |_| {
                self.running.load(Ordering::Relaxed) && self.state.enabled.load(Ordering::Relaxed)
            })
            .unwrap();
        self.running.load(Ordering::Relaxed) && self.state.enabled.load(Ordering::Relaxed)
    }

    /// Start the click worker thread
    pub fn start(&self) -> Result<()> {
        let mut worker_slot = self.worker.lock().unwrap();
        anyhow::ensure!(worker_slot.is_none(), "Click worker is already started");
        let mut device = Self::create_virtual_mouse()?;
        let running = self.running.clone();
        let enabled = self.state.enabled.clone();
        let button = self.state.button.clone();
        let repeat_count = self.state.repeat_count.clone();
        let state = self.state.clone();
        let on_auto_stop = self.on_auto_stop.clone();

        let worker = thread::Builder::new()
            .name("clicklume-clicker".into())
            .spawn(move || {
                log::info!("Click worker thread started");

                let mut clicker = Clicker::new(state);

                clicker.running = running.clone();

                let mut completed_actions = 0u32;
                let mut was_enabled = false;
                let mut last_btn_code = KeyCode::BTN_LEFT;
                let mut rng = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0xDEAD_BEEF)
                    .max(1);

                while running.load(Ordering::Relaxed) {
                    if !enabled.load(Ordering::Relaxed) {
                        if was_enabled {
                            let _ = Self::send_button_event(&mut device, last_btn_code, false);
                            completed_actions = 0;
                            was_enabled = false;
                        }
                        let guard = clicker.state.wake.0.lock().unwrap();
                        let _wait = clicker
                            .state
                            .wake
                            .1
                            .wait_while(guard, |_| {
                                running.load(Ordering::Relaxed) && !enabled.load(Ordering::Relaxed)
                            })
                            .unwrap();
                        continue;
                    }

                    if !was_enabled {
                        completed_actions = 0;
                        was_enabled = true;
                    }

                    let btn_code = Self::get_button_code(&button.lock().unwrap());
                    last_btn_code = btn_code;

                    let interval = clicker.next_interval(&mut rng);
                    let start = std::time::Instant::now();

                    if let Err(e) = clicker.emit_click_event(
                        &mut |button, pressed| {
                            Self::send_button_event(&mut device, button, pressed)
                        },
                        btn_code,
                        interval,
                    ) {
                        log::error!("Click emission failed: {}", e);
                        let _ = Self::send_button_event(&mut device, btn_code, false);
                        enabled.store(false, Ordering::Relaxed);
                        if let Some(ref cb) = on_auto_stop {
                            cb();
                        }
                        continue;
                    }

                    clicker.wait_while_enabled(interval.saturating_sub(start.elapsed()));

                    let limit = repeat_count.load(Ordering::Relaxed);
                    if limit > 0 {
                        completed_actions = completed_actions.saturating_add(1);
                        if completed_actions >= limit {
                            enabled.store(false, Ordering::Relaxed);
                            was_enabled = false;
                            let _ = Self::send_button_event(&mut device, btn_code, false);
                            if let Some(ref cb) = on_auto_stop {
                                cb();
                            }
                        }
                    }
                }

                if was_enabled {
                    let _ = Self::send_button_event(&mut device, last_btn_code, false);
                }

                log::info!("Click worker thread terminated");
            })?;
        *worker_slot = Some(worker);
        Ok(())
    }

    /// Stop the clicker
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.state.notify();
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
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

impl Drop for Clicker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stop_interrupts_long_hold_wait() {
        let state = ClickerState::new(1, "left".into());
        state.enabled.store(true, Ordering::Relaxed);
        let clicker = Arc::new(Clicker::new(state));
        let waiter = clicker.clone();
        let thread = thread::spawn(move || waiter.wait_while_enabled(Duration::from_secs(1)));
        thread::sleep(Duration::from_millis(10));
        let start = std::time::Instant::now();
        clicker.stop();
        assert!(!thread.join().unwrap());
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn disabling_interrupts_wait_without_periodic_polling() {
        let state = ClickerState::new(1, "left".into());
        state.enabled.store(true, Ordering::Relaxed);
        let clicker = Arc::new(Clicker::new(state.clone()));
        let waiter = clicker.clone();
        let worker = thread::spawn(move || waiter.wait_while_enabled(Duration::from_secs(30)));
        thread::sleep(Duration::from_millis(20));
        state.enabled.store(false, Ordering::Relaxed);
        state.notify();
        let start = std::time::Instant::now();
        assert!(!worker.join().unwrap());
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn randomized_intervals_remain_positive_and_within_range() {
        let state = ClickerState::new(20, "left".into());
        state.randomize.store(true, Ordering::Relaxed);
        state.jitter_ms.store(100, Ordering::Relaxed);
        let clicker = Clicker::new(state);
        let mut rng = 42;
        let mut below_base = false;
        let mut above_base = false;
        for _ in 0..10_000 {
            let interval = clicker.next_interval(&mut rng);
            assert!(interval > Duration::ZERO && interval <= Duration::from_millis(150));
            below_base |= interval < Duration::from_millis(50);
            above_base |= interval > Duration::from_millis(50);
        }
        assert!(below_base && above_base);
    }

    #[test]
    fn stop_releases_every_mode_without_emitting_desktop_input() {
        for mode in [ClickMode::Single, ClickMode::Double, ClickMode::Hold] {
            let state = ClickerState::new(1, "left".into());
            *state.mode.lock().unwrap() = mode;
            let clicker = Clicker::new(state.clone());
            for _ in 0..1000 {
                state.enabled.store(true, Ordering::Relaxed);
                let mut events = Vec::new();
                clicker
                    .emit_click_event(
                        &mut |button, pressed| {
                            events.push((button, pressed));
                            if pressed {
                                state.enabled.store(false, Ordering::Relaxed);
                                state.notify();
                            }
                            Ok(())
                        },
                        KeyCode::BTN_LEFT,
                        Duration::from_secs(1),
                    )
                    .unwrap();
                assert_eq!(
                    events,
                    [(KeyCode::BTN_LEFT, true), (KeyCode::BTN_LEFT, false)]
                );
            }
        }
    }

    #[test]
    fn double_click_emits_two_balanced_presses() {
        let state = ClickerState::new(100, "right".into());
        *state.mode.lock().unwrap() = ClickMode::Double;
        state.enabled.store(true, Ordering::Relaxed);
        let clicker = Clicker::new(state);
        let mut events = Vec::new();
        clicker
            .emit_click_event(
                &mut |button, pressed| {
                    events.push((button, pressed));
                    Ok(())
                },
                KeyCode::BTN_RIGHT,
                Duration::from_millis(10),
            )
            .unwrap();
        assert_eq!(
            events,
            [
                (KeyCode::BTN_RIGHT, true),
                (KeyCode::BTN_RIGHT, false),
                (KeyCode::BTN_RIGHT, true),
                (KeyCode::BTN_RIGHT, false)
            ]
        );
    }
}
