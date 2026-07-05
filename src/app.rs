use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

use egui::{CornerRadius, FontId, Rect, RichText, Stroke, StrokeKind, Vec2};
use egui_term::{
    BackendCommand, FontSettings, PtyEvent, TerminalBackend, TerminalFont,
    TerminalTheme, TerminalView,
};

use crate::agent::{self, Agent};
use crate::ai_prompt::{self, PromptMachine, Verdict};
use crate::config;
use crate::keys::{self, Action};
use crate::layout::{self, Node, PaneId, Removal, SplitAxis};
use crate::pane::Pane;
use crate::state::{self, LoadResult, NodeState, StateFile, TabState, WindowState};
use crate::tabbar::{self, TabBarAction};
use crate::theme::{self, UiTheme};
use crate::tmux::{self, TmuxCtl};

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
    ui_theme: UiTheme,
    theme_name: String,
    settings_open: bool,
    dirty: bool,
    config_mtime: Option<SystemTime>,
    last_config_check: Instant,
    /// The "? " prompt line.
    ai: PromptMachine,
    agent: &'static Agent,
    agent_context_lines: u32,
    /// Cache of `binary_available` probes; misses are evicted on failed
    /// submits so an install-then-retry works without a restart.
    agent_ok: HashMap<&'static str, bool>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, tmux: TmuxCtl) -> Self {
        config::ensure_default_file();
        let (style, custom_font) = config::resolve(&config::load());
        config::install_fonts(&cc.egui_ctx, custom_font);
        theme::apply_visuals(&cc.egui_ctx, &style.ui);
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
            font: style.font,
            term_theme: style.term_theme,
            ui_theme: style.ui,
            theme_name: style.name,
            settings_open: false,
            dirty: false,
            config_mtime: config::mtime(),
            last_config_check: Instant::now(),
            ai: PromptMachine::default(),
            agent: style.agent,
            agent_context_lines: style.agent_context_lines,
            agent_ok: HashMap::new(),
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
        // Restored sessions may hold half-typed input from the previous
        // run, so the "? " prompt stays inert there until the first Enter.
        let restored = session.is_some();
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
            line_empty: !restored,
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
            Action::ToggleSettings => {
                self.settings_open = !self.settings_open;
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

    fn reload_config(&mut self, ctx: &egui::Context) {
        self.config_mtime = config::mtime();
        let (style, custom_font) = config::resolve(&config::load());
        config::install_fonts(ctx, custom_font);
        theme::apply_visuals(ctx, &style.ui);
        self.font = style.font;
        self.term_theme = style.term_theme;
        self.ui_theme = style.ui;
        self.theme_name = style.name;
        self.agent = style.agent;
        self.agent_context_lines = style.agent_context_lines;
    }

    /// Route keyboard events through the "? " prompt machine before any
    /// TerminalView clones the frame's input. Events it consumes never
    /// reach the PTY; a submit types the composed agent command into the
    /// focused pane, with recent scrollback redirected to its stdin.
    fn ai_intercept(&mut self, ctx: &egui::Context) {
        if self.settings_open {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let focused = tab.focused;
        self.ai.sync(focused, tab.panes.contains_key(&focused));
        let Some(pane) = tab.panes.get(&focused) else {
            return;
        };
        let mut line_empty = pane.line_empty;
        let session = pane.session.clone();

        // The machine is moved out so the event loop below doesn't have to
        // borrow self while at_shell holds &self.tmux.
        let mut machine = std::mem::take(&mut self.ai);
        let tmux = &self.tmux;
        let mut shell_state: Option<bool> = None;
        let mut at_shell = || {
            *shell_state.get_or_insert_with(|| {
                tmux.pane_current_command(&session)
                    .is_some_and(|c| tmux::is_shell(&c))
            })
        };

        let mut writes: Vec<Vec<u8>> = Vec::new();
        let mut submit: Option<String> = None;
        ctx.input_mut(|i| {
            let events = std::mem::take(&mut i.events);
            let mut kept = Vec::with_capacity(events.len());
            for event in events {
                match machine.on_event(
                    &event,
                    focused,
                    &mut line_empty,
                    &mut at_shell,
                ) {
                    Verdict::Pass => kept.push(event),
                    Verdict::Consume => {},
                    Verdict::PassAndWrite(bytes) => {
                        writes.push(bytes);
                        kept.push(event);
                    },
                    Verdict::Submit(query) => submit = Some(query),
                }
            }
            i.events = kept;
        });

        if let Some(query) = submit {
            let available = *self
                .agent_ok
                .entry(self.agent.id)
                .or_insert_with(|| agent::binary_available(self.agent.bin));
            if available {
                let ctx_file = (self.agent_context_lines > 0)
                    .then(|| {
                        self.tmux
                            .capture_pane(&session, self.agent_context_lines)
                    })
                    .flatten()
                    .and_then(|capture| write_context_file(focused, &capture));
                let mut cmd = self
                    .agent
                    .command(&query, ctx_file.as_deref())
                    .into_bytes();
                cmd.push(b'\r');
                writes.push(cmd);
                machine.cancel();
                line_empty = true;
            } else {
                self.agent_ok.remove(self.agent.id);
                machine
                    .set_error(format!("{} not found in PATH", self.agent.bin));
            }
        }

        self.ai = machine;
        if let Some(pane) = self
            .tabs
            .get_mut(self.active)
            .and_then(|tab| tab.panes.get_mut(&focused))
        {
            pane.line_empty = line_empty;
            for bytes in writes {
                pane.backend.process_command(BackendCommand::Write(bytes));
            }
        }
    }

    fn show_settings(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        let mut chosen: Option<&'static str> = None;
        let mut chosen_agent: Option<&'static str> = None;
        let mut commit_size: Option<f32> = None;

        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_width(300.0);
                ui.label(RichText::new("Theme").strong());
                ui.add_space(4.0);
                for name in theme::PRESET_NAMES {
                    let preset = theme::preset(name).unwrap();
                    ui.horizontal(|ui| {
                        let (strip, _) = ui.allocate_exact_size(
                            Vec2::new(66.0, 14.0),
                            egui::Sense::hover(),
                        );
                        let swatches = [
                            preset.bg,
                            preset.ansi[1],
                            preset.ansi[2],
                            preset.ansi[4],
                            preset.accent,
                        ];
                        for (i, hex) in swatches.iter().enumerate() {
                            let c = theme::parse_hex(hex)
                                .unwrap_or(egui::Color32::BLACK);
                            let r = Rect::from_min_size(
                                strip.min + Vec2::new(i as f32 * 13.0, 0.0),
                                Vec2::new(12.0, 14.0),
                            );
                            ui.painter().rect_filled(
                                r,
                                CornerRadius::same(2),
                                c,
                            );
                            // keep light swatches visible on light panes
                            ui.painter().rect_stroke(
                                r,
                                CornerRadius::same(2),
                                Stroke::new(1.0, self.ui_theme.text_dim),
                                StrokeKind::Inside,
                            );
                        }
                        let selected = self.theme_name == *name;
                        if ui.selectable_label(selected, *name).clicked()
                            && !selected
                        {
                            chosen = Some(name);
                        }
                    });
                }

                ui.add_space(10.0);
                ui.label(RichText::new("Font").strong());
                let mut size = self.font.size;
                let resp = ui.add(
                    egui::Slider::new(&mut size, 8.0..=24.0)
                        .step_by(0.5)
                        .text("size"),
                );
                if resp.changed() {
                    self.font.size = size; // live preview
                }
                if resp.drag_stopped()
                    || (resp.changed() && !resp.dragged())
                {
                    commit_size = Some(size);
                }

                ui.add_space(10.0);
                ui.label(RichText::new("AI agent").strong());
                ui.add_space(4.0);
                for a in agent::AGENTS {
                    let selected = self.agent.id == a.id;
                    if ui.selectable_label(selected, a.label).clicked()
                        && !selected
                    {
                        chosen_agent = Some(a.id);
                    }
                }
                ui.label(
                    RichText::new("type \"? \" at an empty shell prompt to ask")
                        .color(self.ui_theme.text_dim)
                        .size(11.0),
                );

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Edit config file…").clicked() {
                        let _ = std::process::Command::new("/usr/bin/open")
                            .arg("-t")
                            .arg(config::path())
                            .spawn();
                    }
                    ui.label(
                        RichText::new("color overrides & font family")
                            .color(self.ui_theme.text_dim)
                            .size(11.0),
                    );
                });
            });

        self.settings_open = open;
        if let Some(name) = chosen {
            config::set_theme(name);
            self.reload_config(ctx);
        }
        if let Some(id) = chosen_agent {
            config::set_agent(id);
            self.reload_config(ctx);
        }
        if let Some(size) = commit_size {
            config::set_font_size(size);
            // Sync mtime so the poller doesn't reload and yank the slider.
            self.config_mtime = config::mtime();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Live-reload config.toml so theme/font edits apply immediately.
        // The idle repaint below keeps the mtime poll ticking without input.
        ctx.request_repaint_after(Duration::from_secs(2));
        if self.last_config_check.elapsed() > Duration::from_secs(1) {
            self.last_config_check = Instant::now();
            let mtime = config::mtime();
            if mtime != self.config_mtime {
                self.reload_config(ctx);
                log::info!("config.toml reloaded");
            }
        }

        if log::log_enabled!(log::Level::Debug) {
            ctx.input(|i| {
                for event in &i.events {
                    log::debug!("event: {event:?} (mods now: {:?})", i.modifiers);
                }
            });
        }

        // Order is load-bearing: shortcuts and the "? " prompt machine must
        // both run before any TerminalView clones the frame's input events,
        // and shortcuts first so chords never reach the machine.
        let mut actions = keys::drain_shortcuts(ctx);
        if self.settings_open
            && ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
            })
        {
            self.settings_open = false;
        }
        self.ai_intercept(ctx);

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
        for action in tabbar::show(ctx, &labels, self.active, &self.ui_theme) {
            match action {
                TabBarAction::Select(i) => actions.push(Action::GotoTab(i)),
                TabBarAction::NewTab => actions.push(Action::NewTab),
                TabBarAction::OpenSettings => {
                    actions.push(Action::ToggleSettings)
                },
            }
        }

        let mut ui_actions = Vec::new();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(self.ui_theme.bg))
            .show(ctx, |ui| {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    let rect = ui.max_rect();
                    let mut rects = HashMap::new();
                    // While settings or the "? " compose line own the
                    // keyboard, the sentinel matches no pane, so the
                    // terminal stops re-grabbing focus every frame.
                    let focused = if self.settings_open || self.ai.composing()
                    {
                        PaneId(u64::MAX)
                    } else {
                        tab.focused
                    };
                    show_node(
                        ui,
                        &mut tab.tree,
                        rect,
                        1,
                        &mut tab.panes,
                        focused,
                        &self.font,
                        &self.term_theme,
                        &self.ui_theme,
                        &mut rects,
                        &mut ui_actions,
                    );
                    if tab.panes.len() > 1 {
                        if let Some(focused_rect) = rects.get(&tab.focused) {
                            ui.painter().rect_stroke(
                                focused_rect.expand(1.0),
                                CornerRadius::same(2),
                                Stroke::new(1.0, self.ui_theme.accent),
                                StrokeKind::Outside,
                            );
                        }
                    }
                    if let ai_prompt::State::Compose { buffer, error } =
                        &self.ai.state
                    {
                        if let Some(rect) =
                            self.ai.pane.and_then(|p| rects.get(&p))
                        {
                            draw_ai_overlay(
                                ui,
                                *rect,
                                buffer,
                                error.as_deref(),
                                self.agent,
                                &self.font,
                                &self.ui_theme,
                            );
                        }
                    }
                    tab.last_rects = rects;
                }
            });

        if self.settings_open {
            self.show_settings(ctx);
        }

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

