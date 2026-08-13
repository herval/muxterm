//! The read-only PR overlay: what a click on a sidebar PR row opens.
//!
//! Deliberately *not* a pane. Reading a PR is a look, not a workspace, and
//! every pane muxterm opens is a tmux session that outlives the app - so a
//! preview per PR left durable clutter behind for a glance. This renders the
//! text `gh` already gives us over the terminal instead, like Settings does,
//! and dismisses without leaving anything to clean up.
//!
//! The trade is that it is not a terminal: no cmd+f, no tmux scrollback, and
//! selection is egui's rather than copy-mode's. The checkout button beside the
//! row is the path to a real pane when the PR turns out to be worth one.

use egui::text::LayoutJob;
use egui::{Align2, Color32, FontId, TextFormat, Vec2};

use crate::pr_monitor::PrItem;
use crate::theme::UiTheme;

/// A PR being read. The text arrives off-thread; until it does the overlay
/// says so rather than flashing empty.
pub struct Preview {
    pub item: PrItem,
    /// The `gh` output, split into lines once so the scroll area can render
    /// only what is on screen (a big diff is tens of thousands of lines).
    pub lines: Option<Vec<String>>,
    pub error: Option<String>,
}

impl Preview {
    pub fn loading(item: PrItem) -> Self {
        Self { item, lines: None, error: None }
    }

    pub fn set_text(&mut self, text: Result<String, String>) {
        match text {
            Ok(t) => self.lines = Some(t.lines().map(str::to_string).collect()),
            Err(e) => self.error = Some(e),
        }
    }
}

/// What the overlay wants the App to do next.
#[derive(PartialEq, Eq)]
pub enum Outcome {
    None,
    Close,
    /// The button that turns a look into a workspace.
    Checkout,
    OpenInBrowser,
}

/// Colour one line of `gh pr view` + `gh pr diff` output. gh writes plain text
/// when it is not attached to a terminal, so there is no ANSI to parse - the
/// prefixes are the whole signal.
fn line_color(line: &str, t: &UiTheme) -> Color32 {
    // File headers start with --- / +++ and must not read as removals and
    // additions; they are checked first for exactly that reason.
    if line.starts_with("--- ") || line.starts_with("+++ ") {
        t.text_dim
    } else if line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("similarity index")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("rename ")
    {
        t.text_dim
    } else if line.starts_with("@@") {
        t.accent
    } else if line.starts_with('+') {
        t.status_ok
    } else if line.starts_with('-') {
        t.status_err
    } else {
        t.text
    }
}

pub fn show(
    ctx: &egui::Context,
    preview: &Preview,
    font: &FontId,
    t: &UiTheme,
) -> Outcome {
    let mut outcome = Outcome::None;
    let screen = ctx.screen_rect();
    let size = Vec2::new(
        (screen.width() * 0.8).min(1100.0),
        screen.height() * 0.82,
    );
    let row_h = ctx.fonts(|f| f.row_height(font));

    egui::Window::new("pr-view")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .fixed_size(size)
        .frame(
            egui::Frame::new()
                .fill(t.bg)
                .inner_margin(egui::Margin::same(14))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 6],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(100),
                }),
        )
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "#{}  {}",
                        preview.item.number, preview.item.title
                    ))
                    .font(font.clone())
                    .color(t.text),
                );
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui
                            .button(
                                egui::RichText::new("esc")
                                    .font(font.clone())
                                    .color(t.text_dim),
                            )
                            .clicked()
                        {
                            outcome = Outcome::Close;
                        }
                        if ui
                            .button(
                                egui::RichText::new("open ↗")
                                    .font(font.clone())
                                    .color(t.text_dim),
                            )
                            .on_hover_text("open on github.com")
                            .clicked()
                        {
                            outcome = Outcome::OpenInBrowser;
                        }
                        if ui
                            .button(
                                egui::RichText::new("check out ↓")
                                    .font(font.clone())
                                    .color(t.accent),
                            )
                            .on_hover_text("check out as a worktree workspace")
                            .clicked()
                        {
                            outcome = Outcome::Checkout;
                        }
                    },
                );
            });
            ui.label(
                egui::RichText::new(&preview.item.repo)
                    .font(FontId::new(font.size * 0.85, font.family.clone()))
                    .color(t.text_dim),
            );
            ui.separator();

            match (&preview.lines, &preview.error) {
                (_, Some(e)) => {
                    ui.label(
                        egui::RichText::new(e)
                            .font(font.clone())
                            .color(t.status_err),
                    );
                },
                (None, None) => {
                    ui.label(
                        egui::RichText::new("loading…")
                            .font(font.clone())
                            .color(t.text_dim),
                    );
                },
                (Some(lines), None) => {
                    // show_rows renders only the visible slice, so a
                    // thousand-file diff costs the same as a one-line one.
                    egui::ScrollArea::both().auto_shrink([false, false]).show_rows(
                        ui,
                        row_h,
                        lines.len(),
                        |ui, range| {
                            let mut job = LayoutJob::default();
                            for i in range {
                                job.append(
                                    &format!("{}\n", lines[i]),
                                    0.0,
                                    TextFormat {
                                        font_id: font.clone(),
                                        color: line_color(&lines[i], t),
                                        ..Default::default()
                                    },
                                );
                            }
                            ui.label(job);
                        },
                    );
                },
            }
        });

    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        outcome = Outcome::Close;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn theme() -> UiTheme {
        let preset = crate::theme::preset("iterm-dark").unwrap();
        crate::theme::build(preset, &HashMap::new(), 0.12).1
    }

    /// The diff's colour comes from line prefixes, and the file headers must
    /// not be mistaken for a removal and an addition - `---` and `+++` start
    /// with exactly the characters that mean removed and added.
    #[test]
    fn diff_headers_are_not_read_as_additions_or_removals() {
        let t = theme();
        assert_eq!(line_color("--- a/src/app.rs", &t), t.text_dim);
        assert_eq!(line_color("+++ b/src/app.rs", &t), t.text_dim);
        assert_eq!(line_color("diff --git a/x b/x", &t), t.text_dim);
        assert_eq!(line_color("index 05a09..5c1d3 100644", &t), t.text_dim);

        assert_eq!(line_color("+    let x = 1;", &t), t.status_ok);
        assert_eq!(line_color("-    let x = 0;", &t), t.status_err);
        assert_eq!(line_color("@@ -880,15 +880,28 @@", &t), t.accent);
        assert_eq!(line_color(" unchanged context", &t), t.text);
        assert_eq!(line_color("title:\tsomething", &t), t.text);
    }

    /// Text arriving late replaces "loading…"; an error is kept apart from it
    /// so the overlay can say which happened.
    #[test]
    fn text_and_errors_land_separately() {
        let item = PrItem {
            number: 1,
            repo: "o/r".into(),
            title: "t".into(),
            url: "u".into(),
            draft: false,
        };
        let mut p = Preview::loading(item.clone());
        assert!(p.lines.is_none() && p.error.is_none());
        p.set_text(Ok("a\nb\nc".into()));
        assert_eq!(p.lines.as_ref().unwrap().len(), 3);

        let mut p = Preview::loading(item);
        p.set_text(Err("gh is not authenticated".into()));
        assert!(p.lines.is_none());
        assert_eq!(p.error.as_deref(), Some("gh is not authenticated"));
    }
}
