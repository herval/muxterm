use std::collections::HashMap;

use egui::Color32;
use egui_term::{ColorPalette, TerminalTheme};

/// Chrome look derived from the terminal palette (plus overrides).
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
    /// Focused-pane border stroke width in points (color: `accent`).
    pub border_width: f32,
    /// How the pane HUD renders (floating chips or a solid strip).
    pub bar_style: BarStyle,
    /// Pane edge the title badge, status chips, and search bar sit on.
    pub bar_edge: BarEdge,
    /// Solid-strip fill. In chips mode this equals `bg` unless overridden;
    /// chips apply their own translucency at paint time, keeping the old
    /// pixels.
    pub bar_bg: Color32,
    pub bar_fg: Color32,
    pub bar_fg_dim: Color32,
    /// Highlight color inside the bar (search-bar prefix and cursor).
    pub bar_accent: Color32,
    /// Translucent wash painted over unfocused panes (bg at some alpha).
    pub dim_overlay: Color32,
    /// Heavier wash over every pane of a peeked *archived* workspace, marking
    /// it a read-only preview. Fixed alpha, independent of `dim_inactive_panes`
    /// (which can be 0), so the gray-out is always visible.
    pub archived_overlay: Color32,
    /// PR-status chip colors, straight from the palette's ANSI slots so
    /// they follow the theme like the rest of the chrome.
    pub status_ok: Color32,
    pub status_warn: Color32,
    pub status_err: Color32,
    pub status_merged: Color32,
}

/// Where a preset's font bytes come from.
#[derive(Clone, Copy, Debug)]
pub enum FontSource {
    /// An absolute path into the macOS font folders, resolved like the
    /// `[font] family` override.
    System(&'static str),
    /// A face compiled into the binary.
    Builtin(&'static [u8]),
}

/// A preset's monospace face and the point size the theme was designed
/// around. `[font] family` / `[font] size` each override their half.
#[derive(Clone, Copy, Debug)]
pub struct FontSpec {
    /// Short human name for the settings theme row ("Monaco", "VGA 8x16").
    pub label: &'static str,
    pub source: FontSource,
    pub size: f32,
}

/// Which pane edge the HUD (title badge, status chips, search bar) sits
/// on. In chips mode Bottom floats the badges over the prompt line and
/// tmux's copy-mode indicator, so bottom looks want the solid style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarEdge {
    Top,
    Bottom,
}

/// How the pane HUD renders: floating translucent chips in a corner
/// (overlay, no layout space) or a full-width solid strip that reserves
/// layout space so it never covers terminal content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarStyle {
    Chips,
    Solid,
}

/// A preset's HUD bar. Colors are None = derive from the theme, which in
/// chips mode reproduces the classic translucent-bg / chrome-text look
/// exactly.
pub struct BarSpec {
    pub style: BarStyle,
    pub edge: BarEdge,
    pub bg: Option<&'static str>,
    pub fg: Option<&'static str>,
    pub accent: Option<&'static str>,
}

/// The all-derived floating-chips bar, the look every theme wore before
/// bars became part of a preset's wardrobe.
const CHIPS: BarSpec = BarSpec {
    style: BarStyle::Chips,
    edge: BarEdge::Top,
    bg: None,
    fg: None,
    accent: None,
};

