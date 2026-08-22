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
    /// Collapse/expand the live-workspace list (the panel header's click).
    ToggleWorkspaces,
    /// Check this PR out as a worktree workspace (a PR row's body click).
    CheckoutPr(usize),
    /// Open this PR on github.com (a PR row's right-click).
    OpenPr(usize),
    /// Read this PR in a pane without checking it out (a PR row's body click).
    PreviewPr(usize),
    /// Open the creation popup (the header "+").
    NewWorkspace,
    /// A row was dragged to a new position: move `moved` so it sits before
    /// `before`, or last when that is None. Both are tab ids, not indices -
    /// the App applies queued actions in a batch, and an index computed
    /// while rendering goes stale the moment any earlier action moves a tab.
    ReorderWorkspace { moved: String, before: Option<String> },
    /// Collapse/expand the automations section (its header click).
    ToggleAutomations,
    /// Open this automation's run history (an automation row's body click).
    PreviewAutomation(usize),
    /// Run this automation now (an automation row's ▶ button).
    RunAutomation(usize),
    /// Open Settings on the automations tab (the section header's "+").
    NewAutomation,
    /// Collapse the sidebar (the header "‹").
    ToggleSidebar,
}

/// What a drag is carrying: the dragged row's tab id. A newtype rather than
/// a bare String because egui's drag payload is keyed by type alone - a
/// `String` payload would be picked up by any other `String` drop target.
#[derive(Clone, Debug)]
pub struct DraggedRow(pub String);

/// Which half of a row the pointer is over, and so which side of it the
/// dragged row would land on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropSide {
    Above,
    Below,
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
    /// The tab's stable id. Keys the drag payload and this row's egui id, so
    /// neither follows a display slot: an index-keyed id would latch a
    /// half-finished drag onto whichever row later occupies that position.
    pub tab_id: String,
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

