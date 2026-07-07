//! The cmd+n workspace-creation popup, painted in the same terminal-panel
//! idiom as the settings window: a hairline-framed box of monospace rows with
//! `[ bracketed ]` section rules, an `[x]` toggle, and `>` selection markers.
//! Unlike settings (a pure painter grid) this hosts real text entry, so the
//! folder and task fields stay egui `TextEdit`s - restyled to monospace with a
//! hairline border so they sit inside the same grid.

use std::path::Path;

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Key, Pos2, Response,
    Sense, Shadow, Stroke, TextFormat, Vec2,
};

use muxterm::agent::{self, Agent};

use crate::theme::{self, UiTheme};

/// Live state of the open popup, owned by the App as `Option<NewWorkspaceForm>`.
pub struct NewWorkspaceForm {
    pub folder: String,
    pub create_worktree: bool,
    pub prompt: String,
    pub agent: &'static str,
    pub model: String,
    /// Cached `is_git_repo` for `folder`, refreshed only when the folder text
    /// settles on an existing directory (so we don't spawn `git` per keystroke).
    is_repo: bool,
    checked: String,
}

impl NewWorkspaceForm {
    pub fn new(folder: String, agent: &'static str, model: String) -> Self {
        let mut form = Self {
            folder,
            create_worktree: false,
            prompt: String::new(),
            agent,
            model,
            is_repo: false,
            checked: String::from("\0"), // force a first check
        };
        form.refresh_repo();
        // A git repo defaults the checkbox on - the common case for cmd+n.
        form.create_worktree = form.is_repo;
        form
    }

    /// Re-run the git-repo probe when the folder changed to an existing dir.
    /// Non-dirs (mid-typing paths) skip the subprocess and read as non-repos.
    fn refresh_repo(&mut self) {
        if self.folder == self.checked {
            return;
        }
        self.checked = self.folder.clone();
        let path = Path::new(self.folder.trim());
        self.is_repo = path.is_dir() && crate::workspace::is_git_repo(path);
        if !self.is_repo {
            self.create_worktree = false;
        }
    }
}

pub enum Outcome {
    None,
    Cancel,
    Create,
}

pub fn show(
    ctx: &egui::Context,
    form: &mut NewWorkspaceForm,
    agents: &[&'static Agent],
    th: &UiTheme,
    font: &FontId,
) -> Outcome {
    form.refresh_repo();
    let mut outcome = Outcome::None;

    let panel = Panel {
        font: font.clone(),
        char_w: ctx.fonts(|f| f.glyph_width(font, ' ')),
        row_h: ctx.fonts(|f| f.row_height(font)),
        th,
    };

    egui::Window::new("New Workspace")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .default_width(480.0)
        .frame(
            egui::Frame::new()
                .fill(th.bg)
                .inner_margin(16.0)
                .stroke(panel.hairline())
                .shadow(Shadow {
                    offset: [0, 6],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(100),
                }),
        )
        .show(ctx, |ui| {
            ui.set_width(452.0);
            ui.spacing_mut().item_spacing = Vec2::new(0.0, 6.0);
            // The two TextEdits are the only built-in widgets; drag them into
            // the grid's look (monospace, hairline border, accent on focus).
            panel.style_inputs(ui);

            panel.divider(ui, "[ New workspace ]", th.accent);
            ui.add_space(2.0);

            panel.divider(ui, "Folder", th.accent);
            ui.add_space(2.0);
            ui.add(
                egui::TextEdit::singleline(&mut form.folder)
                    .hint_text("~/path/to/project")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(2.0);
            if form.is_repo {
                if panel
                    .toggle(ui, form.create_worktree, "Create git worktree", true)
                    .clicked()
                {
                    form.create_worktree = !form.create_worktree;
                }
            } else {
                panel.toggle(ui, false, "Create git worktree", false);
                if !form.folder.trim().is_empty() {
                    panel.row(
                        ui,
                        vec![(
                            "not a git repo - worktree off".into(),
                            th.text_dim,
                        )],
                        false,
                    );
                }
            }
            ui.add_space(6.0);

            panel.divider(ui, "What do you want to work on?", th.accent);
            ui.add_space(2.0);
            ui.add(
                egui::TextEdit::multiline(&mut form.prompt)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(6.0);

            panel.divider(ui, "Agent", th.accent);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                panel.seg(ui, " ", th.text_dim, false, false); // 1-cell indent
                // Pre-filtered by the caller to agents whose CLI is installed.
                for a in agents {
                    let selected = form.agent == a.id;
                    let color = if selected { th.accent } else { th.text };
                    let marker = if selected { "> " } else { "  " };
                    let label = format!("{marker}{}   ", a.label);
                    if panel.seg(ui, &label, color, true, selected).clicked()
                        && !selected
                    {
                        form.agent = a.id;
                        // Keep the model valid for the newly-picked agent.
                        if !current_agent(form.agent)
                            .models
                            .contains(&form.model.as_str())
                        {
                            form.model = default_model(form.agent);
                        }
                    }
                }
            });

            panel.divider(ui, "Model", th.accent);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                panel.seg(ui, " ", th.text_dim, false, false); // 1-cell indent
                for m in current_agent(form.agent).models {
                    let selected = form.model == *m;
                    let color = if selected { th.accent } else { th.text };
                    let marker = if selected { "> " } else { "  " };
                    let label = format!("{marker}{m}   ");
                    if panel.seg(ui, &label, color, true, selected).clicked() {
                        form.model = m.to_string();
                    }
                }
            });
            ui.add_space(8.0);

            panel.divider(ui, "", th.text_dim);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                if panel.button(ui, "[ Cancel ]", false).clicked() {
                    outcome = Outcome::Cancel;
                }
                // Right-align the primary action, like the settings config row.
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if panel.button(ui, "[ Create ]", true).clicked() {
                            outcome = Outcome::Create;
                        }
                    },
                );
            });
            ui.add_space(4.0);
            panel.divider(ui, "esc cancels - cmd+enter creates", th.text_dim);
        });

    // cmd+Enter submits from anywhere in the form (Enter alone is a newline in
    // the prompt field). Esc is handled by the App, like the settings window.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::Enter)) {
        outcome = Outcome::Create;
    }
    outcome
}

