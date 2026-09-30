//! Compact OP-style layout with restrained monochrome glass styling.

use eframe::egui::{self, Align, Color32, Frame, Layout, Margin, RichText, Stroke, Vec2};

use crate::app::App;
use crate::theme::{self, Palette};

fn caption(text: impl Into<String>, p: Palette) -> RichText {
    RichText::new(text)
        .font(theme::caption_font())
        .color(p.muted)
}

fn section_label(ui: &mut egui::Ui, text: &str, p: Palette) {
    ui.label(
        RichText::new(text.to_uppercase())
            .font(theme::section_font())
            .strong()
            .color(p.text),
    );
}

fn theme_toggle(ui: &mut egui::Ui, app: &mut App, p: Palette) {
    let label = if app.dark_mode { "Light" } else { "Dark" };
    if ui
        .add(
            egui::Button::new(RichText::new(label).font(theme::caption_font()))
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(theme::CONTROL_RADIUS)
                .min_size(Vec2::new(52.0, 28.0)),
        )
        .on_hover_text(format!("Switch to {} mode", label.to_lowercase()))
        .clicked()
    {
        app.set_dark_mode(!app.dark_mode);
    }
}

fn header(ui: &mut egui::Ui, app: &mut App, p: Palette) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new("ClickLume")
                    .font(theme::title_font())
                    .strong()
                    .color(p.text),
            );
            ui.label(caption(if app.enabled { "RUNNING" } else { "READY" }, p));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            theme_toggle(ui, app, p);
            ui.add_space(8.0);
            ui.label(
                RichText::new(if app.backend_present {
                    "● ONLINE"
                } else {
                    "○ OFFLINE"
                })
                .font(theme::caption_font())
                .color(if app.backend_present { p.text } else { p.muted }),
            );
        });
    });
}

fn click_interval(ui: &mut egui::Ui, app: &mut App, p: Palette) {
    section_label(ui, "Click interval", p);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_sized(
            [68.0, 30.0],
            egui::Label::new(
                RichText::new("Speed")
                    .font(theme::body_font())
                    .color(p.muted),
            ),
        );
        let mut cps = app.cps;
        let slider = ui.add_sized(
            [300.0, 30.0],
            egui::Slider::new(&mut cps, 1..=100).show_value(false),
        );
        let value = ui.add_sized(
            [92.0, 30.0],
            egui::DragValue::new(&mut cps)
                .range(1..=100)
                .speed(1.0)
                .suffix(" CPS"),
        );
        if slider.changed() || value.changed() {
            app.set_cps(cps);
        }
    });
    ui.horizontal(|ui| {
        ui.add_sized(
            [68.0, 30.0],
            egui::Label::new(
                RichText::new("Variation")
                    .font(theme::body_font())
                    .color(p.muted),
            ),
        );
        let mut randomize = app.randomize;
        if ui.checkbox(&mut randomize, "Random offset").changed() {
            app.set_randomize(randomize);
        }
        let mut jitter = app.jitter_ms;
        let response = ui.add_enabled(
            app.randomize,
            egui::DragValue::new(&mut jitter)
                .range(0..=100)
                .speed(1.0)
                .suffix(" ms"),
        );
        if response.changed() {
            app.set_jitter_ms(jitter);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(caption(
                format!("{:.2} ms", 1000.0 / app.cps.max(1) as f32),
                p,
            ));
        });
    });
}

fn click_options(ui: &mut egui::Ui, app: &mut App, p: Palette) {
    section_label(ui, "Click options", p);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_sized([82.0, 30.0], egui::Label::new("Mouse button"));
        let mut selected = app.button.clone();
        egui::ComboBox::from_id_salt("mouse_button")
            .width(122.0)
            .selected_text(capitalize(&selected))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut selected, "left".into(), "Left");
                ui.selectable_value(&mut selected, "right".into(), "Right");
                ui.selectable_value(&mut selected, "middle".into(), "Middle");
            });
        if selected != app.button {
            app.set_button(selected);
        }
    });
    ui.horizontal(|ui| {
        ui.add_sized([82.0, 30.0], egui::Label::new("Click type"));
        let mut selected = app.mode.clone();
        egui::ComboBox::from_id_salt("click_mode")
            .width(122.0)
            .selected_text(capitalize(&selected))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut selected, "single".into(), "Single");
                ui.selectable_value(&mut selected, "double".into(), "Double");
                ui.selectable_value(&mut selected, "hold".into(), "Hold");
            });
        if selected != app.mode {
            app.set_mode(selected);
        }
    });
}

fn click_repeat(ui: &mut egui::Ui, app: &mut App, p: Palette) {
    section_label(ui, "Click repeat", p);
    ui.add_space(4.0);
    let mut fixed = app.repeat_count > 0;
    if ui.radio_value(&mut fixed, false, "Until stopped").changed() {
        app.set_repeat_count(0);
    }
    ui.horizontal(|ui| {
        if ui.radio_value(&mut fixed, true, "Fixed amount").changed() {
            app.set_repeat_count(app.repeat_count.max(10));
        }
        let mut count = app.repeat_count.max(1);
        let response = ui.add_enabled(
            fixed,
            egui::DragValue::new(&mut count)
                .range(1..=1_000_000)
                .speed(1.0),
        );
        if response.changed() {
            app.set_repeat_count(count);
        }
    });
    let toggle_key = hotkey_name(&app.hotkeys.toggle);
    ui.label(caption(
        if fixed {
            "Stops at the limit.".to_string()
        } else {
            format!("Stop with {}.", toggle_key)
        },
        p,
    ));
}

