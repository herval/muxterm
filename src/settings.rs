//! The settings window, drawn as a terminal-style panel: a fixed-width
//! grid of monospace rows with hairline borders, `>` selection markers,
//! and `[x]` toggles, so the dialog reads like the app's own terminal
//! content (lazygit-style) rather than a native dialog. Interaction is
//! still egui - rows are clickable, esc closes.
//!
//! The frame and theme swatches are painter primitives, not box-drawing
//! and block glyphs: those come from fallback fonts whose advance widths
//! need not match the primary font's, and egui has no terminal grid to
//! snap them to, so glyph borders drift out of column. Row text must stay
//! ASCII (primary-font glyphs) for the same reason.

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Pos2, Rect,
    Response, Sense, Shadow, Stroke, StrokeKind, TextFormat, Vec2,
};

use muxterm::agent::Agent;

use crate::config;
use crate::theme::{self, UiTheme};

/// Row width in character cells, borders included. Sized so the longest
/// fixed line (the "?" hint row) fits with a little air.
const COLS: usize = 48;

/// What the user changed this frame; the caller persists and reloads.
#[derive(Default)]
pub struct Outcome {
    pub theme: Option<&'static str>,
    pub agent: Option<&'static str>,
    pub copy_on_select: Option<bool>,
    pub pane_titles: Option<bool>,
    pub git_status: Option<bool>,
    pub pr_status: Option<bool>,
    pub notifications: Option<bool>,
    pub font_size: Option<f32>,
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ctx: &egui::Context,
    th: &UiTheme,
    font: &FontId,
    theme_name: &str,
    agent: &'static Agent,
    agents: &[&'static Agent],
    copy_on_select: bool,
    pane_titles: bool,
    git_status: bool,
    pr_status: bool,
    notifications: bool,
) -> Outcome {
    let mut out = Outcome::default();
    let grid = Grid {
        font: font.clone(),
        char_w: ctx.fonts(|f| f.glyph_width(font, ' ')),
        row_h: ctx.fonts(|f| f.row_height(font)),
        th,
    };

    egui::Window::new("Settings")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .frame(egui::Frame::new().fill(th.bg).shadow(Shadow {
            offset: [0, 6],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(100),
        }))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ui.set_width(grid.width());

            grid.divider(ui, "[ Settings ]", th.accent);

            grid.divider(ui, "Theme", th.accent);
            for name in theme::PRESET_NAMES {
                let preset = theme::preset(name).unwrap();
                let selected = theme_name == *name;
                if grid.theme_row(ui, name, preset, selected).clicked()
                    && !selected
                {
                    out.theme = Some(name);
                }
            }

            grid.divider(ui, "Font", th.accent);
            out.font_size = grid.font_row(ui, font.size);
            grid.hint(ui, "overrides the theme's font size");

            grid.divider(ui, "Mouse", th.accent);
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

            grid.divider(ui, "Panes", th.accent);
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

            grid.divider(ui, "Git", th.accent);
            let mark = if git_status { "[x]" } else { "[ ]" };
            let row = grid.body(
                ui,
                vec![
                    (mark.to_string(), th.accent),
                    (" branch status on tabs".to_string(), th.text),
                ],
                true,
                false,
            );
            if row.clicked() {
                out.git_status = Some(!git_status);
            }
            grid.hint(ui, "branch + dirty/ahead-behind state");

            grid.divider(ui, "GitHub", th.accent);
            let mark = if pr_status { "[x]" } else { "[ ]" };
            let row = grid.body(
                ui,
                vec![
                    (mark.to_string(), th.accent),
                    (" PR status on tabs".to_string(), th.text),
                ],
                true,
                false,
            );
            if row.clicked() {
                out.pr_status = Some(!pr_status);
            }
            grid.hint(ui, "branch's PR beside the title; needs gh");

            grid.divider(ui, "Alerts", th.accent);
            let mark = if notifications { "[x]" } else { "[ ]" };
            let row = grid.body(
                ui,
                vec![
                    (mark.to_string(), th.accent),
                    (" notifications".to_string(), th.text),
                ],
                true,
                false,
            );
            if row.clicked() {
                out.notifications = Some(!notifications);
            }
            grid.hint(ui, "dock bounce + banner from background panes");

            grid.divider(ui, "AI agent", th.accent);
            // Pre-filtered by the caller to agents whose CLI is installed.
            for a in agents {
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

            grid.divider(ui, "", th.accent);
            grid.config_row(ui);

            grid.divider(ui, "esc closes", th.text_dim);

            grid.frame_sides(ui);
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

    fn hairline(&self) -> Stroke {
        Stroke::new(1.0, self.th.text_dim)
    }

    /// Paint one full-width row of colored segments, content starting at
    /// cell 2 (inside the frame). Selected rows get an accent wash,
    /// hovered clickable rows a fainter one.
    fn body(
        &self,
        ui: &mut egui::Ui,
        segs: Vec<(String, Color32)>,
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
        for (text, color) in &segs {
            job.append(
                text,
                0.0,
                TextFormat::simple(self.font.clone(), *color),
            );
        }
        let galley = ui.fonts(|f| f.layout_job(job));
        ui.painter().galley(
            rect.min + Vec2::new(2.0 * self.char_w, 0.0),
            galley,
            self.th.text,
        );
        if clickable {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            resp
        }
    }

    /// A section rule: a hairline across the row, interrupted by an
    /// optional title starting at cell 2. The top and bottom rules double
    /// as the frame's horizontal edges.
    fn divider(&self, ui: &mut egui::Ui, title: &str, title_color: Color32) {
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(self.width(), self.row_h),
            Sense::hover(),
        );
        let y = rect.center().y;
        let x0 = rect.min.x + 0.5 * self.char_w;
        let x1 = rect.max.x - 0.5 * self.char_w;
        if title.is_empty() {
            ui.painter().hline(x0..=x1, y, self.hairline());
            return;
        }
        let tx0 = rect.min.x + 2.0 * self.char_w;
        let tx1 = tx0 + title.chars().count() as f32 * self.char_w;
        ui.painter().hline(x0..=tx0, y, self.hairline());
        ui.painter().hline(tx1..=x1, y, self.hairline());
        ui.painter().text(
            Pos2::new(tx0, rect.min.y),
            Align2::LEFT_TOP,
            title,
            self.font.clone(),
            title_color,
        );
    }

    /// The frame's vertical edges: hairlines down cell 0 and cell COLS-1,
    /// meeting the top and bottom rules at their corners. Painted after
    /// the rows so they run over the selection washes, like the border
    /// glyphs used to.
    fn frame_sides(&self, ui: &mut egui::Ui) {
        let panel = ui.min_rect();
        let y = panel.min.y + self.row_h / 2.0
            ..=panel.max.y - self.row_h / 2.0;
        ui.painter().vline(
            panel.min.x + 0.5 * self.char_w,
            y.clone(),
            self.hairline(),
        );
        ui.painter().vline(
            panel.max.x - 0.5 * self.char_w,
            y,
            self.hairline(),
        );
    }

    fn hint(&self, ui: &mut egui::Ui, text: &str) {
        self.body(
            ui,
            vec![(format!("  {text}"), self.th.text_dim)],
            false,
            false,
        );
    }

    /// `> <swatches> name  font` - five palette swatches filled straight
    /// into their cells, with a hairline around the strip so light
    /// swatches survive light themes, and the theme's font trailing dim.
    fn theme_row(
        &self,
        ui: &mut egui::Ui,
        name: &str,
        preset: &theme::Preset,
        selected: bool,
    ) -> Response {
        let marker = if selected { "> " } else { "  " };
        let color = if selected { self.th.accent } else { self.th.text };
        let resp = self.body(
            ui,
            vec![
                (marker.to_string(), self.th.accent),
                // Cells 4..14 stay blank; the swatch strip paints there.
                (" ".repeat(11), self.th.text),
                (format!("{name:<14}"), color),
                (
                    format!(
                        "{} {:.0}",
                        preset.font.label, preset.font.size
                    ),
                    self.th.text_dim,
                ),
            ],
            true,
            selected,
        );
        let strip = Rect::from_min_size(
            resp.rect.min + Vec2::new(4.0 * self.char_w, 0.5),
            Vec2::new(10.0 * self.char_w, self.row_h - 1.0),
        );
        let swatches = [
            preset.bg,
            preset.ansi[1],
            preset.ansi[2],
            preset.ansi[4],
            preset.accent,
        ];
        for (i, hex) in swatches.iter().enumerate() {
            let c = theme::parse_hex(hex).unwrap_or(Color32::BLACK);
            let cell = Rect::from_min_size(
                strip.min + Vec2::new(i as f32 * 2.0 * self.char_w, 0.0),
                Vec2::new(2.0 * self.char_w, strip.height()),
            );
            ui.painter().rect_filled(cell, CornerRadius::ZERO, c);
        }
        ui.painter().rect_stroke(
            strip,
            CornerRadius::ZERO,
            self.hairline(),
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
            self.seg(ui, "  size  ", self.th.text, false);
            let minus = self.seg(ui, "[ - ]", self.th.accent, true);
            self.seg(ui, &value, self.th.text, false);
            let plus = self.seg(ui, "[ + ]", self.th.accent, true);
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
        ui.horizontal(|ui| {
            self.seg(ui, "  ", self.th.text_dim, false);
            let btn =
                self.seg(ui, "[ edit config file ]", self.th.accent, true);
            self.seg(ui, " colors & font family", self.th.text_dim, false);
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
    use muxterm::agent;
    use std::collections::HashMap;

    /// Render the panel headless and check the character grid: every text
    /// run must be pure ASCII (fallback-font glyphs have foreign advance
    /// widths that break the cell math) and must end before the right
    /// border's cell, or content collides with the hairline frame.
    #[test]
    fn text_stays_inside_the_frame() {
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
        let agents: Vec<_> = agent::AGENTS.iter().collect();
        let frame = |ctx: &egui::Context| {
            show(
                ctx,
                &ui_theme,
                &font,
                "iterm-dark",
                agent::default_agent(),
                &agents,
                true,
                true,
                true,
                false,
                true,
            );
        };
        // A window's first frame is an invisible sizing pass; the second
        // frame paints for real.
        let _ = ctx.run(input.clone(), frame);
        let output = ctx.run(input, frame);

        let mut texts: Vec<(f32, f32, f32, String)> = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        // Top + bottom + 7 section rules + 4 themes + 2 agents +
        // 4 checkboxes + 6 hints, plus the seg-built rows in pieces.
        assert!(
            texts.len() >= 20,
            "expected the panel's text runs, found {}",
            texts.len()
        );

        let char_w = ctx.fonts(|f| f.glyph_width(&font, ' '));
        // Every run starts at cell 2 or later, so the leftmost one marks
        // the panel's left edge.
        let left = texts
            .iter()
            .map(|(_, x, ..)| *x)
            .fold(f32::MAX, f32::min)
            - 2.0 * char_w;
        let right = left + (COLS - 1) as f32 * char_w;
        for (_, x, w, text) in texts {
            assert!(text.is_ascii(), "non-ASCII glyphs in row: {text:?}");
            assert!(
                x + w <= right + 0.5,
                "text run overflows the right border: {text:?}"
            );
        }

        // The hairline frame must close: two verticals whose endpoints
        // meet the top and bottom rules, and rules that reach both sides.
        let mut lines: Vec<[egui::Pos2; 2]> = Vec::new();
        for clipped in &output.shapes {
            collect_lines(&clipped.shape, &mut lines);
        }
        let verts: Vec<_> =
            lines.iter().filter(|[a, b]| a.x == b.x).collect();
        assert_eq!(verts.len(), 2, "expected the two frame sides");
        let horiz: Vec<_> =
            lines.iter().filter(|[a, b]| a.y == b.y).collect();
        assert!(horiz.len() >= 8, "expected the section rules");
        let side_x0 = verts[0][0].x.min(verts[1][0].x);
        let side_x1 = verts[0][0].x.max(verts[1][0].x);
        let side_y0 = verts[0][0].y.min(verts[0][1].y);
        let side_y1 = verts[0][0].y.max(verts[0][1].y);
        let near = |a: f32, b: f32| (a - b).abs() < 0.5;
        assert!(
            horiz.iter().any(|[a, _]| near(a.y, side_y0)),
            "no rule meets the sides' top ends"
        );
        assert!(
            horiz.iter().any(|[a, _]| near(a.y, side_y1)),
            "no rule meets the sides' bottom ends"
        );
        for [a, b] in &horiz {
            assert!(
                near(a.x.min(b.x), side_x0) || near(b.x.max(a.x), side_x1),
                "rule floats free of both sides: {a:?}..{b:?}"
            );
        }
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
        let agents: Vec<_> = agent::AGENTS.iter().collect();
        let frame = |ctx: &egui::Context| {
            show(
                ctx,
                &ui_theme,
                &font,
                "iterm-dark",
                agent::default_agent(),
                &agents,
                true,
                true,
                true,
                false,
                true,
            );
        };
        let _ = ctx.run(input.clone(), frame);
        let output = ctx.run(input, frame);

        let mut texts: Vec<(f32, f32, f32, String)> = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        texts.sort_by(|a, b| {
            (a.0.round(), a.1).partial_cmp(&(b.0.round(), b.1)).unwrap()
        });
        let mut last_y = f32::MIN;
        for (y, _, _, s) in texts {
            let y = y.round();
            if y != last_y {
                println!();
                last_y = y;
            }
            print!("{s}");
        }
        println!();
    }

    fn collect_texts(
        shape: &egui::Shape,
        texts: &mut Vec<(f32, f32, f32, String)>,
    ) {
        match shape {
            egui::Shape::Text(t) => texts.push((
                t.pos.y,
                t.pos.x,
                t.galley.size().x,
                t.galley.text().to_string(),
            )),
            egui::Shape::Vec(v) => {
                for s in v {
                    collect_texts(s, texts);
                }
            },
            _ => {},
        }
    }

    fn collect_lines(shape: &egui::Shape, lines: &mut Vec<[egui::Pos2; 2]>) {
        match shape {
            egui::Shape::LineSegment { points, .. } => lines.push(*points),
            egui::Shape::Vec(v) => {
                for s in v {
                    collect_lines(s, lines);
                }
            },
            _ => {},
        }
    }
}