/// Write the captured scrollback where the agent command's stdin
/// redirection can read it. $TMPDIR is per-user private on macOS; the file
/// is overwritten by the pane's next submit and never removed eagerly - the
/// agent reads it while running.
fn write_context_file(pane: PaneId, capture: &str) -> Option<PathBuf> {
    let path =
        std::env::temp_dir().join(format!("muxterm-ctx-{}.txt", pane.0));
    let content = format!(
        "Recent output of this terminal pane (oldest first):\n{capture}\n"
    );
    match fs::write(&path, content) {
        Ok(()) => Some(path),
        Err(e) => {
            log::warn!("could not write agent context file: {e:#}");
            None
        },
    }
}

/// The "? " compose line: an accent strip pinned to the bottom of the pane
/// that owns the pending AI query.
fn draw_ai_overlay(
    ui: &egui::Ui,
    pane_rect: Rect,
    buffer: &str,
    error: Option<&str>,
    agent: &Agent,
    font: &FontId,
    theme: &UiTheme,
) {
    let row_h = ui.fonts(|f| f.row_height(font));
    let char_w = ui.fonts(|f| f.glyph_width(font, 'M'));
    let rect = Rect::from_min_max(
        egui::pos2(pane_rect.min.x, pane_rect.max.y - (row_h + 12.0)),
        pane_rect.max,
    );
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::ZERO,
        theme::blend(theme.bg, theme.accent, 0.18),
    );
    painter.line_segment(
        [rect.left_top(), rect.right_top()],
        Stroke::new(1.0, theme.accent),
    );

    let y = rect.center().y;
    let (hint_text, hint_color) = match error {
        Some(e) => (e.to_string(), egui::Color32::from_rgb(224, 82, 82)),
        None => (
            format!("enter run · esc cancel · {}", agent.label),
            theme.text_dim,
        ),
    };
    let hint = painter.layout_no_wrap(
        hint_text,
        FontId::proportional(11.0),
        hint_color,
    );
    let hint_pos = egui::pos2(
        rect.max.x - 10.0 - hint.size().x,
        y - hint.size().y / 2.0,
    );

    let prefix =
        painter.layout_no_wrap("? ".into(), font.clone(), theme.accent);
    let text_left = rect.min.x + 10.0 + prefix.size().x;
    painter.galley(
        egui::pos2(rect.min.x + 10.0, y - prefix.size().y / 2.0),
        prefix,
        theme.accent,
    );

    // Tail-truncate against the hint; the buffer's cursor is always at the
    // end, so the newest text is the part that must stay visible. One cell
    // is reserved for the block cursor (monospace makes this exact).
    let avail = (hint_pos.x - 16.0 - text_left).max(0.0);
    let budget = ((avail / char_w) as usize).saturating_sub(1);
    let count = buffer.chars().count();
    let visible: String = if count > budget {
        buffer.chars().skip(count - budget).collect()
    } else {
        buffer.to_string()
    };
    let text =
        painter.layout_no_wrap(visible, font.clone(), theme.text);
    let cursor_x = text_left + text.size().x;
    painter.galley(
        egui::pos2(text_left, y - text.size().y / 2.0),
        text,
        theme.text,
    );
    painter.rect_filled(
        Rect::from_min_size(
            egui::pos2(cursor_x + 1.0, y - row_h / 2.0),
            Vec2::new(char_w, row_h),
        ),
        CornerRadius::ZERO,
        theme.accent,
    );
    painter.galley(hint_pos, hint, hint_color);
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
    ui_theme: &UiTheme,
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
            // Wash unfocused panes toward the background, like iTerm's
            // "dim inactive split panes".
            if *id != focused
                && panes.len() > 1
                && ui_theme.dim_overlay.a() > 0
            {
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::ZERO,
                    ui_theme.dim_overlay,
                );
            }
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
                ui_theme.divider,
            );
            show_node(
                ui, first, first_rect, path << 1, panes, focused, font,
                term_theme, ui_theme, rects, ui_actions,
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
                ui_theme,
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
