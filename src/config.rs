use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use egui::{FontData, FontFamily, FontId};
use egui_term::TerminalTheme;
use serde::Deserialize;

use muxterm::state;

use muxterm::agent::{self, Agent};

use crate::theme::{self, UiTheme};

/// The commented seed config written on first run. A function, not a const:
/// the agent list and default are formatted from agent::AGENTS so the seed
/// can't drift from the registry when agents are added.
fn default_config() -> String {
    let agents = agent::AGENTS
        .iter()
        .map(|a| format!("\"{}\" ({})", a.id, a.label))
        .collect::<Vec<_>>()
        .join(", ");
    let default = agent::default_agent().id;
    format!(
        r##"# muxterm configuration - edits apply live while the app is running.

# Built-in themes: "iterm-dark", "bbs", "amber", "iterm-light",
# "github-light". Each is a full look: palette plus its own font (Monaco for
# the iterms, IBM VGA for bbs and amber, SF Mono for github-light).
theme = "iterm-dark"

# Agent CLI behind the "?" prompt line (type "?" at an empty shell
# prompt to ask), one of: {agents}.
agent = "{default}"

# Model passed to the agent as --model. Empty picks the agent's fast
# default; set one of its larger models to trade speed for depth.
# agent_model = ""

# Lines of pane scrollback sent to the agent as context (0 = none).
# agent_context_lines = 200

# How much unfocused split panes fade toward the background (0.0 - 0.8).
# dim_inactive_panes = 0.12

# Show each pane's title in its top-right corner when the tab is split.
# pane_titles = true

# Copy mouse selections straight to the clipboard (iTerm's "copy on
# select"). Off by default: select, then copy explicitly with cmd+c.
copy_on_select = false

# Show the pane's git branch beside the tab title with dirty/ahead-behind
# markers (● branch *changed ↑ahead ↓behind). Local git only, no network.
# git_status = true

# Show the current branch's GitHub PR beside the tab title (status dot +
# number; click opens the PR page). Needs the gh CLI, authenticated.
# pr_status = true

# Make "#123" in terminal text cmd+clickable when it matches a PR the
# pane's repo is known to have (rides the same PR tracking as pr_status).
# pr_detector = true

# Bounce the dock and post a banner when the muxterm window is unfocused
# and a background pane rings the terminal bell or an agent runs
# `mux notify`. Tab badges are always on; this gates only the OS alerts.
# notifications = true

# Recover context when relaunching after a machine reboot. A reboot kills
# the tmux server (and every pane's live process, cwd, and scrollback);
# these bring back what can be brought back - live processes can't.
# session_recovery = true    # reopen each pane in its last directory
# restore_scrollback = true  # replay saved scrollback into restored panes
# restore_agents = true      # relaunch agent CLIs (interactive, no re-prompt)

# The per-pane title/status bar (pane title + git/PR chips). Each theme
# picks a style and edge; override with "chips" (floating corner badges)
# or "solid" (a full-width strip that reserves a row above/below the
# terminal), and "top" or "bottom".
# bar_style = "chips"
# bar_position = "top"
# The status line uses the theme's own font at a size the theme picked;
# override the face and/or size here, independently of [font] below (same
# name-or-path rule as [font] family).
# bar_font = "Menlo"
# bar_font_size = 12.0

[font]
# Each theme picks its own font; set either key to override it for every
# theme. A family is a name searched in the macOS font folders, or a path
# to a .ttf/.otf/.ttc file.
# family = "Menlo"
# size = 14.0

[colors]
# Override any color of the chosen theme with "#rrggbb":
# background = "#1d1e23"
# foreground = "#c7c7c7"
# accent = "#4a90d9"        # focused-pane border + tab highlight
# black = "#000000"         # also: red green yellow blue magenta cyan white
# bright_black = "#686868"  # also: bright_red ... bright_white
# bar_background = "#00aa00" # the pane title/status bar: strip fill (or
# bar_foreground = "#000000" # chip tint), its text, and its highlight
# bar_accent = "#ffff55"     # (search cursor)
"##
    )
}