pub struct Preset {
    pub bg: &'static str,
    pub fg: &'static str,
    pub accent: &'static str,
    /// black, red, green, yellow, blue, magenta, cyan, white, then brights.
    pub ansi: [&'static str; 16],
    /// The theme's monospace face; part of the look, like the palette.
    pub font: FontSpec,
    /// Focused-pane border stroke width in points (color stays `accent`).
    pub border_width: f32,
    /// The HUD bar: style, edge, and optional explicit colors.
    pub bar: BarSpec,
}

/// Px437 IBM VGA 8x16 from the Ultimate Oldschool PC Font Pack
/// (int10h.org, CC BY-SA 4.0 — see assets/fonts/LICENSE.txt). At 16pt on
/// a 2x display each glyph lands on exactly 2x the native 8x16 bitmap
/// grid, so the pixels stay crisp.
const PX437_VGA: &[u8] =
    include_bytes!("../assets/fonts/Px437_IBM_VGA_8x16.ttf");

const MONACO: FontSpec = FontSpec {
    label: "Monaco",
    source: FontSource::System("/System/Library/Fonts/Monaco.ttf"),
    size: 12.0,
};

pub const PRESET_NAMES: &[&str] =
    &["iterm-dark", "bbs", "iterm-light", "github-light"];

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
            font: MONACO,
            border_width: 1.0,
            bar: CHIPS,
        }),
        // The DOS/VGA text-mode 16-color palette on a pure-black CRT
        // screen — the colors BBS ANSI art was drawn with — but with the
        // default text glowing phosphor-green like a green-screen terminal
        // and a bright-cyan accent (the color those box-drawing borders and
        // headers loved most). The deviation from stock CGA: the darkest
        // slots are lifted off the floor for readability on pure black —
        // #0000aa "DOS blue", #aa0000 red, and #555555 gray all bottom out
        // in luminance and turn to mud, so blue becomes a deep cornflower,
        // red a brighter scarlet, and the dim gray a legible mid-gray, each
        // keeping its hue without the eye strain.
        "bbs" => Some(&Preset {
            bg: "#000000",
            fg: "#33ff33",
            accent: "#55ffff",
            ansi: [
                "#000000", "#e03c3c", "#00aa00", "#aa5500", "#3b6fd4",
                "#aa00aa", "#00aaaa", "#aaaaaa", "#808080", "#ff5555",
                "#55ff55", "#ffff55", "#6f8fff", "#ff55ff", "#55ffff",
                "#ffffff",
            ],
            font: FontSpec {
                label: "VGA 8x16",
                source: FontSource::Builtin(PX437_VGA),
                size: 16.0,
            },
            // A retro slab of a frame, like a DOS double-line box border.
            border_width: 3.0,
            // Inverse video: the solid ANSI-green status line every DOS
            // program wore, black text on the green slab, DOS-yellow
            // highlights.
            bar: BarSpec {
                style: BarStyle::Solid,
                edge: BarEdge::Top,
                bg: Some("#00aa00"),
                fg: Some("#000000"),
                accent: Some("#ffff55"),
            },
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
            font: MONACO,
            border_width: 1.0,
            bar: CHIPS,
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
            // What github.com's font stack (ui-monospace) resolves to on
            // macOS.
            font: FontSpec {
                label: "SF Mono",
                source: FontSource::System(
                    "/System/Library/Fonts/SFNSMono.ttf",
                ),
                size: 12.0,
            },
            border_width: 1.0,
            // GitHub's blue as a footer strip. The accent must be explicit:
            // the derived one is the same blue as the strip, which would
            // paint the search cursor invisible.
            bar: BarSpec {
                style: BarStyle::Solid,
                edge: BarEdge::Bottom,
                bg: Some("#0969da"),
                fg: Some("#ffffff"),
                accent: Some("#9ecbff"),
            },
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

    // Bar colors never enter the seeded map (their defaults are *built*
    // chrome colors, not preset hex); they resolve after the chrome below.
    const BAR_KEYS: [&str; 3] =
        ["bar_background", "bar_foreground", "bar_accent"];

    for (key, value) in overrides {
        if BAR_KEYS.contains(&key.as_str()) {
            continue;
        }
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
    let text = blend(
        fg,
        if light { Color32::BLACK } else { Color32::WHITE },
        0.2,
    );
    let text_dim = blend(fg, bg, 0.45);

    // Bar colors: [colors] override > preset spec > derived from the
    // chrome. Invalid override hex warns and falls through, like the main
    // loop above.
    let bar_over = |key: &str| {
        overrides.get(key).and_then(|v| {
            let c = parse_hex(v);
            if c.is_none() {
                log::warn!("config: invalid hex {v:?} for {key:?}");
            }
            c
        })
    };
    let spec = &preset.bar;
    let pick = |over: Option<Color32>,
                hex: Option<&'static str>,
                derived: Color32| {
        over.or_else(|| hex.and_then(parse_hex)).unwrap_or(derived)
    };
    let bar_bg = pick(bar_over("bar_background"), spec.bg, bg);
    let bar_fg = pick(bar_over("bar_foreground"), spec.fg, text);
    let bar_accent = pick(bar_over("bar_accent"), spec.accent, accent);
    // A fully-derived bar reuses text_dim byte-for-byte (the chips-mode
    // pixel contract); any explicit half re-derives dim against the
    // actual pair.
    let bar_fg_dim = if bar_bg == bg && bar_fg == text {
        text_dim
    } else {
        blend(bar_fg, bar_bg, 0.45)
    };

    let ui = UiTheme {
        bg,
        tab_bar_bg: blend(bg, Color32::BLACK, if light { 0.07 } else { 0.35 }),
        tab_active_bg: blend(bg, fg, 0.13),
        tab_hover_bg: blend(bg, fg, 0.07),
        divider: blend(bg, fg, 0.12),
        text,
        text_dim,
        accent,
        border_width: preset.border_width,
        bar_style: spec.style,
        bar_edge: spec.edge,
        bar_bg,
        bar_fg,
        bar_fg_dim,
        bar_accent,
        // A wash of the background recedes inactive panes. The same alpha
        // reads far weaker on dark themes — a near-black pour barely touches
        // the sparse bright glyphs, while on a light bg it drops the
        // contrast of dark text hard — so dark mode needs a heavier pour to
        // recede as convincingly. (Same light/dark asymmetry as tab_bar_bg.)
        dim_overlay: {
            let a = dim_inactive.clamp(0.0, 0.8);
            let a = if light { a } else { (a * 5.0).min(0.9) };
            Color32::from_rgba_unmultiplied(
                bg.r(),
                bg.g(),
                bg.b(),
                (a * 255.0) as u8,
            )
        },
        // A firm, fixed pour - stronger than the inactive-pane wash and never
        // zero - so a peeked archived workspace clearly reads as parked. Same
        // dark-needs-heavier asymmetry as dim_overlay.
        archived_overlay: {
            let a = if light { 0.35 } else { 0.62 };
            Color32::from_rgba_unmultiplied(
                bg.r(),
                bg.g(),
                bg.b(),
                (a * 255.0) as u8,
            )
        },
        status_ok: parse_hex(&get("green")).unwrap(),
        status_warn: parse_hex(&get("yellow")).unwrap(),
        status_err: parse_hex(&get("red")).unwrap(),
        status_merged: parse_hex(&get("magenta")).unwrap(),
    };

    (TerminalTheme::new(Box::new(palette)), ui)
}

fn luma(c: Color32) -> f32 {
    0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32
}

pub(crate) fn is_light(c: Color32) -> bool {
    luma(c) >= 128.0
}

/// Minimum luma distance (of 255) for a status color to read against the
/// solid strip; anything closer falls back to `bar_fg`. The chip icons
/// already encode their state as a shape, so the fallback loses only
/// redundancy.
const BAR_CONTRAST: f32 = 48.0;

fn on_bar(c: Color32, bar_bg: Color32, bar_fg: Color32) -> Color32 {
    if (luma(c) - luma(bar_bg)).abs() >= BAR_CONTRAST {
        c
    } else {
        bar_fg
    }
}

/// Everything `draw_pane_title` / `draw_search_bar` color with: the
/// theme-derived translucent chips, or the bar-derived solid strip with
/// the status-contrast guard applied.
pub struct HudColors {
    pub fg: Color32,
    pub fg_dim: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub merged: Color32,
    /// Some = each box paints its own translucent rounded fill (chips);
    /// None = the solid strip already supplied the background.
    pub chip_fill: Option<Color32>,
}

pub fn hud_colors(ui: &UiTheme) -> HudColors {
    match ui.bar_style {
        BarStyle::Chips => HudColors {
            fg: ui.bar_fg,
            fg_dim: ui.bar_fg_dim,
            ok: ui.status_ok,
            warn: ui.status_warn,
            err: ui.status_err,
            merged: ui.status_merged,
            chip_fill: Some(ui.bar_bg.gamma_multiply(0.8)),
        },
        BarStyle::Solid => HudColors {
            fg: ui.bar_fg,
            fg_dim: ui.bar_fg_dim,
            ok: on_bar(ui.status_ok, ui.bar_bg, ui.bar_fg),
            warn: on_bar(ui.status_warn, ui.bar_bg, ui.bar_fg),
            err: on_bar(ui.status_err, ui.bar_bg, ui.bar_fg),
            merged: on_bar(ui.status_merged, ui.bar_bg, ui.bar_fg),
            chip_fill: None,
        },
    }
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
            assert!(
                (6.0..=40.0).contains(&p.font.size),
                "{name} font size"
            );
            assert!(!p.font.label.is_empty(), "{name} font label");
            if let FontSource::Builtin(bytes) = p.font.source {
                assert!(!bytes.is_empty(), "{name} builtin font empty");
            }
            assert!(
                p.border_width > 0.0 && p.border_width <= 8.0,
                "{name} border width"
            );
            for (half, hex) in
                [("bg", p.bar.bg), ("fg", p.bar.fg), ("accent", p.bar.accent)]
            {
                if let Some(hex) = hex {
                    assert!(parse_hex(hex).is_some(), "{name} bar {half}");
                }
            }
        }
    }

    #[test]
    fn build_carries_look_fields() {
        let (_, ui) =
            build(preset("iterm-dark").unwrap(), &HashMap::new(), 0.25);
        assert_eq!(ui.border_width, 1.0);
        assert_eq!(ui.bar_style, BarStyle::Chips);
        assert_eq!(ui.bar_edge, BarEdge::Top);
        // The chips pixel contract: a derived bar IS the chrome, so the
        // unified paint path reproduces the old look byte-for-byte.
        assert_eq!(ui.bar_bg, ui.bg);
        assert_eq!(ui.bar_fg, ui.text);
        assert_eq!(ui.bar_fg_dim, ui.text_dim);
        assert_eq!(ui.bar_accent, ui.accent);
        let (_, ui) = build(preset("bbs").unwrap(), &HashMap::new(), 0.25);
        assert_eq!(ui.border_width, 3.0);
        assert_eq!(ui.bar_style, BarStyle::Solid);
        assert_eq!(ui.bar_edge, BarEdge::Top);
        assert_eq!(ui.bar_bg, Color32::from_rgb(0x00, 0xaa, 0x00));
        assert_eq!(ui.bar_fg, Color32::from_rgb(0x00, 0x00, 0x00));
        assert_eq!(ui.bar_fg_dim, blend(ui.bar_fg, ui.bar_bg, 0.45));
        let (_, ui) =
            build(preset("github-light").unwrap(), &HashMap::new(), 0.25);
        assert_eq!(ui.bar_style, BarStyle::Solid);
        assert_eq!(ui.bar_edge, BarEdge::Bottom);
    }

    #[test]
    fn bar_colors_override_and_invalid_fall_through() {
        let mut overrides = HashMap::new();
        overrides
            .insert("bar_background".to_string(), "#123456".to_string());
        overrides
            .insert("bar_foreground".to_string(), "nonsense".to_string());
        let (_, ui) = build(preset("bbs").unwrap(), &overrides, 0.25);
        assert_eq!(ui.bar_bg, Color32::from_rgb(0x12, 0x34, 0x56));
        // Invalid hex keeps the preset's half...
        assert_eq!(ui.bar_fg, Color32::from_rgb(0x00, 0x00, 0x00));
        // ...and an explicit pair derives dim from what actually built.
        assert_eq!(ui.bar_fg_dim, blend(ui.bar_fg, ui.bar_bg, 0.45));
    }

    #[test]
    fn on_bar_guards_low_contrast() {
        let (_, ui) = build(preset("bbs").unwrap(), &HashMap::new(), 0.25);
        let hud = hud_colors(&ui);
        // ANSI green on the green strip is invisible - the motivating
        // case - and falls back to the bar's black text.
        assert_eq!(ui.status_ok, ui.bar_bg);
        assert_eq!(hud.ok, ui.bar_fg);
        // A distant color passes through untouched.
        assert_eq!(
            on_bar(Color32::WHITE, ui.bar_bg, ui.bar_fg),
            Color32::WHITE
        );
        // Chips mode never rewrites status colors.
        let (_, ui) =
            build(preset("iterm-dark").unwrap(), &HashMap::new(), 0.25);
        let hud = hud_colors(&ui);
        assert_eq!(hud.ok, ui.status_ok);
        assert_eq!(hud.chip_fill, Some(ui.bar_bg.gamma_multiply(0.8)));
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
