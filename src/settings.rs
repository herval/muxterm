//! The settings window, drawn as a terminal-style panel: a fixed-width
//! grid of monospace rows with box-drawing borders, `>` selection markers,
//! and `[x]` toggles, so the dialog reads like the app's own terminal
//! content (lazygit-style) rather than a native dialog. Interaction is
//! still egui - rows are clickable, esc closes.

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Rect, Response,
    Sense, Stroke, StrokeKind, TextFormat, Vec2,
};

use muxterm::agent::{self, Agent};

use crate::config;
use crate::theme::{self, UiTheme};

/// Row width in character cells, borders included. Sized so the longest
/// fixed line (the "?" hint row) fits with a little air; shorter rows
/// pad out to meet the right border.
const COLS: usize = 48;

/// What the user changed this frame; the caller persists and reloads.
#[derive(Default)]
pub struct Outcome {
    pub theme: Option<&'static str>,
    pub agent: Option<&'static str>,
    pub copy_on_select: Option<bool>,
    pub pane_titles: Option<bool>,
    pub font_size: Option<f32>,
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ctx: &egui::Context,
    th: &UiTheme,
    font: &FontId,
    theme_name: &str,
    agent: &'static Agent,
    copy_on_select: bool,
    pane_titles: bool,
) -> Outcome {
    let mut out = Outcome::default();
    let grid = Grid {
        font: font.clone(),
        char_w: ctx.fonts(|f| f.glyph_width(font, '─')),
        row_h: ctx.fonts(|f| f.row_height(font)),
        th,
    };

    egui::Window::new("Settings")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .frame(egui::Frame::new().fill(th.bg))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ui.set_width(grid.width());

            grid.border(ui, "┌─", "[ Settings ]", "┐", th.accent);

            grid.border(ui, "├─", "Theme", "┤", th.accent);
            for name in theme::PRESET_NAMES {
                let preset = theme::preset(name).unwrap();
                let selected = theme_name == *name;
                if grid.theme_row(ui, name, preset, selected).clicked()
                    && !selected
                {
                    out.theme = Some(name);
                }
            }

            grid.border(ui, "├─", "Font", "┤", th.accent);
            out.font_size = grid.font_row(ui, font.size);

            grid.border(ui, "├─", "Mouse", "┤", th.accent);
            let mark = if copy_on_select { "[x]" } else { "[ ]" };
            let row = grid.body(
                ui,
                vec![
                    (mark.to_string(), th.accent),
                    (" copy on select".to_string(), th.text),
                ],
                true,
                false,
            );
            if row.clicked() {
                out.copy_on_select = Some(!copy_on_select);
            }
            grid.hint(ui, "mouse selections copy as they finish");

            grid.border(ui, "├─", "Panes", "┤", th.accent);
            let mark = if pane_titles { "[x]" } else { "[ ]" };
            let row = grid.body(
                ui,
                vec![
                    (mark.to_string(), th.accent),
                    (" pane title badges".to_string(), th.text),
                ],
                true,
                false,
            );
            if row.clicked() {
                out.pane_titles = Some(!pane_titles);
            }
            grid.hint(ui, "name chip in each pane's corner");

            grid.border(ui, "├─", "AI agent", "┤", th.accent);
            for a in agent::AGENTS {
                let selected = agent.id == a.id;
                let marker = if selected { "> " } else { "  " };
                let color = if selected { th.accent } else { th.text };
                let row = grid.body(
                    ui,
                    vec![
                        (marker.to_string(), th.accent),
                        (a.label.to_string(), color),
                    ],
                    true,
                    selected,
                );
                if row.clicked() && !selected {
                    out.agent = Some(a.id);
                }
            }
            grid.hint(ui, "type \"?\" at an empty shell prompt to ask");

            grid.border(ui, "├─", "", "┤", th.accent);
            grid.config_row(ui);

            grid.border(ui, "└─", "esc closes", "┘", th.text_dim);
        });

    out
}

/// Character-cell geometry shared by every row: the panel is `COLS` cells
/// wide, one row per line, exactly like a terminal grid.
struct Grid<'a> {
    font: FontId,
    char_w: f32,
    row_h: f32,
    th: &'a UiTheme,
}