#[derive(Deserialize, Debug)]
#[serde(default)]
pub struct ConfigFile {
    pub theme: String,
    /// Agent CLI behind the "?" prompt line: one of agent::AGENTS' ids.
    /// (`agent_model` also lives in the file but is read by `mux ask`,
    /// not the GUI.)
    pub agent: String,
    /// Lines of pane scrollback sent to the agent as context; 0 disables.
    pub agent_context_lines: u32,
    /// 0.0 disables dimming of unfocused panes; 0.12 is the default wash.
    pub dim_inactive_panes: f32,
    /// Show each pane's title in its top-right corner when the tab is split.
    pub pane_titles: bool,
    /// Mouse selections copy to the clipboard as soon as they finish.
    pub copy_on_select: bool,
    /// Badge each pane's git branch + dirty/ahead-behind state on the tab.
    pub git_status: bool,
    /// Poll `gh` for the current branch's PR and badge it on the tab.
    pub pr_status: bool,
    /// cmd+click a `#123` token to open the PR when the number matches a
    /// known PR of the pane's repo.
    pub pr_detector: bool,
    /// Dock bounce + banner on bell/`mux notify` while unfocused.
    pub notifications: bool,
    /// Restore each pane's working directory when relaunching after a machine
    /// reboot (which kills the tmux server, and every cwd it held). Master
    /// switch for reboot recovery; also stops the workspace-root sync from
    /// dropping a worktree link when panes come back empty.
    pub session_recovery: bool,
    /// Capture pane scrollback to disk and replay it into panes restored
    /// after a reboot (needs `session_recovery`). Purely cosmetic - the text
    /// is reprinted, no process comes back.
    pub restore_scrollback: bool,
    /// After a reboot, relaunch a workspace tab's agent CLI interactively -
    /// with no task prompt, so the user resumes rather than re-running mid-
    /// flight work (needs `session_recovery`).
    pub restore_agents: bool,
    /// Pane HUD bar edge: "top" | "bottom"; unset keeps the theme's choice.
    pub bar_position: Option<String>,
    /// Pane HUD style: "chips" | "solid"; unset keeps the theme's choice.
    pub bar_style: Option<String>,
    /// Status-line font face, independent of the terminal `[font]`; unset
    /// keeps the theme's own face. A name or path, resolved like `[font]`.
    pub bar_font: Option<String>,
    /// Status-line font size; unset keeps the theme's bar size.
    pub bar_font_size: Option<f32>,
    pub font: FontConfig,
    pub colors: HashMap<String, String>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            theme: "iterm-dark".into(),
            agent: agent::default_agent().id.into(),
            agent_context_lines: 200,
            dim_inactive_panes: 0.12,
            pane_titles: true,
            copy_on_select: false,
            git_status: true,
            pr_status: true,
            pr_detector: true,
            notifications: true,
            session_recovery: true,
            restore_scrollback: true,
            restore_agents: true,
            bar_position: None,
            bar_style: None,
            bar_font: None,
            bar_font_size: None,
            font: FontConfig::default(),
            colors: HashMap::new(),
        }
    }
}

/// User overrides on top of the theme's font: either half alone is fine.
#[derive(Deserialize, Debug, Default)]
#[serde(default)]
pub struct FontConfig {
    pub family: Option<String>,
    pub size: Option<f32>,
}

pub struct Style {
    pub name: String,
    pub term_theme: TerminalTheme,
    pub ui: UiTheme,
    pub font: FontId,
    pub agent: &'static Agent,
    pub agent_context_lines: u32,
    pub pane_titles: bool,
    pub copy_on_select: bool,
    pub git_status: bool,
    pub pr_status: bool,
    pub pr_detector: bool,
    pub notifications: bool,
    pub session_recovery: bool,
    pub restore_scrollback: bool,
    pub restore_agents: bool,
}

pub fn path() -> PathBuf {
    state::config_dir().join("config.toml")
}

pub fn mtime() -> Option<SystemTime> {
    fs::metadata(path()).and_then(|m| m.modified()).ok()
}

/// Write a commented default config on first run so the options are
/// discoverable. Never overwrites an existing file.
pub fn ensure_default_file() {
    let path = path();
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(&path, default_config()) {
        log::warn!("could not write default config: {e:#}");
    }
}

pub fn load() -> ConfigFile {
    match fs::read_to_string(path()) {
        Err(_) => ConfigFile::default(),
        Ok(text) => match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                log::warn!("config.toml is invalid, using defaults: {e}");
                ConfigFile::default()
            },
        },
    }
}

