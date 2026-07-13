//! clicklume-gui — Thin entry point.
//!
//! All state, lifecycle, IPC, signal handling, persistent settings, and
//! drawing are in `app.rs` (single-file monolithic for now; the
//! multi-file refactor is tracked in agent-docs but reverted to keep the
//! codebase simple per Sonnet's earlier split).
//!
//! This file only:
//! 1. Initializes the logger
//! 2. Sets up the eframe window options
//! 3. Constructs the App and hands it to eframe

mod app;
mod ipc;
mod state;
mod theme;
mod ui;

use eframe::egui;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    log::info!("Starting ClickLume v{}", env!("CARGO_PKG_VERSION"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 455.0])
            .with_min_inner_size([520.0, 420.0])
            .with_resizable(true)
            .with_transparent(true)
            .with_title("ClickLume"),
        ..Default::default()
    };

    eframe::run_native(
        "ClickLume",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
    .unwrap();
}
