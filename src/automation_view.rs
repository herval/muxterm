//! The automation run-history overlay: every execution of one automation,
//! and the captured output of whichever you pick.
//!
//! An overlay rather than a pane, for the same reason the PR reader is one
//! (`pr_view`): every pane is a tmux session that outlives the app, and
//! glancing at a run's log should not leave one behind. The automation's own
//! tab is still there for the live view - this is the "what happened while I
//! was away" surface, and the header's `open tab →` goes to the other one.

use egui::text::LayoutJob;
use egui::{Align2, Color32, FontId, TextFormat, Vec2};

use muxterm::automation::{self, Run};
use crate::theme::UiTheme;
use crate::sidebar::Status;

/// The overlay's state, owned by the App. Runs and log text are refreshed
/// from disk by the App while it is open, so a run finishing under the
/// overlay updates it.
pub struct Preview {
    pub id: String,
    pub name: String,
    pub schedule: String,
    /// When it next fires, already formatted (the App owns the clock).
    pub next: String,
    pub enabled: bool,
    pub runs: Vec<Run>,
    /// Which run's log is showing; None means the newest.
    pub selected: Option<String>,
    /// The selected run's captured log, split once so `show_rows` can
    /// virtualize it.
    pub lines: Option<Vec<String>>,
}

impl Preview {
    /// The run whose log is on screen: the explicit selection, else the
    /// newest (so opening the overlay lands on the latest run).
    pub fn current(&self) -> Option<&Run> {
        match &self.selected {
            Some(id) => self.runs.iter().find(|r| &r.id == id),
            None => self.runs.first(),
        }
    }
}

pub enum Outcome {
    None,
    Close,
    /// Run it now (the header's ▶).
    RunNow,
    /// Go to the automation's own tab, where runs happen live.
    OpenTab,
    /// Flip enabled/disabled.
    ToggleEnabled,
    /// Show this run's log instead.
    Select(String),
}

/// The colour a run's status reads in. Shape carries the meaning in the
/// sidebar; here there is room for a word, so colour is the whole signal.
pub fn status_color(status: &str, t: &UiTheme) -> Color32 {
    match status {
        automation::OK => t.status_ok,
        automation::FAILED => t.status_err,
        automation::RUNNING => t.accent,
        _ => t.text_dim,
    }
}

/// Map a run's status onto the sidebar's icon vocabulary, so the same state
/// looks the same in both places.
pub fn status_of(run: Option<&Run>) -> Status {
    match run.map(|r| r.status.as_str()) {
        Some(automation::RUNNING) => Status::Working,
        Some(automation::FAILED) => Status::Blocked,
        _ => Status::Idle,
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
    let size =
        Vec2::new((screen.width() * 0.8).min(1100.0), screen.height() * 0.82);
    let row_h = ctx.fonts(|f| f.row_height(font));
    let small = FontId::new(font.size * 0.85, font.family.clone());

    egui::Window::new("automation-view")
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
                    egui::RichText::new(&preview.name)
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
                        let (label, hint) = match preview.enabled {
                            true => ("disable", "stop it firing on its schedule"),
                            false => ("enable", "let it fire on its schedule"),
                        };
                        if ui
                            .button(
                                egui::RichText::new(label)
                                    .font(font.clone())
                                    .color(t.text_dim),
                            )
                            .on_hover_text(hint)
                            .clicked()
                        {
                            outcome = Outcome::ToggleEnabled;
                        }
                        if ui
                            .button(
                                egui::RichText::new("open tab →")
                                    .font(font.clone())
                                    .color(t.text_dim),
                            )
                            .on_hover_text("go to the tab its runs happen in")
                            .clicked()
                        {
                            outcome = Outcome::OpenTab;
                        }
                        if ui
                            .button(
                                egui::RichText::new("run now ▶")
                                    .font(font.clone())
                                    .color(t.accent),
                            )
                            .on_hover_text("run it once, right now")
                            .clicked()
                        {
                            outcome = Outcome::RunNow;
                        }
                    },
                );
            });
            ui.label(
                egui::RichText::new(format!(
                    "{}   ·   next: {}",
                    preview.schedule, preview.next
                ))
                .font(small.clone())
                .color(t.text_dim),
            );
            ui.separator();

            if preview.runs.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "no runs yet - it will appear here once it fires",
                    )
                    .font(font.clone())
                    .color(t.text_dim),
                );
                return;
            }

            // The history list gets a fixed slice of the window so a long
            // one cannot push the log off screen entirely.
            let list_h = (size.y * 0.32).min(row_h * 10.0);
            let current = preview.current().map(|r| r.id.clone());
            egui::ScrollArea::vertical()
                .id_salt("automation_runs")
                .max_height(list_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for run in &preview.runs {
                        if run_row(ui, run, current.as_deref(), font, t) {
                            outcome = Outcome::Select(run.id.clone());
                        }
                    }
                });
            ui.separator();

            match &preview.lines {
                None => {
                    ui.label(
                        egui::RichText::new("no output captured for this run")
                            .font(font.clone())
                            .color(t.text_dim),
                    );
                },
                Some(lines) => {
                    // show_rows renders only the visible slice, so an hour of
                    // agent output costs the same as a one-line command's.
                    egui::ScrollArea::both()
                        .id_salt("automation_log")
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show_rows(ui, row_h, lines.len(), |ui, range| {
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
                        });
                },
            }
        });

    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        outcome = Outcome::Close;
    }
    outcome
}