fn settings_panel(ui: &mut egui::Ui, app: &mut App, p: Palette) {
    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(theme::CARD_RADIUS)
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            click_interval(ui, app, p);
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            ui.columns(2, |columns| {
                click_options(&mut columns[0], app, p);
                click_repeat(&mut columns[1], app, p);
            });
        });
}

fn action_button(
    ui: &mut egui::Ui,
    text: &str,
    enabled: bool,
    fill: Color32,
    text_color: Color32,
    stroke: Stroke,
) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(
            RichText::new(text)
                .font(theme::button_font())
                .color(text_color),
        )
        .fill(fill)
        .stroke(stroke)
        .corner_radius(theme::CONTROL_RADIUS)
        .min_size(Vec2::new(168.0, 38.0)),
    )
}

fn action_bar(ui: &mut egui::Ui, app: &mut App, p: Palette) {
    let toggle_key = hotkey_name(&app.hotkeys.toggle);
    let start_text = format!("Start   {}", toggle_key);
    let stop_text = format!("Stop   {}", toggle_key);
    ui.horizontal(|ui| {
        if action_button(
            ui,
            &start_text,
            !app.enabled,
            p.primary,
            p.on_primary,
            Stroke::NONE,
        )
        .clicked()
        {
            app.start();
        }
        if action_button(
            ui,
            &stop_text,
            app.enabled,
            Color32::TRANSPARENT,
            p.text,
            Stroke::new(1.0, p.border),
        )
        .clicked()
        {
            app.stop();
        }
        if action_button(
            ui,
            "Hotkeys",
            true,
            Color32::TRANSPARENT,
            p.text,
            Stroke::new(1.0, p.border),
        )
        .clicked()
        {
            app.open_hotkey_settings();
        }
    });
}

fn footer(ui: &mut egui::Ui, app: &mut App, p: Palette) {
    ui.horizontal(|ui| {
        let mut autostart = app.autostart;
        if ui.checkbox(&mut autostart, "Start on login").changed() {
            app.set_autostart(autostart);
        }
        let inc_key = hotkey_name(&app.hotkeys.increase);
        let dec_key = hotkey_name(&app.hotkeys.decrease);
        let quit_key = hotkey_name(&app.hotkeys.quit);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(caption(
                format!("{} faster  ·  {} slower  ·  {} quit", inc_key, dec_key, quit_key),
                p,
            ));
        });
    });
}

fn hotkey_options() -> [String; 12] {
    std::array::from_fn(|i| format!("KEY_F{}", i + 1))
}

fn hotkey_name(value: &str) -> String {
    value.strip_prefix("KEY_").unwrap_or(value).to_string()
}

fn hotkey_picker(ui: &mut egui::Ui, id: &str, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.add_sized([80.0, 30.0], egui::Label::new(label));
        egui::ComboBox::from_id_salt(id)
            .width(160.0)
            .selected_text(hotkey_name(value))
            .show_ui(ui, |ui| {
                for option in hotkey_options() {
                    ui.selectable_value(value, option.clone(), hotkey_name(&option));
                }
            });
    });
}

fn hotkey_settings_window(app: &mut App, ctx: &egui::Context, p: Palette) {
    if !app.show_hotkey_settings {
        return;
    }
    let mut open = true;
    let mut close_requested = false;
    egui::Window::new("Hotkey settings")
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(300.0)
        .show(ctx, |ui| {
            ui.label(caption("Each action needs a unique F-key.", p));
            ui.add_space(6.0);
            hotkey_picker(ui, "hotkey_toggle", "Toggle", &mut app.hotkey_draft.toggle);
            hotkey_picker(
                ui,
                "hotkey_increase",
                "Faster",
                &mut app.hotkey_draft.increase,
            );
            hotkey_picker(
                ui,
                "hotkey_decrease",
                "Slower",
                &mut app.hotkey_draft.decrease,
            );
            hotkey_picker(ui, "hotkey_quit", "Quit", &mut app.hotkey_draft.quit);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Apply").clicked() && app.apply_hotkey_settings() {
                    close_requested = true;
                }
                if ui.button("Cancel").clicked() {
                    app.cancel_hotkey_settings();
                    close_requested = true;
                }
            });
            if let Some(ref err) = app.last_error {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(err)
                        .font(theme::caption_font())
                        .color(p.text),
                );
            }
        });
    if close_requested {
        open = false;
    }
    if !open && app.show_hotkey_settings {
        app.cancel_hotkey_settings();
    }
}

pub fn render(app: &mut App, ui: &mut egui::Ui) {
    if app.theme_refresh_needed {
        theme::apply(ui.ctx(), app.dark_mode);
        app.theme_refresh_needed = false;
    }
    let p = theme::palette(app.dark_mode);
    egui::CentralPanel::default()
        .frame(
            Frame::new()
                .fill(p.backdrop)
                .inner_margin(Margin::same(theme::PADDING as i8)),
        )
        .show(ui, |ui| {
            header(ui, app, p);
            ui.add_space(theme::GAP);
            settings_panel(ui, app, p);
            ui.add_space(theme::GAP);
            action_bar(ui, app, p);
            if let Some(error) = &app.last_error {
                ui.label(
                    RichText::new(error)
                        .font(theme::caption_font())
                        .color(p.text),
                );
            }
            ui.add_space(4.0);
            footer(ui, app, p);
        });
    hotkey_settings_window(app, ui.ctx(), p);
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