/// One saved automation. Like `PrRow` this is indexed by position in the
/// caller's list, not by tab: an automation's tab is made lazily and may not
/// exist yet.
pub struct AutomationRow {
    pub index: usize,
    pub name: String,
    /// `<schedule> · <last run>` or the reason it will never fire.
    pub subtitle: String,
    /// Drives the leading icon: `Working` while a run is in flight, `Blocked`
    /// when the last one failed, `Idle` otherwise.
    pub status: Status,
    /// A disabled automation dims and loses its ▶.
    pub enabled: bool,
    /// A run is in flight: the row is inert, as `PrRow.busy` is.
    pub running: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ctx: &egui::Context,
    rows: &[Row],
    workspaces_collapsed: bool,
    prs: &[PrRow],
    pr_note: Option<&str>,
    prs_collapsed: bool,
    automations: Option<&[AutomationRow]>,
    automations_collapsed: bool,
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
            let live = rows.iter().filter(|r| !r.archived).count();
            ui.horizontal(|ui| {
                // The panel title doubles as the live list's section header,
                // folding it the way "Pull requests" and "Archived" fold -
                // one header, not two saying "Workspaces". Its click band
                // stops short of the trailing ‹/+ buttons, which own their
                // own clicks, so it can't swallow them.
                let band = (ui.available_width() - HEAD_BUTTONS_W).max(1.0);
                let head_h = ui.fonts(|f| f.row_height(&head_font));
                let toggled = ui
                    .allocate_ui_with_layout(
                        Vec2::new(band, head_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            section_header(
                                ui,
                                "Workspaces",
                                live,
                                workspaces_collapsed,
                                &head_font,
                                t,
                            )
                        },
                    )
                    .inner;
                if toggled {
                    actions.push(SidebarAction::ToggleWorkspaces);
                }
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
                // Active pile: the workspaces in the tab flow, folded away by
                // the panel header (cmd+1..9 still reach them while folded -
                // this hides the list, it doesn't park the tabs).
                if !workspaces_collapsed {
                    // Collected first so a drop can look one row ahead:
                    // "below row i" means "before row i+1", and the last row
                    // has no successor to name.
                    let live: Vec<&Row> =
                        rows.iter().filter(|r| !r.archived).collect();
                    for (i, row) in live.iter().enumerate() {
                        let r = workspace_row(ui, row, font, t);
                        let next = live.get(i + 1).map(|n| n.tab_id.as_str());
                        if let Some(a) = drop_action(&r, row, next) {
                            actions.push(a);
                        } else if let Some(a) = row_action(&r, row) {
                            actions.push(a);
                        }
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
                // Automations, under the PRs: also not-quite-workspaces, but
                // ones that already exist and run on their own.
                // Present whenever the feature is on, empty included: the
                // section *is* how you find automations, so hiding it until
                // one exists hides its own "+". Off (None) removes it
                // entirely, the way the PR section vanishes with its extra.
                if let Some(autos) = automations {
                    ui.add_space(12.0);
                    let head = section_header_with_add(
                        ui,
                        "Automations",
                        autos.len(),
                        automations_collapsed,
                        &head_font,
                        t,
                    );
                    if head.toggled {
                        actions.push(SidebarAction::ToggleAutomations);
                    }
                    if head.add {
                        actions.push(SidebarAction::NewAutomation);
                    }
                    if !automations_collapsed {
                        ui.add_space(4.0);
                        // An on-but-empty section says what it is for,
                        // rather than sitting there as a bare header.
                        if autos.is_empty() {
                            note_row(
                                ui,
                                "none yet - + adds one",
                                &head_font,
                                t,
                            );
                        }
                        for a in autos {
                            if let Some(act) = automation_row(ui, a, font, t) {
                                actions.push(act);
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

/// Turn a drag released over `row` into the move it means. `next` is the tab
/// id of the row below this one, or None when this is the last live row.
///
/// Pure, like `row_action`: a drag is awkward to synthesize headlessly (it
/// spans frames and egui only calls it a drag once the pointer has moved far
/// enough), so the arithmetic that decides *where a row lands* is kept where
/// it can be tested directly.
///
/// A drop that resolves to where the row already sits reports `Select`
/// rather than nothing. That is not a nicety: egui reclassifies a press held
/// longer than ~0.8s as a drag even if the pointer never moved, so without
/// this a slow click on a workspace would silently fail to select it.
fn drop_action(
    r: &RowResponse,
    row: &Row,
    next: Option<&str>,
) -> Option<SidebarAction> {
    let moved = r.released.clone()?;
    let before = match r.drop? {
        DropSide::Above => Some(row.tab_id.clone()),
        DropSide::Below => next.map(str::to_string),
    };
    // Released over its own row: nothing to move, and this is the shape a
    // hesitant click takes, so report the select it was meant to be.
    //
    // Other drops that happen to resolve to the row's current position (just
    // above its successor, say) are left as a Reorder - the App recognises a
    // move to where it already is and does nothing. Catching every such case
    // here would mean duplicating that arithmetic against the display list
    // instead of the tab list, which is exactly how the two drift apart.
    if moved == row.tab_id {
        return Some(SidebarAction::Select(row.tab_index));
    }
    Some(SidebarAction::ReorderWorkspace { moved, before })
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

/// One saved automation. The body click opens its run history; the ▶ beside
/// it fires a run now - the same body-reads / icon-acts split the PR rows
/// use. A row stays clickable while running: mid-run is exactly when you
/// want to look at it.
fn automation_row(
    ui: &mut egui::Ui,
    row: &AutomationRow,
    font: &FontId,
    t: &UiTheme,
) -> Option<SidebarAction> {
    let pad = Vec2::new(8.0, 5.0);
    let status_w = font.size * 1.1;
    let wrap =
        (ui.available_width() - pad.x * 2.0 - status_w - ICON_W).max(1.0);
    // Same battery contract as `workspace_row`: breathe only while focused,
    // and only at PULSE_FRAME.
    let animate = row.status == Status::Working && ui.input(|i| i.focused);
    if animate {
        ui.ctx().request_repaint_after(PULSE_FRAME);
    }

    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap;
    job.append(
        &row.name,
        0.0,
        TextFormat {
            font_id: font.clone(),
            color: if row.enabled { t.text } else { t.text_dim },
            ..Default::default()
        },
    );
    job.append(
        &format!("\n  {}", row.subtitle),
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
        .on_hover_text("click to see its runs")
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    // Registered after the body so it wins the click.
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.max.x - pad.x - ICON_W / 2.0, rect.center().y),
        Vec2::splat(ICON_W),
    );
    let icon_resp = (row.enabled && !row.running).then(|| {
        ui.interact(
            icon_rect,
            ui.id().with(("automation_run", row.index)),
            egui::Sense::click(),
        )
        .on_hover_text("run it now")
        .on_hover_cursor(egui::CursorIcon::PointingHand)
    });

    let hovered =
        resp.hovered() || icon_resp.as_ref().is_some_and(|r| r.hovered());
    if hovered {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(4),
            theme::blend(t.bg, t.accent, 0.06),
        );
    }
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
                "▶",
                FontId::new(font.size * 0.8, font.family.clone()),
                if r.hovered() { t.text } else { t.text_dim },
            );
        }
    }

    automation_action(
        icon_resp.is_some_and(|r| r.clicked()),
        resp.clicked(),
        row,
    )
}

/// Map an automation row's clicks into the action they mean. Pure, like
/// `row_action`, so the split that matters - the body only ever *reads*, the
/// ▶ is the only thing that starts work - is testable without an egui pass.
///
/// It has to be pure, because this icon is *overlaid*: an `interact` rect
/// painted on top of an already-registered full-width body loses a
/// synthesized same-frame click to that body, so every x across the row
/// resolves to the body in a headless render pass. The PR row's ↓ has the
/// same property, which is why its test only asserts the body half. Note the
/// distinction - a click target that *allocates* its own region instead of
/// overlaying one stays reachable (the panel header's "+", which
/// `HEAD_BUTTONS_W` holds a band back for, is render-tested). Allocating
/// would buy coverage here too; it is not done because these rows
/// deliberately mirror `pr_row`'s shape, and the pure fn covers the split.
fn automation_action(
    icon_clicked: bool,
    body_clicked: bool,
    row: &AutomationRow,
) -> Option<SidebarAction> {
    // The icon overlaps the body and is registered after it, so it wins.
    if icon_clicked && row.enabled && !row.running {
        return Some(SidebarAction::RunAutomation(row.index));
    }
    if body_clicked {
        return Some(SidebarAction::PreviewAutomation(row.index));
    }
    None
}

/// What a section header reported this frame.
struct HeadResponse {
    /// The header itself was clicked: fold/unfold the section.
    toggled: bool,
    /// The trailing "+" was clicked.
    add: bool,
}

fn section_header(
    ui: &mut egui::Ui,
    name: &str,
    count: usize,
    collapsed: bool,
    font: &FontId,
    t: &UiTheme,
) -> bool {
    head_row(ui, name, count, collapsed, font, t, false).toggled
}

/// A section header carrying a trailing "+", for a section whose items the
/// user creates (automations) rather than discovers (PRs).
fn section_header_with_add(
    ui: &mut egui::Ui,
    name: &str,
    count: usize,
    collapsed: bool,
    font: &FontId,
    t: &UiTheme,
) -> HeadResponse {
    head_row(ui, name, count, collapsed, font, t, true)
}

#[allow(clippy::too_many_arguments)]
fn head_row(
    ui: &mut egui::Ui,
    name: &str,
    count: usize,
    collapsed: bool,
    font: &FontId,
    t: &UiTheme,
    with_add: bool,
) -> HeadResponse {
    let row_h = ui.fonts(|f| f.row_height(font));
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), row_h),
        egui::Sense::click(),
    );
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    // Registered after the header body so it wins the click, like the rows'
    // trailing icons.
    let add_rect = Rect::from_center_size(
        Pos2::new(rect.max.x - ICON_W / 2.0, rect.center().y),
        Vec2::splat(ICON_W),
    );
    let add_resp = with_add.then(|| {
        ui.interact(
            add_rect,
            ui.id().with(("section_add", name)),
            egui::Sense::click(),
        )
        .on_hover_text("New automation")
        .on_hover_cursor(egui::CursorIcon::PointingHand)
    });
    if let Some(r) = &add_resp {
        ui.painter().text(
            add_rect.center(),
            Align2::CENTER_CENTER,
            "+",
            font.clone(),
            if r.hovered() { t.text } else { t.text_dim },
        );
    }
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
    HeadResponse {
        toggled: resp.clicked(),
        add: add_resp.is_some_and(|r| r.clicked()),
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

/// Width reserved on the right of every row for each hover icon (one on
/// active rows, archive/restore + delete on archived ones), so a long title
/// wraps before them instead of running underneath.
const ICON_W: f32 = 16.0;

/// Width the panel header keeps for its two `icon_button`s (‹ and +, 20pt
/// each) plus the spacing around them - the slice the foldable title band
/// must not claim. Rounded up from the 48pt the pair actually needs: the band
/// losing a few points costs nothing, the buttons losing them squeezes a
/// click target.
const HEAD_BUTTONS_W: f32 = 64.0;

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
    /// A drag is hovering this row, and would land on this side of it.
    drop: Option<DropSide>,
    /// A drag was *released* over this row this frame, carrying this tab id.
    released: Option<String>,
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
    // Allocated with an explicit, tab-id-keyed id rather than
    // `allocate_exact_size`'s positional one: a slot-keyed id would hand a
    // half-finished drag to whichever row later sits in that slot.
    //
    // Live rows sense drags (they reorder the tab flow); archived ones do
    // not, because the archived pile is ordered by `archived_at`, so moving
    // a row within it would be undone on the next frame.
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let sense = if row.archived {
        egui::Sense::click()
    } else {
        egui::Sense::click_and_drag()
    };
    let resp =
        ui.interact(rect, ui.id().with(("ws_row", row.tab_id.as_str())), sense);
    // Gated on `drag_started` rather than called every frame: the payload is
    // only stored on that frame anyway, and the row list repaints constantly
    // (a breathing status light alone drives ~15fps), so an ungated call
    // would clone every visible row's id on every one of those frames.
    if !row.archived && resp.drag_started() {
        resp.dnd_set_drag_payload(DraggedRow(row.tab_id.clone()));
    }
    let dragging = resp.dragged();
    // Where a hovering drag would land. `dnd_hover_payload` tests
    // `contains_pointer`, not `hovered` - egui reports every widget as
    // un-hovered while a drag is in flight, so the row's own hover flag is
    // useless here. Same reason the pointer position comes from the input
    // state rather than `resp.hover_pos()`.
    let hovering = (!row.archived)
        .then(|| resp.dnd_hover_payload::<DraggedRow>())
        .flatten();
    let drop = hovering.as_ref().and_then(|_| {
        let pointer = ui.input(|i| i.pointer.interact_pos())?;
        Some(if pointer.y < rect.center().y {
            DropSide::Above
        } else {
            DropSide::Below
        })
    });
    let released = hovering
        .is_some()
        .then(|| resp.dnd_release_payload::<DraggedRow>())
        .flatten()
        .map(|p| p.0.clone());

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
    } else if hovered || dragging {
        // `dragging` counts here because egui reports every widget as
        // un-hovered mid-drag: without it the row being dragged is the one
        // row on screen with no highlight at all.
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
    // The insertion caret: where the dragged row would land. Painted last so
    // it sits over the row's own background and text.
    if let Some(side) = drop {
        let y = match side {
            DropSide::Above => rect.top(),
            DropSide::Below => rect.bottom(),
        };
        ui.painter().hline(
            rect.x_range(),
            y,
            Stroke::new((font.size * 0.14).max(2.0), t.accent),
        );
    }
    RowResponse {
        body: resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked(),
        icon: icon_resp.clicked(),
        delete: del_resp.is_some_and(|r| r.clicked()),
        hovered,
        drop,
        released,
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
                tab_id: "mux-tab-0".into(),
                title: "resting-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Idle,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 1,
                tab_id: "mux-tab-1".into(),
                title: "busy-ws".into(),
                subtitle: Some("feat/x".into()),
                active: false,
                status: Status::Working,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 2,
                tab_id: "mux-tab-2".into(),
                title: "stuck-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Blocked,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 3,
                tab_id: "mux-tab-3".into(),
                title: "bg-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Background,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 4,
                tab_id: "mux-tab-4".into(),
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
            let _ = show(ctx, &rows, false, &[], None, false, None, false, false, &font, &th);
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
                tab_id: "mux-tab-0".into(),
                title: "live-ws".into(),
                subtitle: None,
                active: true,
                status: Status::Idle,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 1,
                tab_id: "mux-tab-1".into(),
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
                let _ = show(ctx, &rows, false, &[], None, false, None, false, collapsed, &font, &th);
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

    /// The live list folds behind the panel header the same way, and the
    /// header keeps its own "+"/"‹" buttons: folding the workspaces must not
    /// take the archived pile (or the way to make a new workspace) with it.
    #[test]
    fn workspace_list_folds_behind_the_panel_header() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let rows = vec![
            Row {
                tab_index: 0,
                tab_id: "mux-tab-0".into(),
                title: "live-ws".into(),
                subtitle: None,
                active: true,
                status: Status::Idle,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 1,
                tab_id: "mux-tab-1".into(),
                title: "other-ws".into(),
                subtitle: None,
                active: false,
                status: Status::Idle,
                archived: false,
                delete_armed: false,
            },
            Row {
                tab_index: 2,
                tab_id: "mux-tab-2".into(),
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
                let _ = show(
                    ctx, &rows, collapsed, &[], None, false, None, false, false,
                    &font, &th,
                );
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
        assert!(open.contains("live-ws"), "expanded list lost its rows");
        assert!(open.contains("Workspaces"), "expanded list lost its header");

        let folded = texts(true);
        assert!(
            !folded.contains("live-ws") && !folded.contains("other-ws"),
            "collapsed list still renders rows: {folded:?}"
        );
        assert!(
            folded.contains("Workspaces (2)"),
            "collapsed header lost the fold count: {folded:?}"
        );
        assert!(
            folded.contains("parked-ws") && folded.contains("Archived"),
            "folding the live list ate the archived pile: {folded:?}"
        );
        assert!(folded.contains('+'), "the header's new-workspace + went away");
    }

    /// The panel header now carries a click of its own, so its two buttons
    /// must still win theirs: a click on the label folds the list, a click on
    /// the "+" opens the popup and folds nothing. Guards `HEAD_BUTTONS_W`
    /// actually holding the band off them.
    #[test]
    fn panel_header_folds_without_swallowing_its_buttons() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let rows = vec![Row {
            tab_index: 0,
            tab_id: "mux-tab-0".into(),
            title: "live-ws".into(),
            subtitle: None,
            active: true,
            status: Status::Idle,
            archived: false,
            delete_armed: false,
        }];
        let screen = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            Vec2::new(900.0, 700.0),
        );
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
                got = show(
                    ctx, &rows, false, &[], None, false, None, false, false,
                    &font, &th,
                );
            };
            let warm = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let _ = ctx.run(warm, &mut frame);
            let _ = ctx.run(input, &mut frame);
            got
        };

        // The header sits on the first row: label on the left, ‹ hard right,
        // + one button inward (the panel opens 210 wide, 12pt margins).
        let label = click_at(egui::Pos2::new(40.0, 16.0));
        assert!(
            label
                .iter()
                .any(|a| matches!(a, SidebarAction::ToggleWorkspaces)),
            "the header label no longer folds the list",
        );
        let plus = click_at(egui::Pos2::new(168.0, 16.0));
        assert!(
            plus.iter().any(|a| matches!(a, SidebarAction::NewWorkspace)),
            "the fold band swallowed the header's + button",
        );
        assert!(
            !plus
                .iter()
                .any(|a| matches!(a, SidebarAction::ToggleWorkspaces)),
            "clicking + also folded the list",
        );
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
                tab_id: "mux-tab-0".into(),
                title: "busy-ws".into(),
                subtitle: None,
                active: false,
                status,
                archived: false,
                delete_armed: false,
            }];
            let mut frame = |ctx: &egui::Context| {
                let _ = show(ctx, &rows, false, &[], None, false, None, false, false, &font, &th);
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

    /// A live row to spread over with `..`; callers set what they care about.
    fn plain_row() -> Row {
        Row {
            tab_index: 0,
            tab_id: "mux-tab-0".into(),
            title: "live-ws".into(),
            subtitle: None,
            active: false,
            status: Status::Idle,
            archived: false,
            delete_armed: false,
        }
    }

    fn archived_row(delete_armed: bool) -> Row {
        Row {
            tab_index: 0,
            tab_id: "mux-tab-0".into(),
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
                let _ = show(ctx, &[], false, prs, note, collapsed, None, false, false, &font, &th);
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

    fn automation(index: usize, name: &str, running: bool) -> AutomationRow {
        AutomationRow {
            index,
            name: name.to_string(),
            subtitle: "every 30m · ok 2m ago".into(),
            status: if running { Status::Working } else { Status::Idle },
            enabled: true,
            running,
        }
    }

    /// The automations section paints its header, rows and fold count, and
    /// folds away cleanly - without eating the sections either side of it.
    #[test]
    fn automation_section_folds_behind_its_header() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let autos = vec![automation(0, "nightly", false)];
        let rows = vec![Row {
            tab_index: 0,
            tab_id: "mux-tab-0".into(),
            title: "live-ws".into(),
            subtitle: None,
            active: true,
            status: Status::Idle,
            archived: false,
            delete_armed: false,
        }];
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let painted = |collapsed: bool| {
            let mut frame = |ctx: &egui::Context| {
                let _ = show(
                    ctx, &rows, false, &[], None, false, Some(&autos), collapsed,
                    false, &font, &th,
                );
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

        let open = painted(false);
        assert!(
            open.iter().any(|t| t.contains("Automations")),
            "header missing: {open:?}",
        );
        assert!(
            open.iter().any(|t| t.contains("nightly")),
            "the automation row should paint when open: {open:?}",
        );
        assert!(
            open.iter().any(|t| t.contains("every 30m")),
            "the schedule subtitle should paint: {open:?}",
        );
        // The workspace list above it is untouched by this section.
        assert!(
            open.iter().any(|t| t.contains("live-ws")),
            "the automations section ate the workspace rows: {open:?}",
        );

        let folded = painted(true);
        assert!(
            folded.iter().any(|t| t.contains("Automations (1)")),
            "a folded header carries the count: {folded:?}",
        );
        assert!(
            !folded.iter().any(|t| t.contains("nightly")),
            "the row must be hidden when folded: {folded:?}",
        );
    }

    /// The section is how automations are *found*, so it must be on screen
    /// whenever the feature is on - including before the first one exists,
    /// or its own "+" would be unreachable. Switched off it vanishes whole.
    #[test]
    fn the_automations_section_appears_with_the_feature_not_the_data() {
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
        let painted = |autos: Option<&[AutomationRow]>| {
            let mut frame = |ctx: &egui::Context| {
                let _ = show(
                    ctx, &[], false, &[], None, false, autos, false, false,
                    &font, &th,
                );
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

        // On with nothing saved: header + the hint that says what to do.
        let empty = painted(Some(&[]));
        assert!(
            empty.iter().any(|t| t.contains("Automations")),
            "an empty-but-on section still needs its header: {empty:?}",
        );
        assert!(
            empty.iter().any(|t| t.contains("none yet")),
            "an empty section should say so, not sit blank: {empty:?}",
        );

        // Off: gone entirely, header and all.
        let off = painted(None);
        assert!(
            !off.iter().any(|t| t.contains("Automations")),
            "the section must vanish with the feature off: {off:?}",
        );
        assert!(
            !off.iter().any(|t| t.contains("none yet")),
            "no empty-state hint when the feature is off: {off:?}",
        );
    }

    /// The body opens the run history, the ▶ runs it - the same
    /// body-reads/icon-acts split the PR rows use, so a click meaning "how
    /// did it go?" can never start a run. Pure over the two click flags,
    /// because the overlapping icon rect is not reachable from a headless
    /// render pass (see `automation_action`).
    #[test]
    fn automation_body_previews_and_the_button_runs() {
        let idle = automation(3, "nightly", false);
        assert!(matches!(
            automation_action(false, true, &idle),
            Some(SidebarAction::PreviewAutomation(3))
        ));
        assert!(matches!(
            automation_action(true, true, &idle),
            Some(SidebarAction::RunAutomation(3))
        ));
        assert!(automation_action(false, false, &idle).is_none());

        // A run already in flight: the ▶ is not drawn, and even if the click
        // arrived it must not stack a second run on the first.
        let busy = automation(3, "nightly", true);
        assert!(matches!(
            automation_action(true, false, &busy),
            None
        ));
        // ...but its history is still readable mid-run, which is when you
        // most want it.
        assert!(matches!(
            automation_action(false, true, &busy),
            Some(SidebarAction::PreviewAutomation(3))
        ));

        // A disabled automation cannot be started from its row either.
        let off = AutomationRow { enabled: false, ..automation(3, "n", false) };
        assert!(automation_action(true, false, &off).is_none());
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
                got = show(ctx, &[], false, &prs, None, false, None, false, false, &font, &th);
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

    /// Where a released drag says the row should land. Pure, because a drag
    /// spans frames and only becomes a drag once the pointer has moved far
    /// enough, which makes the geometry awkward to synthesize headlessly.
    #[test]
    fn drop_above_and_below_resolve_to_the_row_it_lands_before() {
        let target = Row { tab_index: 1, tab_id: "b".into(), ..plain_row() };
        let dropped = |side, released: &str| RowResponse {
            body: false,
            icon: false,
            delete: false,
            hovered: false,
            
            drop: Some(side),
            released: Some(released.to_string()),
        };

        // Upper half of b: land before b.
        let a = drop_action(&dropped(DropSide::Above, "a"), &target, Some("c"));
        assert!(matches!(
            a,
            Some(SidebarAction::ReorderWorkspace { ref moved, ref before })
                if moved == "a" && before.as_deref() == Some("b")
        ));

        // Lower half of b: land before whatever follows b.
        let a = drop_action(&dropped(DropSide::Below, "a"), &target, Some("c"));
        assert!(matches!(
            a,
            Some(SidebarAction::ReorderWorkspace { ref moved, ref before })
                if moved == "a" && before.as_deref() == Some("c")
        ));

        // Lower half of the last row: nothing follows, so land last.
        let a = drop_action(&dropped(DropSide::Below, "a"), &target, None);
        assert!(matches!(
            a,
            Some(SidebarAction::ReorderWorkspace { ref moved, before: None })
                if moved == "a"
        ));
    }

    /// Releasing over the row you picked up selects it instead of moving it.
    /// egui calls a press held ~0.8s a drag even if the pointer never moved,
    /// so without this a slow click would silently stop selecting.
    #[test]
    fn a_drag_released_on_its_own_row_is_a_select() {
        let row = Row { tab_index: 7, tab_id: "b".into(), ..plain_row() };
        for side in [DropSide::Above, DropSide::Below] {
            let r = RowResponse {
                body: false,
                icon: false,
                delete: false,
                hovered: false,
                
                drop: Some(side),
                released: Some("b".into()),
            };
            assert!(matches!(
                drop_action(&r, &row, Some("c")),
                Some(SidebarAction::Select(7))
            ));
        }
    }

    /// The gesture end to end, through real egui interaction: press on the
    /// first row, move onto the second, release. Takes several passes
    /// because egui deliberately withholds judgement on press - a press that
    /// has not moved yet might still become a click - so `dragged()` cannot
    /// be true on the same frame as the press.
    #[test]
    fn dragging_a_row_onto_another_reorders_it() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let rows = vec![
            Row { tab_index: 0, tab_id: "a".into(), ..plain_row() },
            Row {
                tab_index: 1,
                tab_id: "b".into(),
                title: "second-ws".into(),
                ..plain_row()
            },
        ];
        let screen = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            Vec2::new(900.0, 700.0),
        );
        let mut seen: Vec<SidebarAction> = Vec::new();
        let mut frame = |ctx: &egui::Context, input: egui::RawInput| {
            let mut got = Vec::new();
            let _ = ctx.run(input, |ctx| {
                got = show(
                    ctx, &rows, false, &[], None, false, None, false, false,
                    &font, &th,
                );
            });
            got
        };
        let at = |events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(screen),
            events,
            ..Default::default()
        };
        let press = |pos| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        let release = |pos| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        };

        // Find the two rows' centres from what actually got painted, rather
        // than hard-coding geometry that every padding tweak would break.
        let _ = frame(&ctx, at(vec![]));
        let out = ctx.run(at(vec![]), |ctx| {
            let _ = show(
                ctx, &rows, false, &[], None, false, None, false, false,
                &font, &th,
            );
        });
        let mut texts: Vec<(String, egui::Pos2)> = Vec::new();
        let mut shapes = Vec::new();
        for clipped in &out.shapes {
            collect(&clipped.shape, &mut shapes);
        }
        for s in &shapes {
            if let egui::Shape::Text(t) = s {
                texts.push((t.galley.text().to_string(), t.pos));
            }
        }
        let row_y = |needle: &str| {
            texts
                .iter()
                .find(|(s, _)| s.contains(needle))
                .map(|(_, p)| p.y + 8.0)
                .unwrap_or_else(|| panic!("row {needle} never painted"))
        };
        let a = egui::Pos2::new(80.0, row_y("live-ws"));
        let b = egui::Pos2::new(80.0, row_y("second-ws"));
        assert!(b.y - a.y > 6.0, "rows too close to tell a drag from a click");

        // Press on row a, then move onto row b (two passes: the move has to
        // be seen while the button is still down), then release over b.
        seen.extend(frame(&ctx, at(vec![egui::Event::PointerMoved(a), press(a)])));
        seen.extend(frame(&ctx, at(vec![egui::Event::PointerMoved(b)])));
        seen.extend(frame(&ctx, at(vec![egui::Event::PointerMoved(b)])));
        seen.extend(frame(&ctx, at(vec![release(b)])));

        let reorder = seen.iter().find_map(|a| match a {
            SidebarAction::ReorderWorkspace { moved, before } => {
                Some((moved.clone(), before.clone()))
            },
            _ => None,
        });
        let (moved, before) = reorder.expect(
            "dragging row a onto row b should have reordered it",
        );
        assert_eq!(moved, "a");
        // Dropped on b's lower half (b is the last row), so: land last.
        assert_eq!(before, None);
        // And it must not also have selected something on the way.
        assert!(
            !seen.iter().any(|a| matches!(a, SidebarAction::Select(_))),
            "a drag must not also fire a select",
        );
    }

    /// No release, no action - a drag merely passing over a row must not
    /// move anything, or the list would reshuffle under the pointer.
    #[test]
    fn hovering_without_releasing_moves_nothing() {
        let row = Row { tab_index: 1, tab_id: "b".into(), ..plain_row() };
        let r = RowResponse {
            body: false,
            icon: false,
            delete: false,
            hovered: false,
            
            drop: Some(DropSide::Above),
            released: None,
        };
        assert!(drop_action(&r, &row, Some("c")).is_none());
    }

    #[test]
    fn row_action_delete_arms_then_fires() {
        let click = |body, icon, delete| RowResponse {
            body,
            icon,
            delete,
            hovered: true,
            
            drop: None,
            released: None,
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
                for a in show(ctx, &rows, false, &[], None, false, None, false, false, &font, &th) {
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
                let _ = show(ctx, &rows, false, &[], None, false, None, false, false, &font, &th);
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
