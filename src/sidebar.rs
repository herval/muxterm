//! The workspace sidebar: a collapsible, resizable left panel listing every
//! tab as a workspace (Conductor-style), in tab order. Styled to read like
//! terminal content - the terminal background and the pane's monospace font,
//! not a native gray panel - so it sits flush with the panes beside it.
//! Display-only, like `tabbar`: it returns a vec of actions the App applies.

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, CornerRadius, FontId, Margin, Pos2, Rect, TextFormat, Vec2,
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
    /// Drives the leading status dot (pulsating green / steady red / accent).
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

/// A breathing brightness for the "working" dot: a sine over `time` (seconds)
/// eases the color between a dimmed-toward-background trough and the full
/// status green. ~1.4s period reads as a calm pulse, not a blink.
fn pulse(bright: Color32, bg: Color32, time: f64) -> Color32 {
    let s = 0.5 + 0.5 * (time * 4.5).sin() as f32; // 0..1
    let dim = theme::blend(bright, bg, 0.6);
    theme::blend(dim, bright, s)
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

    // The leading dot doubles as the status light: a pulsating green while an
    // agent is working, steady red while one is blocked waiting, else a quiet
    // accent dot. Working keeps repainting so the pulse stays smooth (and so
    // the light goes out promptly once output stops).
    let (dot_color, dot_scale) = match row.status {
        Status::Working => {
            ui.ctx().request_repaint();
            let phase = ui.input(|i| i.time);
            (pulse(t.status_ok, t.bg, phase), 0.72)
        },
        Status::Blocked => (t.status_err, 0.72),
        Status::Idle => (t.accent, 0.6),
    };
    let mut job = LayoutJob::default();
    // Reserve the icon's width so wrapping never collides with it.
    job.wrap.max_width = (ui.available_width() - pad.x * 2.0 - ICON_W).max(1.0);
    job.append(
        "● ",
        0.0,
        TextFormat::simple(
            FontId::new(font.size * dot_scale, font.family.clone()),
            dot_color,
        ),
    );
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
    ui.painter().galley(rect.min + pad, galley, title_color);
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
