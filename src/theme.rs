use std::collections::HashMap;

use egui::Color32;
use egui_term::{ColorPalette, TerminalTheme};

/// Chrome colors derived from the terminal palette (plus overrides).
#[derive(Clone, Debug)]
pub struct UiTheme {
    pub bg: Color32,
    pub tab_bar_bg: Color32,
    pub tab_active_bg: Color32,
    pub tab_hover_bg: Color32,
    pub divider: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    /// Translucent wash painted over unfocused panes (bg at some alpha).
    pub dim_overlay: Color32,
    /// PR-status chip colors, straight from the palette's ANSI slots so
    /// they follow the theme like the rest of the chrome.
    pub status_ok: Color32,
    pub status_warn: Color32,
    pub status_err: Color32,
    pub status_merged: Color32,
}

pub struct Preset {
    pub bg: &'static str,
    pub fg: &'static str,
    pub accent: &'static str,
    /// black, red, green, yellow, blue, magenta, cyan, white, then brights.
    pub ansi: [&'static str; 16],
}

pub const PRESET_NAMES: &[&str] = &[
    "iterm-dark",
    "dracula",
    "solarized-dark",
    "gruvbox-dark",
    "iterm-light",
    "solarized-light",
    "github-light",
];

pub fn preset(name: &str) -> Option<&'static Preset> {
    match name {
        "iterm-dark" => Some(&Preset {
            bg: "#1d1e23",
            fg: "#c7c7c7",
            accent: "#4a90d9",
            ansi: [
                "#000000", "#c91b00", "#00c200", "#c7c400", "#3b6fd4",
                "#ca30c7", "#00c5c7", "#c7c7c7", "#686868", "#ff6e67",
                "#5ffa68", "#fffc67", "#6871ff", "#ff77ff", "#60fdff",
                "#ffffff",
            ],
        }),
        "dracula" => Some(&Preset {
            bg: "#282a36",
            fg: "#f8f8f2",
            accent: "#bd93f9",
            ansi: [
                "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9",
                "#ff79c6", "#8be9fd", "#f8f8f2", "#6272a4", "#ff6e6e",
                "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff",
                "#ffffff",
            ],
        }),
        "solarized-dark" => Some(&Preset {
            bg: "#002b36",
            fg: "#839496",
            accent: "#268bd2",
            ansi: [
                "#073642", "#dc322f", "#859900", "#b58900", "#268bd2",
                "#d33682", "#2aa198", "#eee8d5", "#002b36", "#cb4b16",
                "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1",
                "#fdf6e3",
            ],
        }),
        "gruvbox-dark" => Some(&Preset {
            bg: "#282828",
            fg: "#ebdbb2",
            accent: "#83a598",
            ansi: [
                "#282828", "#cc241d", "#98971a", "#d79921", "#458588",
                "#b16286", "#689d6a", "#a89984", "#928374", "#fb4934",
                "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c",
                "#ebdbb2",
            ],
        }),
        "iterm-light" => Some(&Preset {
            bg: "#ffffff",
            fg: "#000000",
            accent: "#3b78d1",
            ansi: [
                "#000000", "#c91b00", "#00a600", "#997d00", "#0225c7",
                "#ca30c7", "#009393", "#c7c7c7", "#686868", "#e02d24",
                "#00b310", "#8f7500", "#4a63e0", "#c540c2", "#00a3a3",
                "#f2f2f2",
            ],
        }),
        "solarized-light" => Some(&Preset {
            bg: "#fdf6e3",
            fg: "#657b83",
            accent: "#268bd2",
            ansi: [
                "#073642", "#dc322f", "#859900", "#b58900", "#268bd2",
                "#d33682", "#2aa198", "#eee8d5", "#002b36", "#cb4b16",
                "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1",
                "#fdf6e3",
            ],
        }),
        "github-light" => Some(&Preset {
            bg: "#ffffff",
            fg: "#24292f",
            accent: "#0969da",
            ansi: [
                "#24292e", "#cf222e", "#116329", "#9a6700", "#0969da",
                "#8250df", "#1b7c83", "#6e7781", "#57606a", "#a40e26",
                "#1a7f37", "#bf8700", "#218bff", "#a475f9", "#3192aa",
                "#8c959f",
            ],
        }),
        _ => None,
    }
}

pub fn parse_hex(s: &str) -> Option<Color32> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}