/// Resolve a parsed config into applied styles plus (optionally) the bytes
/// of the custom terminal font and the status-line (HUD) font to register.
pub fn resolve(
    cfg: &ConfigFile,
) -> (Style, Option<FontData>, Option<FontData>) {
    let (name, preset) = match theme::preset(&cfg.theme) {
        Some(p) => (cfg.theme.clone(), p),
        None => {
            log::warn!(
                "unknown theme {:?} (available: {}), using iterm-dark",
                cfg.theme,
                theme::PRESET_NAMES.join(", ")
            );
            ("iterm-dark".to_string(), theme::preset("iterm-dark").unwrap())
        },
    };
    let (term_theme, mut ui) =
        theme::build(preset, &cfg.colors, cfg.dim_inactive_panes);
    // Flat bar knobs override the theme's choice; bar colors ride
    // `[colors]` into build above.
    match cfg.bar_position.as_deref() {
        None => {},
        Some("top") => ui.bar_edge = theme::BarEdge::Top,
        Some("bottom") => ui.bar_edge = theme::BarEdge::Bottom,
        Some(other) => log::warn!(
            "config: bar_position must be \"top\" or \"bottom\", got {other:?}"
        ),
    }
    match cfg.bar_style.as_deref() {
        None => {},
        Some("chips") => ui.bar_style = theme::BarStyle::Chips,
        Some("solid") => ui.bar_style = theme::BarStyle::Solid,
        Some(other) => log::warn!(
            "config: bar_style must be \"chips\" or \"solid\", got {other:?}"
        ),
    }

    let agent = agent::by_id(&cfg.agent).unwrap_or_else(|| {
        log::warn!(
            "unknown agent {:?} (available: {}), using {}",
            cfg.agent,
            agent::ids().join(", "),
            agent::default_agent().id
        );
        agent::default_agent()
    });

    // The theme's own face, loaded from its source. Used for both the
    // terminal grid and — by default — the status line, so the closure is
    // reusable (it only reads the preset).
    let theme_face = || match preset.font.source {
        theme::FontSource::System(path) => load_font_data(path),
        theme::FontSource::Builtin(bytes) => {
            Some(FontData::from_static(bytes))
        },
    };

    // The theme's font, with `[font]` as user overrides: family replaces
    // the face, size the point size. A family that fails to load falls
    // back to the theme's font, not egui's built-in.
    let size = cfg.font.size.unwrap_or(preset.font.size).clamp(6.0, 40.0);
    let font_data = match cfg.font.family.as_deref() {
        Some(family) => load_font_data(family).or_else(|| theme_face()),
        None => theme_face(),
    };

    // Status-line (HUD) font: the theme's own face at the bar's size, with
    // `bar_font`/`bar_font_size` overriding each half independently of
    // `[font]`. A dedicated family (filled in `install_fonts`) keeps it
    // pinned to the theme even when the terminal font is overridden.
    let bar_size =
        cfg.bar_font_size.unwrap_or(preset.bar.font_size).clamp(6.0, 40.0);
    let bar_font_data = match cfg.bar_font.as_deref() {
        Some(family) => load_font_data(family).or_else(|| theme_face()),
        None => theme_face(),
    };
    ui.bar_font = FontId::new(
        bar_size,
        FontFamily::Name(theme::BAR_FONT_FAMILY.into()),
    );

    (
        Style {
            name,
            term_theme,
            ui,
            font: FontId::monospace(size),
            agent,
            agent_context_lines: cfg.agent_context_lines,
            pane_titles: cfg.pane_titles,
            copy_on_select: cfg.copy_on_select,
            git_status: cfg.git_status,
            pr_status: cfg.pr_status,
            pr_detector: cfg.pr_detector,
            notifications: cfg.notifications,
            session_recovery: cfg.session_recovery,
            restore_scrollback: cfg.restore_scrollback,
            restore_agents: cfg.restore_agents,
        },
        font_data,
        bar_font_data,
    )
}

/// System fonts appended after the primary font as glyph fallbacks. egui has
/// no OS font cascade, so anything the primary font lacks renders as a box
/// unless a registered font covers it: Menlo supplies the dingbat stars CLI
/// spinners use (✻ ✳ ✽), Apple Symbols the braille spinners and misc symbols.
const FALLBACK_FONTS: &[(&str, &str)] = &[
    ("fallback-menlo", "/System/Library/Fonts/Menlo.ttc"),
    ("fallback-apple-symbols", "/System/Library/Fonts/Apple Symbols.ttf"),
];

