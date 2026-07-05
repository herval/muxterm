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

use muxterm::agent::{self, Agent};

use crate::ai_prompt::{self, LineTracker, PromptMachine, Verdict};
use crate::config;
use crate::keys::{self, Action};
use muxterm::layout::{self, Node, PaneId, Removal, SplitAxis};
use muxterm::mesh;
use crate::pane::Pane;
use crate::settings;
use muxterm::state::{self, LoadResult, NodeState, StateFile, TabState, WindowState};
use crate::tabbar::{self, TabBarAction};
use crate::theme::{self, UiTheme};
use crate::tmux::{self, TmuxCtl};

const PANE_GAP: f32 = 4.0;

/// Ceiling on `mux split` requests per tab (the user's own splits are
/// ungated); a confused agent must not be able to shred the layout.
const AGENT_SPLIT_MAX_PANES: usize = 8;

pub struct Tab {
    /// Stable id (`mux-tab-<8hex>`) scoping the agent mesh to this tab.
    pub tab_id: String,
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
    /// Per-pane title badges on split tabs (config `pane_titles`).
    pane_titles: bool,
    /// Mouse selections copy to the clipboard as soon as they finish
    /// (config `copy_on_select`; the tmux side lives in tmux.conf).
    copy_on_select: bool,
    settings_open: bool,
    dirty: bool,
    config_mtime: Option<SystemTime>,
    last_config_check: Instant,
    /// The "?" prompt line.
    ai: PromptMachine,
    agent: &'static Agent,
    agent_context_lines: u32,
    /// Cache of `binary_available` probes; misses are evicted on failed
    /// submits so an install-then-retry works without a restart.
    agent_ok: HashMap<&'static str, bool>,
    /// session -> registered agent (mesh registry, polled like the config).
    agents: HashMap<String, mesh::AgentInfo>,
    agents_mtime: Option<SystemTime>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, tmux: TmuxCtl) -> Self {
        config::ensure_default_file();
        let (style, custom_font) = config::resolve(&config::load());
        config::install_fonts(&cc.egui_ctx, custom_font);
        theme::apply_visuals(&cc.egui_ctx, &style.ui);
        if let Err(e) = tmux.write_conf(style.copy_on_select) {
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
            pane_titles: style.pane_titles,
            copy_on_select: style.copy_on_select,
            settings_open: false,
            dirty: false,
            config_mtime: config::mtime(),
            last_config_check: Instant::now(),
            ai: PromptMachine::default(),
            agent: style.agent,
            agent_context_lines: style.agent_context_lines,
            agent_ok: HashMap::new(),
            agents: mesh::load_registry().agents.into_iter().collect(),
            agents_mtime: mesh::registry_mtime(),
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

        // Mesh housekeeping keyed strictly off live sessions/tabs, so it
        // can never remove a live agent (and is safe even when the state
        // file was corrupt).
        let live_sessions: HashSet<String> =
            app.tmux.list_sessions().into_iter().collect();
        let live_tabs: HashSet<String> =
            app.tabs.iter().map(|t| t.tab_id.clone()).collect();
        mesh::prune(&live_sessions, &live_tabs);
        // Spooled split requests are from writers that gave up long ago.
        mesh::clear_split_requests();

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
        // run, so the "?" prompt stays inert there until the first Enter.
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
            line: if restored {
                LineTracker::Dirty
            } else {
                LineTracker::Known(0)
            },
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
                    tab_id: mesh::new_tab_id(),
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
        // Backfill ids for pre-mesh state files; the dirty flag persists it.
        let tab_id = if saved.id.is_empty() {
            self.dirty = true;
            mesh::new_tab_id()
        } else {
            saved.id
        };
        self.tabs.push(Tab {
            tab_id,
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

    /// Agent-requested splits (`mux split`): the CLI spools a request file,
    /// we split the requester's pane with the pre-agreed session name, and
    /// the CLI sees the session appear on the tmux socket (or a refusal
    /// file) on its side.
    fn drain_split_requests(&mut self, ctx: &egui::Context) {
        for req in mesh::take_split_requests() {
            if let Err(reason) = self.apply_split_request(ctx, &req) {
                log::warn!("refused split from {}: {reason}", req.from);
                mesh::write_split_refusal(&req.session, &reason);
            }
        }
    }

    /// Unlike a user split this targets the *requester's* pane, not the
    /// focused one, and steals no focus - the user may be typing elsewhere.
    fn apply_split_request(
        &mut self,
        ctx: &egui::Context,
        req: &mesh::SplitRequest,
    ) -> Result<(), String> {
        if req.v != 1 {
            return Err(format!("unsupported split request version {}", req.v));
        }
        if mesh::now().saturating_sub(req.ts) > 30 {
            return Err("request expired before muxterm saw it".to_string());
        }
        // The name must be fresh: `new-session -A` on an existing session
        // would attach it here, hijacking it into this tab's layout.
        if !req.session.starts_with(mesh::SESSION_PREFIX)
            || self.tmux.list_sessions().contains(&req.session)
            || self
                .tabs
                .iter()
                .any(|t| t.panes.values().any(|p| p.session == req.session))
        {
            return Err(format!("session name {:?} is not usable", req.session));
        }
        let found = self.tabs.iter().enumerate().find_map(|(i, tab)| {
            tab.panes
                .iter()
                .find(|(_, p)| p.session == req.from)
                .map(|(id, _)| (i, *id))
        });
        let Some((tab_idx, target)) = found else {
            return Err(format!("session {} is not a muxterm pane", req.from));
        };
        if self.tabs[tab_idx].panes.len() >= AGENT_SPLIT_MAX_PANES {
            return Err(format!(
                "tab already has {AGENT_SPLIT_MAX_PANES} panes; close one first"
            ));
        }
        let pane = self
            .create_pane(ctx, Some(req.session.clone()), req.cwd.clone())
            .map_err(|e| format!("failed to create pane: {e:#}"))?;
        let id = pane.id;
        let session = pane.session.clone();
        let tab = &mut self.tabs[tab_idx];
        if tab.tree.split(target, req.axis, id) {
            tab.panes.insert(id, pane);
            self.dirty = true;
            Ok(())
        } else {
            self.tmux.kill_session(&session);
            Err("requesting pane vanished mid-split".to_string())
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
            mesh::remove_session(&pane.session);
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
                PtyEvent::ResetTitle => {
                    if let Some(pane) = self.pane_mut(backend_id) {
                        pane.title = "shell".into();
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
                        id: tab.tab_id.clone(),
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
        self.pane_titles = style.pane_titles;
        self.agent = style.agent;
        self.agent_context_lines = style.agent_context_lines;
        if self.copy_on_select != style.copy_on_select {
            self.copy_on_select = style.copy_on_select;
            // The drag-end side of copy-on-select is a tmux binding;
            // rewrite the conf and re-source it into the running server
            // (config files only apply on server start).
            if let Err(e) = self.tmux.write_conf(self.copy_on_select) {
                log::error!("failed to write tmux.conf: {e:#}");
            }
            self.tmux.source_conf();
        }
    }

    /// Route keyboard events through the "?" prompt machine before any
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
        let mut line = pane.line;
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
                    &mut line,
                    &mut at_shell,
                ) {
                    Verdict::Pass => kept.push(event),
                    Verdict::Consume => {},
                    Verdict::Submit(query) => submit = Some(query),
                }
            }
            i.events = kept;
        });

        if let Some(query) = submit {
            // Both binaries must exist: mux runs the query (streaming the
            // agent's output), the agent CLI answers it.
            let mut missing: Option<&'static str> = None;
            for bin in ["mux", self.agent.bin] {
                let ok = *self
                    .agent_ok
                    .entry(bin)
                    .or_insert_with(|| agent::binary_available(bin));
                if !ok {
                    missing = Some(bin);
                    break;
                }
            }
            if let Some(bin) = missing {
                self.agent_ok.remove(bin);
                machine.set_error(format!("{bin} not found in PATH"));
            } else {
                let ctx_file = (self.agent_context_lines > 0)
                    .then(|| {
                        self.tmux
                            .capture_pane(&session, self.agent_context_lines)
                    })
                    .flatten()
                    .and_then(|capture| write_context_file(focused, &capture));
                let mut cmd = agent::ask_command(&query, ctx_file.as_deref())
                    .into_bytes();
                cmd.push(b'\r');
                writes.push(cmd);
                machine.cancel();
                line = LineTracker::Known(0);
            }
        }

        self.ai = machine;
        if let Some(pane) = self
            .tabs
            .get_mut(self.active)
            .and_then(|tab| tab.panes.get_mut(&focused))
        {
            pane.line = line;
            for bytes in writes {
                pane.backend.process_command(BackendCommand::Write(bytes));
            }
        }
    }

    /// cmd+c when the selection lives in tmux, not in the local grid: the
    /// widget sees an empty local selection and ignores the Copy event, so
    /// a copy-mode selection (the copy_on_select=off drag path, or any
    /// scrollback selection) would be uncopyable. Ask tmux to copy it; the
    /// text reaches the clipboard through the OSC 52 round trip
    /// (PtyEvent::ClipboardStore). The event is left in place - the widget's
    /// own Copy handling stays a no-op for an empty local selection.
    fn copy_intercept(&mut self, ctx: &egui::Context) {
        if self.settings_open {
            return;
        }
        let copied = ctx.input(|i| {
            i.events.iter().any(|e| matches!(e, egui::Event::Copy))
        });
        if !copied {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(pane) = tab.panes.get(&tab.focused) else {
            return;
        };
        // A local (shift+drag) selection is the terminal widget's to copy.
        if pane.backend.last_content().selectable_range.is_some() {
            return;
        }
        if self.tmux.selection_present(&pane.session) {
            self.tmux.copy_selection(&pane.session);
        }
    }

    fn show_settings(&mut self, ctx: &egui::Context) {
        let out = settings::show(
            ctx,
            &self.ui_theme,
            &self.font,
            &self.theme_name,
            self.agent,
            self.copy_on_select,
            self.pane_titles,
        );
        if let Some(name) = out.theme {
            config::set_theme(name);
            self.reload_config(ctx);
        }
        if let Some(id) = out.agent {
            config::set_agent(id);
            self.reload_config(ctx);
        }
        if let Some(on) = out.copy_on_select {
            config::set_copy_on_select(on);
            // reload_config picks up the flag and re-sources tmux.conf.
            self.reload_config(ctx);
        }
        if let Some(on) = out.pane_titles {
            config::set_pane_titles(on);
            self.reload_config(ctx);
        }
        if let Some(size) = out.font_size {
            self.font.size = size;
            config::set_font_size(size);
            // Sync mtime so the poller doesn't reload and fight the buttons.
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
            let agents_mtime = mesh::registry_mtime();
            if agents_mtime != self.agents_mtime {
                self.agents_mtime = agents_mtime;
                self.agents =
                    mesh::load_registry().agents.into_iter().collect();
            }
            self.drain_split_requests(ctx);
        }

        if log::log_enabled!(log::Level::Debug) {
            ctx.input(|i| {
                for event in &i.events {
                    log::debug!("event: {event:?} (mods now: {:?})", i.modifiers);
                }
            });
        }

        // Order is load-bearing: shortcuts and the "?" prompt machine must
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
        self.copy_intercept(ctx);

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
                let pane = tab.panes.get(&tab.focused);
                tab_label(
                    pane.and_then(|p| self.agents.get(&p.session)),
                    pane.map(|p| p.title.as_str()).unwrap_or("shell"),
                )
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
                    // While settings or the "?" compose line own the
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
                        self.copy_on_select,
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
                        if self.pane_titles {
                            for (id, rect) in &rects {
                                let Some(pane) = tab.panes.get(id) else {
                                    continue;
                                };
                                let label = tab_label(
                                    self.agents.get(&pane.session),
                                    &pane.title,
                                );
                                draw_pane_title(
                                    ui,
                                    *rect,
                                    &label,
                                    *id == tab.focused,
                                    &self.ui_theme,
                                );
                            }
                        }
                    }
                    if let ai_prompt::State::Compose { buffer, error } =
                        &self.ai.state
                    {
                        if let Some((rect, pane)) = self.ai.pane.and_then(|p| {
                            Some((*rects.get(&p)?, tab.panes.get(&p)?))
                        }) {
                            let content = pane.backend.last_content();
                            let size = &content.terminal_size;
                            let point = content.grid.cursor.point;
                            let row = point.line.0
                                + content.grid.display_offset() as i32;
                            let caret = Rect::from_min_size(
                                rect.min
                                    + Vec2::new(
                                        size.cell_width
                                            * point.column.0 as f32,
                                        size.cell_height * row as f32,
                                    ),
                                Vec2::new(
                                    size.cell_width,
                                    size.cell_height,
                                ),
                            );
                            draw_ai_overlay(
                                ui,
                                rect,
                                caret,
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

/// The "?" compose line, drawn inline over the caret's row so the query
/// reads as if typed at the shell prompt. `caret` is the terminal cursor
/// cell in screen coordinates.
fn draw_ai_overlay(
    ui: &egui::Ui,
    pane_rect: Rect,
    caret: Rect,
    buffer: &str,
    error: Option<&str>,
    agent: &Agent,
    font: &FontId,
    theme: &UiTheme,
) {
    let char_w = caret.width().max(1.0);
    let row_h = caret.height().max(1.0);
    // Scrollback can move the caret's row out of view, and a deep prompt
    // can leave it hugging the right edge; pin the strip to the pane and
    // keep at least a dozen cells of entry room.
    let x = caret.min.x.clamp(
        pane_rect.min.x,
        (pane_rect.max.x - 12.0 * char_w).max(pane_rect.min.x),
    );
    let y = caret.min.y.clamp(
        pane_rect.min.y,
        (pane_rect.max.y - row_h).max(pane_rect.min.y),
    );
    let rect = Rect::from_min_max(
        egui::pos2(x, y),
        egui::pos2(pane_rect.max.x, y + row_h),
    );
    let painter = ui.painter();
    painter.rect_filled(
        rect.expand2(Vec2::new(3.0, 2.0)).intersect(pane_rect),
        CornerRadius::same(3),
        theme::blend(theme.bg, theme.accent, 0.18),
    );

    let mid = rect.center().y;
    let prefix =
        painter.layout_no_wrap("? ".into(), font.clone(), theme.accent);
    let text_left = rect.min.x + prefix.size().x;
    painter.galley(
        egui::pos2(rect.min.x, mid - prefix.size().y / 2.0),
        prefix,
        theme.accent,
    );

    // The hint yields when the row runs out of room, but an error always
    // shows - it is the only feedback that a submit was rejected.
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
    let show_hint = error.is_some()
        || rect.max.x - 10.0 - hint.size().x - text_left >= 12.0 * char_w;
    let right_limit = if show_hint {
        rect.max.x - 10.0 - hint.size().x - char_w
    } else {
        rect.max.x - 4.0
    };

    // Tail-truncate against the hint; the buffer's cursor is always at the
    // end, so the newest text is the part that must stay visible. One cell
    // is reserved for the block cursor (monospace makes this exact).
    let avail = (right_limit - text_left).max(0.0);
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
        egui::pos2(text_left, mid - text.size().y / 2.0),
        text,
        theme.text,
    );
    painter.rect_filled(
        Rect::from_min_size(
            egui::pos2(cursor_x + 1.0, y),
            Vec2::new(char_w, row_h),
        ),
        CornerRadius::ZERO,
        theme.accent,
    );
    if show_hint {
        painter.galley(
            egui::pos2(
                rect.max.x - 10.0 - hint.size().x,
                mid - hint.size().y / 2.0,
            ),
            hint,
            hint_color,
        );
    }
}

/// Small badge naming what a pane runs, drawn over its top-right corner.
/// Only split tabs get badges; a lone pane's title is already in the
/// tab bar.
fn draw_pane_title(
    ui: &egui::Ui,
    pane_rect: Rect,
    label: &str,
    focused: bool,
    theme: &UiTheme,
) {
    if label.is_empty() {
        return;
    }
    let font = FontId::proportional(11.0);
    let color = if focused { theme.text } else { theme.text_dim };
    let painter = ui.painter();
    let mut galley =
        painter.layout_no_wrap(label.to_string(), font.clone(), color);
    // Cap the badge at roughly half the pane so it never reads as
    // terminal content; panes too narrow for even a few characters get
    // no badge at all.
    let max_w = pane_rect.width() * 0.5 - 12.0;
    if galley.size().x > max_w {
        let char_w = galley.size().x / label.chars().count().max(1) as f32;
        let budget = (max_w / char_w) as usize;
        if budget < 3 {
            return;
        }
        galley = painter.layout_no_wrap(elide(label, budget), font, color);
    }
    let pad = Vec2::new(6.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let rect = Rect::from_min_size(
        egui::pos2(pane_rect.max.x - size.x - 4.0, pane_rect.min.y + 4.0),
        size,
    );
    // Translucent theme background: readable over any terminal content
    // without fully hiding what's underneath.
    painter.rect_filled(
        rect,
        CornerRadius::same(3),
        theme.bg.gamma_multiply(0.8),
    );
    painter.galley(rect.min + pad, galley, color);
}

/// Head-preserving elision: the interesting part of a badge (agent name,
/// command) comes first.
fn elide(label: &str, budget: usize) -> String {
    if label.chars().count() <= budget {
        return label.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let mut out: String = label.chars().take(budget - 1).collect();
    out.push('…');
    out
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
    copy_on_select: bool,
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
                .set_theme(term_theme.clone())
                .set_copy_on_select(copy_on_select);
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
                term_theme, ui_theme, copy_on_select, rects, ui_actions,
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
                copy_on_select,
                rects,
                ui_actions,
            );
        },
    }
}

/// Tab label: registered agents show as `● name · role`, everything else
/// falls back to the pane's OSC title.
fn tab_label(agent: Option<&mesh::AgentInfo>, title: &str) -> String {
    match agent {
        Some(info) => match &info.role {
            Some(role) => format!("● {} · {}", info.name, role),
            None => format!("● {}", info.name),
        },
        None => title.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_labels() {
        let agent = |role: Option<&str>| mesh::AgentInfo {
            name: "alice".into(),
            role: role.map(str::to_string),
            desc: None,
            joined_at: 0,
        };
        assert_eq!(tab_label(None, "zsh"), "zsh");
        assert_eq!(tab_label(Some(&agent(None)), "zsh"), "● alice");
        assert_eq!(
            tab_label(Some(&agent(Some("reviewer"))), "zsh"),
            "● alice · reviewer"
        );
    }

    #[test]
    fn elide_keeps_the_head() {
        assert_eq!(elide("zsh", 10), "zsh");
        assert_eq!(elide("● alice · reviewer", 8), "● alice…");
        assert_eq!(elide("abcdef", 1), "…");
        assert_eq!(elide("abcdef", 0), "");
    }
}