impl Grid<'_> {
    fn width(&self) -> f32 {
        COLS as f32 * self.char_w
    }

    /// Paint one full-width row from colored segments. Selected rows get an
    /// accent wash, hovered clickable rows a fainter one.
    fn paint(
        &self,
        ui: &mut egui::Ui,
        segs: &[(String, Color32)],
        clickable: bool,
        selected: bool,
    ) -> Response {
        let sense = if clickable {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, resp) = ui
            .allocate_exact_size(Vec2::new(self.width(), self.row_h), sense);
        let bg = if selected {
            Some(theme::blend(self.th.bg, self.th.accent, 0.22))
        } else if clickable && resp.hovered() {
            Some(theme::blend(self.th.bg, self.th.accent, 0.10))
        } else {
            None
        };
        if let Some(bg) = bg {
            ui.painter().rect_filled(rect, CornerRadius::ZERO, bg);
        }
        let mut job = LayoutJob::default();
        for (text, color) in segs {
            job.append(
                text,
                0.0,
                TextFormat::simple(self.font.clone(), *color),
            );
        }
        let galley = ui.fonts(|f| f.layout_job(job));
        ui.painter().galley(rect.min, galley, self.th.text);
        if clickable {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            resp
        }
    }

    /// A border row: `┌─[ Settings ]───┐`, `├─Theme───┤`, `└─...┘`.
    fn border(
        &self,
        ui: &mut egui::Ui,
        left: &str,
        title: &str,
        right: &str,
        title_color: Color32,
    ) {
        let used = left.chars().count() + title.chars().count();
        let dashes = COLS.saturating_sub(used + right.chars().count());
        self.paint(
            ui,
            &[
                (left.to_string(), self.th.text_dim),
                (title.to_string(), title_color),
                (
                    format!("{}{}", "─".repeat(dashes), right),
                    self.th.text_dim,
                ),
            ],
            false,
            false,
        );
    }

    /// A content row between the side borders: `│ <segments><pad>│`.
    fn body(
        &self,
        ui: &mut egui::Ui,
        segs: Vec<(String, Color32)>,
        clickable: bool,
        selected: bool,
    ) -> Response {
        let used: usize = 2 + segs
            .iter()
            .map(|(t, _)| t.chars().count())
            .sum::<usize>();
        let pad = COLS.saturating_sub(used + 1);
        let mut all = vec![("│ ".to_string(), self.th.text_dim)];
        all.extend(segs);
        all.push((format!("{}│", " ".repeat(pad)), self.th.text_dim));
        self.paint(ui, &all, clickable, selected)
    }

    fn hint(&self, ui: &mut egui::Ui, text: &str) {
        self.body(
            ui,
            vec![(format!("  {text}"), self.th.text_dim)],
            false,
            false,
        );
    }

    /// `> ██████████ name` - five palette swatches as block glyphs, with a
    /// hairline around the strip so light swatches survive light themes.
    fn theme_row(
        &self,
        ui: &mut egui::Ui,
        name: &str,
        preset: &theme::Preset,
        selected: bool,
    ) -> Response {
        let marker = if selected { "> " } else { "  " };
        let mut segs: Vec<(String, Color32)> =
            vec![(marker.to_string(), self.th.accent)];
        let swatches = [
            preset.bg,
            preset.ansi[1],
            preset.ansi[2],
            preset.ansi[4],
            preset.accent,
        ];
        for hex in swatches {
            let c = theme::parse_hex(hex).unwrap_or(Color32::BLACK);
            segs.push(("██".to_string(), c));
        }
        segs.push((" ".to_string(), self.th.text));
        let color = if selected { self.th.accent } else { self.th.text };
        segs.push((name.to_string(), color));
        let resp = self.body(ui, segs, true, selected);
        // Swatch strip occupies cells 4..14 ("│ " + marker before it).
        let strip = Rect::from_min_size(
            resp.rect.min + Vec2::new(4.0 * self.char_w, 0.5),
            Vec2::new(10.0 * self.char_w, self.row_h - 1.0),
        );
        ui.painter().rect_stroke(
            strip,
            CornerRadius::ZERO,
            Stroke::new(1.0, self.th.text_dim),
            StrokeKind::Inside,
        );
        resp
    }

    /// One independently-clickable run of cells inside a horizontal row.
    fn seg(
        &self,
        ui: &mut egui::Ui,
        text: &str,
        color: Color32,
        clickable: bool,
    ) -> Response {
        let w = text.chars().count() as f32 * self.char_w;
        let sense = if clickable {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(w, self.row_h), sense);
        if clickable && resp.hovered() {
            ui.painter().rect_filled(
                rect,
                CornerRadius::ZERO,
                theme::blend(self.th.bg, self.th.accent, 0.10),
            );
        }
        ui.painter().text(
            rect.min,
            Align2::LEFT_TOP,
            text,
            self.font.clone(),
            color,
        );
        if clickable {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            resp
        }
    }

    /// `size  [ - ] 14.0 [ + ]`, stepping the old slider's 8..=24 range.
    fn font_row(&self, ui: &mut egui::Ui, size: f32) -> Option<f32> {
        let mut new = None;
        let value = format!(" {:>4.1} ", size);
        ui.horizontal(|ui| {
            self.seg(ui, "│ ", self.th.text_dim, false);
            self.seg(ui, "size  ", self.th.text, false);
            let minus = self.seg(ui, "[ - ]", self.th.accent, true);
            self.seg(ui, &value, self.th.text, false);
            let plus = self.seg(ui, "[ + ]", self.th.accent, true);
            let used = ["│ ", "size  ", "[ - ]", &value, "[ + ]"]
                .iter()
                .map(|s| s.chars().count())
                .sum::<usize>();
            let pad = COLS.saturating_sub(used + 1);
            self.seg(
                ui,
                &format!("{}│", " ".repeat(pad)),
                self.th.text_dim,
                false,
            );
            if minus.clicked() {
                new = Some((size - 0.5).max(8.0));
            }
            if plus.clicked() {
                new = Some((size + 0.5).min(24.0));
            }
        });
        new.filter(|n| *n != size)
    }

    /// `[ edit config file ]  colors & font family`
    fn config_row(&self, ui: &mut egui::Ui) {
        let button = "[ edit config file ]";
        let hint = " colors & font family";
        ui.horizontal(|ui| {
            self.seg(ui, "│ ", self.th.text_dim, false);
            let btn = self.seg(ui, button, self.th.accent, true);
            self.seg(ui, hint, self.th.text_dim, false);
            let used = ["│ ", button, hint]
                .iter()
                .map(|s| s.chars().count())
                .sum::<usize>();
            let pad = COLS.saturating_sub(used + 1);
            self.seg(
                ui,
                &format!("{}│", " ".repeat(pad)),
                self.th.text_dim,
                false,
            );
            if btn.clicked() {
                let _ = std::process::Command::new("/usr/bin/open")
                    .arg("-t")
                    .arg(config::path())
                    .spawn();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Render the panel headless and check the character grid: every
    /// full-width row (borders and `│ …` body rows painted as one galley)
    /// must be exactly COLS cells, or the right border zig-zags.
    #[test]
    fn rows_are_exactly_cols_wide() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, ui_theme) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);

        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(800.0, 600.0),
            )),
            ..Default::default()
        };
        let frame = |ctx: &egui::Context| {
            show(
                ctx,
                &ui_theme,
                &font,
                "iterm-dark",
                agent::default_agent(),
                true,
                true,
            );
        };
        // A window's first frame is an invisible sizing pass; the second
        // frame paints for real.
        let _ = ctx.run(input.clone(), frame);
        let output = ctx.run(input, frame);

        let mut full_rows = 0;
        for clipped in &output.shapes {
            collect_rows(&clipped.shape, &mut full_rows);
        }
        // Top + bottom + 6 section dividers + 7 themes + 2 agents +
        // 2 checkboxes + 3 hints: the seg-built rows (font size, edit
        // config) are painted piecewise and aren't counted here.
        assert!(
            full_rows >= 20,
            "expected the panel's full-width rows, found {full_rows}"
        );
    }

    #[test]
    #[ignore]
    fn print_panel() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, ui_theme) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(800.0, 600.0),
            )),
            ..Default::default()
        };
        let frame = |ctx: &egui::Context| {
            show(
                ctx,
                &ui_theme,
                &font,
                "iterm-dark",
                agent::default_agent(),
                true,
                true,
            );
        };
        let _ = ctx.run(input.clone(), frame);
        let output = ctx.run(input, frame);

        let mut texts: Vec<(i32, f32, String)> = Vec::new();
        fn walk(shape: &egui::Shape, texts: &mut Vec<(i32, f32, String)>) {
            match shape {
                egui::Shape::Text(t) => texts.push((
                    t.pos.y.round() as i32,
                    t.pos.x,
                    t.galley.text().to_string(),
                )),
                egui::Shape::Vec(v) => {
                    for s in v {
                        walk(s, texts);
                    }
                },
                _ => {},
            }
        }
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut texts);
        }
        texts.sort_by(|a, b| (a.0, a.1).partial_cmp(&(b.0, b.1)).unwrap());
        let mut last_y = i32::MIN;
        for (y, _, s) in texts {
            if y != last_y {
                println!();
                last_y = y;
            }
            print!("{s}");
        }
        println!();
    }

    fn collect_rows(shape: &egui::Shape, full_rows: &mut usize) {
        match shape {
            egui::Shape::Text(text) => {
                let s = text.galley.text();
                let starts_bordered = s.starts_with('┌')
                    || s.starts_with('├')
                    || s.starts_with('└')
                    || (s.starts_with('│') && s.chars().count() > 2);
                if starts_bordered {
                    assert_eq!(
                        s.chars().count(),
                        COLS,
                        "row has wrong width: {s:?}"
                    );
                    *full_rows += 1;
                }
            },
            egui::Shape::Vec(shapes) => {
                for s in shapes {
                    collect_rows(s, full_rows);
                }
            },
            _ => {},
        }
    }
}
