use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use egui::{FontData, FontFamily, FontId};
use egui_term::TerminalTheme;
use serde::Deserialize;

use crate::state;
use crate::theme::{self, UiTheme};

const DEFAULT_CONFIG: &str = r##"# muxterm configuration - edits apply live while the app is running.

# Built-in themes: "iterm-dark", "dracula", "solarized-dark", "gruvbox-dark"
theme = "iterm-dark"

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
    pub font: FontConfig,
    pub colors: HashMap<String, String>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            theme: "iterm-dark".into(),
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
    pub term_theme: TerminalTheme,
    pub ui: UiTheme,
    pub font: FontId,
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
    let preset = theme::preset(&cfg.theme).unwrap_or_else(|| {
        log::warn!(
            "unknown theme {:?} (available: {}), using iterm-dark",
            cfg.theme,
            theme::PRESET_NAMES.join(", ")
        );
        theme::preset("iterm-dark").unwrap()
    });
    let (term_theme, ui) = theme::build(preset, &cfg.colors);

    let size = cfg.font.size.clamp(6.0, 40.0);
    let font_data = cfg.font.family.as_deref().and_then(load_font_data);

    (
        Style {
            term_theme,
            ui,
            font: FontId::monospace(size),
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
