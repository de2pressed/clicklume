//! Common types shared between the GUI and backend

use serde::{Deserialize, Serialize};

/// Message from GUI to backend
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuiToBackend {
    /// Start autoclicking
    Start,
    /// Stop autoclicking
    Stop,
    /// Toggle enabled state (used by hotkey CLI)
    Toggle,
    /// Set clicks per second
    SetCps(u32),
    /// Increase CPS by the configured step (used by hotkey CLI)
    IncreaseCps,
    /// Decrease CPS by the configured step (used by hotkey CLI)
    DecreaseCps,
    /// Set mouse button (left, right, middle)
    SetButton(String),
    /// Set click mode (single, double, hold)
    SetMode(String),
    /// Set randomization on/off
    SetRandomize(bool),
    /// Set jitter range in milliseconds (0-100)
    SetJitterMs(u32),
    /// Set the number of click actions before stopping. Zero means unlimited.
    SetRepeatCount(u32),
    /// Query current status without mutating state
    GetStatus,
    /// Register this socket as the long-lived GUI notification listener.
    /// The backend keeps the connection open and pushes BackendToGui messages.
    SubscribeGui,
    /// Shutdown backend
    Quit,
}

/// Message from backend to GUI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackendToGui {
    /// Current status update
    Status {
        enabled: bool,
        cps: u32,
        button: String,
        mode: String,
        randomize: bool,
        jitter_ms: u32,
        #[serde(default)]
        repeat_count: u32,
    },
    /// Hotkey was pressed (for GUI to update its state)
    HotkeyPressed(String),
    /// Error occurred
    Error(String),
    /// Backend is ready
    Ready,
}

#[cfg(test)]
mod tests {
    use super::{BackendToGui, GuiToBackend};

    #[test]
    fn gui_command_round_trips_through_json() {
        let command = GuiToBackend::SetRepeatCount(42);
        let json = serde_json::to_string(&command).expect("command should serialize");
        let decoded: GuiToBackend =
            serde_json::from_str(&json).expect("command should deserialize");
        assert!(matches!(decoded, GuiToBackend::SetRepeatCount(42)));
    }

    #[test]
    fn full_status_round_trips_without_losing_fields() {
        let status = BackendToGui::Status {
            enabled: true,
            cps: 37,
            button: "middle".into(),
            mode: "double".into(),
            randomize: true,
            jitter_ms: 9,
            repeat_count: 50,
        };
        let json = serde_json::to_string(&status).expect("status should serialize");
        let decoded: BackendToGui = serde_json::from_str(&json).expect("status should deserialize");
        assert!(matches!(
            decoded,
            BackendToGui::Status {
                enabled: true,
                cps: 37,
                ref button,
                ref mode,
                randomize: true,
                jitter_ms: 9,
                repeat_count: 50,
            } if button == "middle" && mode == "double"
        ));
    }
}
