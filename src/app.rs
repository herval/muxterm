use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};

use egui::{CornerRadius, FontId, Rect, RichText, Stroke, StrokeKind, Vec2};
use egui_term::{
    BackendCommand, FontSettings, PtyEvent, TerminalBackend, TerminalFont,
    TerminalTheme, TerminalView,
};

use crate::keys::{self, Action};
use crate::layout::{self, Node, PaneId, Removal, SplitAxis};
use crate::pane::Pane;
use crate::state::{self, LoadResult, NodeState, StateFile, TabState, WindowState};
use crate::tabbar::{self, TabBarAction};
use crate::theme;
use crate::tmux::TmuxCtl;

const PANE_GAP: f32 = 4.0;

pub struct Tab {
    pub tree: Node,
    pub panes: HashMap<PaneId, Pane>,
    pub focused: PaneId,
    /// Screen rects from the last frame; drives cmd+opt+arrow navigation.
    pub last_rects: HashMap<PaneId, Rect>,
}

enum UiAction {
    FocusPane(PaneId),
    LayoutChanged,
}

pub struct App {
    tabs: Vec<Tab>,
    active: usize,
    next_pane_id: u64,
    pty_tx: Sender<(u64, PtyEvent)>,
    pty_rx: Receiver<(u64, PtyEvent)>,
    tmux: TmuxCtl,
    font: FontId,
    term_theme: TerminalTheme,
    dirty: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, tmux: TmuxCtl) -> Self {
        theme::apply_visuals(&cc.egui_ctx);
        if let Err(e) = tmux.write_conf() {
            log::error!("failed to write tmux.conf: {e:#}");
        }

        let (pty_tx, pty_rx) = mpsc::channel();
        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            next_pane_id: 1,
            pty_tx,
            pty_rx,
            tmux,
            font: theme::font(),
            term_theme: theme::terminal_theme(),
            dirty: false,
        };

        match state::load() {
            LoadResult::Loaded(saved) => {
                let mut referenced = HashSet::new();
                for window in &saved.windows {
                    for tab in &window.tabs {
                        tab.tree.sessions(&mut referenced);
                    }
                }
                app.tmux.gc(&referenced);
                if let Some(window) = saved.windows.into_iter().next() {
                    for tab_state in window.tabs {
                        if let Err(e) =
                            app.restore_tab(&cc.egui_ctx, tab_state)
                        {
                            log::error!("failed to restore a tab: {e:#}");
                        }
                    }
                    if window.active_tab < app.tabs.len() {
                        app.active = window.active_tab;
                    }
                }
            },
            LoadResult::FirstRun => app.tmux.gc(&HashSet::new()),
            // Never GC on a corrupt state file: the sessions it referenced
            // are unknown, and killing live ones is the one unforgivable bug.
            LoadResult::Corrupt => {},
        }

        if app.tabs.is_empty() {
            app.new_tab(&cc.egui_ctx, None);
        }
        app
    }

    fn create_pane(
        &mut self,
        ctx: &egui::Context,
        session: Option<String>,
        start_dir: Option<String>,
    ) -> anyhow::Result<Pane> {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        let session = session.unwrap_or_else(TmuxCtl::new_session_name);
        let backend = TerminalBackend::new(
            id.0,
            ctx.clone(),
            self.pty_tx.clone(),
            self.tmux.spawn_settings(&session, start_dir),
        )?;
        Ok(Pane {
            id,
            session,
            backend,
            title: "shell".into(),
        })
    }

    /// cwd of the active tab's focused pane, for cwd inheritance.
    fn focused_cwd(&self) -> Option<String> {
        let tab = self.tabs.get(self.active)?;
        let pane = tab.panes.get(&tab.focused)?;
        self.tmux.pane_current_path(&pane.session)
    }

    fn new_tab(&mut self, ctx: &egui::Context, session: Option<String>) {
        let start_dir = self.focused_cwd();
        match self.create_pane(ctx, session, start_dir) {
            Ok(pane) => {
                let id = pane.id;
                let mut panes = HashMap::new();
                panes.insert(id, pane);
                self.tabs.push(Tab {
                    tree: Node::Leaf(id),
                    panes,
                    focused: id,
                    last_rects: HashMap::new(),
                });
                self.active = self.tabs.len() - 1;
                self.dirty = true;
            },
            Err(e) => log::error!("failed to open a new tab: {e:#}"),
        }
    }

    fn restore_tab(
        &mut self,
        ctx: &egui::Context,
        saved: TabState,
    ) -> anyhow::Result<()> {
        fn build(
            app: &mut App,
            ctx: &egui::Context,
            node: NodeState,
            panes: &mut HashMap<PaneId, Pane>,
        ) -> anyhow::Result<Node> {
            match node {
                NodeState::Leaf { session } => {
                    let pane = app.create_pane(ctx, Some(session), None)?;
                    let id = pane.id;
                    panes.insert(id, pane);
                    Ok(Node::Leaf(id))
                },
                NodeState::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => Ok(Node::Split {
                    axis,
                    ratio: ratio.clamp(0.1, 0.9),
                    first: Box::new(build(app, ctx, *first, panes)?),
                    second: Box::new(build(app, ctx, *second, panes)?),
                }),
            }
        }

        let mut panes = HashMap::new();
        let tree = build(self, ctx, saved.tree, &mut panes)?;
        let focused = panes
            .values()
            .find(|p| p.session == saved.focused_session)
            .map(|p| p.id)
            .unwrap_or_else(|| tree.first_leaf());
        self.tabs.push(Tab {
            tree,
            panes,
            focused,
            last_rects: HashMap::new(),
        });
        Ok(())
    }

    fn split_focused(&mut self, ctx: &egui::Context, axis: SplitAxis) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let target = tab.focused;
        let start_dir = self.focused_cwd();
        match self.create_pane(ctx, None, start_dir) {
            Ok(pane) => {
                let id = pane.id;
                let session = pane.session.clone();
                let tab = &mut self.tabs[self.active];
                if tab.tree.split(target, axis, id) {
                    tab.panes.insert(id, pane);
                    tab.focused = id;
                    self.dirty = true;
                } else {
                    log::error!("split target vanished; dropping new pane");
                    self.tmux.kill_session(&session);
                }
            },
            Err(e) => log::error!("failed to split: {e:#}"),
        }
    }

    /// The single close path. `kill` distinguishes an explicit close (cmd+w:
    /// kill the tmux session) from a reactive one (the shell exited, so the
    /// session is already gone). App quit goes through neither - backends
    /// just drop, clients detach, sessions survive.
    fn close_pane(
        &mut self,
        ctx: &egui::Context,
        tab_idx: usize,
        pane_id: PaneId,
        kill: bool,
    ) {
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return;
        };
        let Some(pane) = tab.panes.remove(&pane_id) else {
            return;
        };
        if kill {
            self.tmux.kill_session(&pane.session);
        }
        drop(pane);
        tab.last_rects.remove(&pane_id);

        match tab.tree.remove(pane_id) {
            Removal::BecameEmpty => {
                self.tabs.remove(tab_idx);
                if self.active >= self.tabs.len() {
                    self.active = self.tabs.len().saturating_sub(1);
                }
                if self.tabs.is_empty() {
                    self.save_state();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            },
            Removal::Removed { focus_hint } => {
                if tab.focused == pane_id {
                    tab.focused = focus_hint;
                }
            },
            Removal::NotFound => {},
        }
        self.dirty = true;
    }

    fn close_pane_by_backend(&mut self, ctx: &egui::Context, backend_id: u64) {
        let target = self.tabs.iter().enumerate().find_map(|(i, tab)| {
            tab.panes
                .contains_key(&PaneId(backend_id))
                .then_some((i, PaneId(backend_id)))
        });
        if let Some((tab_idx, pane_id)) = target {
            self.close_pane(ctx, tab_idx, pane_id, false);
        }
    }

    fn pane_mut(&mut self, backend_id: u64) -> Option<&mut Pane> {
        self.tabs
            .iter_mut()
            .find_map(|tab| tab.panes.get_mut(&PaneId(backend_id)))
    }

    fn drain_pty_events(&mut self, ctx: &egui::Context) {
        while let Ok((backend_id, event)) = self.pty_rx.try_recv() {
            match event {
                PtyEvent::Exit | PtyEvent::ChildExit(_) => {
                    self.close_pane_by_backend(ctx, backend_id);
                },
                PtyEvent::Title(title) => {
                    if let Some(pane) = self.pane_mut(backend_id) {
                        pane.title = title;
                    }
                },
                // tmux copy-mode copies arrive here as OSC 52.
                PtyEvent::ClipboardStore(_, data) => ctx.copy_text(data),
                // Terminal query responses (DA, DSR, ...) must be written
                // back to the PTY; the widget never handles these itself.
                PtyEvent::PtyWrite(text) => {
                    if let Some(pane) = self.pane_mut(backend_id) {
                        pane.backend.process_command(BackendCommand::Write(
                            text.into_bytes(),
                        ));
                    }
                },
                _ => {},
            }
        }
    }

    fn apply_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::NewTab => self.new_tab(ctx, None),
            Action::ClosePane => {
                if let Some(tab) = self.tabs.get(self.active) {
                    let focused = tab.focused;
                    self.close_pane(ctx, self.active, focused, true);
                }
            },
            Action::Split(axis) => self.split_focused(ctx, axis),
            Action::PrevTab => {
                let n = self.tabs.len();
                if n > 1 {
                    self.active = (self.active + n - 1) % n;
                    self.dirty = true;
                }
            },
            Action::NextTab => {
                let n = self.tabs.len();
                if n > 1 {
                    self.active = (self.active + 1) % n;
                    self.dirty = true;
                }
            },
            Action::GotoTab(i) => {
                if i < self.tabs.len() && i != self.active {
                    self.active = i;
                    self.dirty = true;
                }
            },
            Action::Focus(dir) => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    if let Some(next) =
                        layout::neighbor(&tab.last_rects, tab.focused, dir)
                    {
                        tab.focused = next;
                        self.dirty = true;
                    }
                }
            },
            Action::CyclePane(step) => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    let leaves = tab.tree.leaves();
                    if leaves.len() > 1 {
                        if let Some(pos) =
                            leaves.iter().position(|&p| p == tab.focused)
                        {
                            let n = leaves.len() as isize;
                            let next = (pos as isize + step).rem_euclid(n);
                            tab.focused = leaves[next as usize];
                            self.dirty = true;
                        }
                    }
                }
            },
        }
    }

    fn to_state(&self) -> StateFile {
        fn node_state(
            node: &Node,
            panes: &HashMap<PaneId, Pane>,
        ) -> NodeState {
            match node {
                Node::Leaf(id) => NodeState::Leaf {
                    session: panes
                        .get(id)
                        .map(|p| p.session.clone())
                        .unwrap_or_default(),
                },
                Node::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => NodeState::Split {
                    axis: *axis,
                    ratio: *ratio,
                    first: Box::new(node_state(first, panes)),
                    second: Box::new(node_state(second, panes)),
                },
            }
        }

        StateFile {
            version: state::VERSION,
            windows: vec![WindowState {
                active_tab: self.active,
                tabs: self
                    .tabs
                    .iter()
                    .map(|tab| TabState {
                        tree: node_state(&tab.tree, &tab.panes),
                        focused_session: tab
                            .panes
                            .get(&tab.focused)
                            .map(|p| p.session.clone())
                            .unwrap_or_default(),
                    })
                    .collect(),
            }],
        }
    }

    fn save_state(&self) {
        if let Err(e) = state::save(&self.to_state()) {
            log::error!("failed to save state: {e:#}");
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if log::log_enabled!(log::Level::Debug) {
            ctx.input(|i| {
                for event in &i.events {
                    log::debug!("event: {event:?} (mods now: {:?})", i.modifiers);
                }
            });
        }

        // Order is load-bearing: shortcuts must be consumed before any
        // TerminalView clones the frame's input events.
        let mut actions = keys::drain_shortcuts(ctx);

        self.drain_pty_events(ctx);

        if ctx.input(|i| i.viewport().close_requested()) {
            // Sessions deliberately survive: dropping the app only detaches
            // the tmux clients.
            self.save_state();
            return;
        }

        let labels: Vec<String> = self
            .tabs
            .iter()
            .map(|tab| {
                tab.panes
                    .get(&tab.focused)
                    .map(|p| p.title.clone())
                    .unwrap_or_else(|| "shell".into())
            })
            .collect();
        for action in tabbar::show(ctx, &labels, self.active) {
            match action {
                TabBarAction::Select(i) => actions.push(Action::GotoTab(i)),
                TabBarAction::NewTab => actions.push(Action::NewTab),
            }
        }

        let mut ui_actions = Vec::new();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG))
            .show(ctx, |ui| {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    let rect = ui.max_rect();
                    let mut rects = HashMap::new();
                    show_node(
                        ui,
                        &mut tab.tree,
                        rect,
                        1,
                        &mut tab.panes,
                        tab.focused,
                        &self.font,
                        &self.term_theme,
                        &mut rects,
                        &mut ui_actions,
                    );
                    if tab.panes.len() > 1 {
                        if let Some(focused_rect) = rects.get(&tab.focused) {
                            ui.painter().rect_stroke(
                                focused_rect.expand(1.0),
                                CornerRadius::same(2),
                                Stroke::new(1.0, theme::ACCENT),
                                StrokeKind::Outside,
                            );
                        }
                    }
                    tab.last_rects = rects;
                }
            });

        for action in ui_actions {
            match action {
                UiAction::FocusPane(id) => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        if tab.focused != id && tab.panes.contains_key(&id) {
                            tab.focused = id;
                            self.dirty = true;
                        }
                    }
                },
                UiAction::LayoutChanged => self.dirty = true,
            }
        }
        for action in actions {
            self.apply_action(ctx, action);
        }

        if self.dirty {
            self.save_state();
            self.dirty = false;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn show_node(
    ui: &mut egui::Ui,
    node: &mut Node,
    rect: Rect,
    path: u64,
    panes: &mut HashMap<PaneId, Pane>,
    focused: PaneId,
    font: &FontId,
    term_theme: &TerminalTheme,
    rects: &mut HashMap<PaneId, Rect>,
    ui_actions: &mut Vec<UiAction>,
) {
    match node {
        Node::Leaf(id) => {
            rects.insert(*id, rect);
            let Some(pane) = panes.get_mut(id) else {
                return;
            };
            let mut child =
                ui.new_child(egui::UiBuilder::new().max_rect(rect));
            let view = TerminalView::new(&mut child, &mut pane.backend)
                .set_size(rect.size())
                .set_focus(*id == focused)
                .set_font(TerminalFont::new(FontSettings {
                    font_type: font.clone(),
                }))
                .set_theme(term_theme.clone());
            let response = child.add(view);
            if response.clicked() {
                ui_actions.push(UiAction::FocusPane(*id));
            }
        },
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let (_, divider, _) =
                layout::split_rect(rect, *axis, *ratio, PANE_GAP);
            let hit = match axis {
                SplitAxis::SideBySide => divider.expand2(Vec2::new(2.0, 0.0)),
                SplitAxis::Stacked => divider.expand2(Vec2::new(0.0, 2.0)),
            };
            let divider_id = ui.id().with(("divider", path));
            let response = ui
                .interact(hit, divider_id, egui::Sense::drag())
                .on_hover_cursor(match axis {
                    SplitAxis::SideBySide => egui::CursorIcon::ResizeHorizontal,
                    SplitAxis::Stacked => egui::CursorIcon::ResizeVertical,
                });
            if response.dragged() {
                let delta = match axis {
                    SplitAxis::SideBySide => {
                        response.drag_delta().x
                            / (rect.width() - PANE_GAP).max(1.0)
                    },
                    SplitAxis::Stacked => {
                        response.drag_delta().y
                            / (rect.height() - PANE_GAP).max(1.0)
                    },
                };
                *ratio = (*ratio + delta).clamp(0.1, 0.9);
            }
            if response.drag_stopped() {
                ui_actions.push(UiAction::LayoutChanged);
            }

            // Recompute with the possibly-updated ratio so drags track live.
            let (first_rect, divider, second_rect) =
                layout::split_rect(rect, *axis, *ratio, PANE_GAP);
            ui.painter().rect_filled(
                divider,
                CornerRadius::ZERO,
                theme::DIVIDER,
            );
            show_node(
                ui, first, first_rect, path << 1, panes, focused, font,
                term_theme, rects, ui_actions,
            );
            show_node(
                ui,
                second,
                second_rect,
                (path << 1) | 1,
                panes,
                focused,
                font,
                term_theme,
                rects,
                ui_actions,
            );
        },
    }
}

/// Shown instead of the real app when tmux can't be found.
pub struct ErrorApp(pub String);

impl eframe::App for ErrorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new(&self.0).size(15.0));
            });
        });
    }
}
