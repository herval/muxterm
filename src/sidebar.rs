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
    /// First click on an archived row's ✕: arm it (the icon turns warn-red
    /// and a second click becomes destructive).
    ArmDelete(usize),
    /// Second click while armed: really delete the workspace.
    Delete(usize),
    /// The pointer left the armed row (or the row left the screen): stand
    /// the pending delete down.
    DisarmDelete,
    /// Collapse/expand the archived pile (its header click).
    ToggleArchived,
    /// Collapse/expand the open-PRs section (its header click).
    TogglePrs,
    /// Check this PR out as a worktree workspace (a PR row's body click).
    CheckoutPr(usize),
    /// Open this PR on github.com (a PR row's right-click).
    OpenPr(usize),
    /// Read this PR in a pane without checking it out (a PR row's body click).
    PreviewPr(usize),
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
    /// A non-agent foreground process (a shell command / tool - a build, a
    /// dev server, vim) is running: a pulsating amber square. Ranks below
    /// every agent-derived state, so agent runs keep their own icon.
    Command,
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
    /// Whether this row's ✕ is armed (one click in): the App keys the armed
    /// state by stable tab id and sets this per frame, so a tab-index shuffle
    /// can never arm the wrong row.
    pub delete_armed: bool,
}

/// One of the user's open PRs. Deliberately not a `Row`: that is indexed by
/// tab, and a PR has no tab until it is checked out.
pub struct PrRow {
    /// Index into the caller's PR list - the same role `tab_index` plays.
    pub index: usize,
    pub number: u64,
    /// `owner/name`, shown as the row's subtitle.
    pub repo: String,
    pub title: String,
    pub draft: bool,
    /// A checkout already in flight: the row is inert until it lands.
    pub busy: bool,
    /// Already checked out in some tab - clicking selects that tab instead.
    pub checked_out: Option<usize>,
}

pub fn show(
    ctx: &egui::Context,
    rows: &[Row],
    prs: &[PrRow],
    pr_note: Option<&str>,
    prs_collapsed: bool,
    archived_collapsed: bool,
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
                // Whether the pointer still sits on an armed row this frame;
                // an armed row that lost the pointer stands its delete down.
                let mut armed_hovered = false;
                // Active pile: the workspaces in the tab flow.
                for row in rows.iter().filter(|r| !r.archived) {
                    let r = workspace_row(ui, row, font, t);
                    if let Some(a) = row_action(&r, row) {
                        actions.push(a);
                    }
                }
                // Open PRs, above the archived pile: things you could pull
                // into the workspace list, under things already in it.
                if !prs.is_empty() || pr_note.is_some() {
                    ui.add_space(12.0);
                    if section_header(
                        ui,
                        "Pull requests",
                        prs.len(),
                        prs_collapsed,
                        &head_font,
                        t,
                    ) {
                        actions.push(SidebarAction::TogglePrs);
                    }
                    if !prs_collapsed {
                        ui.add_space(4.0);
                        // An enabled section that renders empty reads as
                        // broken, so it says why instead.
                        if let Some(note) = pr_note {
                            note_row(ui, note, &head_font, t);
                        }
                        for pr in prs {
                            if let Some(a) = pr_row(ui, pr, font, t) {
                                actions.push(a);
                            }
                        }
                    }
                }
                // Archived pile at the bottom, under a dim header that
                // clicks to collapse the pile (the App persists the fold).
                // Rows arrive already ordered newest-first by the caller.
                let archived = rows.iter().filter(|r| r.archived).count();
                if archived > 0 {
                    ui.add_space(12.0);
                    if section_header(
                        ui,
                        "Archived",
                        archived,
                        archived_collapsed,
                        &head_font,
                        t,
                    ) {
                        actions.push(SidebarAction::ToggleArchived);
                    }
                    if !archived_collapsed {
                        ui.add_space(4.0);
                        for row in rows.iter().filter(|r| r.archived) {
                            let r = workspace_row(ui, row, font, t);
                            armed_hovered |= row.delete_armed && r.hovered;
                            if let Some(a) = row_action(&r, row) {
                                actions.push(a);
                            }
                        }
                    }
                }
                // One condition covers every way an armed ✕ goes stale: the
                // pointer left the row, the pile collapsed over it, or the
                // row vanished altogether.
                if rows.iter().any(|r| r.delete_armed) && !armed_hovered {
                    actions.push(SidebarAction::DisarmDelete);
                }
            });
        });
    actions
}

