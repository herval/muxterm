use egui::{Color32, FontId};
use egui_term::{ColorPalette, TerminalTheme};

pub const BG: Color32 = Color32::from_rgb(0x1d, 0x1e, 0x23);
pub const TAB_BAR_BG: Color32 = Color32::from_rgb(0x15, 0x16, 0x1a);
pub const TAB_ACTIVE_BG: Color32 = Color32::from_rgb(0x2c, 0x2d, 0x33);
pub const TAB_HOVER_BG: Color32 = Color32::from_rgb(0x24, 0x25, 0x2b);
pub const ACCENT: Color32 = Color32::from_rgb(0x4a, 0x90, 0xd9);
pub const DIVIDER: Color32 = Color32::from_rgb(0x2e, 0x2f, 0x36);
pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe6, 0xe6);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8a, 0x8b, 0x92);

pub fn font() -> FontId {
    FontId::monospace(14.0)
}

/// iTerm2 default-dark-ish ANSI palette (blue nudged lighter for legibility).
pub fn terminal_theme() -> TerminalTheme {
    TerminalTheme::new(Box::new(ColorPalette {
        background: "#1d1e23".into(),
        foreground: "#c7c7c7".into(),
        black: "#000000".into(),
        red: "#c91b00".into(),
        green: "#00c200".into(),
        yellow: "#c7c400".into(),
        blue: "#3b6fd4".into(),
        magenta: "#ca30c7".into(),
        cyan: "#00c5c7".into(),
        white: "#c7c7c7".into(),
        bright_black: "#686868".into(),
        bright_red: "#ff6e67".into(),
        bright_green: "#5ffa68".into(),
        bright_yellow: "#fffc67".into(),
        bright_blue: "#6871ff".into(),
        bright_magenta: "#ff77ff".into(),
        bright_cyan: "#60fdff".into(),
        bright_white: "#ffffff".into(),
        ..ColorPalette::default()
    }))
}

pub fn apply_visuals(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = TAB_BAR_BG;
    visuals.window_fill = BG;
    ctx.set_visuals(visuals);
}