/// Install the custom monospace terminal font and the status-line (HUD)
/// font (each `None` = fall back to egui's defaults / the fallback faces).
pub fn install_fonts(
    ctx: &egui::Context,
    custom: Option<FontData>,
    bar: Option<FontData>,
) {
    let mut defs = egui::FontDefinitions::default();
    if let Some(data) = custom {
        let name = "muxterm-user-font".to_string();
        defs.font_data.insert(name.clone(), Arc::new(data));
        if let Some(mono) = defs.families.get_mut(&FontFamily::Monospace) {
            mono.insert(0, name);
        }
    }
    // The HUD's own family, so its face stays the theme's regardless of the
    // terminal font. Seeded with the bar face (when it loaded); fallbacks
    // are appended below so glyphs a bitmap face lacks (⏎ ⇧ ·) still render.
    let bar_family = FontFamily::Name(theme::BAR_FONT_FAMILY.into());
    let mut bar_list = Vec::new();
    if let Some(data) = bar {
        let name = "muxterm-bar-font".to_string();
        defs.font_data.insert(name.clone(), Arc::new(data));
        bar_list.push(name);
    }
    defs.families.insert(bar_family.clone(), bar_list);
    for (name, path) in FALLBACK_FONTS {
        let Ok(bytes) = fs::read(path) else {
            log::warn!("fallback font {path} not readable, skipping");
            continue;
        };
        defs.font_data
            .insert((*name).to_string(), Arc::new(FontData::from_owned(bytes)));
        for family in
            [FontFamily::Monospace, FontFamily::Proportional, bar_family.clone()]
        {
            if let Some(list) = defs.families.get_mut(&family) {
                list.push((*name).to_string());
            }
        }
    }
    ctx.set_fonts(defs);
}

/// Persist a theme choice by rewriting only the `theme = ...` line,
/// preserving comments and every other setting in the file.
pub fn set_theme(name: &str) {
    let text = fs::read_to_string(path()).unwrap_or_default();
    let line = format!("theme = \"{name}\"");
    if let Err(e) =
        fs::write(path(), replace_top_level_line(&text, "theme", &line))
    {
        log::warn!("could not save theme choice: {e:#}");
    }
}

/// Same surgical rewrite for the "?" prompt's agent choice.
pub fn set_agent(id: &str) {
    let text = fs::read_to_string(path()).unwrap_or_default();
    let line = format!("agent = \"{id}\"");
    if let Err(e) =
        fs::write(path(), replace_top_level_line(&text, "agent", &line))
    {
        log::warn!("could not save agent choice: {e:#}");
    }
}

/// Same surgical rewrite for the settings window's copy-on-select checkbox.
pub fn set_copy_on_select(on: bool) {
    let text = fs::read_to_string(path()).unwrap_or_default();
    let line = format!("copy_on_select = {on}");
    if let Err(e) = fs::write(
        path(),
        replace_top_level_line(&text, "copy_on_select", &line),
    ) {
        log::warn!("could not save copy_on_select: {e:#}");
    }
}

/// Same surgical rewrite for the settings window's pane-titles checkbox.
pub fn set_pane_titles(on: bool) {
    let text = fs::read_to_string(path()).unwrap_or_default();
    let line = format!("pane_titles = {on}");
    if let Err(e) = fs::write(
        path(),
        replace_top_level_line(&text, "pane_titles", &line),
    ) {
        log::warn!("could not save pane_titles: {e:#}");
    }
}

/// Same surgical rewrite for the settings window's git-status checkbox.
pub fn set_git_status(on: bool) {
    let text = fs::read_to_string(path()).unwrap_or_default();
    let line = format!("git_status = {on}");
    if let Err(e) = fs::write(
        path(),
        replace_top_level_line(&text, "git_status", &line),
    ) {
        log::warn!("could not save git_status: {e:#}");
    }
}

/// Same surgical rewrite for the settings window's PR-status checkbox.
pub fn set_pr_status(on: bool) {
    let text = fs::read_to_string(path()).unwrap_or_default();
    let line = format!("pr_status = {on}");
    if let Err(e) = fs::write(
        path(),
        replace_top_level_line(&text, "pr_status", &line),
    ) {
        log::warn!("could not save pr_status: {e:#}");
    }
}

/// Same surgical rewrite for the settings window's PR-detector checkbox.
pub fn set_pr_detector(on: bool) {
    let text = fs::read_to_string(path()).unwrap_or_default();
    let line = format!("pr_detector = {on}");
    if let Err(e) = fs::write(
        path(),
        replace_top_level_line(&text, "pr_detector", &line),
    ) {
        log::warn!("could not save pr_detector: {e:#}");
    }
}

/// Same surgical rewrite for the settings window's notifications checkbox.
pub fn set_notifications(on: bool) {
    let text = fs::read_to_string(path()).unwrap_or_default();
    let line = format!("notifications = {on}");
    if let Err(e) = fs::write(
        path(),
        replace_top_level_line(&text, "notifications", &line),
    ) {
        log::warn!("could not save notifications: {e:#}");
    }
}