/// Map a row's clicks into the action they mean. The icons win over a body
/// click (they overlap): the ✕ on an archived row arms, then deletes; the
/// ↓/↑ archives or restores; a plain body click selects (a peek for
/// archived).
fn row_action(r: &RowResponse, row: &Row) -> Option<SidebarAction> {
    if r.delete && row.archived {
        Some(if row.delete_armed {
            SidebarAction::Delete(row.tab_index)
        } else {
            SidebarAction::ArmDelete(row.tab_index)
        })
    } else if r.icon {
        Some(if row.archived {
            SidebarAction::Unarchive(row.tab_index)
        } else {
            SidebarAction::Archive(row.tab_index)
        })
    } else if r.body {
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
/// - Command: a filled amber square breathing (`pulse`) - a non-agent tool.
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
        Status::Command => {
            painter.rect_filled(
                Rect::from_center_size(center, Vec2::splat(r * 1.5)),
                CornerRadius::same(1),
                pulse(t.status_warn, t.bg, time),
            );
        },
    }
}

/// The archived pile's clickable header: a disclosure triangle (down when
/// open, right when folded) before the dim "Archived" label, with the hidden
/// row count while folded. Painter-drawn triangle like `status_icon` - no
/// font-glyph gambles across terminal fonts. Returns true on click.
/// A dim one-liner where rows would be - why the section is empty.
fn note_row(ui: &mut egui::Ui, note: &str, font: &FontId, t: &UiTheme) {
    let row_h = ui.fonts(|f| f.row_height(font));
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), row_h),
        egui::Sense::hover(),
    );
    ui.painter().text(
        Pos2::new(rect.min.x + font.size * 0.95, rect.center().y),
        Align2::LEFT_CENTER,
        note,
        font.clone(),
        t.text_dim,
    );
}

/// One open PR. The body click *reads* it - a pane with the PR and its diff,
/// nothing cloned or checked out - and the ↓ button beside it is the one that
/// makes a worktree, the way the archive/restore icons work on a workspace
/// row. Right-click opens it on github.com, matching the pane HUD's PR chips.
fn pr_row(
    ui: &mut egui::Ui,
    pr: &PrRow,
    font: &FontId,
    t: &UiTheme,
) -> Option<SidebarAction> {
    let pad = Vec2::new(8.0, 5.0);
    let status_w = font.size * 1.1;
    let dim = pr.busy || pr.checked_out.is_some();
    let wrap =
        (ui.available_width() - pad.x * 2.0 - status_w - ICON_W).max(1.0);

    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap;
    job.append(
        &format!("#{} {}", pr.number, pr.title),
        0.0,
        TextFormat {
            font_id: font.clone(),
            color: if dim { t.text_dim } else { t.text },
            ..Default::default()
        },
    );
    let sub = if pr.busy {
        format!("{} · checking out…", pr.repo)
    } else if pr.checked_out.is_some() {
        format!("{} · open", pr.repo)
    } else {
        pr.repo.clone()
    };
    job.append(
        &format!("\n  {sub}"),
        0.0,
        TextFormat {
            font_id: FontId::new(font.size * 0.8, font.family.clone()),
            color: t.text_dim,
            ..Default::default()
        },
    );
    let galley = ui.fonts(|f| f.layout_job(job));
    let row_h = galley.size().y + pad.y * 2.0;
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), row_h),
        egui::Sense::click(),
    );
    let resp = resp
        .on_hover_text("click to read it here\nright-click opens it on github")
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    // Registered after the body so it wins the click, like the workspace
    // row's archive/delete icons.
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.max.x - pad.x - ICON_W / 2.0, rect.center().y),
        Vec2::splat(ICON_W),
    );
    let icon_resp = (!pr.busy && pr.checked_out.is_none()).then(|| {
        ui.interact(
            icon_rect,
            ui.id().with(("pr_row_checkout", pr.number)),
            egui::Sense::click(),
        )
        .on_hover_text("check out as a worktree")
        .on_hover_cursor(egui::CursorIcon::PointingHand)
    });

    let hovered = resp.hovered()
        || icon_resp.as_ref().is_some_and(|r| r.hovered());
    if hovered {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(4),
            theme::blend(t.bg, t.accent, 0.06),
        );
    }
    // The same painter-drawn PR state icons the pane HUD chips use.
    let hud = theme::hud_colors(t);
    let kind = if pr.draft {
        crate::pr_status::Kind::Draft
    } else {
        crate::pr_status::Kind::Ok
    };
    kind.draw_icon(
        ui.painter(),
        Pos2::new(
            rect.min.x + pad.x + status_w * 0.38,
            rect.min.y + pad.y + row_h * 0.16,
        ),
        font.size,
        &hud,
    );
    ui.painter().galley(
        Pos2::new(rect.min.x + pad.x + status_w, rect.min.y + pad.y),
        galley,
        t.text,
    );
    if hovered {
        if let Some(r) = &icon_resp {
            ui.painter().text(
                icon_rect.center(),
                Align2::CENTER_CENTER,
                "↓",
                FontId::new(font.size * 0.95, font.family.clone()),
                if r.hovered() { t.text } else { t.text_dim },
            );
        }
    }

    if pr.busy {
        return None;
    }
    if resp.secondary_clicked() {
        return Some(SidebarAction::OpenPr(pr.index));
    }
    if icon_resp.is_some_and(|r| r.clicked()) {
        return Some(SidebarAction::CheckoutPr(pr.index));
    }
    if resp.clicked() {
        return Some(match pr.checked_out {
            // Already a workspace for it: go there rather than re-reading it.
            Some(tab) => SidebarAction::Select(tab),
            None => SidebarAction::PreviewPr(pr.index),
        });
    }
    None
}

