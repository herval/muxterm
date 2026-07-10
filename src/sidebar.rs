//! The workspace sidebar: a collapsible, resizable left panel listing every
//! tab as a workspace (Conductor-style), in tab order. Styled to read like
//! terminal content - the terminal background and the pane's monospace font,
//! not a native gray panel - so it sits flush with the panes beside it.
//! Display-only, like `tabbar`: it returns a vec of actions the App applies.

use std::time::Duration;

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, CornerRadius, FontId, Margin, Pos2, Rect, Stroke,
    TextFormat, Vec2,
};

use crate::theme::{self, UiTheme};

pub enum SidebarAction {
    /// Activate the tab at this index. For an archived row this is the "peek":
    /// it comes to the foreground while staying in the archived pile.
    Select(usize),
    /// Park the tab at this index in the archived pile (the row's archive icon).
    Archive(usize),
    /// Pull the tab at this index back out of the archived pile (restore icon).
    Unarchive(usize),
    /// Open the creation popup (the header "+").
    NewWorkspace,
    /// Collapse the sidebar (the header "‹").
    ToggleSidebar,
}

/// The status-light state of a workspace's leading dot.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No agent, or an agent that has gone quiet: a static accent dot.
    Idle,
    /// An agent's turn ended but a background job it started still runs
    /// (bg_jobs.rs): the working triangle, hollow.
    Background,
    /// An agent produced output recently: a pulsating green light.
    Working,
    /// An agent raised its hand / rang the bell and is waiting: steady red.
    Blocked,
}

/// One row's render data. `tab_index` maps back to `App.tabs` so click order
/// is independent of display order.
pub struct Row {
    pub tab_index: usize,
    pub title: String,
    pub subtitle: Option<String>,
    pub active: bool,
    /// Drives the leading status icon (`status_icon`: ring / play / `!`).
    pub status: Status,
    /// Whether this workspace is archived: it renders in the bottom pile and
    /// its hover icon restores rather than archives.
    pub archived: bool,
}

pub fn show(
    ctx: &egui::Context,
    rows: &[Row],
    font: &FontId,
    t: &UiTheme,
) -> Vec<SidebarAction> {
    let mut actions = Vec::new();
    egui::SidePanel::left("workspace_sidebar")
        .default_width(210.0)
        .min_width(150.0)
        .max_width(460.0)
        .resizable(true)
        .frame(
            egui::Frame::new().fill(t.bg).inner_margin(Margin {
                left: 12,
                right: 12,
                top: 6,
                bottom: 8,
            }),
        )
        .show(ctx, |ui| {
            let head_font = FontId::new(font.size * 0.82, font.family.clone());
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("workspaces")
                        .font(head_font.clone())
                        .color(t.text_dim),
                );
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if icon_button(ui, "‹", t)
                            .on_hover_text("Hide sidebar (cmd+\\)")
                            .clicked()
                        {
                            actions.push(SidebarAction::ToggleSidebar);
                        }
                        if icon_button(ui, "+", t)
                            .on_hover_text("New workspace (cmd+n)")
                            .clicked()
                        {
                            actions.push(SidebarAction::NewWorkspace);
                        }
                    },
                );
            });
            ui.add_space(8.0);

            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                // Active pile: the workspaces in the tab flow.
                for row in rows.iter().filter(|r| !r.archived) {
                    let (resp, icon_clicked) = workspace_row(ui, row, font, t);
                    if let Some(a) = row_action(resp.clicked(), icon_clicked, row)
                    {
                        actions.push(a);
                    }
                }
                // Archived pile at the bottom, under a dim header. Rows arrive
                // already ordered newest-first by the caller.
                if rows.iter().any(|r| r.archived) {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("Archived")
                            .font(head_font.clone())
                            .color(t.text_dim),
                    );
                    ui.add_space(4.0);
                    for row in rows.iter().filter(|r| r.archived) {
                        let (resp, icon_clicked) =
                            workspace_row(ui, row, font, t);
                        if let Some(a) =
                            row_action(resp.clicked(), icon_clicked, row)
                        {
                            actions.push(a);
                        }
                    }
                }
            });
        });
    actions
}

