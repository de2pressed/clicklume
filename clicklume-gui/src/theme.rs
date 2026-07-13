//! Compact monochrome glass theme.

use eframe::egui::{self, Color32, FontId, Stroke, Vec2};

#[derive(Clone, Copy)]
pub struct Palette {
    pub backdrop: Color32,
    pub surface: Color32,
    pub control: Color32,
    pub hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub primary: Color32,
    pub on_primary: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            backdrop: Color32::from_rgba_unmultiplied(10, 10, 10, 236),
            surface: Color32::from_rgba_unmultiplied(255, 255, 255, 10),
            control: Color32::from_rgba_unmultiplied(255, 255, 255, 12),
            hover: Color32::from_rgba_unmultiplied(255, 255, 255, 20),
            border: Color32::from_rgba_unmultiplied(255, 255, 255, 28),
            text: Color32::from_rgb(238, 238, 238),
            muted: Color32::from_rgb(145, 145, 145),
            primary: Color32::from_rgb(235, 235, 235),
            on_primary: Color32::from_rgb(16, 16, 16),
        }
    } else {
        Palette {
            backdrop: Color32::from_rgba_unmultiplied(244, 244, 244, 236),
            surface: Color32::from_rgba_unmultiplied(255, 255, 255, 128),
            control: Color32::from_rgba_unmultiplied(0, 0, 0, 8),
            hover: Color32::from_rgba_unmultiplied(0, 0, 0, 14),
            border: Color32::from_rgba_unmultiplied(0, 0, 0, 24),
            text: Color32::from_rgb(24, 24, 24),
            muted: Color32::from_rgb(105, 105, 105),
            primary: Color32::from_rgb(24, 24, 24),
            on_primary: Color32::from_rgb(246, 246, 246),
        }
    }
}

pub fn apply(ctx: &egui::Context, dark: bool) {
    let p = palette(dark);
    let active_theme = if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = Color32::TRANSPARENT;
    visuals.window_fill = p.backdrop;
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.extreme_bg_color = p.control;
    visuals.faint_bg_color = p.control;
    visuals.selection.bg_fill = p.primary;
    visuals.selection.stroke = Stroke::new(1.0, p.on_primary);
    visuals.widgets.noninteractive.bg_fill = p.control;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.muted);
    visuals.widgets.inactive.bg_fill = p.control;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    visuals.widgets.hovered.bg_fill = p.hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, p.border);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    visuals.widgets.active.bg_fill = p.hover;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, p.text);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, p.text);
    for widgets in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widgets.corner_radius = CONTROL_RADIUS.into();
    }

    let mut style = (*ctx.style_of(active_theme)).clone();
    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(7.0, 7.0);
    style.spacing.button_padding = Vec2::new(10.0, 6.0);
    style.spacing.interact_size.y = 30.0;
    ctx.set_style_of(active_theme, style);
    ctx.set_theme(active_theme);
}

pub fn title_font() -> FontId {
    FontId::proportional(20.0)
}
pub fn section_font() -> FontId {
    FontId::proportional(12.0)
}
pub fn body_font() -> FontId {
    FontId::proportional(12.0)
}
pub fn caption_font() -> FontId {
    FontId::proportional(10.0)
}
pub fn button_font() -> FontId {
    FontId::proportional(13.0)
}

pub const CARD_RADIUS: f32 = 12.0;
pub const CONTROL_RADIUS: f32 = 8.0;
pub const PADDING: f32 = 16.0;
pub const GAP: f32 = 10.0;