fn section_header(
    ui: &mut egui::Ui,
    name: &str,
    count: usize,
    collapsed: bool,
    font: &FontId,
    t: &UiTheme,
) -> bool {
    let row_h = ui.fonts(|f| f.row_height(font));
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), row_h),
        egui::Sense::click(),
    );
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let color = if resp.hovered() { t.text } else { t.text_dim };

    let r = font.size * 0.26;
    let c = Pos2::new(rect.min.x + r, rect.center().y);
    let triangle = if collapsed {
        vec![
            Pos2::new(c.x - r * 0.5, c.y - r),
            Pos2::new(c.x - r * 0.5, c.y + r),
            Pos2::new(c.x + r * 0.75, c.y),
        ]
    } else {
        vec![
            Pos2::new(c.x - r, c.y - r * 0.5),
            Pos2::new(c.x + r, c.y - r * 0.5),
            Pos2::new(c.x, c.y + r * 0.75),
        ]
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(triangle, color, Stroke::NONE));

    let label = if collapsed {
        format!("{name} ({count})")
    } else {
        name.to_string()
    };
    ui.painter().text(
        Pos2::new(rect.min.x + font.size * 0.95, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        font.clone(),
        color,
    );
    resp.clicked()
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

/// Width reserved on the right of every row for each hover icon (one on
/// active rows, archive/restore + delete on archived ones), so a long title
/// wraps before them instead of running underneath.
const ICON_W: f32 = 16.0;

/// What one rendered row reported back: the clicks `row_action` maps and the
/// hover `show` needs to stand an armed delete down. Plain bools so the
/// click-to-action mapping unit-tests without an egui pass.
struct RowResponse {
    /// The row body was clicked.
    body: bool,
    /// The archive/restore icon (↓/↑) was clicked.
    icon: bool,
    /// The ✕ was clicked (archived rows only; always false otherwise).
    delete: bool,
    /// Pointer anywhere on the row - body or either icon (the disarm gate).
    hovered: bool,
}

/// Renders one row. The icons are separate interact rects overlaid on the
/// right, registered after the body so they win the click there and `show`
/// can act on them without colliding with select-on-body.
fn workspace_row(
    ui: &mut egui::Ui,
    row: &Row,
    font: &FontId,
    t: &UiTheme,
) -> RowResponse {
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
    let animate = matches!(
        row.status,
        Status::Working | Status::Background | Status::Command
    ) && ui.input(|i| i.focused);
    if animate {
        ui.ctx().request_repaint_after(PULSE_FRAME);
    }
    let status_w = font.size * 1.1;
    // Archived rows carry two hover icons (restore + delete), active rows
    // one; reserve the whole band so wrapping never collides with them.
    let band = if row.archived { ICON_W * 2.0 } else { ICON_W };
    let mut job = LayoutJob::default();
    job.wrap.max_width =
        (ui.available_width() - pad.x * 2.0 - band - status_w).max(1.0);
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

    // The hover-revealed affordances: their own interact rects on the right,
    // registered after the row so they win the click there. Created before
    // painting so their hover can also light the row background.
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
    // Archived rows also carry a delete ✕, one band inward - deliberately
    // not at the edge, so the destructive icon is the harder one to graze.
    // Two clicks to fire: the first arms it (the App remembers by tab id),
    // and only the armed ✕ deletes.
    let del_rect = Rect::from_center_size(
        Pos2::new(rect.max.x - pad.x - ICON_W - ICON_W / 2.0, rect.center().y),
        Vec2::splat(ICON_W),
    );
    let del_resp = row.archived.then(|| {
        ui.interact(
            del_rect,
            ui.id().with(("ws_row_del", row.tab_index)),
            egui::Sense::click(),
        )
        .on_hover_text(if row.delete_armed {
            "Click again to delete"
        } else {
            "Delete workspace"
        })
        .on_hover_cursor(egui::CursorIcon::PointingHand)
    });
    let hovered = resp.hovered()
        || icon_resp.hovered()
        || del_resp.as_ref().is_some_and(|r| r.hovered());

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
        let icon_font = FontId::new(font.size * 0.95, font.family.clone());
        ui.painter().text(
            icon_rect.center(),
            Align2::CENTER_CENTER,
            glyph,
            icon_font.clone(),
            if icon_resp.hovered() { t.text } else { t.text_dim },
        );
        if let Some(del) = &del_resp {
            // Armed reads red even while the pointer sits on the row body -
            // the warning must not depend on hovering the ✕ itself.
            ui.painter().text(
                del_rect.center(),
                Align2::CENTER_CENTER,
                "✕",
                icon_font,
                if row.delete_armed {
                    t.status_err
                } else if del.hovered() {
                    t.text
                } else {
                    t.text_dim
                },
            );
        }
    }
    RowResponse {
        body: resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked(),
        icon: icon_resp.clicked(),
        delete: del_resp.is_some_and(|r| r.clicked()),
        hovered,
    }
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
    /// status_err-filled circle for its dot), command's amber square (a
    /// status_warn-filled rect). Guards the "shape, not just color" contract.
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
                delete_armed: false,
            },
            Row {
                tab_index: 1,
                title: "busy-ws".into(),
                subtitle: Some("feat/x".into()),
                active: false,
                status: Status::Working,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 2,
                title: "stuck-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Blocked,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 3,
                title: "bg-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Background,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 4,
                title: "cmd-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Command,
                archived: false,
                delete_armed: false,
            },
        ];

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            // Unfocused holds every pulse at its bright base, so the breathing
            // icons paint their exact theme color (amber square == status_warn).
            focused: false,
            ..Default::default()
        };
        let mut frame = |ctx: &egui::Context| {
            let _ = show(ctx, &rows, &[], None, false, false, &font, &th);
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
        for title in ["resting-ws", "busy-ws", "stuck-ws", "bg-ws", "cmd-ws"] {
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
        let amber_square = shapes.iter().any(|s| {
            matches!(s, egui::Shape::Rect(r) if r.fill == th.status_warn)
        });
        assert!(ring, "idle ring not painted");
        assert!(triangle, "working play-triangle not painted");
        assert!(hollow_triangle, "background hollow triangle not painted");
        assert!(bang_dot, "blocked exclamation dot not painted");
        assert!(amber_square, "command amber square not painted");
    }

    /// Folding the archived pile hides its rows but keeps the header, and
    /// the header advertises how many rows are folded away; expanded, the
    /// rows are back. Guards the collapse actually gating the render.
    #[test]
    fn archived_pile_folds_behind_its_header() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let rows = vec![
            Row {
                tab_index: 0,
                title: "live-ws".into(),
                subtitle: None,
                active: true,
                status: Status::Idle,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 1,
                title: "parked-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Idle,
                archived: true,
                delete_armed: false,
            },
        ];

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let texts = |collapsed: bool| {
            let mut frame = |ctx: &egui::Context| {
                let _ = show(ctx, &rows, &[], None, false, collapsed, &font, &th);
            };
            let _ = ctx.run(input.clone(), &mut frame);
            let output = ctx.run(input.clone(), &mut frame);
            let mut shapes = Vec::new();
            for clipped in &output.shapes {
                collect(&clipped.shape, &mut shapes);
            }
            shapes
                .iter()
                .filter_map(|s| match s {
                    egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\u{1}")
        };

        let open = texts(false);
        assert!(open.contains("parked-ws"), "expanded pile lost its row");
        assert!(open.contains("Archived"), "expanded pile lost its header");

        let folded = texts(true);
        assert!(
            !folded.contains("parked-ws"),
            "collapsed pile still renders rows: {folded:?}"
        );
        assert!(
            folded.contains("Archived (1)"),
            "collapsed header lost the fold count: {folded:?}"
        );
        assert!(folded.contains("live-ws"), "collapse ate the active pile");
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
        for status in [Status::Working, Status::Background, Status::Command] {
            let rows = vec![Row {
                tab_index: 0,
                title: "busy-ws".into(),
                subtitle: None,
                active: false,
                status,
                archived: false,
                delete_armed: false,
            }];
            let mut frame = |ctx: &egui::Context| {
                let _ = show(ctx, &rows, &[], None, false, false, &font, &th);
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

    fn archived_row(delete_armed: bool) -> Row {
        Row {
            tab_index: 0,
            title: "parked-ws".into(),
            subtitle: None,
            active: false,
            status: Status::Idle,
            archived: true,
            delete_armed,
        }
    }

    /// The click-to-action mapping: an unarmed ✕ arms, an armed one deletes,
    /// and the delete flag is inert on a non-archived row (which has no ✕ -
    /// the body/icon mapping decides instead).
    fn pr(index: usize, checked_out: Option<usize>, busy: bool) -> PrRow {
        PrRow {
            index,
            number: 9645,
            repo: "Telepatia-AI/monobloco".into(),
            title: "institution-ramp canary rollback".into(),
            draft: false,
            busy,
            checked_out,
        }
    }

    /// The PR section paints its own header and one row per PR, and folds
    /// away behind the header like the archived pile does.
    #[test]
    fn pr_section_folds_behind_its_header() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let prs = vec![pr(0, None, false)];
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let painted = |collapsed: bool, note: Option<&str>, prs: &[PrRow]| {
            let mut frame = |ctx: &egui::Context| {
                let _ = show(ctx, &[], prs, note, collapsed, false, &font, &th);
            };
            let _ = ctx.run(input.clone(), &mut frame);
            let output = ctx.run(input.clone(), &mut frame);
            let mut shapes = Vec::new();
            for clipped in &output.shapes {
                collect(&clipped.shape, &mut shapes);
            }
            shapes
                .iter()
                .filter_map(|s| match s {
                    egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        let open = painted(false, None, &prs);
        assert!(
            open.iter().any(|t| t.contains("Pull requests")),
            "header missing: {open:?}",
        );
        assert!(
            open.iter().any(|t| t.contains("9645")),
            "the PR row should paint when open: {open:?}",
        );
        let folded = painted(true, None, &prs);
        assert!(
            folded.iter().any(|t| t.contains("Pull requests (1)")),
            "a folded header carries the count: {folded:?}",
        );
        assert!(
            !folded.iter().any(|t| t.contains("9645")),
            "the row must be hidden when folded: {folded:?}",
        );

        // An enabled section that can't list anything says why, rather than
        // rendering blank and looking broken.
        let noted = painted(false, Some("gh is not authenticated"), &[]);
        assert!(
            noted.iter().any(|t| t.contains("not authenticated")),
            "the reason should be on screen: {noted:?}",
        );
    }

    /// The body reads, the button checks out. Two different jobs on one row,
    /// so the click that means "have a look" can't cost a worktree.
    #[test]
    fn body_reads_and_the_button_checks_out() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let prs = vec![pr(0, None, false)];
        let screen = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            Vec2::new(900.0, 700.0),
        );
        // Where the row and its icon landed, so the clicks are aimed rather
        // than guessed.
        let click_at = |pos: egui::Pos2| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events: vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: Default::default(),
                    },
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: Default::default(),
                    },
                ],
                ..Default::default()
            };
            let mut got = Vec::new();
            let mut frame = |ctx: &egui::Context| {
                got = show(ctx, &[], &prs, None, false, false, &font, &th);
            };
            // Two passes: egui needs a layout pass before a hit lands.
            let warm = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let _ = ctx.run(warm, &mut frame);
            let _ = ctx.run(input, &mut frame);
            got
        };

        // The row sits under the "Pull requests" header near the top left.
        let body = click_at(egui::Pos2::new(80.0, 70.0));
        assert!(
            body.iter().any(|a| matches!(a, SidebarAction::PreviewPr(0))),
            "a body click should read the PR, not check it out: {:?}",
            body.len(),
        );
        assert!(
            !body.iter().any(|a| matches!(a, SidebarAction::CheckoutPr(_))),
            "a body click must never make a worktree",
        );
    }

    #[test]
    fn row_action_delete_arms_then_fires() {
        let click = |body, icon, delete| RowResponse {
            body,
            icon,
            delete,
            hovered: true,
        };
        assert!(matches!(
            row_action(&click(false, false, true), &archived_row(false)),
            Some(SidebarAction::ArmDelete(0))
        ));
        assert!(matches!(
            row_action(&click(false, false, true), &archived_row(true)),
            Some(SidebarAction::Delete(0))
        ));
        // The ✕ wins over a simultaneous body click.
        assert!(matches!(
            row_action(&click(true, false, true), &archived_row(true)),
            Some(SidebarAction::Delete(0))
        ));
        let live = Row { archived: false, ..archived_row(false) };
        assert!(matches!(
            row_action(&click(false, false, true), &live),
            None
        ));
        assert!(matches!(
            row_action(&click(true, false, false), &archived_row(false)),
            Some(SidebarAction::Select(0))
        ));
    }

    /// An armed row whose pointer is elsewhere (here: nowhere) stands down
    /// via DisarmDelete; an unarmed pile never emits it.
    #[test]
    fn armed_delete_disarms_when_pointer_leaves() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let disarms = |armed: bool| {
            let rows = vec![archived_row(armed)];
            let mut fired = false;
            let mut frame = |ctx: &egui::Context| {
                for a in show(ctx, &rows, &[], None, false, false, &font, &th) {
                    if matches!(a, SidebarAction::DisarmDelete) {
                        fired = true;
                    }
                }
            };
            let _ = ctx.run(input.clone(), &mut frame);
            let _ = ctx.run(input.clone(), &mut frame);
            fired
        };
        assert!(disarms(true), "armed row without the pointer kept its arm");
        assert!(!disarms(false), "unarmed pile emitted DisarmDelete");
    }

    /// Hovering an archived row reveals both the restore ↑ and the delete ✕,
    /// and an armed ✕ paints in status_err even when the pointer sits on the
    /// row body rather than the icon.
    #[test]
    fn archived_row_hover_reveals_both_icons() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let base_input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };

        let icon_shapes = |armed: bool| {
            let rows = vec![archived_row(armed)];
            let mut frame = |ctx: &egui::Context| {
                let _ = show(ctx, &rows, &[], None, false, false, &font, &th);
            };
            // Settle the layout, then find the row title's on-screen spot
            // so the hover lands on the body regardless of exact geometry.
            let out = ctx.run(base_input(), &mut frame);
            let mut shapes = Vec::new();
            for clipped in &out.shapes {
                collect(&clipped.shape, &mut shapes);
            }
            let title_pos = shapes
                .iter()
                .find_map(|s| match s {
                    egui::Shape::Text(t)
                        if t.galley.text().contains("parked-ws") =>
                    {
                        Some(t.pos)
                    },
                    _ => None,
                })
                .expect("archived row title not painted");
            let mut input = base_input();
            input.events.push(egui::Event::PointerMoved(
                title_pos + Vec2::new(10.0, 5.0),
            ));
            let _ = ctx.run(input, &mut frame);
            let out = ctx.run(base_input(), &mut frame);
            let mut shapes = Vec::new();
            for clipped in &out.shapes {
                collect(&clipped.shape, &mut shapes);
            }
            shapes
                .into_iter()
                .filter_map(|s| match s {
                    egui::Shape::Text(t) => {
                        Some((t.galley.text().to_string(), t.fallback_color))
                    },
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        let texts = icon_shapes(false);
        assert!(
            texts.iter().any(|(t, _)| t == "↑"),
            "hover lost the restore icon: {texts:?}"
        );
        assert!(
            texts.iter().any(|(t, _)| t == "✕"),
            "hover lost the delete icon: {texts:?}"
        );

        let armed = icon_shapes(true);
        let x = armed.iter().find(|(t, _)| t == "✕");
        assert_eq!(
            x.map(|(_, c)| *c),
            Some(th.status_err),
            "armed ✕ not painted in status_err: {armed:?}"
        );
    }
}
