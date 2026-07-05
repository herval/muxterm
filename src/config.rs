use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use egui::{FontData, FontFamily, FontId};
use egui_term::TerminalTheme;
use serde::Deserialize;

use muxterm::state;

use crate::agent::{self, Agent};
use crate::theme::{self, UiTheme};

const DEFAULT_CONFIG: &str = r##"# muxterm configuration - edits apply live while the app is running.

# Built-in themes: "iterm-dark", "dracula", "solarized-dark", "gruvbox-dark",
#                  "iterm-light", "solarized-light", "github-light"
theme = "iterm-dark"

# Agent CLI behind the "? " prompt line (type "? " at an empty shell
# prompt to ask): "claude" (Claude Code) or "codex".
agent = "claude"

# Lines of pane scrollback sent to the agent as context (0 = none).
# agent_context_lines = 200

# How much unfocused split panes fade toward the background (0.0 - 0.8).
# dim_inactive_panes = 0.12

[font]
# Monospace font: a name searched in the macOS font folders, or a path to a
# .ttf/.otf/.ttc file. Comment out for the built-in font (Hack).
# family = "Menlo"
size = 14.0

[colors]
# Override any color of the chosen theme with "#rrggbb":
# background = "#1d1e23"
# foreground = "#c7c7c7"
# accent = "#4a90d9"        # focused-pane border + tab highlight
# black = "#000000"         # also: red green yellow blue magenta cyan white
# bright_black = "#686868"  # also: bright_red ... bright_white
"##;

#[derive(Deserialize, Debug)]
#[serde(default)]
pub struct ConfigFile {
    pub theme: String,
    /// Agent CLI behind the "? " prompt line: "claude" or "codex".
    pub agent: String,
    /// Lines of pane scrollback sent to the agent as context; 0 disables.
    pub agent_context_lines: u32,
    /// 0.0 disables dimming of unfocused panes; 0.12 is the default wash.
    pub dim_inactive_panes: f32,
    pub font: FontConfig,
    pub colors: HashMap<String, String>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            theme: "iterm-dark".into(),
            agent: "claude".into(),
            agent_context_lines: 200,
            dim_inactive_panes: 0.12,
            font: FontConfig::default(),
            colors: HashMap::new(),
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(default)]
pub struct FontConfig {
    pub family: Option<String>,
    pub size: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: None,
            size: 14.0,
        }
    }
}

pub struct Style {
    pub name: String,
    pub term_theme: TerminalTheme,
    pub ui: UiTheme,
    pub font: FontId,
    pub agent: &'static Agent,
    pub agent_context_lines: u32,
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
    if let Err(e) = fs::write(&path, DEFAULT_CONFIG) {
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
/// of a custom font to register.
pub fn resolve(cfg: &ConfigFile) -> (Style, Option<FontData>) {
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
    let (term_theme, ui) =
        theme::build(preset, &cfg.colors, cfg.dim_inactive_panes);

    let agent = agent::by_id(&cfg.agent).unwrap_or_else(|| {
        log::warn!(
            "unknown agent {:?} (available: {}), using {}",
            cfg.agent,
            agent::AGENTS
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>()
                .join(", "),
            agent::default_agent().id
        );
        agent::default_agent()
    });

    let size = cfg.font.size.clamp(6.0, 40.0);
    let font_data = cfg.font.family.as_deref().and_then(load_font_data);

    (
        Style {
            name,
            term_theme,
            ui,
            font: FontId::monospace(size),
            agent,
            agent_context_lines: cfg.agent_context_lines,
        },
        font_data,
    )
}

/// Install the custom monospace font (or reset to egui's default fonts when
/// `custom` is None).
pub fn install_fonts(ctx: &egui::Context, custom: Option<FontData>) {
    let mut defs = egui::FontDefinitions::default();
    if let Some(data) = custom {
        let name = "muxterm-user-font".to_string();
        defs.font_data.insert(name.clone(), Arc::new(data));
        if let Some(mono) = defs.families.get_mut(&FontFamily::Monospace) {
            mono.insert(0, name);
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

/// Same surgical rewrite for the "? " prompt's agent choice.
pub fn set_agent(id: &str) {
    let text = fs::read_to_string(path()).unwrap_or_default();
    let line = format!("agent = \"{id}\"");
    if let Err(e) =
        fs::write(path(), replace_top_level_line(&text, "agent", &line))
    {
        log::warn!("could not save agent choice: {e:#}");
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
                 /Library/Fonts, /System/Library/Fonts); using built-in"
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
    fn theme_line_is_replaced_in_place() {
        let text = "# comment\ntheme = \"iterm-dark\"\n\n[font]\nsize = 14.0\n";
        let out = replace_theme_line(text, "dracula");
        assert!(out.contains("theme = \"dracula\""));
        assert!(!out.contains("iterm-dark"));
        assert!(out.contains("# comment"));
        assert!(out.contains("size = 14.0"));
    }

    #[test]
    fn theme_line_is_prepended_when_missing() {
        let out = replace_theme_line("[font]\nsize = 14.0\n", "gruvbox-dark");
        assert!(out.starts_with("theme = \"gruvbox-dark\""));
    }

    #[test]
    fn agent_line_is_replaced_in_place() {
        let text = "theme = \"dracula\"\nagent = \"claude\"\n\n[font]\n";
        let out =
            replace_top_level_line(text, "agent", "agent = \"codex\"");
        assert!(out.contains("agent = \"codex\""));
        assert!(!out.contains("claude"));
        assert!(out.contains("theme = \"dracula\""));
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
    fn unknown_agent_falls_back_to_claude() {
        let cfg = ConfigFile {
            agent: "gpt".into(),
            ..ConfigFile::default()
        };
        let (style, _) = resolve(&cfg);
        assert_eq!(style.agent.id, "claude");
    }

    #[test]
    fn size_line_only_touches_font_section() {
        let text = "theme = \"dracula\"\n\n[font]\nsize = 14.0\n\n[colors]\n";
        let out = replace_size_line(text, 16.0);
        assert!(out.contains("size = 16.0"));
        assert!(!out.contains("size = 14.0"));
        let out2 = replace_size_line("theme = \"dracula\"\n", 12.0);
        assert!(out2.contains("[font]\nsize = 12.0"));
    }
}