fn to_hex(c: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

pub(crate) fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// tmux copy-mode search highlight (cmd+f), derived from the theme like
/// the rest of the chrome: every match gets a subtle accent wash with
/// its own text kept, the current match inverts to full accent.
pub(crate) fn search_highlight(ui: &UiTheme) -> crate::tmux::SearchStyle {
    crate::tmux::SearchStyle {
        match_bg: to_hex(blend(ui.bg, ui.accent, 0.35)),
        current_bg: to_hex(ui.accent),
        current_fg: to_hex(ui.bg),
    }
}

fn dim_hex(s: &str) -> String {
    let c = parse_hex(s).unwrap_or(Color32::GRAY);
    to_hex(blend(c, Color32::BLACK, 0.3))
}

/// Build the terminal + chrome themes from a preset with hex-string
/// overrides (invalid values are dropped with a warning).
pub fn build(
    preset: &Preset,
    overrides: &HashMap<String, String>,
    dim_inactive: f32,
) -> (TerminalTheme, UiTheme) {
    let mut colors: HashMap<&str, String> = HashMap::new();
    colors.insert("background", preset.bg.into());
    colors.insert("foreground", preset.fg.into());
    colors.insert("accent", preset.accent.into());
    const ANSI_KEYS: [&str; 16] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan",
        "white", "bright_black", "bright_red", "bright_green",
        "bright_yellow", "bright_blue", "bright_magenta", "bright_cyan",
        "bright_white",
    ];
    for (key, value) in ANSI_KEYS.iter().zip(preset.ansi) {
        colors.insert(key, value.into());
    }

    for (key, value) in overrides {
        let known = colors.contains_key(key.as_str());
        if !known {
            log::warn!("config: unknown color key {key:?}");
            continue;
        }
        if parse_hex(value).is_none() {
            log::warn!("config: invalid hex {value:?} for {key:?}");
            continue;
        }
        // keys live as &'static str; re-insert via the matching constant
        if let Some(k) = ANSI_KEYS
            .iter()
            .chain(["background", "foreground", "accent"].iter())
            .find(|k| **k == key.as_str())
        {
            colors.insert(k, value.clone());
        }
    }

    let get = |k: &str| colors[k].clone();
    let palette = ColorPalette {
        background: get("background"),
        foreground: get("foreground"),
        black: get("black"),
        red: get("red"),
        green: get("green"),
        yellow: get("yellow"),
        blue: get("blue"),
        magenta: get("magenta"),
        cyan: get("cyan"),
        white: get("white"),
        bright_black: get("bright_black"),
        bright_red: get("bright_red"),
        bright_green: get("bright_green"),
        bright_yellow: get("bright_yellow"),
        bright_blue: get("bright_blue"),
        bright_magenta: get("bright_magenta"),
        bright_cyan: get("bright_cyan"),
        bright_white: get("bright_white"),
        bright_foreground: None,
        dim_foreground: dim_hex(&get("foreground")),
        dim_black: dim_hex(&get("black")),
        dim_red: dim_hex(&get("red")),
        dim_green: dim_hex(&get("green")),
        dim_yellow: dim_hex(&get("yellow")),
        dim_blue: dim_hex(&get("blue")),
        dim_magenta: dim_hex(&get("magenta")),
        dim_cyan: dim_hex(&get("cyan")),
        dim_white: dim_hex(&get("white")),
    };

    let bg = parse_hex(&get("background")).unwrap();
    let fg = parse_hex(&get("foreground")).unwrap();
    let accent = parse_hex(&get("accent")).unwrap();
    // Chrome must darken subtly on light backgrounds and strongly on dark
    // ones, and text emphasis goes toward the opposite pole of the bg.
    let light = is_light(bg);
    let ui = UiTheme {
        bg,
        tab_bar_bg: blend(bg, Color32::BLACK, if light { 0.07 } else { 0.35 }),
        tab_active_bg: blend(bg, fg, 0.13),
        tab_hover_bg: blend(bg, fg, 0.07),
        divider: blend(bg, fg, 0.12),
        text: blend(
            fg,
            if light { Color32::BLACK } else { Color32::WHITE },
            0.2,
        ),
        text_dim: blend(fg, bg, 0.45),
        accent,
        dim_overlay: Color32::from_rgba_unmultiplied(
            bg.r(),
            bg.g(),
            bg.b(),
            (dim_inactive.clamp(0.0, 0.8) * 255.0) as u8,
        ),
        status_ok: parse_hex(&get("green")).unwrap(),
        status_warn: parse_hex(&get("yellow")).unwrap(),
        status_err: parse_hex(&get("red")).unwrap(),
        status_merged: parse_hex(&get("magenta")).unwrap(),
    };

    (TerminalTheme::new(Box::new(palette)), ui)
}

fn is_light(c: Color32) -> bool {
    0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32
        >= 128.0
}

pub fn apply_visuals(ctx: &egui::Context, ui: &UiTheme) {
    let mut visuals = if is_light(ui.bg) {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    visuals.panel_fill = ui.tab_bar_bg;
    visuals.window_fill = ui.bg;
    ctx.set_visuals(visuals);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_valid_hex() {
        for name in PRESET_NAMES {
            let p = preset(name).unwrap();
            assert!(parse_hex(p.bg).is_some(), "{name} bg");
            assert!(parse_hex(p.fg).is_some(), "{name} fg");
            assert!(parse_hex(p.accent).is_some(), "{name} accent");
            for c in p.ansi {
                assert!(parse_hex(c).is_some(), "{name} ansi {c}");
            }
        }
    }

    #[test]
    fn search_highlight_derives_valid_hex() {
        let (_, ui) =
            build(preset("iterm-dark").unwrap(), &HashMap::new(), 0.25);
        let style = search_highlight(&ui);
        for hex in [&style.match_bg, &style.current_bg, &style.current_fg] {
            assert!(parse_hex(hex).is_some(), "bad hex {hex}");
        }
        assert_eq!(style.current_bg, to_hex(ui.accent));
        assert_eq!(style.current_fg, to_hex(ui.bg));
    }

    #[test]
    fn overrides_apply_and_invalid_are_dropped() {
        let mut overrides = HashMap::new();
        overrides.insert("background".to_string(), "#101010".to_string());
        overrides.insert("red".to_string(), "not-a-color".to_string());
        overrides.insert("bogus_key".to_string(), "#ffffff".to_string());
        let (_, ui) = build(preset("iterm-dark").unwrap(), &overrides, 0.25);
        assert_eq!(ui.bg, Color32::from_rgb(0x10, 0x10, 0x10));
    }
}