/// One run in the history list. Returns true when it was clicked.
fn run_row(
    ui: &mut egui::Ui,
    run: &Run,
    current: Option<&str>,
    font: &FontId,
    t: &UiTheme,
) -> bool {
    let row_h = ui.fonts(|f| f.row_height(font));
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), row_h),
        egui::Sense::click(),
    );
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let selected = current == Some(run.id.as_str());
    if selected || resp.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(3),
            crate::theme::blend(t.bg, t.accent, if selected { 0.14 } else { 0.06 }),
        );
    }
    let duration = match run.duration() {
        Some(d) => format!("{d}s"),
        None => "…".to_string(),
    };
    ui.painter().text(
        egui::Pos2::new(rect.min.x + 6.0, rect.center().y),
        Align2::LEFT_CENTER,
        format!(
            "{:<14} {:<12} {:>6}   {}",
            automation::stamp(run.started_at),
            run.status,
            duration,
            run.trigger
        ),
        font.clone(),
        status_color(&run.status, t),
    );
    resp.clicked()
}

/// Captured logs are plain text, but agents and shells both lean on a few
/// conventions worth colouring: muxterm's own narration, and anything that
/// announces itself as an error.
fn line_color(line: &str, t: &UiTheme) -> Color32 {
    let lower = line.trim_start().to_ascii_lowercase();
    if line.starts_with("[muxterm]") {
        t.accent
    } else if lower.starts_with("error")
        || lower.starts_with("fatal")
        || lower.starts_with("panic")
    {
        t.status_err
    } else {
        t.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, status: &str) -> Run {
        Run {
            id: id.to_string(),
            started_at: 1_800_000_000,
            finished_at: Some(1_800_000_030),
            status: status.to_string(),
            exit_code: Some(0),
            trigger: automation::TRIGGER_SCHEDULE.to_string(),
        }
    }

    fn preview(runs: Vec<Run>, selected: Option<&str>) -> Preview {
        Preview {
            id: "auto-1".into(),
            name: "nightly".into(),
            schedule: "daily at 09:00".into(),
            next: "Mar 11 09:00".into(),
            enabled: true,
            runs,
            selected: selected.map(str::to_string),
            lines: None,
        }
    }

    #[test]
    fn current_defaults_to_the_newest_run() {
        let p = preview(
            vec![run("0000000002-bbbb", automation::OK), run("0000000001-aaaa", automation::OK)],
            None,
        );
        assert_eq!(p.current().unwrap().id, "0000000002-bbbb");
    }

    #[test]
    fn an_explicit_selection_wins() {
        let p = preview(
            vec![run("0000000002-bbbb", automation::OK), run("0000000001-aaaa", automation::OK)],
            Some("0000000001-aaaa"),
        );
        assert_eq!(p.current().unwrap().id, "0000000001-aaaa");
    }

    #[test]
    fn a_selection_that_was_pruned_away_falls_back_to_nothing() {
        // Not to the wrong run: showing run A's log under run B's heading
        // would be worse than showing none.
        let p = preview(vec![run("0000000002-bbbb", automation::OK)], Some("gone"));
        assert!(p.current().is_none());
    }

    #[test]
    fn run_status_maps_onto_the_sidebar_icons() {
        assert!(matches!(
            status_of(Some(&run("1", automation::RUNNING))),
            Status::Working
        ));
        assert!(matches!(
            status_of(Some(&run("1", automation::FAILED))),
            Status::Blocked
        ));
        assert!(matches!(
            status_of(Some(&run("1", automation::OK))),
            Status::Idle
        ));
        assert!(matches!(status_of(None), Status::Idle));
    }

    #[test]
    fn muxterm_narration_and_errors_stand_out() {
        let preset = crate::theme::preset("iterm-dark").unwrap();
        let t = crate::theme::build(preset, &std::collections::HashMap::new(), 0.12).1;
        assert_eq!(line_color("[muxterm] nightly - schedule run", &t), t.accent);
        assert_eq!(line_color("error: no such file", &t), t.status_err);
        assert_eq!(line_color("  ERROR: boom", &t), t.status_err);
        assert_eq!(line_color("all good", &t), t.text);
    }
}