pub fn set_font_size(size: f32) {
    let text = fs::read_to_string(path()).unwrap_or_default();
    if let Err(e) = fs::write(path(), replace_size_line(&text, size)) {
        log::warn!("could not save font size: {e:#}");
    }
}

fn replace_top_level_line(text: &str, key: &str, line: &str) -> String {
    let prefix_spaced = format!("{key} =");
    let prefix_tight = format!("{key}=");
    let mut out: Vec<String> = Vec::new();
    let mut in_table = false;
    let mut replaced = false;
    for l in text.lines() {
        let t = l.trim_start();
        // Only top-level keys count: an identically named key inside a
        // [table] (e.g. a future [agent] section) must be left alone.
        if t.starts_with('[') {
            in_table = true;
        }
        if !replaced
            && !in_table
            && (t.starts_with(&prefix_spaced) || t.starts_with(&prefix_tight))
        {
            out.push(line.to_string());
            replaced = true;
        } else {
            out.push(l.to_string());
        }
    }
    if !replaced {
        // Top-level keys must precede any [table] in TOML.
        out.insert(0, line.to_string());
    }
    out.join("\n") + "\n"
}

fn replace_size_line(text: &str, size: f32) -> String {
    let line = format!("size = {size:.1}");
    let mut out: Vec<String> = Vec::new();
    let mut section = String::new();
    let mut replaced = false;
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            section = t.to_string();
        }
        if !replaced
            && section == "[font]"
            && (t.starts_with("size =") || t.starts_with("size="))
        {
            out.push(line.clone());
            replaced = true;
        } else {
            out.push(l.to_string());
        }
    }
    if !replaced {
        if let Some(pos) = out.iter().position(|l| l.trim() == "[font]") {
            out.insert(pos + 1, line);
        } else {
            out.push(String::new());
            out.push("[font]".to_string());
            out.push(line);
        }
    }
    out.join("\n") + "\n"
}