/// Map a row's body-click / icon-click into the action it means. The icon
/// wins over a body click (they overlap): on an active row it archives, on an
/// archived row it restores; a plain body click selects (a peek for archived).
fn row_action(
    clicked: bool,
    icon_clicked: bool,
    row: &Row,
) -> Option<SidebarAction> {
    if icon_clicked {
        Some(if row.archived {
            SidebarAction::Unarchive(row.tab_index)
        } else {
            SidebarAction::Archive(row.tab_index)
        })
    } else if clicked {
        Some(SidebarAction::Select(row.tab_index))
    } else {
        None
    }
}

/// Repaint cadence while a working pulse is on screen. A 1.4s sine is
/// indistinguishable at ~15fps, and agents work for hours at a stretch - the
/// pulse must never be what pins the render loop at display refresh rate.
const PULSE_FRAME: Duration = Duration::from_millis(66);

/// A breathing brightness for the "working" icon: a sine over `time`
/// (seconds) eases the color between a dimmed-toward-background trough and
/// the full status green. ~1.4s period reads as a calm pulse, not a blink.
/// `None` (window unfocused - nobody is watching) holds steady full green.
fn pulse(bright: Color32, bg: Color32, time: Option<f64>) -> Color32 {
    let Some(time) = time else { return bright };
    let s = 0.5 + 0.5 * (time * 4.5).sin() as f32; // 0..1
    let dim = theme::blend(bright, bg, 0.6);
    theme::blend(dim, bright, s)
}

/// The row's status icon, sized against the row font. One distinct shape
/// per state - color alone must never be the only signal:
/// - Idle: a hollow accent ring (nothing running).
/// - Background: the play-triangle stroked hollow, breathing green - a job
///   runs, but not the agent's own turn.
/// - Working: a filled play-triangle breathing green (`pulse`).
/// - Blocked: a steady red exclamation mark (bar + dot).
fn status_icon(
    painter: &egui::Painter,
    center: Pos2,
    font_size: f32,
    status: Status,
    t: &UiTheme,
    time: Option<f64>,
) {
    let r = font_size * 0.30;
    let triangle = || {
        vec![
            Pos2::new(center.x - r * 0.62, center.y - r),
            Pos2::new(center.x - r * 0.62, center.y + r),
            Pos2::new(center.x + r * 0.9, center.y),
        ]
    };
    match status {
        Status::Idle => {
            painter.circle_stroke(
                center,
                r * 0.72,
                Stroke::new((font_size * 0.09).max(1.0), t.accent),
            );
        },
        Status::Background => {
            painter.add(egui::Shape::closed_line(
                triangle(),
                Stroke::new(
                    (font_size * 0.09).max(1.0),
                    pulse(t.status_ok, t.bg, time),
                ),
            ));
        },
        Status::Working => {
            painter.add(egui::Shape::convex_polygon(
                triangle(),
                pulse(t.status_ok, t.bg, time),
                Stroke::NONE,
            ));
        },
        Status::Blocked => {
            let w = (font_size * 0.10).max(1.0);
            let bar = Rect::from_min_max(
                Pos2::new(center.x - w, center.y - r),
                Pos2::new(center.x + w, center.y + r * 0.35),
            );
            painter.rect_filled(bar, CornerRadius::same(1), t.status_err);
            painter.circle_filled(
                Pos2::new(center.x, center.y + r * 0.85),
                w * 1.2,
                t.status_err,
            );
        },
    }
}

fn icon_button(ui: &mut egui::Ui, glyph: &str, t: &UiTheme) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(glyph).size(15.0).color(t.text_dim),
        )
        .fill(Color32::TRANSPARENT)
        .corner_radius(CornerRadius::same(5))
        .min_size(Vec2::new(20.0, 20.0)),
    )
}

/// Width reserved on the right of every row for the hover archive/restore
/// icon, so a long title wraps before it instead of running underneath.
const ICON_W: f32 = 16.0;