/// Character-cell geometry and painters shared by the popup's rows, mirroring
/// `settings::Grid` so the two dialogs read as one family.
struct Panel<'a> {
    font: FontId,
    char_w: f32,
    row_h: f32,
    th: &'a UiTheme,
}

impl Panel<'_> {
    fn hairline(&self) -> Stroke {
        Stroke::new(1.0, self.th.text_dim)
    }

    /// Restyle egui's built-in widgets (the two TextEdits) to sit inside the
    /// monospace grid: monospace text, a subtle fill, square corners, and a
    /// hairline border that turns accent on hover/focus.
    fn style_inputs(&self, ui: &mut egui::Ui) {
        ui.style_mut().override_font_id = Some(self.font.clone());
        let accent = Stroke::new(1.0, self.th.accent);
        let v = ui.visuals_mut();
        v.override_text_color = Some(self.th.text);
        v.extreme_bg_color = theme::blend(self.th.bg, self.th.text, 0.06);
        v.selection.bg_fill = theme::blend(self.th.bg, self.th.accent, 0.35);
        v.selection.stroke = accent; // the border of a focused field
        v.widgets.inactive.corner_radius = CornerRadius::ZERO;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, self.th.text_dim);
        v.widgets.hovered.corner_radius = CornerRadius::ZERO;
        v.widgets.hovered.bg_stroke = accent;
        v.widgets.active.corner_radius = CornerRadius::ZERO;
        v.widgets.active.bg_stroke = accent;
    }

    /// A section rule: a hairline across the full content width, interrupted by
    /// an optional inline title. Mirrors `settings::Grid::divider`.
    fn divider(&self, ui: &mut egui::Ui, title: &str, color: Color32) {
        let w = ui.available_width();
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(w, self.row_h), Sense::hover());
        let y = rect.center().y;
        let (x0, x1) = (rect.min.x, rect.max.x);
        if title.is_empty() {
            ui.painter().hline(x0..=x1, y, self.hairline());
            return;
        }
        let tx0 = x0 + self.char_w;
        let tx1 = tx0 + title.chars().count() as f32 * self.char_w;
        let air = 0.4 * self.char_w;
        ui.painter().hline(x0..=(tx0 - air), y, self.hairline());
        ui.painter().hline((tx1 + air)..=x1, y, self.hairline());
        ui.painter().text(
            Pos2::new(tx0, rect.min.y),
            Align2::LEFT_TOP,
            title,
            self.font.clone(),
            color,
        );
    }

    /// A full-width painted row of colored segments, content indented one cell.
    /// Clickable rows get a faint accent wash on hover.
    fn row(
        &self,
        ui: &mut egui::Ui,
        segs: Vec<(String, Color32)>,
        clickable: bool,
    ) -> Response {
        let w = ui.available_width();
        let sense = if clickable { Sense::click() } else { Sense::hover() };
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(w, self.row_h), sense);
        if clickable && resp.hovered() {
            ui.painter().rect_filled(
                rect,
                CornerRadius::ZERO,
                theme::blend(self.th.bg, self.th.accent, 0.10),
            );
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
            rect.min + Vec2::new(self.char_w, 0.0),
            galley,
            self.th.text,
        );
        if clickable {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            resp
        }
    }

    /// A `[x]`/`[ ]` toggle row: accent bracket, themed label; dimmed and inert
    /// when disabled (the folder isn't a git repo).
    fn toggle(
        &self,
        ui: &mut egui::Ui,
        on: bool,
        label: &str,
        enabled: bool,
    ) -> Response {
        let mark = if on { "[x]" } else { "[ ]" };
        let bracket = if enabled { self.th.accent } else { self.th.text_dim };
        let fg = if enabled { self.th.text } else { self.th.text_dim };
        self.row(
            ui,
            vec![(mark.into(), bracket), (format!(" {label}"), fg)],
            enabled,
        )
    }

    /// One independently-clickable run of cells inside a horizontal row, with a
    /// selection wash / hover wash. Mirrors `settings::Grid::seg`.
    fn seg(
        &self,
        ui: &mut egui::Ui,
        text: &str,
        color: Color32,
        clickable: bool,
        selected: bool,
    ) -> Response {
        let w = text.chars().count() as f32 * self.char_w;
        let sense = if clickable { Sense::click() } else { Sense::hover() };
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(w, self.row_h), sense);
        let wash = if selected {
            Some(theme::blend(self.th.bg, self.th.accent, 0.22))
        } else if clickable && resp.hovered() {
            Some(theme::blend(self.th.bg, self.th.accent, 0.10))
        } else {
            None
        };
        if let Some(c) = wash {
            ui.painter().rect_filled(rect, CornerRadius::ZERO, c);
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

    /// A `[ bracketed ]` action button. The primary one carries a standing
    /// accent wash; both deepen on hover.
    fn button(
        &self,
        ui: &mut egui::Ui,
        text: &str,
        primary: bool,
    ) -> Response {
        let w = text.chars().count() as f32 * self.char_w;
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(w, self.row_h), Sense::click());
        let base = if primary { 0.20 } else { 0.0 };
        let hover = if resp.hovered() { 0.12 } else { 0.0 };
        if base + hover > 0.0 {
            ui.painter().rect_filled(
                rect,
                CornerRadius::ZERO,
                theme::blend(self.th.bg, self.th.accent, base + hover),
            );
        }
        let color = if primary { self.th.accent } else { self.th.text };
        ui.painter().text(
            rect.min,
            Align2::LEFT_TOP,
            text,
            self.font.clone(),
            color,
        );
        resp.on_hover_cursor(CursorIcon::PointingHand)
    }
}