/// Find font bytes by absolute path or by name in the macOS font folders.
fn load_font_data(family: &str) -> Option<FontData> {
    let read = |p: PathBuf| -> Option<Vec<u8>> {
        p.is_file().then(|| fs::read(&p).ok()).flatten()
    };

    let bytes = if family.contains('/') {
        read(PathBuf::from(family))
    } else {
        let dirs = [
            dirs::home_dir().map(|h| h.join("Library/Fonts")),
            Some(PathBuf::from("/Library/Fonts")),
            Some(PathBuf::from("/System/Library/Fonts")),
            Some(PathBuf::from("/System/Library/Fonts/Supplemental")),
        ];
        let no_space = family.replace(' ', "");
        let stems =
            [family.to_string(), format!("{family}-Regular"), no_space.clone(), format!("{no_space}-Regular")];
        let mut found = None;
        'outer: for dir in dirs.into_iter().flatten() {
            for stem in &stems {
                for ext in ["ttf", "otf", "ttc"] {
                    if let Some(b) = read(dir.join(format!("{stem}.{ext}"))) {
                        found = Some(b);
                        break 'outer;
                    }
                }
            }
        }
        found
    };

    match bytes {
        Some(bytes) => Some(FontData::from_owned(bytes)),
        None => {
            log::warn!(
                "font {family:?} not found (searched ~/Library/Fonts, \
                 /Library/Fonts, /System/Library/Fonts); falling back"
            );
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replace_theme_line(text: &str, name: &str) -> String {
        replace_top_level_line(text, "theme", &format!("theme = \"{name}\""))
    }

    #[test]
    fn fallback_fonts_cover_cli_spinner_glyphs() {
        // Claude Code's dingbat spinner (✻ ✳ ✽ ✢) and braille spinners (⠋)
        // are outside egui's built-in fonts; without the system fallbacks
        // they render as boxes.
        let ctx = egui::Context::default();
        install_fonts(&ctx, None, None);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let font = FontId::monospace(14.0);
        ctx.fonts(|f| {
            assert!(f.has_glyphs(&font, "✻✳✽✢✶✦★"), "dingbats missing");
            assert!(f.has_glyphs(&font, "⠋⠙⠹"), "braille missing");
            // Block elements (Claude Code's banner art, TUI gauges) come
            // from Hack itself; keep them covered if the stack changes.
            assert!(
                f.has_glyphs(&font, "▐▛█▜▌▘▝▎░▒▓▀▄"),
                "block elements missing"
            );
        });
    }

    /// Resolve a theme + [font] overrides and install the result, so
    /// glyph coverage can be asserted against the actual primary font.
    fn install_theme(theme: &str, font: FontConfig) -> egui::Context {
        let cfg = ConfigFile {
            theme: theme.into(),
            font,
            ..ConfigFile::default()
        };
        let (_, font_data, bar_data) = resolve(&cfg);
        assert!(font_data.is_some(), "{theme}: no font resolved");
        let ctx = egui::Context::default();
        install_fonts(&ctx, font_data, bar_data);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        ctx
    }

    #[test]
    fn bbs_bundled_font_covers_cp437_art() {
        // The whole point of the bundled Px437 face: box drawing, shades,
        // and the card suits BBS ANSI art is made of.
        let ctx = install_theme("bbs", FontConfig::default());
        ctx.fonts(|f| {
            assert!(
                f.has_glyphs(
                    &FontId::monospace(16.0),
                    "░▒▓█▄▀│┤╣║╗╝┌└├─┼═╔╚╩╦╠╬♥♦♣♠"
                ),
                "CP437 art glyphs missing from Px437"
            );
        });
    }

    #[test]
    fn github_light_font_loads_and_covers_ascii() {
        // The SFNSMono canary: system SF Mono has quirky internals, so
        // prove it parses and shapes ASCII before shipping it as a theme
        // font. If this ever fails, point the preset at Menlo.ttc.
        let ctx = install_theme("github-light", FontConfig::default());
        ctx.fonts(|f| {
            assert!(
                f.has_glyphs(
                    &FontId::monospace(12.0),
                    "abcXYZ019{}[]|~"
                ),
                "SF Mono failed to shape ASCII"
            );
        });
    }

    #[test]
    fn bar_font_family_carries_the_theme_face() {
        // The status line must render in the theme's own face: bbs's bar
        // family should carry the bundled Px437 VGA glyphs (CP437 art), not
        // a generic fallback. Proves the theme bytes reach `muxterm-bar` and
        // that `ui.bar_font` (a Name-family FontId) resolves to them.
        let cfg = ConfigFile {
            theme: "bbs".into(),
            ..ConfigFile::default()
        };
        let (style, font_data, bar_data) = resolve(&cfg);
        let ctx = egui::Context::default();
        install_fonts(&ctx, font_data, bar_data);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        ctx.fonts(|f| {
            assert!(
                f.has_glyphs(&style.ui.bar_font, "░▒▓█│┤╣╬"),
                "bbs bar font missing CP437 art (theme face not in bar family)"
            );
        });
    }

    #[test]
    fn theme_font_yields_to_config_overrides() {
        // No overrides: the theme's own font and size.
        let (style, font_data, _) = resolve(&ConfigFile::default());
        assert_eq!(style.font.size, 12.0); // iterm-dark = Monaco 12
        assert!(font_data.is_some(), "Monaco failed to load");
        let bbs = ConfigFile {
            theme: "bbs".into(),
            ..ConfigFile::default()
        };
        let (style, _, _) = resolve(&bbs);
        assert_eq!(style.font.size, 16.0);

        // [font] size wins over the theme's.
        let cfg = ConfigFile {
            theme: "bbs".into(),
            font: FontConfig {
                family: None,
                size: Some(13.0),
            },
            ..ConfigFile::default()
        };
        let (style, _, _) = resolve(&cfg);
        assert_eq!(style.font.size, 13.0);

        // A [font] family that fails to load falls back to the theme's
        // font, not to None (which would mean egui's built-in).
        let cfg = ConfigFile {
            theme: "bbs".into(),
            font: FontConfig {
                family: Some("NoSuchFace".into()),
                size: None,
            },
            ..ConfigFile::default()
        };
        let (style, font_data, _) = resolve(&cfg);
        assert!(font_data.is_some(), "theme font fallback lost");
        assert_eq!(style.font.size, 16.0);
    }

    #[test]
    fn seed_config_leaves_font_to_the_theme() {
        let cfg: ConfigFile = toml::from_str(&default_config()).unwrap();
        assert_eq!(cfg.font.size, None);
        assert_eq!(cfg.font.family, None);
        assert_eq!(cfg.bar_position, None);
        assert_eq!(cfg.bar_style, None);
        assert_eq!(cfg.bar_font, None);
        assert_eq!(cfg.bar_font_size, None);
    }

    #[test]
    fn bar_font_defaults_to_theme_size_and_overrides_independently() {
        // Unset: the status-line font is the theme's bar size, and its
        // face is the theme's own (bar bytes resolved).
        let (style, _, bar_data) = resolve(&ConfigFile::default());
        assert_eq!(style.ui.bar_font.size, 11.0); // iterm-dark CHIPS
        assert!(bar_data.is_some(), "theme bar face lost");
        let bbs = ConfigFile {
            theme: "bbs".into(),
            ..ConfigFile::default()
        };
        let (style, _, _) = resolve(&bbs);
        assert_eq!(style.ui.bar_font.size, 16.0); // bbs solid strip

        // bar_font_size overrides the bar size, independently of [font] size.
        let cfg = ConfigFile {
            theme: "bbs".into(),
            font: FontConfig {
                family: None,
                size: Some(20.0),
            },
            bar_font_size: Some(9.0),
            ..ConfigFile::default()
        };
        let (style, _, _) = resolve(&cfg);
        assert_eq!(style.font.size, 20.0); // terminal font unaffected
        assert_eq!(style.ui.bar_font.size, 9.0); // bar font follows its own key

        // A bar_font family that fails to load falls back to the theme face,
        // never to None (which would leave the family fallbacks-only).
        let cfg = ConfigFile {
            theme: "bbs".into(),
            bar_font: Some("NoSuchFace".into()),
            ..ConfigFile::default()
        };
        let (_, _, bar_data) = resolve(&cfg);
        assert!(bar_data.is_some(), "bar font fallback lost");
    }

    #[test]
    fn bar_keys_override_the_theme_and_invalid_values_warn() {
        // Unset keeps each theme's own choice.
        let (style, _, _) = resolve(&ConfigFile::default());
        assert_eq!(style.ui.bar_style, theme::BarStyle::Chips);
        assert_eq!(style.ui.bar_edge, theme::BarEdge::Top);
        let cfg = ConfigFile {
            theme: "github-light".into(),
            ..ConfigFile::default()
        };
        let (style, _, _) = resolve(&cfg);
        assert_eq!(style.ui.bar_style, theme::BarStyle::Solid);
        assert_eq!(style.ui.bar_edge, theme::BarEdge::Bottom);

        // Valid values flip both knobs on any theme...
        let cfg = ConfigFile {
            bar_position: Some("bottom".into()),
            bar_style: Some("solid".into()),
            ..ConfigFile::default()
        };
        let (style, _, _) = resolve(&cfg);
        assert_eq!(style.ui.bar_style, theme::BarStyle::Solid);
        assert_eq!(style.ui.bar_edge, theme::BarEdge::Bottom);

        // ...and junk warns, keeping the theme's choice.
        let cfg = ConfigFile {
            bar_position: Some("middle".into()),
            bar_style: Some("floaty".into()),
            ..ConfigFile::default()
        };
        let (style, _, _) = resolve(&cfg);
        assert_eq!(style.ui.bar_style, theme::BarStyle::Chips);
        assert_eq!(style.ui.bar_edge, theme::BarEdge::Top);
    }

    #[test]
    fn theme_line_is_replaced_in_place() {
        let text = "# comment\ntheme = \"iterm-dark\"\n\n[font]\nsize = 14.0\n";
        let out = replace_theme_line(text, "bbs");
        assert!(out.contains("theme = \"bbs\""));
        assert!(!out.contains("iterm-dark"));
        assert!(out.contains("# comment"));
        assert!(out.contains("size = 14.0"));
    }

    #[test]
    fn theme_line_is_prepended_when_missing() {
        let out = replace_theme_line("[font]\nsize = 14.0\n", "github-light");
        assert!(out.starts_with("theme = \"github-light\""));
    }

    #[test]
    fn agent_line_is_replaced_in_place() {
        let text = "theme = \"bbs\"\nagent = \"claude\"\n\n[font]\n";
        let out =
            replace_top_level_line(text, "agent", "agent = \"codex\"");
        assert!(out.contains("agent = \"codex\""));
        assert!(!out.contains("claude"));
        assert!(out.contains("theme = \"bbs\""));
    }

    #[test]
    fn agent_line_is_prepended_when_missing_and_tables_are_skipped() {
        let text = "[font]\nagent = \"inside-a-table\"\n";
        let out =
            replace_top_level_line(text, "agent", "agent = \"codex\"");
        assert!(out.starts_with("agent = \"codex\""));
        // The identically named key inside [font] is untouched.
        assert!(out.contains("agent = \"inside-a-table\""));
    }

    #[test]
    fn pane_titles_default_on_and_resolved() {
        assert!(ConfigFile::default().pane_titles);
        let (style, _, _) = resolve(&ConfigFile::default());
        assert!(style.pane_titles);

        let cfg = ConfigFile {
            pane_titles: false,
            ..ConfigFile::default()
        };
        let (style, _, _) = resolve(&cfg);
        assert!(!style.pane_titles);
    }

    #[test]
    fn copy_on_select_defaults_off_and_parses() {
        assert!(!ConfigFile::default().copy_on_select);
        let cfg: ConfigFile = toml::from_str("copy_on_select = true").unwrap();
        assert!(cfg.copy_on_select);
        let (style, _, _) = resolve(&cfg);
        assert!(style.copy_on_select);

        // The default config file documents the real default.
        let cfg: ConfigFile = toml::from_str(&default_config()).unwrap();
        assert!(!cfg.copy_on_select);
        assert_eq!(cfg.agent, agent::default_agent().id);
    }

    #[test]
    fn pr_status_defaults_on_and_parses() {
        assert!(ConfigFile::default().pr_status);
        let cfg: ConfigFile = toml::from_str("pr_status = false").unwrap();
        assert!(!cfg.pr_status);
        let (style, _, _) = resolve(&cfg);
        assert!(!style.pr_status);
    }

    #[test]
    fn pr_detector_defaults_on_and_parses() {
        assert!(ConfigFile::default().pr_detector);
        let cfg: ConfigFile = toml::from_str("pr_detector = false").unwrap();
        assert!(!cfg.pr_detector);
        let (style, _, _) = resolve(&cfg);
        assert!(!style.pr_detector);
    }

    #[test]
    fn git_status_defaults_on_and_parses() {
        assert!(ConfigFile::default().git_status);
        let cfg: ConfigFile = toml::from_str("git_status = false").unwrap();
        assert!(!cfg.git_status);
        let (style, _, _) = resolve(&cfg);
        assert!(!style.git_status);
    }

    #[test]
    fn notifications_default_on_and_parses() {
        assert!(ConfigFile::default().notifications);
        let cfg: ConfigFile =
            toml::from_str("notifications = false").unwrap();
        assert!(!cfg.notifications);
        let (style, _, _) = resolve(&cfg);
        assert!(!style.notifications);
    }

    #[test]
    fn recovery_flags_default_on_and_parse() {
        // All three default on (the chosen "full recovery" scope), and an
        // older config with none of the keys still loads them on.
        let d = ConfigFile::default();
        assert!(d.session_recovery && d.restore_scrollback && d.restore_agents);
        let seed: ConfigFile = toml::from_str(&default_config()).unwrap();
        assert!(seed.session_recovery);
        assert!(seed.restore_scrollback);
        assert!(seed.restore_agents);
        // Each disables independently and reaches the resolved Style.
        let cfg: ConfigFile = toml::from_str(
            "session_recovery = false\nrestore_scrollback = false\nrestore_agents = false",
        )
        .unwrap();
        let (style, _, _) = resolve(&cfg);
        assert!(!style.session_recovery);
        assert!(!style.restore_scrollback);
        assert!(!style.restore_agents);
    }

    #[test]
    fn copy_on_select_line_is_rewritten_at_top_level() {
        let text = "theme = \"bbs\"\ncopy_on_select = true\n\n[font]\n";
        let out = replace_top_level_line(
            text,
            "copy_on_select",
            "copy_on_select = false",
        );
        assert!(out.contains("copy_on_select = false"));
        assert!(!out.contains("copy_on_select = true"));
        assert!(out.contains("theme = \"bbs\""));
    }

    #[test]
    fn unknown_agent_falls_back_to_claude() {
        let cfg = ConfigFile {
            agent: "gpt".into(),
            ..ConfigFile::default()
        };
        let (style, _, _) = resolve(&cfg);
        assert_eq!(style.agent.id, "claude");
    }

    #[test]
    fn size_line_only_touches_font_section() {
        let text = "theme = \"bbs\"\n\n[font]\nsize = 14.0\n\n[colors]\n";
        let out = replace_size_line(text, 16.0);
        assert!(out.contains("size = 16.0"));
        assert!(!out.contains("size = 14.0"));
        let out2 = replace_size_line("theme = \"bbs\"\n", 12.0);
        assert!(out2.contains("[font]\nsize = 12.0"));
        // The seed's size line is commented out (the theme owns the size),
        // so the stepper's first write inserts a live line under [font]
        // and leaves the comment alone.
        let out3 = replace_size_line(&default_config(), 15.0);
        assert!(out3.contains("[font]\nsize = 15.0"));
        assert!(out3.contains("# size = 14.0"));
        let cfg: ConfigFile = toml::from_str(&out3).unwrap();
        assert_eq!(cfg.font.size, Some(15.0));
    }
}