/// Renders one row and returns `(body response, icon_clicked)`. The icon is a
/// separate interact rect overlaid on the right, so `show` can archive/restore
/// on the icon and select on the body without the two colliding.
fn workspace_row(
    ui: &mut egui::Ui,
    row: &Row,
    font: &FontId,
    t: &UiTheme,
) -> (egui::Response, bool) {
    let title_color = if row.active { t.text } else { t.text_dim };
    let pad = Vec2::new(8.0, 5.0);

    // The leading icon is the status light, and its *shape* carries the
    // state as much as its color (so it reads without color vision): a
    // quiet ring when idle, a breathing play-triangle while an agent works
    // (hollow when only a background job it left behind still runs), a
    // steady red exclamation while one is blocked waiting. Painter-drawn,
    // not a glyph: advance widths vary across terminal fonts/fallbacks, a
    // fixed band doesn't.
    //
    // The breathing states animate only while the window has focus, and at
    // PULSE_FRAME rather than every frame: agents run for hours, so an
    // unthrottled request_repaint here means the whole app renders at
    // display refresh rate more or less permanently - even sitting behind
    // other windows. Unfocused, the triangle holds steady and the light
    // stays honest via the App's idle heartbeat and PTY-event repaints.
    let animate = matches!(row.status, Status::Working | Status::Background)
        && ui.input(|i| i.focused);
    if animate {
        ui.ctx().request_repaint_after(PULSE_FRAME);
    }
    let status_w = font.size * 1.1;
    let mut job = LayoutJob::default();
    // Reserve both icons' widths so wrapping never collides with them.
    job.wrap.max_width =
        (ui.available_width() - pad.x * 2.0 - ICON_W - status_w).max(1.0);
    job.append(&row.title, 0.0, TextFormat::simple(font.clone(), title_color));
    if let Some(sub) = &row.subtitle {
        job.append(
            &format!("\n  {sub}"),
            0.0,
            TextFormat::simple(
                FontId::new(font.size * 0.8, font.family.clone()),
                t.text_dim,
            ),
        );
    }
    let galley = ui.fonts(|f| f.layout_job(job));

    let size = Vec2::new(ui.available_width(), galley.size().y + pad.y * 2.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());

    // The hover-revealed archive/restore affordance: its own interact rect on
    // the right, registered after the row so it wins the click there. Created
    // before painting so its hover can also light the row background.
    let (glyph, hint) = if row.archived {
        ("↑", "Restore workspace")
    } else {
        ("↓", "Archive workspace")
    };
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.max.x - pad.x - ICON_W / 2.0, rect.center().y),
        Vec2::splat(ICON_W),
    );
    let icon_resp = ui
        .interact(
            icon_rect,
            ui.id().with(("ws_row_icon", row.tab_index)),
            egui::Sense::click(),
        )
        .on_hover_text(hint)
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let hovered = resp.hovered() || icon_resp.hovered();

    // Background first, then text on top - a tinted selection (bg blended
    // toward accent) reads as terminal chrome, not a flat gray box.
    if row.active {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(4),
            theme::blend(t.bg, t.accent, 0.14),
        );
    } else if hovered {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(4),
            theme::blend(t.bg, t.accent, 0.06),
        );
    }
    ui.painter()
        .galley(rect.min + pad + Vec2::new(status_w, 0.0), galley, title_color);
    // Centered on the title's first line, inside the reserved band.
    status_icon(
        ui.painter(),
        Pos2::new(
            rect.min.x + pad.x + status_w * 0.38,
            rect.min.y + pad.y + ui.fonts(|f| f.row_height(font)) * 0.52,
        ),
        font.size,
        row.status,
        t,
        animate.then(|| ui.input(|i| i.time)),
    );
    if hovered {
        ui.painter().text(
            icon_rect.center(),
            Align2::CENTER_CENTER,
            glyph,
            FontId::new(font.size * 0.95, font.family.clone()),
            if icon_resp.hovered() { t.text } else { t.text_dim },
        );
    }
    (resp.on_hover_cursor(egui::CursorIcon::PointingHand), icon_resp.clicked())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn collect(shape: &egui::Shape, out: &mut Vec<egui::Shape>) {
        if let egui::Shape::Vec(v) = shape {
            for s in v {
                collect(s, out);
            }
        } else {
            out.push(shape.clone());
        }
    }

    /// Render one row per status headlessly and check each state's shape
    /// actually paints: idle's hollow ring (stroked, unfilled circle),
    /// working's play-triangle (filled path), background's hollow triangle
    /// (stroked, unfilled path), blocked's red exclamation (a
    /// status_err-filled circle for its dot). Guards the "shape, not
    /// just color" contract.
    #[test]
    fn sidebar_paints_distinct_status_shapes() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let rows = vec![
            Row {
                tab_index: 0,
                title: "resting-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Idle,
                archived: false,
            },
            Row {
                tab_index: 1,
                title: "busy-ws".into(),
                subtitle: Some("feat/x".into()),
                active: false,
                status: Status::Working,
                archived: false,
            },
            Row {
                tab_index: 2,
                title: "stuck-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Blocked,
                archived: false,
            },
            Row {
                tab_index: 3,
                title: "bg-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Background,
                archived: false,
            },
        ];

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let mut frame = |ctx: &egui::Context| {
            let _ = show(ctx, &rows, &font, &th);
        };
        let _ = ctx.run(input.clone(), &mut frame);
        let output = ctx.run(input, &mut frame);

        let mut shapes = Vec::new();
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut shapes);
        }
        let texts: String = shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\u{1}");
        for title in ["resting-ws", "busy-ws", "stuck-ws", "bg-ws"] {
            assert!(texts.contains(title), "missing {title:?} in {texts:?}");
        }

        let ring = shapes.iter().any(|s| {
            matches!(s, egui::Shape::Circle(c)
                if c.fill == Color32::TRANSPARENT && c.stroke.width > 0.0)
        });
        let triangle = shapes.iter().any(|s| {
            matches!(s, egui::Shape::Path(p) if p.fill != Color32::TRANSPARENT)
        });
        let hollow_triangle = shapes.iter().any(|s| {
            matches!(s, egui::Shape::Path(p)
                if p.fill == Color32::TRANSPARENT && p.stroke.width > 0.0)
        });
        let bang_dot = shapes.iter().any(|s| {
            matches!(s, egui::Shape::Circle(c) if c.fill == th.status_err)
        });
        assert!(ring, "idle ring not painted");
        assert!(triangle, "working play-triangle not painted");
        assert!(hollow_triangle, "background hollow triangle not painted");
        assert!(bang_dot, "blocked exclamation dot not painted");
    }

    /// The breathing pulse (working and background alike) schedules its own
    /// repaints, but throttled (at PULSE_FRAME, never every frame) and only
    /// while the window is focused. Guards the battery contract: agents work
    /// for hours, and one breathing row must not keep the render loop at
    /// display refresh rate - nor render at all while the app sits behind
    /// other windows.
    #[test]
    fn working_pulse_repaint_is_throttled_and_focus_gated() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        for status in [Status::Working, Status::Background] {
            let rows = vec![Row {
                tab_index: 0,
                title: "busy-ws".into(),
                subtitle: None,
                active: false,
                status,
                archived: false,
            }];
            let mut frame = |ctx: &egui::Context| {
                let _ = show(ctx, &rows, &font, &th);
            };
            let input = |focused: bool| egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(900.0, 700.0),
                )),
                focused,
                ..Default::default()
            };

            let _ = ctx.run(input(true), &mut frame);
            let out = ctx.run(input(true), &mut frame);
            let delay =
                out.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            assert!(
                delay > Duration::ZERO,
                "focused pulse repaints every frame (delay {delay:?})"
            );
            assert!(
                delay <= PULSE_FRAME,
                "focused pulse stopped animating (delay {delay:?})"
            );

            let out = ctx.run(input(false), &mut frame);
            let delay =
                out.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            assert!(
                delay > Duration::from_secs(1),
                "unfocused pulse still schedules repaints (delay {delay:?})"
            );
        }
    }
}