fn current_agent(id: &str) -> &'static Agent {
    agent::by_id(id).unwrap_or_else(agent::default_agent)
}

/// The dropdown's default selection for an agent: its first curated model.
pub fn default_model(id: &str) -> String {
    current_agent(id)
        .models
        .first()
        .map(|m| m.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Render the popup headless and check that its labels appear, that the
    /// hand-painted title/marker/button runs are pure ASCII (fallback-font
    /// glyphs carry foreign advance widths that break the char-cell math), and
    /// that the whole thing lays out without panicking. An empty folder keeps
    /// `refresh_repo` from spawning `git`, so the test stays subprocess-free.
    #[test]
    fn popup_renders_ascii_and_labelled() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, ui_theme) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let mut form =
            NewWorkspaceForm::new(String::new(), "claude", "opus".into());

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let agents: Vec<_> = agent::AGENTS.iter().collect();
        let mut frame = |ctx: &egui::Context| {
            let _ = show(ctx, &mut form, &agents, &ui_theme, &font);
        };
        // First frame sizes the window invisibly; the second paints for real.
        let _ = ctx.run(input.clone(), &mut frame);
        let output = ctx.run(input, &mut frame);

        let mut texts: Vec<String> = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        for run in &texts {
            assert!(run.is_ascii(), "non-ASCII painted run: {run:?}");
        }
        let joined = texts.join("\u{1}");
        for needle in [
            "[ New workspace ]",
            "Folder",
            "What do you want to work on?",
            "Agent",
            "Model",
            "Claude Code",
            "[ Cancel ]",
            "[ Create ]",
        ] {
            assert!(
                joined.contains(needle),
                "missing {needle:?} in painted runs: {texts:?}"
            );
        }
    }

    fn collect_texts(shape: &egui::Shape, texts: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => texts.push(t.galley.text().to_string()),
            egui::Shape::Vec(v) => {
                for s in v {
                    collect_texts(s, texts);
                }
            },
            _ => {},
        }
    }
}
