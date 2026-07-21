use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use egui::{
    CornerRadius, CursorIcon, FontId, Rect, RichText, Sense, Stroke,
    StrokeKind, Vec2,
};
use egui_term::{
    BackendCommand, FontSettings, PtyEvent, RepaintPolicy, TerminalBackend,
    TerminalFont, TerminalMode, TerminalTheme, TerminalView,
};

use muxterm::agent::{self, Agent};

use crate::ai_prompt::{self, LineTracker, PromptMachine, Verdict};
use crate::attention;
use crate::bg_jobs;
use crate::config;
use crate::keys::{self, Action};
use muxterm::layout::{self, Node, PaneId, Removal, SplitAxis};
use muxterm::mesh;
use crate::git_status;
use crate::pane::Pane;
use crate::pr_status;
use crate::search::{self, SearchBar, SearchOp};
use crate::settings;
use crate::sidebar::{self, SidebarAction};
use muxterm::state::{self, LoadResult, NodeState, StateFile, TabState, WindowState};
use crate::tabbar::{self, TabBarAction};
use crate::theme::{self, UiTheme};
use crate::tmux::{self, TmuxCtl};
use crate::workspace::{self, Workspace};
use crate::workspace_popup::{self, NewWorkspaceForm};

const PANE_GAP: f32 = 4.0;

/// Ceiling on `mux split` requests per tab (the user's own splits are
/// ungated); a confused agent must not be able to shred the layout.
const AGENT_SPLIT_MAX_PANES: usize = 8;

/// Seconds of tmux `window_activity` past a stuck "attention" ts before the
/// poll tick treats it as stale (the agent moved on but its clearing hook
/// never fired). Wide enough that the permission prompt's own render - near
/// the attention ts - never trips it, so a genuinely blocked, silent prompt
/// keeps its "!". See the agent_states prune below.
const STALE_ATTENTION_GRACE: u64 = 30;

pub struct Tab {
    /// Stable id (`mux-tab-<8hex>`) scoping the agent mesh to this tab.
    pub tab_id: String,
    pub tree: Node,
    pub panes: HashMap<PaneId, Pane>,
    pub focused: PaneId,
    /// Screen rects from the last frame; drives cmd+opt+arrow navigation.
    pub last_rects: HashMap<PaneId, Rect>,
    /// What this tab is for. Always present: bare for a plain cmd+t shell
    /// tab, rich for a cmd+n workspace (folder/worktree/prompt/agent/title).
    pub workspace: Workspace,
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
    /// Which settings tab shows; survives close so cmd+shift+n's "no
    /// projects yet" jump lands on Projects.
    settings_tab: settings::Tab,
    /// The Projects tab's add form, typed across frames.
    project_draft: settings::ProjectDraft,
    /// The Templates tab's edit form, typed across frames.
    template_draft: settings::TemplateDraft,
    dirty: bool,
    config_mtime: Option<SystemTime>,
    last_config_check: Instant,
    /// The "?" prompt line.
    ai: PromptMachine,
    /// The cmd+f scrollback-search bar.
    search: SearchBar,
    agent: &'static Agent,
    agent_context_lines: u32,
    /// Cache of `binary_available` probes; misses are evicted on failed
    /// submits so an install-then-retry works without a restart. Pre-warmed
    /// at startup by a background probe of every registry agent, so the
    /// settings/popup agent lists can hide uninstalled CLIs.
    agent_ok: HashMap<&'static str, bool>,
    /// Results from background `binary_available` probes (bin -> ok).
    probe_rx: Receiver<(&'static str, bool)>,
    probe_tx: Sender<(&'static str, bool)>,
    /// session -> registered agent (mesh registry, polled like the config).
    agents: HashMap<String, mesh::AgentInfo>,
    agents_mtime: Option<SystemTime>,
    /// session -> GitHub PR badges - every PR the session's checkout has
    /// touched - streamed in by the pr_status poller thread; the atomic
    /// gates that thread live. One poller feeds two features: the HUD
    /// chips (config `pr_status`) and the cmd+clickable `#123` tokens
    /// (config `pr_detector`), each gated at its own consumer.
    pr: HashMap<String, Vec<pr_status::Badge>>,
    pr_rx: Receiver<HashMap<String, Vec<pr_status::Badge>>>,
    pr_enabled: Arc<AtomicBool>,
    pr_status: bool,
    pr_detector: bool,
    /// session -> the repo's GitHub web base ("https://github.com/o/r"),
    /// derived from its badges; the pane link openers resolve a clicked
    /// `#123` through it. Arc'd into those `Send + Sync` closures - both
    /// sides run on the UI thread, the lock is never contended.
    pr_link_bases: Arc<Mutex<HashMap<String, String>>>,
    /// Chips right-clicked away, shared with the poller (which forgets
    /// them) and used to filter snapshots it composed before it did.
    pr_dismissed: pr_status::Dismissed,
    /// session -> git branch/dirty state (config `git_status`), streamed in
    /// by the git_status poller thread; the atomic gates that thread live.
    git: HashMap<String, git_status::Git>,
    git_rx: Receiver<HashMap<String, git_status::Git>>,
    git_enabled: Arc<AtomicBool>,
    git_status: bool,
    /// Dock bounce + banner on bell/`mux notify` while unfocused
    /// (config `notifications`); tab badges are not gated by this.
    notifications: bool,
    /// The workspace sidebar's visibility (cmd+\); persisted in state.json.
    sidebar_open: bool,
    /// Whether the sidebar's archived pile is folded to its header (its
    /// header click); persisted in state.json like the sidebar itself.
    archived_collapsed: bool,
    /// The open workspace-creation popup (cmd+n), or None.
    new_workspace: Option<NewWorkspaceForm>,
    /// Folder the creation popup pre-fills - the last one a workspace used.
    last_workspace_dir: Option<String>,
    /// The saved project registry (Settings > Projects): what cmd+shift+n
    /// starts sessions from. Persisted in state.json.
    projects: Vec<workspace::Project>,
    /// The saved workspace-layout templates (Settings > Templates): the
    /// multi-pane presets the new-workspace popup can apply. Persisted in
    /// state.json.
    templates: Vec<workspace::Template>,
    /// tab_id -> AI-generated title, streamed in by `workspace::spawn_title`.
    title_tx: Sender<(String, String)>,
    title_rx: Receiver<(String, String)>,
    /// tab_ids with a title generation in flight. A drained title only lands
    /// while its tab is still here; `mux rename` removes it, so a deliberate
    /// name can't be clobbered by a late auto-title.
    naming: HashSet<String>,
    /// session -> foreground command + cwd, refreshed once a second (one
    /// `list-panes -a` round trip). The command is liveness for the
    /// agent-state files (a pane whose foreground returned to a shell has no
    /// live agent, however its hooks died); the cwd feeds the workspace-root
    /// sync.
    pane_snap: HashMap<String, tmux::PaneSnap>,
    /// session -> hook-reported agent state (working/idle/attention), read
    /// from ~/.muxterm/agent-state on the poll tick. The sole driver of the
    /// sidebar's working/attention dot: agents report themselves through
    /// their lifecycle hooks -> `mux agent-event` (installed by
    /// agent_hooks::ensure_installed); non-agent programs never light it.
    agent_states: std::collections::BTreeMap<String, mesh::AgentState>,
    /// Sessions whose idle agent still has a live Claude-style background
    /// shell (Bash run_in_background): Stop reported idle but a job is in
    /// flight. Derived on the poll tick, never written to agent-state. Feeds
    /// the sidebar's Background status; only idle agent panes are scanned -
    /// Working and Blocked outrank Background, so scanning them could never
    /// change a pixel. Refreshed by one-shot bg_jobs::spawn_scan threads: a
    /// full ps is tens of ms, too slow for the synchronous tick.
    bg_jobs: HashSet<String>,
    bg_tx: Sender<HashSet<String>>,
    bg_rx: Receiver<HashSet<String>>,
    /// A ps scan is in flight; the tick starts at most one at a time.
    bg_scan_inflight: bool,
    /// tab_id -> worktree progress/result, streamed in by
    /// `workspace::spawn_worktree`. The checkout runs off the UI thread into
    /// the pre-claimed directory the pane already sits in; when it lands we
    /// launch the agent, so a big/lfs repo never freezes the window.
    worktree_tx: Sender<(String, workspace::WorktreeMsg)>,
    worktree_rx: Receiver<(String, workspace::WorktreeMsg)>,
    /// Tabs whose worktree checkout is still in flight: their pane sits in a
    /// claimed directory the workspace doesn't reference yet, so the root
    /// sync must not read that as "the panes left".
    pending_worktrees: HashSet<String>,
    /// tab_id -> what the checkout thread is doing right now ("cloning …",
    /// "fetching …", "checking out worktree…"), floated over the agent pane
    /// while the tab is pending. Seeded at Create, updated on Progress,
    /// removed on Done - render-only, never persisted.
    worktree_progress: HashMap<String, String>,
    /// tab_id -> the template's extra panes awaiting their boot: (pane, the
    /// command to run after the shared cd + setup). Filled when a template
    /// workspace is created and drained by `launch_template_panes` on the same
    /// tick the main pane boots (immediately, or after the worktree lands).
    /// Transient, never persisted - the panes themselves are real sessions the
    /// layout tree already captures.
    pending_panes: HashMap<String, Vec<(PaneId, String)>>,
    /// (active tab, window focused) as last published to the panes' repaint
    /// policies - see `sync_repaint_policies`.
    policy_state: Option<(usize, bool)>,
    /// The poll tick's pane snapshot, shared with the pr/git pollers so
    /// they never spawn their own `tmux list-panes`.
    pane_snap_shared: tmux::SharedPanes,
    /// Window focus, shared with the pr/git pollers: they stretch their
    /// scan cadence while nobody can read the chips.
    focused_flag: Arc<AtomicBool>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, tmux: TmuxCtl) -> Self {
        config::ensure_default_file();
        let (style, custom_font, bar_font) = config::resolve(&config::load());
        config::install_fonts(&cc.egui_ctx, custom_font, bar_font);
        theme::apply_visuals(&cc.egui_ctx, &style.ui);
        // The server may have survived a previous run (sessions outlive the
        // app), so source the conf when its content changed - a no-op when
        // no server is up yet.
        match tmux.write_conf(
            style.copy_on_select,
            &theme::search_highlight(&style.ui),
        ) {
            Ok(true) => tmux.source_conf(),
            Ok(false) => {},
            Err(e) => log::error!("failed to write tmux.conf: {e:#}"),
        }

        let (pty_tx, pty_rx) = mpsc::channel();
        let pane_snap_shared: tmux::SharedPanes = Default::default();
        let focused_flag = Arc::new(AtomicBool::new(true));
        let (pr_tx, pr_rx) = mpsc::channel();
        let pr_enabled =
            Arc::new(AtomicBool::new(style.pr_status || style.pr_detector));
        let pr_dismissed: pr_status::Dismissed =
            Arc::new(Mutex::new(HashSet::new()));
        pr_status::spawn(
            cc.egui_ctx.clone(),
            pr_tx,
            pr_enabled.clone(),
            pane_snap_shared.clone(),
            focused_flag.clone(),
            pr_dismissed.clone(),
        );
        let (git_tx, git_rx) = mpsc::channel();
        let git_enabled = Arc::new(AtomicBool::new(style.git_status));
        git_status::spawn(
            cc.egui_ctx.clone(),
            git_tx,
            git_enabled.clone(),
            pane_snap_shared.clone(),
            focused_flag.clone(),
        );
        let (title_tx, title_rx) = mpsc::channel();
        let (worktree_tx, worktree_rx) = mpsc::channel();
        let (bg_tx, bg_rx) = mpsc::channel();
        let (probe_tx, probe_rx) = mpsc::channel();
        // Pre-warm the binary probes for every registry agent so the
        // settings/popup lists can hide uninstalled CLIs. Each probe spawns
        // an interactive login shell, so it must stay off the UI thread.
        spawn_agent_probe(
            agent::AGENTS.iter().map(|a| a.bin).collect(),
            probe_tx.clone(),
            cc.egui_ctx.clone(),
        );
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
            settings_tab: settings::Tab::default(),
            project_draft: settings::ProjectDraft::default(),
            template_draft: settings::TemplateDraft::default(),
            dirty: false,
            config_mtime: config::mtime(),
            last_config_check: Instant::now(),
            ai: PromptMachine::default(),
            search: SearchBar::default(),
            agent: style.agent,
            agent_context_lines: style.agent_context_lines,
            agent_ok: HashMap::new(),
            probe_rx,
            probe_tx,
            agents: mesh::load_registry().agents.into_iter().collect(),
            agents_mtime: mesh::registry_mtime(),
            pr: HashMap::new(),
            pr_rx,
            pr_enabled,
            pr_status: style.pr_status,
            pr_detector: style.pr_detector,
            pr_link_bases: Arc::new(Mutex::new(HashMap::new())),
            pr_dismissed,
            git: HashMap::new(),
            git_rx,
            git_enabled,
            git_status: style.git_status,
            notifications: style.notifications,
            sidebar_open: true,
            archived_collapsed: false,
            new_workspace: None,
            last_workspace_dir: None,
            projects: Vec::new(),
            templates: Vec::new(),
            title_tx,
            title_rx,
            naming: HashSet::new(),
            pane_snap: HashMap::new(),
            agent_states: std::collections::BTreeMap::new(),
            bg_jobs: HashSet::new(),
            bg_tx,
            bg_rx,
            bg_scan_inflight: false,
            worktree_tx,
            worktree_rx,
            pending_worktrees: HashSet::new(),
            worktree_progress: HashMap::new(),
            pending_panes: HashMap::new(),
            policy_state: None,
            pane_snap_shared,
            focused_flag,
        };

        // Wire the agents' lifecycle hooks to `mux agent-event` (the sidebar
        // status dot). Off-thread: it probes the login shell for mux's path.
        std::thread::spawn(crate::agent_hooks::ensure_installed);

        match state::load() {
            LoadResult::Loaded(saved) => {
                app.sidebar_open = saved.sidebar_open;
                app.archived_collapsed = saved.archived_collapsed;
                app.last_workspace_dir = saved.last_workspace_dir.clone();
                app.projects = saved
                    .projects
                    .iter()
                    .cloned()
                    .map(workspace::Project::from_state)
                    .collect();
                app.templates = saved
                    .templates
                    .iter()
                    .cloned()
                    .map(workspace::Template::from_state)
                    .collect();
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
        // Spooled split requests are from writers that gave up long ago,
        // and spooled raises are stale by the next launch.
        mesh::clear_split_requests();
        mesh::clear_notify_requests();
        mesh::clear_rename_requests();
        mesh::clear_newtab_requests();

        // Reclaim empty claim dirs that failed checkouts left behind
        // (deleting them at failure time would yank a booting shell's cwd).
        let snap = app.tmux.pane_snapshot();
        let referenced: Vec<&Path> = app
            .tabs
            .iter()
            .filter_map(|t| t.workspace.worktree.as_ref())
            .map(|w| w.path.as_path())
            .collect();
        let cwds: Vec<&Path> = snap
            .values()
            .filter_map(|p| p.cwd.as_deref())
            .collect();
        workspace::sweep_stale_claims(&referenced, &cwds);

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
        let mut backend = TerminalBackend::new(
            id.0,
            ctx.clone(),
            self.pty_tx.clone(),
            self.tmux
                .spawn_settings(&session, start_dir, !theme::is_light(self.ui_theme.bg)),
        )?;
        // cmd+clicked URLs/paths: relative paths resolve against this
        // pane's cwd, so the opener is tied to the session. A `#123`
        // candidate (config `pr_detector`) resolves through the shared
        // base map instead - looked up at click time, so a repo learned
        // after the pane opened still resolves.
        let (tmux, opener_session) = (self.tmux.clone(), session.clone());
        let pr_bases = self.pr_link_bases.clone();
        backend.set_link_opener(move |texts| {
            let base =
                pr_bases.lock().unwrap().get(&opener_session).cloned();
            crate::links::spawn_open(
                tmux.clone(),
                opener_session.clone(),
                texts.to_vec(),
                base,
            )
        });
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
            attn: attention::Cell::new(Instant::now()),
        })
    }

    /// cwd of the active tab's focused pane, for cwd inheritance.
    fn focused_cwd(&self) -> Option<String> {
        let tab = self.tabs.get(self.active)?;
        let pane = tab.panes.get(&tab.focused)?;
        self.tmux.pane_current_path(&pane.session)
    }

    /// The cwd to seed a new tab/workspace with: the focused pane's cwd, unless
    /// it sits inside this tab's own worktree - then the worktree's parent repo,
    /// so new work opens in the real project (see `workspace::escape_worktree`).
    fn new_tab_cwd(&self) -> Option<String> {
        let cwd = self.focused_cwd()?;
        let ws = &self.tabs.get(self.active)?.workspace;
        Some(
            workspace::escape_worktree(
                Path::new(&cwd),
                ws.worktree.as_ref().map(|w| w.path.as_path()),
                ws.root.as_deref(),
            )
            .display()
            .to_string(),
        )
    }

    fn new_tab(&mut self, ctx: &egui::Context, session: Option<String>) {
        let start_dir = self.new_tab_cwd();
        let workspace =
            Workspace::bare(start_dir.as_ref().map(PathBuf::from));
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
                    workspace,
                });
                self.active = self.tabs.len() - 1;
                self.dirty = true;
            },
            Err(e) => log::error!("failed to open a new tab: {e:#}"),
        }
    }

    /// Indices into `self.tabs` of the visible (non-archived) tabs, in tab
    /// order. Archived tabs stay in `self.tabs` - so their tmux sessions ride
    /// through GC and restore untouched - but drop out of the tab bar and the
    /// cmd+1..9 / next-prev flow, which walk this list instead of raw indices.
    fn visible_tab_indices(&self) -> Vec<usize> {
        self.tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.workspace.is_archived())
            .map(|(i, _)| i)
            .collect()
    }

    /// Step the active tab through the visible list by `delta` with wraparound
    /// (cmd+shift+[ / ]). When the active tab is archived (a peek, so it isn't
    /// in the list), Next lands on the first visible tab and Prev on the last.
    fn step_visible(&mut self, delta: isize) {
        let visible = self.visible_tab_indices();
        if let Some(target) = step_visible_target(&visible, self.active, delta) {
            if target != self.active {
                self.active = target;
                self.dirty = true;
            }
        }
    }

    /// Park a workspace in the sidebar's archived pile. Never touches its tmux
    /// session - the tab stays in `self.tabs`, just hidden from the active
    /// flow. Archiving the active tab moves focus off it to the nearest
    /// remaining visible tab, spawning a fresh bare tab if it was the last one.
    fn archive_tab(&mut self, ctx: &egui::Context, i: usize) {
        match self.tabs.get_mut(i) {
            Some(tab) if !tab.workspace.is_archived() => {
                tab.workspace.archived_at = Some(mesh::now());
            },
            _ => return,
        }
        if i == self.active {
            // `visible` already excludes the tab just archived above.
            let visible = self.visible_tab_indices();
            match nearest_visible(&visible, i) {
                Some(next) => self.active = next,
                None => self.new_tab(ctx, None), // sets active to the new tab
            }
        }
        self.dirty = true;
    }

    /// Pull a workspace back out of the archived pile and bring it to the
    /// foreground.
    fn unarchive_tab(&mut self, i: usize) {
        match self.tabs.get_mut(i) {
            Some(tab) if tab.workspace.is_archived() => {
                tab.workspace.archived_at = None;
            },
            _ => return,
        }
        self.active = i;
        self.dirty = true;
    }

    /// cmd+n: build a workspace from the creation popup's form, opening an
    /// agent-pane/side-shell pair. If a worktree was requested its directory
    /// is claimed synchronously (cheap - a name pick and a mkdir) so both
    /// panes open directly inside it and no `cd` is ever typed; the slow
    /// checkout populates it off the UI thread (see `drain_worktrees`),
    /// which is also what defers the agent launch until the files exist.
    /// Without a worktree the agent launches straight away.
    fn create_workspace(&mut self, ctx: &egui::Context, form: NewWorkspaceForm) {
        if form.submit_blocker().is_some() {
            // The popup gates Create on the same check; anything arriving
            // here anyway is stale. Never create a workspace whose clone
            // is already known to fail.
            log::warn!("refusing workspace: remote preflight not passed");
            return;
        }
        let root = workspace::expand_dir(&form.folder);
        let prompt = form.prompt.trim().to_string();
        let agent = muxterm::agent::by_id(form.agent);
        let model = (!form.model.is_empty()).then(|| form.model.clone());

        // Project mode may need a clone first (a repo project's first use);
        // that path always worktrees, checkbox or no - the checkbox logic
        // reads the missing clone dest as "not a repo".
        let clone = form.clone_needed();
        // A worktree only happens for a git repo; a failed claim falls back
        // to a plain workspace in the root.
        let want_worktree = clone.is_some()
            || (form.create_worktree
                && root.as_deref().is_some_and(workspace::is_git_repo));
        let branch_choice = form.branch_choice();
        let claim = want_worktree
            .then(|| {
                let repo = root.as_deref().expect("want_worktree implies a root");
                workspace::claim_worktree(repo, &branch_choice)
                    .map_err(|e| log::warn!("worktree claim failed: {e:#}"))
                    .ok()
            })
            .flatten();
        if clone.is_some() && claim.is_none() {
            // Without the claim there is nowhere to open panes: the clone
            // dest doesn't exist yet and root falling back to it would just
            // spawn a dead shell.
            log::error!("no worktree claim for a clone; not opening the tab");
            return;
        }
        let start_dir = claim
            .as_ref()
            .map(|w| w.path.clone())
            .or_else(|| root.clone())
            .map(|p| p.display().to_string());

        let pane = match self.create_pane(ctx, None, start_dir.clone()) {
            Ok(p) => p,
            Err(e) => {
                log::error!("failed to open workspace pane: {e:#}");
                return;
            },
        };
        // A workspace opens as a single agent pane (split later with cmd+d);
        // the agent launch types into the tree's *first* leaf, which a lone
        // leaf trivially is. A picked layout template pre-splits it into
        // several: one extra pane per spec, arranged by build_template_tree
        // (split-previous, sized), each running its command after the same
        // cd + setup boot the main pane gets (deferred via pending_panes so
        // the side panes wait for the worktree checkout too).
        let id = pane.id;
        let mut panes = HashMap::new();
        panes.insert(id, pane);
        let template = form.selected_template().cloned();
        let mut ids = vec![id];
        let mut specs: Vec<workspace::TemplatePane> = Vec::new();
        let mut extra_cmds: Vec<(PaneId, String)> = Vec::new();
        if let Some(main) = template.as_ref().and_then(|t| t.panes.first()) {
            // specs stays aligned with ids: index 0 is the main pane (its
            // split/size are ignored), then one entry per extra pane that
            // actually opened - a failed create_pane drops both together.
            specs.push(main.clone());
            for spec in template.as_ref().map(|t| t.extra_panes()).unwrap_or(&[])
            {
                match self.create_pane(ctx, None, start_dir.clone()) {
                    Ok(p) => {
                        let pid = p.id;
                        panes.insert(pid, p);
                        ids.push(pid);
                        specs.push(spec.clone());
                        extra_cmds.push((pid, spec.command.clone()));
                    },
                    Err(e) => log::warn!("template pane failed to open: {e:#}"),
                }
            }
        }
        let tree = if ids.len() > 1 {
            workspace::build_template_tree(&specs, &ids)
        } else {
            Node::Leaf(id)
        };
        let tab_id = mesh::new_tab_id();

        // A prompt-derived placeholder shows instantly; the AI title upgrades
        // it out of band (spawn_title below). A prompt-less workspace has
        // nothing to summarize, so it keeps a random codename like a bare tab.
        let title = if prompt.is_empty() {
            workspace::random_title()
        } else {
            workspace::placeholder_title(&prompt)
        };
        // Plain cmd+n on a folder a saved project points at (by path, or by
        // origin URL for repo projects): inherit that project's setup script,
        // so the fresh worktree boots the same as a cmd+shift+n one. Subdir
        // is deliberately not inherited - the user picked the folder, not
        // the app.
        let setup = match form.selected_project() {
            Some(p) => p.setup.clone(),
            None if want_worktree => {
                let origin = root
                    .as_deref()
                    .filter(|_| self.projects.iter().any(|p| p.repo.is_some()))
                    .and_then(workspace::origin_url);
                root.as_deref()
                    .and_then(|r| {
                        workspace::inherited_setup(
                            &self.projects,
                            r,
                            origin.as_deref(),
                        )
                    })
                    .map(str::to_string)
            },
            None => None,
        };
        let workspace = Workspace {
            title,
            root: root.clone(),
            description: None,
            prompt: prompt.clone(),
            worktree: None, // filled in when the async checkout finishes
            agent: agent.map(|a| a.id),
            model: model.clone(),
            created_at: mesh::now(),
            archived_at: None,
            // The workspace owns its copies; later project edits don't
            // reach it.
            setup,
            subdir: form.selected_project().and_then(|p| p.subdir.clone()),
        };

        self.tabs.push(Tab {
            tab_id: tab_id.clone(),
            tree,
            panes,
            focused: id,
            last_rects: HashMap::new(),
            workspace,
        });
        self.active = self.tabs.len() - 1;

        // Remember the template's extra panes for their boot (immediately
        // below when there's no worktree, else after the checkout lands in
        // drain_worktrees).
        if !extra_cmds.is_empty() {
            self.pending_panes.insert(tab_id.clone(), extra_cmds);
        }

        // The AI title only needs the prompt, so it can start now regardless
        // of the worktree checkout. Best-effort: no agent or a failed call
        // leaves the placeholder title in place.
        if let (Some(agent), false) = (agent, prompt.is_empty()) {
            self.naming.insert(tab_id.clone());
            workspace::spawn_title(
                tab_id.clone(),
                prompt.clone(),
                agent,
                self.title_tx.clone(),
                ctx.clone(),
            );
        }

        if let Some(repo) = clone {
            // Repo project, first use: clone, then resolve the typed branch
            // against the fresh clone, then check out - all off-thread, the
            // agent launch waiting at the end (drain_worktrees).
            self.pending_worktrees.insert(tab_id.clone());
            self.worktree_progress
                .insert(tab_id.clone(), "preparing worktree…".into());
            workspace::spawn_clone_worktree(
                tab_id,
                repo,
                root.clone().expect("a clone implies a root"),
                claim.expect("a clone always claims"),
                form.branch.clone(),
                self.worktree_tx.clone(),
                ctx.clone(),
            );
        } else if let Some(claim) = claim {
            // The pane is already sitting in the claimed directory; the agent
            // launch waits for the checkout to land (drain_worktrees).
            // Project mode freshens the base branch from its remote first.
            self.pending_worktrees.insert(tab_id.clone());
            self.worktree_progress
                .insert(tab_id.clone(), "preparing worktree…".into());
            workspace::spawn_worktree(
                tab_id,
                root.clone().expect("a claim implies a root"),
                claim,
                branch_choice,
                !form.projects.is_empty(),
                self.worktree_tx.clone(),
                ctx.clone(),
            );
        } else {
            // No worktree: run the agent straight away in the root (its cd
            // into a subfolder project's subdir rides launch_agent), then
            // boot the template's side panes in the same place.
            self.launch_agent(&tab_id, None, None);
            self.launch_template_panes(&tab_id, None, false);
        }

        // Project roots (often a ~/.muxterm/clones dest) would make a
        // strange cmd+n prefill; only folder-picked workspaces remember.
        if let (Some(r), true) = (&root, form.projects.is_empty()) {
            self.last_workspace_dir = Some(r.display().to_string());
        }
        self.dirty = true;
    }

    /// Type the workspace's boot sequence into its first pane: an optional
    /// failure notice (echoed so the user sees *why* they're not in a
    /// worktree), an optional `cd`, the project's setup script, then the
    /// agent's launch command. The pane spawns where it belongs (root or
    /// claimed worktree), so the `cd` is either the project's *subfolder*
    /// (`workspace::boot_cd` - monorepo projects work in a subdir of the
    /// checkout, entered before setup runs) or, on the failed-checkout
    /// path, the caller's `fallback` - which isn't the project, so setup
    /// is skipped there (a `direnv allow` in $HOME is pure noise). Setup
    /// lines are typed as-is, one per line, exactly like a user would - a
    /// slow or failed line delays but never blocks the launch. A
    /// prompt-less workspace without setup is left as a plain shell.
    fn launch_agent(
        &mut self,
        tab_id: &str,
        fallback: Option<&std::path::Path>,
        notice: Option<&str>,
    ) {
        let Some(tab) = self.tabs.iter().find(|t| t.tab_id == tab_id) else {
            return;
        };
        let pane_id = tab.tree.first_leaf();
        let agent = tab.workspace.agent.and_then(muxterm::agent::by_id);
        let model = tab.workspace.model.clone();
        let prompt = tab.workspace.prompt.clone();
        let setup = tab.workspace.setup.clone();
        let failed = fallback.is_some();
        let cd = match fallback {
            Some(p) => Some(p.to_path_buf()),
            None => workspace::boot_cd(
                tab.workspace.worktree.as_ref().map(|w| w.path.as_path()),
                tab.workspace.root.as_deref(),
                tab.workspace.subdir.as_deref(),
            ),
        };

        let mut lines: Vec<String> = Vec::new();
        if let Some(n) = notice {
            lines.push(format!(
                "echo {}",
                muxterm::agent::shell_quote(&format!("[muxterm] {n}"))
            ));
        }
        if let Some(p) = &cd {
            lines.push(format!(
                "cd {}",
                muxterm::agent::shell_quote(&p.display().to_string())
            ));
        }
        if let (Some(setup), false) = (&setup, failed) {
            lines.extend(
                setup.lines().map(str::to_string).filter(|l| !l.is_empty()),
            );
        }
        if let (Some(a), false) = (agent, prompt.is_empty()) {
            lines.push(muxterm::agent::launch_command(
                a,
                model.as_deref(),
                &prompt,
            ));
        }
        if lines.is_empty() {
            return;
        }

        let mut bytes = lines.join("\r").into_bytes();
        bytes.push(b'\r');
        if let Some(pane) = self
            .tabs
            .iter_mut()
            .find(|t| t.tab_id == tab_id)
            .and_then(|t| t.panes.get_mut(&pane_id))
        {
            pane.backend.process_command(BackendCommand::Write(bytes));
        }
    }

    /// Type a `cd` into every pane of a tab *except* the first leaf (whose
    /// boot sequence `launch_agent` owns) and the template's own panes (whose
    /// full boot `launch_template_panes` owns): panes the user split while the
    /// checkout was pending follow the workspace to its subfolder on
    /// success and back out on the failed-checkout walk-back.
    fn cd_side_panes(&mut self, tab_id: &str, dir: &std::path::Path) {
        // Snapshot the template panes before borrowing tabs - they get their
        // cd (plus setup + command) from launch_template_panes, so cd'ing them
        // here too would double-fire.
        let template_ids: HashSet<PaneId> = self
            .pending_panes
            .get(tab_id)
            .map(|v| v.iter().map(|(id, _)| *id).collect())
            .unwrap_or_default();
        let Some(tab) = self.tabs.iter_mut().find(|t| t.tab_id == tab_id)
        else {
            return;
        };
        let agent_pane = tab.tree.first_leaf();
        let cd = format!(
            "cd {}\r",
            muxterm::agent::shell_quote(&dir.display().to_string())
        );
        for (pane_id, pane) in tab.panes.iter_mut() {
            if *pane_id != agent_pane && !template_ids.contains(pane_id) {
                pane.backend.process_command(BackendCommand::Write(
                    cd.clone().into_bytes(),
                ));
            }
        }
    }

    /// Boot the template's extra panes: type `cd <boot_cd/fallback>`, the
    /// workspace's setup script, then each pane's own command - mirroring
    /// `launch_agent` for the main pane, and drained from `pending_panes` so
    /// the side panes wait for the same worktree checkout the agent does. As
    /// with the main pane, a failed checkout (`notice_failed`) cds to the
    /// `fallback` and skips setup, but still runs the command (a terminal or
    /// gitwatch in the root is still useful). No-ops without a stashed entry.
    fn launch_template_panes(
        &mut self,
        tab_id: &str,
        fallback: Option<&std::path::Path>,
        notice_failed: bool,
    ) {
        let Some(commands) = self.pending_panes.remove(tab_id) else {
            return;
        };
        let Some(tab) = self.tabs.iter().find(|t| t.tab_id == tab_id) else {
            return;
        };
        let setup = tab.workspace.setup.clone();
        let cd = match fallback {
            Some(p) => Some(p.to_path_buf()),
            None => workspace::boot_cd(
                tab.workspace.worktree.as_ref().map(|w| w.path.as_path()),
                tab.workspace.root.as_deref(),
                tab.workspace.subdir.as_deref(),
            ),
        };
        for (pane_id, command) in commands {
            let lines = workspace::pane_boot_lines(
                cd.as_deref(),
                setup.as_deref(),
                &command,
                notice_failed,
            );
            if lines.is_empty() {
                continue;
            }
            let mut bytes = lines.join("\r").into_bytes();
            bytes.push(b'\r');
            if let Some(pane) = self
                .tabs
                .iter_mut()
                .find(|t| t.tab_id == tab_id)
                .and_then(|t| t.panes.get_mut(&pane_id))
            {
                pane.backend.process_command(BackendCommand::Write(bytes));
            }
        }
    }

    /// Apply finished worktree checkouts: the pane opened inside the claimed
    /// directory, so success just records the worktree and launches the agent
    /// in place. A failed checkout cd's the pane back (root, or home when
    /// even the root never materialized - a failed clone) and launches
    /// there, echoing the failure so the user sees why - the workspace
    /// still works, just degraded.
    fn drain_worktrees(&mut self) {
        while let Ok((tab_id, msg)) = self.worktree_rx.try_recv() {
            let result = match msg {
                workspace::WorktreeMsg::Progress(line) => {
                    // Render-only: must not mark state dirty per line.
                    self.worktree_progress.insert(tab_id, line);
                    continue;
                },
                workspace::WorktreeMsg::Done(result) => result,
            };
            self.worktree_progress.remove(&tab_id);
            self.pending_worktrees.remove(&tab_id);
            match result {
                Ok(wt) => {
                    let mut side_cd = None;
                    if let Some(tab) =
                        self.tabs.iter_mut().find(|t| t.tab_id == tab_id)
                    {
                        tab.workspace.worktree = Some(wt);
                        // Panes split during the checkout follow into a
                        // subfolder project's subdir; the agent pane's cd
                        // rides launch_agent.
                        side_cd = workspace::boot_cd(
                            tab.workspace
                                .worktree
                                .as_ref()
                                .map(|w| w.path.as_path()),
                            tab.workspace.root.as_deref(),
                            tab.workspace.subdir.as_deref(),
                        );
                    }
                    self.launch_agent(&tab_id, None, None);
                    if let Some(dir) = side_cd {
                        self.cd_side_panes(&tab_id, &dir);
                    }
                    // After cd_side_panes (which reads pending_panes to skip
                    // template panes), boot the template's own panes.
                    self.launch_template_panes(&tab_id, None, false);
                },
                Err(e) => {
                    log::warn!("worktree creation failed: {e}");
                    // The walk-back target must exist: after a failed
                    // *clone* the root never came to be, so land at home
                    // and repoint the workspace off the ghost path (the
                    // root sync must not keep chasing it).
                    let root = self
                        .tabs
                        .iter()
                        .find(|t| t.tab_id == tab_id)
                        .and_then(|t| t.workspace.root.clone());
                    let fallback = root
                        .filter(|r| r.exists())
                        .or_else(dirs::home_dir);
                    if let Some(tab) =
                        self.tabs.iter_mut().find(|t| t.tab_id == tab_id)
                    {
                        if !tab
                            .workspace
                            .root
                            .as_deref()
                            .is_some_and(|r| r.exists())
                        {
                            tab.workspace.root = fallback.clone();
                        }
                    }
                    // One readable line of why; the full error is in the log.
                    let notice = format!(
                        "worktree failed: {}",
                        e.lines().next().unwrap_or("unknown error")
                    );
                    self.launch_agent(
                        &tab_id,
                        fallback.as_deref(),
                        Some(&notice),
                    );
                    // Any panes split while the checkout ran sit in the
                    // dead claim dir too; walk them back as well.
                    if let Some(fallback) = fallback.as_deref() {
                        self.cd_side_panes(&tab_id, fallback);
                    }
                    // Template panes walk back too, skip setup, keep command.
                    self.launch_template_panes(
                        &tab_id,
                        fallback.as_deref(),
                        true,
                    );
                },
            }
            self.dirty = true;
        }
    }

    /// Publish every pane's repaint eagerness (egui_term P21): the active
    /// tab's panes repaint per output event while the window is focused,
    /// coalesce to ~4 Hz while it isn't, and background tabs' panes to
    /// ~2 Hz - enough for attention badges without a frame per chunk. Runs
    /// at the *end* of update() so a tab switch applied this frame (key
    /// actions apply after the CentralPanel drew) is already reflected; the
    /// repaint requested on a change is what renders the newly active tab,
    /// now that PTY events no longer flood frames to hide that gap.
    fn sync_repaint_policies(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.focused);
        self.focused_flag.store(focused, Ordering::Relaxed);
        for (idx, tab) in self.tabs.iter().enumerate() {
            let policy = if idx != self.active {
                RepaintPolicy::Background
            } else if focused {
                RepaintPolicy::Live
            } else {
                RepaintPolicy::Throttled
            };
            for pane in tab.panes.values() {
                pane.backend.set_repaint_policy(policy);
            }
        }
        if self.policy_state != Some((self.active, focused)) {
            self.policy_state = Some((self.active, focused));
            ctx.request_repaint();
        }
    }

    /// Follow the panes when a workspace's displayed folder goes stale (the
    /// user cd'd everywhere else): once *no* pane in the tab is left under
    /// the root or worktree - or anywhere in their git repo - repoint the
    /// root at the focused pane's repo (or bare cwd), so the sidebar names
    /// where work actually happens. One pane still inside pins the reference
    /// (the decision is `workspace::retarget`, pure and tested there). Rides
    /// the per-second `list-panes` snapshot the liveness check already pays
    /// for; a pane with no known cwd (mid-teardown) skips its tab for the
    /// tick rather than guess.
    fn sync_workspace_roots(&mut self) {
        // Memoize `git rev-parse --show-toplevel` per path: only consulted
        // on ticks where a tab's panes all wandered off, but then cwds
        // repeat across panes and tabs.
        let mut tops: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
        let mut toplevel = |p: &Path| {
            tops.entry(p.to_path_buf())
                .or_insert_with(|| workspace::repo_toplevel(p))
                .clone()
        };
        let snap = &self.pane_snap;
        let pending = &self.pending_worktrees;
        let mut changed = false;
        for tab in &mut self.tabs {
            // A checkout still populating its claimed worktree directory
            // would read as "the pane left" - the workspace doesn't
            // reference the claimed dir until drain_worktrees records it.
            if pending.contains(&tab.tab_id) {
                continue;
            }
            let ws = &tab.workspace;
            let mut homes: Vec<PathBuf> = Vec::new();
            homes.extend(ws.root.as_deref().map(canon));
            homes.extend(ws.worktree.as_ref().map(|w| canon(&w.path)));
            if homes.is_empty() {
                continue; // nothing displayed, nothing to go stale
            }
            // Focused pane first: it picks the new root if all have left.
            let mut panes: Vec<&Pane> = tab.panes.values().collect();
            panes.sort_by_key(|p| p.id != tab.focused);
            let cwds: Vec<PathBuf> = panes
                .iter()
                .filter_map(|p| {
                    snap.get(&p.session)
                        .and_then(|s| s.cwd.as_deref())
                        .map(canon)
                })
                .collect();
            if cwds.len() != panes.len() {
                continue; // an unknown cwd makes "none left" unprovable
            }
            let homes: Vec<&Path> =
                homes.iter().map(PathBuf::as_path).collect();
            let cwds: Vec<&Path> = cwds.iter().map(PathBuf::as_path).collect();
            let Some(root) = workspace::retarget(&homes, &cwds, &mut toplevel)
            else {
                continue;
            };
            // Belt-and-braces against a same-root re-fire marking the state
            // dirty every tick.
            if ws.root.as_deref() == Some(root.as_path())
                && ws.worktree.is_none()
            {
                continue;
            }
            log::info!(
                "workspace '{}' followed its panes: {} -> {}",
                ws.title,
                homes[0].display(),
                root.display(),
            );
            tab.workspace.root = Some(root);
            // The sidebar subtitle prefers the worktree branch; keeping a
            // worktree every pane abandoned would name it forever. The
            // worktree itself stays on disk untouched.
            tab.workspace.worktree = None;
            changed = true;
        }
        if changed {
            self.dirty = true;
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
        // Pre-workspace state files (or a bare tab) have no workspace; give
        // them a bare one so the sidebar still lists them.
        let workspace = saved
            .workspace
            .map(Workspace::from_state)
            .unwrap_or_else(|| Workspace::bare(None));
        self.tabs.push(Tab {
            tab_id,
            tree,
            panes,
            focused,
            last_rects: HashMap::new(),
            workspace,
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

    /// `mux notify`: a pane raising its hand. Unlike a bell it can carry
    /// a message, and it always re-alerts - raising twice means it twice.
    fn drain_notify_requests(&mut self, ctx: &egui::Context) {
        for req in mesh::take_notify_requests() {
            let found = self.tabs.iter_mut().find_map(|tab| {
                tab.panes.values_mut().find(|p| p.session == req.from)
            });
            let Some(pane) = found else {
                log::debug!("notify from {}: not a muxterm pane", req.from);
                continue;
            };
            pane.attn.notify(req.message.clone());
            let name = self
                .agents
                .get(&req.from)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| req.from.clone());
            let body = match &req.message {
                Some(msg) => format!("{name}: {msg}"),
                None => format!("{name} raised a hand"),
            };
            self.fire_alert(ctx, &body);
        }
    }

    /// `mux rename`: a pane relabelling the workspace it lives in. We resolve
    /// the requester's session to its tab (as splits do) and update that
    /// workspace's title/description - display-only, the git branch/worktree
    /// are untouched. Drops the tab from `naming` so a still-in-flight
    /// auto-title can't clobber the deliberate name.
    fn drain_rename_requests(&mut self) {
        for req in mesh::take_rename_requests() {
            if req.v != 1 {
                log::warn!("unsupported rename request version {}", req.v);
                continue;
            }
            let found = self.tabs.iter_mut().find(|tab| {
                tab.panes.values().any(|p| p.session == req.from)
            });
            let Some(tab) = found else {
                log::debug!("rename from {}: not a muxterm pane", req.from);
                continue;
            };
            let tab_id = tab.tab_id.clone();
            if let Some(title) = req.title {
                tab.workspace.title = title;
            }
            if let Some(description) = req.description {
                tab.workspace.description = Some(description);
            }
            // A deliberate rename ends any pending auto-title for this tab, so
            // a late one can't clobber the name the agent just chose.
            self.naming.remove(&tab_id);
            self.dirty = true;
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

    fn drain_newtab_requests(&mut self, ctx: &egui::Context) {
        for req in mesh::take_newtab_requests() {
            if let Err(reason) = self.apply_newtab_request(ctx, &req) {
                log::warn!("refused new-tab from {}: {reason}", req.from);
                mesh::write_newtab_refusal(&req.session, &reason);
            }
        }
    }

    /// `mux new-tab`: open a brand-new tab (not a split) at the caller's
    /// request. Validates like `apply_split_request`, but builds a fresh
    /// single-pane `Tab` and - unlike a user's cmd+t - steals no focus, since
    /// the user may be working elsewhere when an agent opens it.
    fn apply_newtab_request(
        &mut self,
        ctx: &egui::Context,
        req: &mesh::NewTabRequest,
    ) -> Result<(), String> {
        if req.v != 1 {
            return Err(format!("unsupported new-tab request version {}", req.v));
        }
        if mesh::now().saturating_sub(req.ts) > 30 {
            return Err("request expired before muxterm saw it".to_string());
        }
        // The name must be fresh: `new-session -A` on an existing session
        // would attach it here instead of creating a new one.
        if !req.session.starts_with(mesh::SESSION_PREFIX)
            || self.tmux.list_sessions().contains(&req.session)
            || self
                .tabs
                .iter()
                .any(|t| t.panes.values().any(|p| p.session == req.session))
        {
            return Err(format!("session name {:?} is not usable", req.session));
        }
        // Only a live muxterm pane may open tabs (authorization).
        if !self
            .tabs
            .iter()
            .any(|t| t.panes.values().any(|p| p.session == req.from))
        {
            return Err(format!("session {} is not a muxterm pane", req.from));
        }
        let pane = self
            .create_pane(ctx, Some(req.session.clone()), req.cwd.clone())
            .map_err(|e| format!("failed to create pane: {e:#}"))?;
        let id = pane.id;
        let mut workspace =
            Workspace::bare(req.cwd.as_ref().map(PathBuf::from));
        if let Some(title) = req.title.clone() {
            workspace.title = title;
        }
        let mut panes = HashMap::new();
        panes.insert(id, pane);
        self.tabs.push(Tab {
            tab_id: mesh::new_tab_id(),
            tree: Node::Leaf(id),
            panes,
            focused: id,
            last_rects: HashMap::new(),
            workspace,
        });
        self.dirty = true;
        Ok(())
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
                // Drop any template panes still awaiting boot for this tab.
                if let Some(t) = self.tabs.get(tab_idx) {
                    let tab_id = t.tab_id.clone();
                    self.pending_panes.remove(&tab_id);
                }
                self.tabs.remove(tab_idx);
                self.active =
                    active_after_removal(self.active, tab_idx, self.tabs.len());
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

    /// Like `pane_mut`, but also yields the tab index - pty events need
    /// to know whether their pane is visible right now.
    fn pane_and_tab_mut(
        &mut self,
        backend_id: u64,
    ) -> Option<(usize, &mut Pane)> {
        self.tabs.iter_mut().enumerate().find_map(|(i, tab)| {
            tab.panes.get_mut(&PaneId(backend_id)).map(|p| (i, p))
        })
    }

    /// Roll a tab's pane badges up to one indicator: the strongest level,
    /// with one hover line per pane flagged at that level.
    fn tab_attention(
        &self,
        tab: &Tab,
    ) -> Option<(attention::Level, String)> {
        let mut flagged: Vec<(attention::Level, String)> = tab
            .panes
            .values()
            .filter_map(|p| {
                p.attn.indicator().map(|(level, reason)| {
                    (level, self.reason_line(&p.session, reason))
                })
            })
            .collect();
        let top = flagged.iter().map(|(level, _)| *level).max()?;
        flagged.retain(|(level, _)| *level == top);
        let detail = flagged
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n");
        Some((top, detail))
    }

    /// One hover line for a flagged pane, named like the mesh knows it.
    fn reason_line(
        &self,
        session: &str,
        reason: &attention::Reason,
    ) -> String {
        let line = match reason {
            attention::Reason::Output => "new output".to_string(),
            attention::Reason::Bell => "bell".to_string(),
            attention::Reason::Notify(Some(msg)) => format!("raised: {msg}"),
            attention::Reason::Notify(None) => "raised a hand".to_string(),
        };
        match self.agents.get(session) {
            Some(a) => format!("{}: {line}", a.name),
            None => line,
        }
    }

    /// The alert body's tab name, matching what the tab bar shows.
    fn tab_alert_label(&self, tab_idx: usize) -> String {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return "?".into();
        };
        let pane = tab.panes.get(&tab.focused);
        tab_label(
            pane.and_then(|p| self.agents.get(&p.session)),
            pane.map(|p| p.title.as_str()).unwrap_or("shell"),
        )
    }

    /// OS-level side of an attention rise: a single dock bounce (macOS
    /// ignores it while the app is active) plus an osascript banner. Only
    /// when the window is unfocused - a visible rise is the tab bar's
    /// job - and only with config `notifications` on.
    fn fire_alert(&self, ctx: &egui::Context, body: &str) {
        if !self.notifications || ctx.input(|i| i.focused) {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
            egui::UserAttentionType::Informational,
        ));
        attention::banner("muxterm", body);
    }

    fn drain_pty_events(&mut self, ctx: &egui::Context) {
        // Visibility at drain time decides what counts as "background":
        // any pane outside the active tab, or every pane while the window
        // is unfocused (a bell in the active tab still alerts from
        // another app, like iTerm).
        let focused = ctx.input(|i| i.focused);
        let active = self.active;
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
                PtyEvent::Wakeup => {
                    if let Some((tab_idx, pane)) =
                        self.pane_and_tab_mut(backend_id)
                    {
                        if tab_idx != active || !focused {
                            pane.attn.output(Instant::now());
                        }
                    }
                },
                PtyEvent::Bell => {
                    let rang = self
                        .pane_and_tab_mut(backend_id)
                        .filter(|(tab_idx, _)| *tab_idx != active || !focused)
                        .and_then(|(tab_idx, pane)| {
                            pane.attn.bell().then_some(tab_idx)
                        });
                    if let Some(tab_idx) = rang {
                        let body = format!(
                            "bell in tab {}: {}",
                            tab_idx + 1,
                            self.tab_alert_label(tab_idx),
                        );
                        self.fire_alert(ctx, &body);
                    }
                },
                _ => {},
            }
        }
    }

    fn apply_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::NewTab => self.new_tab(ctx, None),
            Action::NewWorkspace => {
                if self.new_workspace.is_none() {
                    self.reprobe_missing_agents(ctx);
                    let prefill = self
                        .last_workspace_dir
                        .clone()
                        .or_else(|| self.new_tab_cwd())
                        .unwrap_or_default();
                    // Seed with the configured agent unless its CLI is
                    // missing; then the first installed one.
                    let installed = agent::installed(&self.agent_ok);
                    let seed = installed
                        .iter()
                        .find(|a| a.id == self.agent.id)
                        .copied()
                        .unwrap_or(installed[0]);
                    let mut form = NewWorkspaceForm::new(
                        prefill,
                        seed.id,
                        workspace_popup::default_model(seed.id),
                    );
                    form.templates = self.templates.clone();
                    self.new_workspace = Some(form);
                }
            },
            Action::NewProjectWorkspace => {
                // Nothing to pick from yet: open Settings on the Projects
                // tab so the chord teaches the feature.
                if self.projects.is_empty() {
                    self.settings_tab = settings::Tab::Projects;
                    self.settings_open = true;
                } else if self.new_workspace.is_none() {
                    self.reprobe_missing_agents(ctx);
                    let installed = agent::installed(&self.agent_ok);
                    let seed = installed
                        .iter()
                        .find(|a| a.id == self.agent.id)
                        .copied()
                        .unwrap_or(installed[0]);
                    let mut form = NewWorkspaceForm::for_project(
                        self.projects.clone(),
                        seed.id,
                        workspace_popup::default_model(seed.id),
                    );
                    form.templates = self.templates.clone();
                    self.new_workspace = Some(form);
                }
            },
            Action::ToggleSidebar => {
                self.sidebar_open = !self.sidebar_open;
                self.dirty = true;
            },
            Action::ClearScreen => {
                let session = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.panes.get(&t.focused))
                    .map(|p| p.session.clone());
                if let Some(session) = session {
                    self.tmux.clear(&session);
                }
            },
            Action::ClosePane => {
                if let Some(tab) = self.tabs.get(self.active) {
                    let focused = tab.focused;
                    self.close_pane(ctx, self.active, focused, true);
                }
            },
            Action::Split(axis) => self.split_focused(ctx, axis),
            Action::PrevTab => self.step_visible(-1),
            Action::NextTab => self.step_visible(1),
            Action::GotoTab(i) => {
                if i < self.tabs.len() && i != self.active {
                    self.active = i;
                    self.dirty = true;
                }
            },
            Action::GotoVisibleTab(n) => {
                if let Some(&i) = self.visible_tab_indices().get(n) {
                    if i != self.active {
                        self.active = i;
                        self.dirty = true;
                    }
                }
            },
            Action::Archive(i) => self.archive_tab(ctx, i),
            Action::Unarchive(i) => self.unarchive_tab(i),
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
                if self.settings_open {
                    self.reprobe_missing_agents(ctx);
                }
            },
            Action::ToggleSearch => {
                if self.search.active() {
                    self.search.close();
                } else if !self.settings_open && self.new_workspace.is_none() {
                    if let Some(tab) = self.tabs.get(self.active) {
                        if tab.panes.contains_key(&tab.focused) {
                            // The bar owns the keyboard; the "?" compose
                            // line can't coexist with it.
                            self.ai.cancel();
                            self.search.open(tab.focused);
                        }
                    }
                }
            },
            Action::SearchNext => self.search_step(SearchOp::Next),
            Action::SearchPrev => self.search_step(SearchOp::Prev),
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
            last_workspace_dir: self.last_workspace_dir.clone(),
            sidebar_open: self.sidebar_open,
            archived_collapsed: self.archived_collapsed,
            projects: self.projects.iter().map(|p| p.to_state()).collect(),
            templates: self.templates.iter().map(|t| t.to_state()).collect(),
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
                        workspace: Some(tab.workspace.to_state()),
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
        let (style, custom_font, bar_font) = config::resolve(&config::load());
        config::install_fonts(ctx, custom_font, bar_font);
        theme::apply_visuals(ctx, &style.ui);
        self.font = style.font;
        self.term_theme = style.term_theme;
        self.ui_theme = style.ui;
        self.theme_name = style.name;
        self.pane_titles = style.pane_titles;
        self.agent = style.agent;
        self.agent_context_lines = style.agent_context_lines;
        self.pr_status = style.pr_status;
        self.pr_detector = style.pr_detector;
        // One poller serves both the chips and the `#123` links; it only
        // stops when neither wants its badges.
        self.pr_enabled.store(
            style.pr_status || style.pr_detector,
            Ordering::Relaxed,
        );
        self.git_status = style.git_status;
        self.git_enabled.store(style.git_status, Ordering::Relaxed);
        self.notifications = style.notifications;
        if !style.pr_status && !style.pr_detector {
            // The poller also sends a clearing snapshot, but drop the
            // badges now so the toggle feels instant.
            self.pr.clear();
        }
        self.sync_pr_links();
        if !style.git_status {
            self.git.clear();
        }
        self.copy_on_select = style.copy_on_select;
        // The drag-end side of copy-on-select and the cmd+f search
        // highlight are tmux settings; rewrite the conf and re-source it
        // into the running server whenever its content actually changed
        // (config files only apply on server start).
        match self.tmux.write_conf(
            self.copy_on_select,
            &theme::search_highlight(&self.ui_theme),
        ) {
            Ok(true) => self.tmux.source_conf(),
            Ok(false) => {},
            Err(e) => log::error!("failed to write tmux.conf: {e:#}"),
        }
    }

    /// Push each pane's clickable PR numbers into its terminal backend
    /// (egui_term P24) and refresh the session -> repo-web-base map the
    /// link openers read (config `pr_detector`). Derived from the current
    /// badge snapshot, so a dismissed chip's number goes inert with it.
    fn sync_pr_links(&mut self) {
        let mut sets: HashMap<&str, Arc<HashSet<u64>>> = HashMap::new();
        {
            let mut bases = self.pr_link_bases.lock().unwrap();
            bases.clear();
            if self.pr_detector {
                for (session, badges) in &self.pr {
                    sets.insert(
                        session,
                        Arc::new(
                            badges.iter().map(|b| b.number).collect(),
                        ),
                    );
                    if let Some(base) = badges
                        .iter()
                        .find_map(|b| crate::links::pr_base(&b.url))
                    {
                        bases.insert(session.clone(), base.to_string());
                    }
                }
            }
        }
        log::debug!("pr_links: {sets:?}");
        for tab in &mut self.tabs {
            for pane in tab.panes.values_mut() {
                pane.backend.set_pr_links(
                    sets.get(pane.session.as_str()).cloned(),
                );
            }
        }
    }

    /// Route keyboard events through the cmd+f search bar before any
    /// TerminalView clones the frame's input. Each edit drives tmux
    /// copy-mode search on the bound pane; the resulting redraw (scroll
    /// position, match highlights) comes back through the PTY like any
    /// other tmux output.
    fn search_intercept(&mut self, ctx: &egui::Context) {
        if self.settings_open || !self.search.active() {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let focused = tab.focused;
        self.search.sync(focused, tab.panes.contains_key(&focused));
        if !self.search.active() {
            return;
        }
        let Some(pane) = tab.panes.get(&focused) else {
            return;
        };
        let session = pane.session.clone();

        let mut ops = Vec::new();
        ctx.input_mut(|i| {
            let events = std::mem::take(&mut i.events);
            let mut kept = Vec::with_capacity(events.len());
            for event in events {
                match self.search.on_event(&event) {
                    search::Verdict::Pass => kept.push(event),
                    search::Verdict::Consume => {},
                    search::Verdict::Op(op) => ops.push(op),
                }
            }
            i.events = kept;
        });
        for op in search::coalesce(ops) {
            self.run_search_op(&session, op);
        }
    }

    fn run_search_op(&mut self, session: &str, op: SearchOp) {
        let status = match op {
            SearchOp::Search(q) => self.tmux.search_text(session, &q),
            SearchOp::Next => self.tmux.search_next(session),
            SearchOp::Prev => self.tmux.search_prev(session),
            SearchOp::Clear => {
                self.tmux.search_clear(session);
                self.search.set_count(None);
                return;
            },
        };
        self.search.set_count(status.and_then(|s| {
            s.total.map(|total| search::MatchCount {
                total,
                partial: s.partial,
            })
        }));
    }

    /// cmd+g / cmd+shift+g: walk matches; a no-op unless the bar is open
    /// with a query (the chords are consumed unconditionally in keys.rs,
    /// which has no access to this state).
    fn search_step(&mut self, op: SearchOp) {
        let has_query = matches!(
            &self.search.state,
            search::State::Open { query, .. } if !query.is_empty()
        );
        if !has_query {
            return;
        }
        let session = self
            .search
            .pane
            .and_then(|p| self.tabs.get(self.active)?.panes.get(&p))
            .map(|pane| pane.session.clone());
        if let Some(session) = session {
            self.run_search_op(&session, op);
        }
    }

    /// Route keyboard events through the "?" prompt machine before any
    /// TerminalView clones the frame's input. Events it consumes never
    /// reach the PTY; a submit types the composed agent command into the
    /// focused pane, with recent scrollback redirected to its stdin.
    fn ai_intercept(&mut self, ctx: &egui::Context) {
        if self.settings_open
            || self.new_workspace.is_some()
            || self.search.active()
        {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        // A peeked archived workspace is read-only: the "?" prompt must not
        // arm or type `mux ask` into it.
        if tab.workspace.is_archived() {
            self.ai.cancel();
            return;
        }
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
        if self.settings_open || self.new_workspace.is_some() {
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

    /// Keep a mouse selection alive when the wheel scrolls it away. muxterm
    /// forwards the wheel to tmux (patch P2), which repaints the pane and
    /// wipes the local selection (the widget anchors it to on-screen rows).
    /// When the focused pane has a live selection and the wheel fires over it,
    /// recreate that selection in tmux copy-mode at the same visible
    /// coordinates and scroll there: tmux owns scrollback, so the selection
    /// persists, scrolls with the content (copy-mode style: it extends as it
    /// scrolls), and stays copyable via `copy_intercept`. One-shot -
    /// `clear_selection` drops the local copy so the next wheel forwards
    /// normally and tmux keeps its own copy-mode selection. Must run before
    /// any TerminalView clones the frame's input, like the other intercepts.
    fn scroll_intercept(&mut self, ctx: &egui::Context) {
        if self.settings_open || self.new_workspace.is_some() {
            return;
        }
        // This frame's wheel: summed vertical delta plus the unit (mice report
        // lines, trackpads points). No wheel event -> nothing to do.
        let (dy, unit) = ctx.input(|i| {
            let mut dy = 0.0;
            let mut unit = None;
            for e in &i.events {
                if let egui::Event::MouseWheel { unit: u, delta, .. } = e {
                    dy += delta.y;
                    unit = Some(*u);
                }
            }
            (dy, unit)
        });
        if unit.is_none() || dy == 0.0 {
            return;
        }
        let hover = ctx.input(|i| i.pointer.hover_pos());

        // Gather everything from the focused pane under one immutable borrow,
        // then drop it before touching tmux / the mutable re-borrow.
        let handoff = {
            let Some(tab) = self.tabs.get(self.active) else {
                return;
            };
            let Some(pane) = tab.panes.get(&tab.focused) else {
                return;
            };
            // Only hijack a scroll aimed at the focused pane itself.
            let Some(&rect) = tab.last_rects.get(&tab.focused) else {
                return;
            };
            if !hover.is_some_and(|p| rect.contains(p)) {
                return;
            }
            let content = pane.backend.last_content();
            let Some(range) = content.selectable_range else {
                return;
            };
            // Only when the wheel is being forwarded to tmux (its `mouse on`);
            // a local scroll already preserves the selection.
            if !content.terminal_mode.intersects(TerminalMode::MOUSE_MODE) {
                return;
            }
            // Selection points are absolute grid coords; under tmux
            // display_offset stays ~0, so line.0 is the visible row (the
            // selection was made on-screen, so it is non-negative here).
            let off = content.grid.display_offset() as i64;
            let row = |line: i32| (line as i64 + off).max(0) as usize;
            let sr = row(range.start.line.0);
            let sc = range.start.column.0;
            let er = row(range.end.line.0);
            let ec = range.end.column.0;
            let cell_h = content.terminal_size.cell_height.max(1.0);
            let mut lines = match unit {
                Some(egui::MouseWheelUnit::Line) => dy.round() as i32,
                Some(egui::MouseWheelUnit::Point) => (dy / cell_h).round() as i32,
                _ => 0,
            };
            if lines == 0 {
                lines = dy.signum() as i32; // at least one line in the direction
            }
            (pane.session.clone(), sr, sc, er, ec, lines)
        };

        let (session, sr, sc, er, ec, lines) = handoff;
        self.tmux.select_and_scroll(&session, sr, sc, er, ec, lines);
        if let Some(pane) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.panes.get(&t.focused))
        {
            pane.backend.clear_selection();
        }
        // Swallow the wheel so the widget doesn't also forward it (which would
        // scroll twice and, worse, wipe the selection we just handed off).
        ctx.input_mut(|i| {
            i.events
                .retain(|e| !matches!(e, egui::Event::MouseWheel { .. }))
        });
    }

    /// Re-probe agents whose CLI was last seen missing, so installing one
    /// while muxterm runs makes it appear the next time a picker opens.
    /// Known-good bins are not re-probed (each probe costs a login shell).
    fn reprobe_missing_agents(&self, ctx: &egui::Context) {
        let bins: Vec<_> = agent::AGENTS
            .iter()
            .map(|a| a.bin)
            .filter(|b| self.agent_ok.get(b) == Some(&false))
            .collect();
        spawn_agent_probe(bins, self.probe_tx.clone(), ctx.clone());
    }

    fn show_settings(&mut self, ctx: &egui::Context) {
        let out = settings::show(
            ctx,
            &self.ui_theme,
            &self.font,
            &self.theme_name,
            self.agent,
            &agent::installed(&self.agent_ok),
            self.copy_on_select,
            self.pane_titles,
            self.git_status,
            self.pr_status,
            self.pr_detector,
            self.notifications,
            &mut self.settings_tab,
            &self.projects,
            &mut self.project_draft,
            &self.templates,
            &mut self.template_draft,
        );
        if let Some(p) = out.add_project {
            // Upsert by name: `[ add ]` with a saved project's name is the
            // edit path (rows load themselves into the draft).
            match self.projects.iter_mut().find(|q| q.name == p.name) {
                Some(q) => *q = p,
                None => self.projects.push(p),
            }
            self.dirty = true;
        }
        if let Some(i) = out.remove_project {
            if i < self.projects.len() {
                self.projects.remove(i);
                self.dirty = true;
            }
        }
        if let Some(t) = out.add_template {
            // Same upsert-by-name edit path as projects.
            match self.templates.iter_mut().find(|q| q.name == t.name) {
                Some(q) => *q = t,
                None => self.templates.push(t),
            }
            self.dirty = true;
        }
        if let Some(i) = out.remove_template {
            if i < self.templates.len() {
                self.templates.remove(i);
                self.dirty = true;
            }
        }
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
        if let Some(on) = out.git_status {
            config::set_git_status(on);
            self.reload_config(ctx);
        }
        if let Some(on) = out.pr_status {
            config::set_pr_status(on);
            self.reload_config(ctx);
        }
        if let Some(on) = out.pr_detector {
            config::set_pr_detector(on);
            self.reload_config(ctx);
        }
        if let Some(on) = out.notifications {
            config::set_notifications(on);
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
            self.drain_notify_requests(ctx);
            self.drain_rename_requests();
            self.drain_newtab_requests(ctx);
            self.pane_snap = self.tmux.pane_snapshot();
            *self.pane_snap_shared.lock().unwrap() = self.pane_snap.clone();
            // Hook-reported agent states, pruned against liveness: a session
            // whose foreground returned to a shell (or vanished) has no live
            // agent - it exited or was killed without firing its end hook.
            // The ts grace covers the hook-fired-before-fg-poll-caught-up
            // race on a freshly launched agent.
            self.agent_states = mesh::read_agent_states();
            self.agent_states.retain(|session, state| {
                let snap = self.pane_snap.get(session);
                let live =
                    snap.is_some_and(|snap| !tmux::is_shell(&snap.cmd));
                if !live && mesh::now().saturating_sub(state.ts) > 5 {
                    mesh::remove_agent_state(session);
                    return false;
                }
                // A stuck "attention" whose pane kept producing terminal
                // output after the permission fired: the agent moved on, but
                // its clearing hook (Stop/PreToolUse -> idle/working) never
                // ran - a session that predates the hooks, or had them edited
                // out. tmux's per-pane window_activity postdating the ts
                // proves activity since; a genuinely blocked prompt stays
                // silent, so its activity never advances and the "!" persists.
                if state.state == "attention" {
                    let moved_on = snap
                        .and_then(|snap| snap.activity)
                        .is_some_and(|act| {
                            act > state.ts.saturating_add(STALE_ATTENTION_GRACE)
                        });
                    if moved_on {
                        mesh::remove_agent_state(session);
                        return false;
                    }
                }
                true
            });
            // Background-job scan roots: idle hook state AND the agent CLI
            // still foreground. A shell foreground means the agent exited
            // (the prune's territory), and a marker match under a *working*
            // agent is just a foreground Bash tool (bg_jobs.rs).
            let roots: Vec<(String, u32)> = self
                .agent_states
                .iter()
                .filter(|(_, s)| s.state == "idle")
                .filter_map(|(session, _)| {
                    let snap = self.pane_snap.get(session)?;
                    (!tmux::is_shell(&snap.cmd))
                        .then_some((session.clone(), snap.pid?))
                })
                .collect();
            // Gate closed (agent resumed or exited) -> lights out on this
            // tick, not when the next scan lands.
            self.bg_jobs
                .retain(|s| roots.iter().any(|(sess, _)| sess == s));
            if !roots.is_empty() && !self.bg_scan_inflight {
                self.bg_scan_inflight = true;
                bg_jobs::spawn_scan(roots, self.bg_tx.clone(), ctx.clone());
            }
            self.sync_workspace_roots();
        }

        if log::log_enabled!(log::Level::Debug) {
            ctx.input(|i| {
                for event in &i.events {
                    log::debug!("event: {event:?} (mods now: {:?})", i.modifiers);
                }
            });
        }

        // Order is load-bearing: shortcuts, the search bar, and the "?"
        // prompt machine must all run before any TerminalView clones the
        // frame's input events; shortcuts first so chords never reach the
        // machines, and the search bar before "?" so an open bar owns the
        // keyboard (ai_intercept bows out while it is active).
        let mut actions = keys::drain_shortcuts(ctx);
        if self.settings_open
            && ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
            })
        {
            self.settings_open = false;
        }
        // Esc closes the workspace popup, like the settings window.
        if self.new_workspace.is_some()
            && ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
            })
        {
            self.new_workspace = None;
        }
        // Open/close the bar *before* search_intercept: cmd+f is already
        // consumed above, but the bar only starts intercepting once it is
        // active. If a first query character is batched into the same egui
        // frame as the cmd+f chord (fast typing), applying the toggle here
        // means the bar owns that character instead of leaking it to the
        // still-focused TerminalView. The rest of the batch stays deferred.
        if actions.iter().any(|a| matches!(a, Action::ToggleSearch)) {
            actions.retain(|a| !matches!(a, Action::ToggleSearch));
            self.apply_action(ctx, Action::ToggleSearch);
        }
        self.search_intercept(ctx);
        self.ai_intercept(ctx);
        self.copy_intercept(ctx);
        self.scroll_intercept(ctx);

        self.drain_pty_events(ctx);
        // Looking at a tab acknowledges its badges: seeing the active tab
        // with the window focused clears them, which covers every
        // tab-switch path and window refocus in one sweep.
        if ctx.input(|i| i.focused) {
            if let Some(tab) = self.tabs.get_mut(self.active) {
                for pane in tab.panes.values_mut() {
                    pane.attn.viewed();
                }
            }
        }
        let mut pr_changed = false;
        while let Ok(snapshot) = self.pr_rx.try_recv() {
            self.pr = snapshot;
            pr_changed = true;
            // A chip dismissed after this snapshot was composed must not
            // flicker back; the poller empties the set once its memory
            // has forgotten the key (within a tick).
            let dismissed = self.pr_dismissed.lock().unwrap();
            if !dismissed.is_empty() {
                for badges in self.pr.values_mut() {
                    badges.retain(|b| {
                        !dismissed
                            .iter()
                            .any(|(r, br)| *r == b.root && *br == b.branch)
                    });
                }
                self.pr.retain(|_, badges| !badges.is_empty());
            }
        }
        if pr_changed {
            self.sync_pr_links();
        }
        while let Ok(snapshot) = self.git_rx.try_recv() {
            self.git = snapshot;
        }
        // A scan result landing just after its gate closed can be stale for
        // at most one tick; the tick's retain corrects it.
        while let Ok(found) = self.bg_rx.try_recv() {
            self.bg_scan_inflight = false;
            self.bg_jobs = found;
        }
        while let Ok((bin, ok)) = self.probe_rx.try_recv() {
            self.agent_ok.insert(bin, ok);
        }
        // AI titles arrive out of band; upgrade the workspace's label. A tab
        // dropped from `naming` (killed, or renamed via `mux rename`) means the
        // title is no longer up for grabs, so a late arrival is ignored.
        while let Ok((tab_id, title)) = self.title_rx.try_recv() {
            if !self.naming.remove(&tab_id) {
                continue;
            }
            if let Some(tab) = self.tabs.iter_mut().find(|t| t.tab_id == tab_id)
            {
                tab.workspace.title = title;
                self.dirty = true;
            }
        }
        // Finished worktree checkouts: launch the agent in the waiting pane.
        self.drain_worktrees();

        if ctx.input(|i| i.viewport().close_requested()) {
            // Sessions deliberately survive: dropping the app only detaches
            // the tmux clients.
            self.save_state();
            return;
        }

        // The tab bar shows only visible (non-archived) tabs; its indices are
        // positions in that filtered list, remapped through `visible` on the
        // way back. `active` is passed as the active tab's visible position, or
        // an out-of-range sentinel while peeking an archived tab (no highlight).
        let visible = self.visible_tab_indices();
        let labels: Vec<tabbar::TabInfo> = visible
            .iter()
            .map(|&i| {
                let tab = &self.tabs[i];
                let pane = tab.panes.get(&tab.focused);
                let label = tab_label(
                    pane.and_then(|p| self.agents.get(&p.session)),
                    pane.map(|p| p.title.as_str()).unwrap_or("shell"),
                );
                let attn = self.tab_attention(tab);
                (label, attn)
            })
            .collect();
        let active_pos = visible
            .iter()
            .position(|&i| i == self.active)
            .unwrap_or(usize::MAX);
        for action in
            tabbar::show(ctx, &labels, active_pos, self.sidebar_open, &self.ui_theme)
        {
            match action {
                TabBarAction::Select(pos) => {
                    actions.push(Action::GotoVisibleTab(pos))
                },
                TabBarAction::NewTab => actions.push(Action::NewTab),
                TabBarAction::OpenSettings => {
                    actions.push(Action::ToggleSettings)
                },
                TabBarAction::ToggleSidebar => {
                    actions.push(Action::ToggleSidebar)
                },
            }
        }

        // The workspace sidebar (a left panel) must be added before the
        // CentralPanel. Rows are in tab order; `tab_index` maps a click back
        // to the real tab regardless of display order.
        if self.sidebar_open {
            let mut rows: Vec<sidebar::Row> = self
                .tabs
                .iter()
                .enumerate()
                .map(|(i, tab)| {
                    let ws = &tab.workspace;
                    let subtitle = ws
                        .worktree
                        .as_ref()
                        .map(|w| w.branch.clone())
                        .or_else(|| {
                            ws.root.as_ref().and_then(|r| {
                                r.file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                            })
                        })
                        // Don't echo a title that already is the folder name
                        // (a rename can make them coincide).
                        .filter(|s| *s != ws.title);
                    // The dot reflects *AI agents only*, self-reported
                    // through their lifecycle hooks (`mux agent-event`, see
                    // agent_hooks.rs): "working" while a turn runs (even
                    // silent thinking), "attention" when the agent stopped
                    // for a permission/notification. Non-agent programs never
                    // light it. "Blocked" also covers a pane raising its hand
                    // (bell / `mux notify`), and outranks working. "Background"
                    // is the one derived state: an idle agent whose
                    // run_in_background shell still lives under the pane's
                    // process tree (bg_jobs.rs) - no hook exists for those.
                    let hook_state = |wanted: &str| {
                        tab.panes.values().any(|p| {
                            self.agent_states
                                .get(&p.session)
                                .is_some_and(|s| s.state == wanted)
                        })
                    };
                    let blocked = matches!(
                        self.tab_attention(tab),
                        Some((attention::Level::Attention, _))
                    ) || hook_state("attention");
                    let status = if blocked {
                        sidebar::Status::Blocked
                    } else if hook_state("working") {
                        sidebar::Status::Working
                    } else if tab
                        .panes
                        .values()
                        .any(|p| self.bg_jobs.contains(&p.session))
                    {
                        sidebar::Status::Background
                    } else {
                        sidebar::Status::Idle
                    };
                    // A trailing ellipsis marks the AI title still in flight
                    // (`naming`), so the placeholder doesn't read as final.
                    let mut title = ws.title.clone();
                    if self.naming.contains(&tab.tab_id) {
                        title.push('…');
                    }
                    sidebar::Row {
                        tab_index: i,
                        title,
                        subtitle,
                        active: i == self.active,
                        status,
                        archived: ws.is_archived(),
                    }
                })
                .collect();
            // Visible rows stay in tab order (top = cmd+1); archived rows sink
            // to the bottom pile newest-archived-first. `sort_by` is stable, so
            // two visible rows keep their tab order. `sidebar::show` splits the
            // two piles by the `archived` flag, preserving this order in each.
            rows.sort_by(|a, b| match (a.archived, b.archived) {
                (false, false) => std::cmp::Ordering::Equal,
                (false, true) => std::cmp::Ordering::Less,
                (true, false) => std::cmp::Ordering::Greater,
                (true, true) => {
                    let at =
                        self.tabs[a.tab_index].workspace.archived_at.unwrap_or(0);
                    let bt =
                        self.tabs[b.tab_index].workspace.archived_at.unwrap_or(0);
                    bt.cmp(&at)
                },
            });
            for action in sidebar::show(
                ctx,
                &rows,
                self.archived_collapsed,
                &self.font,
                &self.ui_theme,
            ) {
                match action {
                    // A row body-click selects; for an archived row that is the
                    // peek (GotoTab just sets `active`, archived or not).
                    SidebarAction::Select(i) => {
                        actions.push(Action::GotoTab(i))
                    },
                    SidebarAction::Archive(i) => {
                        actions.push(Action::Archive(i))
                    },
                    SidebarAction::Unarchive(i) => {
                        actions.push(Action::Unarchive(i))
                    },
                    // Pure sidebar state, no keybinding: applied here rather
                    // than through an Action.
                    SidebarAction::ToggleArchived => {
                        self.archived_collapsed = !self.archived_collapsed;
                        self.dirty = true;
                    },
                    SidebarAction::NewWorkspace => {
                        actions.push(Action::NewWorkspace)
                    },
                    SidebarAction::ToggleSidebar => {
                        actions.push(Action::ToggleSidebar)
                    },
                }
            }
        }

        let mut ui_actions = Vec::new();
        // PR chips right-clicked away this frame; applied after the panel
        // closure (it holds the tab borrow).
        let mut pr_dismiss: Vec<(String, String)> = Vec::new();
        // Panes sit flush against the sidebar, exactly like every other
        // edge - the focus ring insets itself where it lacks outside room,
        // the terminal grid carries its own left inset (egui_term), and the
        // sidebar's resize separator draws its own line, so no gutter is
        // needed to keep them apart.
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(self.ui_theme.bg))
            .show(ctx, |ui| {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    let rect = ui.max_rect();
                    let mut rects = HashMap::new();
                    // A solid HUD strip reserves layout space (floating
                    // chips reserve none), gated on pane_titles - the HUD
                    // loop below is the only thing that paints the strip,
                    // so never reserve a row nothing will fill.
                    let bar_h = (self.pane_titles
                        && self.ui_theme.bar_style == theme::BarStyle::Solid)
                        .then(|| {
                            bar_height(ui.fonts(|f| {
                                f.row_height(&self.ui_theme.bar_font)
                            }))
                        });
                    // A peeked archived workspace is a read-only preview: no
                    // pane is interactive and none holds keyboard focus.
                    let archived = tab.workspace.is_archived();
                    // While settings, the "?" compose line, or the search
                    // bar own the keyboard, the sentinel matches no pane,
                    // so the terminal stops re-grabbing focus every frame.
                    let focused = if archived
                        || self.settings_open
                        || self.new_workspace.is_some()
                        || self.ai.composing()
                        || self.search.active()
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
                        !archived,
                        bar_h,
                        &mut rects,
                        &mut ui_actions,
                    );
                    if tab.panes.len() > 1 {
                        // No focus ring on a read-only preview - nothing there
                        // is focused.
                        if let Some(focused_rect) =
                            (!archived).then(|| rects.get(&tab.focused)).flatten()
                        {
                            // The ring hugs the pane from just outside - the
                            // inter-pane gap and the sidebar gutter have room -
                            // but clamped into the clip region and stroked
                            // inward: an edge-flush pane would otherwise get
                            // the stroke sliced away wherever it touches the
                            // panel border (invisible top/bottom, a half-drawn
                            // seam against the sidebar).
                            let clip = ui.clip_rect();
                            let ring = focused_rect.expand(1.0);
                            let ring = Rect::from_min_max(
                                ring.min.max(clip.min),
                                ring.max.min(clip.max),
                            );
                            ui.painter().rect_stroke(
                                ring,
                                CornerRadius::same(2),
                                Stroke::new(
                                    self.ui_theme.border_width,
                                    self.ui_theme.accent,
                                ),
                                StrokeKind::Inside,
                            );
                        }
                    }
                    if self.pane_titles {
                        for (id, rect) in &rects {
                            let Some(pane) = tab.panes.get(id) else {
                                continue;
                            };
                            // A lone pane is the whole tab - no title
                            // badge - but its PR/git chips still show.
                            let label = if tab.panes.len() > 1 {
                                tab_label(
                                    self.agents.get(&pane.session),
                                    &pane.title,
                                )
                            } else {
                                String::new()
                            };
                            if let Some(key) = draw_pane_title(
                                ui,
                                *rect,
                                &label,
                                self.git.get(&pane.session),
                                // The badge map also feeds pr_detector, so
                                // chips gate on their own config here.
                                if self.pr_status {
                                    self.pr
                                        .get(&pane.session)
                                        .map_or(&[][..], Vec::as_slice)
                                } else {
                                    &[]
                                },
                                *id,
                                *id == tab.focused,
                                &self.ui_theme,
                                bar_h,
                            ) {
                                pr_dismiss.push(key);
                            }
                        }
                    }
                    // The checkout thread's current step, floated over the
                    // agent pane until Done lands (drain_worktrees clears it).
                    if let Some(line) =
                        self.worktree_progress.get(&tab.tab_id)
                    {
                        if let Some(rect) =
                            rects.get(&tab.tree.first_leaf())
                        {
                            draw_worktree_progress(
                                ui,
                                *rect,
                                line,
                                &self.ui_theme,
                            );
                        }
                    }
                    if let ai_prompt::State::Compose { buffer, error } =
                        &self.ai.state
                    {
                        if let Some((rect, pane)) = self.ai.pane.and_then(|p| {
                            Some((*rects.get(&p)?, tab.panes.get(&p)?))
                        }) {
                            // The grid sits inside the solid strip's inset;
                            // caret math against the full pane claim would
                            // land one strip-height off.
                            let rect = bar_h
                                .and_then(|h| {
                                    split_bar(
                                        rect,
                                        self.ui_theme.bar_edge,
                                        h,
                                    )
                                })
                                .map_or(rect, |(_, term)| term);
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
                    if let search::State::Open { query, count } =
                        &self.search.state
                    {
                        if let Some(rect) =
                            self.search.pane.and_then(|p| rects.get(&p))
                        {
                            draw_search_bar(
                                ui,
                                *rect,
                                query,
                                *count,
                                &self.font,
                                &self.ui_theme,
                                bar_h,
                            );
                        }
                    }
                    tab.last_rects = rects;
                }
            });

        if self.settings_open {
            self.show_settings(ctx);
        }

        // The workspace-creation popup. Taken out so `create_workspace` can
        // borrow self mutably; put back unless the user finished with it.
        if let Some(mut form) = self.new_workspace.take() {
            match workspace_popup::show(
                ctx,
                &mut form,
                &agent::installed(&self.agent_ok),
                &self.ui_theme,
                &self.font,
            ) {
                workspace_popup::Outcome::Create => {
                    self.create_workspace(ctx, form);
                },
                workspace_popup::Outcome::Cancel => {},
                workspace_popup::Outcome::None => {
                    self.new_workspace = Some(form);
                },
            }
        }

        // Dismissed chips vanish now (the poller's own snapshot lags a
        // tick) and go to the poller, which forgets them durably.
        if !pr_dismiss.is_empty() {
            let mut dismissed = self.pr_dismissed.lock().unwrap();
            for key in pr_dismiss {
                for badges in self.pr.values_mut() {
                    badges.retain(|b| b.root != key.0 || b.branch != key.1);
                }
                dismissed.insert(key);
            }
            self.pr.retain(|_, badges| !badges.is_empty());
            drop(dismissed);
            self.sync_pr_links();
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

        self.sync_repaint_policies(ctx);

        if self.dirty {
            self.save_state();
            self.dirty = false;
        }
    }
}

/// Probe agent binaries on a background thread (each probe spawns an
/// interactive login shell - see agent::binary_available); results land in
/// `agent_ok` via `probe_rx` on a later frame.
fn spawn_agent_probe(
    bins: Vec<&'static str>,
    tx: Sender<(&'static str, bool)>,
    ctx: egui::Context,
) {
    if bins.is_empty() {
        return;
    }
    thread::spawn(move || {
        for bin in bins {
            let ok = agent::binary_available(bin);
            if tx.send((bin, ok)).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    });
}

/// Resolve symlinks so tmux's kernel-real cwds (`/private/tmp`) compare
/// equal to user-typed workspace roots (`/tmp`); a vanished path keeps its
/// spelling.
fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
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
        theme.bar_font.clone(),
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

/// The worktree-progress line, floated over the agent pane's bottom-left
/// while its clone/fetch/checkout runs off-thread - the only feedback
/// between Create and the agent command appearing. Painter-drawn like
/// draw_ai_overlay: reserves no layout space.
fn draw_worktree_progress(
    ui: &egui::Ui,
    pane_rect: Rect,
    text: &str,
    theme: &UiTheme,
) {
    // A char budget from the pane width, scaled by the bar font's size
    // (~0.55px per pt for a typical face), so a chunky bar font truncates
    // instead of overflowing.
    let px_per_char = (theme.bar_font.size * 0.55).max(1.0);
    let budget = ((pane_rect.width() - 24.0).max(0.0) / px_per_char) as usize;
    let painter = ui.painter();
    let galley = painter.layout_no_wrap(
        elide(text, budget),
        theme.bar_font.clone(),
        theme.text_dim,
    );
    let pad = Vec2::new(6.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let rect = Rect::from_min_size(
        egui::pos2(
            pane_rect.min.x + 8.0,
            pane_rect.max.y - size.y - 8.0,
        ),
        size,
    );
    painter.rect_filled(
        rect,
        CornerRadius::same(3),
        theme::blend(theme.bg, theme.accent, 0.18),
    );
    painter.galley(rect.min + pad, galley, theme.text_dim);
}

/// Height of the solid HUD strip: one HUD text row plus breathing room,
/// ceil'd so the terminal inset lands on a whole pixel.
fn bar_height(hud_row_h: f32) -> f32 {
    (hud_row_h + 8.0).ceil()
}

/// Split a pane into (strip, terminal) at the theme's bar edge. None when
/// the pane is too short to give up a strip - the terminal keeps the
/// whole rect and the HUD is skipped entirely. Every consumer of the
/// solid bar's geometry must come through here: grid math against a
/// diverging rect paints one strip-height off.
fn split_bar(
    pane: Rect,
    edge: theme::BarEdge,
    bar_h: f32,
) -> Option<(Rect, Rect)> {
    if pane.height() < bar_h * 5.0 {
        return None;
    }
    Some(match edge {
        theme::BarEdge::Top => (
            Rect::from_min_max(
                pane.min,
                egui::pos2(pane.max.x, pane.min.y + bar_h),
            ),
            Rect::from_min_max(
                egui::pos2(pane.min.x, pane.min.y + bar_h),
                pane.max,
            ),
        ),
        theme::BarEdge::Bottom => (
            Rect::from_min_max(
                egui::pos2(pane.min.x, pane.max.y - bar_h),
                pane.max,
            ),
            Rect::from_min_max(
                pane.min,
                egui::pos2(pane.max.x, pane.max.y - bar_h),
            ),
        ),
    })
}

/// The y where the HUD line (title box, chips, search bar) sits: 4pt
/// inside the pane's bar edge when the chips float, or vertically
/// centered when a solid strip is the `area`.
fn hud_line_y(
    area: Rect,
    line_h: f32,
    edge: theme::BarEdge,
    solid: bool,
) -> f32 {
    if solid {
        area.center().y - line_h / 2.0
    } else {
        match edge {
            theme::BarEdge::Top => area.min.y + 4.0,
            theme::BarEdge::Bottom => area.max.y - line_h - 4.0,
        }
    }
}

/// The cmd+f search bar, a strip over the theme's HUD line (the
/// pane-title spot - it covers the badge and, up top, tmux's own
/// copy-mode indicator, both redundant while searching). tmux draws the
/// matches and moves the viewport; this is only the query line and
/// counter.
#[allow(clippy::too_many_arguments)]
fn draw_search_bar(
    ui: &egui::Ui,
    pane_rect: Rect,
    query: &str,
    count: Option<search::MatchCount>,
    font: &FontId,
    theme: &UiTheme,
    bar_h: Option<f32>,
) {
    let hud = theme::hud_colors(theme);
    let painter = ui.painter();
    let probe =
        painter.layout_no_wrap("M".into(), font.clone(), hud.fg);
    let char_w = probe.size().x.max(1.0);
    let row_h = probe.size().y.max(1.0);

    // Compact fixed-width strip; panes too narrow for a usable query
    // field get no bar (the search itself still runs in tmux).
    let width = (36.0 * char_w + 20.0).min(pane_rect.width() - 8.0);
    if width < 14.0 * char_w {
        return;
    }
    let pad = Vec2::new(8.0, 3.0);
    let strip = bar_h.and_then(|h| split_bar(pane_rect, theme.bar_edge, h));
    let area = strip.map_or(pane_rect, |(bar, _)| bar);
    let line_h = row_h + pad.y * 2.0;
    let y = hud_line_y(area, line_h, theme.bar_edge, strip.is_some());
    let rect = Rect::from_min_size(
        egui::pos2(area.max.x - width - 4.0, y),
        Vec2::new(width, line_h),
    );
    painter.rect_filled(
        rect,
        CornerRadius::same(3),
        theme::blend(theme.bar_bg, theme.bar_accent, 0.18),
    );

    let mid = rect.center().y;
    let prefix =
        painter.layout_no_wrap("/ ".into(), font.clone(), theme.bar_accent);
    let text_left = rect.min.x + pad.x + prefix.size().x;
    painter.galley(
        egui::pos2(rect.min.x + pad.x, mid - prefix.size().y / 2.0),
        prefix,
        theme.bar_accent,
    );

    // Right-aligned: the match counter, then the key hint while the bar
    // still leaves the query a dozen cells of room.
    let count_text = match count {
        Some(c) if c.partial => format!("{}+", c.total),
        Some(c) => c.total.to_string(),
        None => String::new(),
    };
    let mut right_limit = rect.max.x - pad.x;
    if !count_text.is_empty() {
        let counter = painter.layout_no_wrap(
            count_text,
            theme.bar_font.clone(),
            hud.fg_dim,
        );
        right_limit -= counter.size().x;
        painter.galley(
            egui::pos2(right_limit, mid - counter.size().y / 2.0),
            counter,
            hud.fg_dim,
        );
        right_limit -= char_w;
    }
    let hint = painter.layout_no_wrap(
        "esc close · ⏎ next · ⇧⏎ prev".into(),
        theme.bar_font.clone(),
        hud.fg_dim,
    );
    if right_limit - hint.size().x - text_left >= 12.0 * char_w {
        right_limit -= hint.size().x;
        painter.galley(
            egui::pos2(right_limit, mid - hint.size().y / 2.0),
            hint,
            hud.fg_dim,
        );
        right_limit -= char_w;
    }

    // Tail-truncate the query (its cursor sits at the end, so the newest
    // text must stay visible); one cell is reserved for the block cursor.
    // No matches tints the query, like iTerm's not-found field.
    let avail = (right_limit - text_left).max(0.0);
    let budget = ((avail / char_w) as usize).saturating_sub(1);
    let chars = query.chars().count();
    let visible: String = if chars > budget {
        query.chars().skip(chars - budget).collect()
    } else {
        query.to_string()
    };
    let query_color = match count {
        Some(c) if c.total == 0 => hud.err,
        _ => hud.fg,
    };
    let text =
        painter.layout_no_wrap(visible, font.clone(), query_color);
    let cursor_x = text_left + text.size().x;
    painter.galley(
        egui::pos2(text_left, mid - text.size().y / 2.0),
        text,
        query_color,
    );
    painter.rect_filled(
        Rect::from_min_size(
            egui::pos2(cursor_x + 1.0, rect.min.y + pad.y),
            Vec2::new(char_w, row_h),
        ),
        CornerRadius::ZERO,
        theme.bar_accent,
    );
}

/// One pane's HUD line: the title badge (split tabs only - `label`
/// arrives empty on a lone pane, which gets chips without a title) and
/// the PR/git chips beside it, floating in a corner or laid on the solid
/// strip (`bar_h` set when the strip reserved layout space this frame).
/// Returns the (root, branch) of a PR chip right-clicked away, for the
/// App to dismiss (a free function cannot reach the poller's shared set).
#[allow(clippy::too_many_arguments)]
fn draw_pane_title(
    ui: &egui::Ui,
    pane_rect: Rect,
    label: &str,
    git: Option<&git_status::Git>,
    pr: &[pr_status::Badge],
    pane: PaneId,
    focused: bool,
    theme: &UiTheme,
    bar_h: Option<f32>,
) -> Option<(String, String)> {
    let font = theme.bar_font.clone();
    let hud = theme::hud_colors(theme);
    let color = if focused { hud.fg } else { hud.fg_dim };
    let painter = ui.painter();
    let (area, solid) = match bar_h {
        Some(h) => match split_bar(pane_rect, theme.bar_edge, h) {
            Some((bar, _)) => (bar, true),
            // Too short for a strip: the terminal kept the whole rect
            // (same split_bar verdict), so no HUD at all.
            None => return None,
        },
        None => (pane_rect, false),
    };
    if solid {
        painter.rect_filled(area, CornerRadius::ZERO, theme.bar_bg);
    }
    // Cap the badge at roughly half the pane so it never reads as
    // terminal content; a pane too narrow for even a few characters gets
    // no title, only chips.
    let max_w = pane_rect.width() * 0.5 - 12.0;
    let mut galley = (!label.is_empty()).then(|| {
        painter.layout_no_wrap(label.to_string(), font.clone(), color)
    });
    if let Some(g) = &galley {
        if g.size().x > max_w {
            let char_w = g.size().x / label.chars().count().max(1) as f32;
            let budget = (max_w / char_w) as usize;
            galley = (budget >= 3).then(|| {
                painter.layout_no_wrap(
                    elide(label, budget),
                    font.clone(),
                    color,
                )
            });
        }
    }
    let pad = Vec2::new(6.0, 2.0);
    // The line's y comes from the font's row height, not the title galley
    // (a lone pane has no title but its chips still sit on the line).
    let line_h = ui.fonts(|f| f.row_height(&font)) + pad.y * 2.0;
    let y = hud_line_y(area, line_h, theme.bar_edge, solid);
    let size = galley.as_ref().map_or(Vec2::ZERO, |g| g.size() + pad * 2.0);
    let rect = Rect::from_min_size(
        egui::pos2(area.max.x - size.x - 4.0, y),
        size,
    );
    if let Some(galley) = galley {
        // Translucent theme background: readable over any terminal content
        // without fully hiding what's underneath. The solid strip already
        // supplied one.
        if let Some(fill) = hud.chip_fill {
            painter.rect_filled(rect, CornerRadius::same(3), fill);
        }
        painter.galley(rect.min + pad, galley, color);
    }

    // Chips stack from the title box toward the pane's center; `edge`
    // tracks the last drawn edge, and a chip that would spill past the
    // pane's far side is dropped rather than clipped. PR chips sit
    // nearest the title, the git chip beyond them.
    let mut edge = rect.min.x;
    let chip_rect = |content: Vec2, edge: f32| {
        let size = content + pad * 2.0;
        Rect::from_min_size(
            egui::pos2(edge - size.x - 4.0, rect.min.y),
            size,
        )
    };
    let fits = |chip: &Rect| {
        chip.min.x >= pane_rect.min.x + 4.0
            && chip.max.x <= pane_rect.max.x - 4.0
    };

    let mut dismissed = None;
    // Newest PR nearest the title, so on overflow the oldest drop first
    // (`break`, not per-chip skipping - a gap would misread as order).
    for b in pr.iter().rev() {
        let galley = painter.layout_no_wrap(
            format!("#{}", b.number),
            font.clone(),
            color,
        );
        let icon_w = font.size;
        let content = Vec2::new(icon_w + galley.size().x, galley.size().y);
        let chip = chip_rect(content, edge);
        if !fits(&chip) {
            break;
        }
        if let Some(fill) = hud.chip_fill {
            painter.rect_filled(chip, CornerRadius::same(3), fill);
        }
        b.kind.draw_icon(
            painter,
            egui::pos2(chip.min.x + pad.x + icon_w * 0.38, chip.center().y),
            font.size,
            &hud,
        );
        painter.galley(chip.min + pad + Vec2::new(icon_w, 0.0), galley, color);
        // A merged/closed chip can be right-clicked away - unless the
        // pane still sits on its branch (the scan would just re-learn
        // it next tick, so don't offer).
        let done = matches!(
            b.kind,
            pr_status::Kind::Merged | pr_status::Kind::Neutral
        );
        let dismissable =
            done && !git.is_some_and(|g| g.branch == b.branch);
        let hover = if dismissable {
            format!("{}\nright-click to dismiss", b.detail)
        } else {
            b.detail.clone()
        };
        let resp = ui
            .interact(
                chip,
                ui.id().with(("pr-chip", pane, b.number)),
                Sense::click(),
            )
            .on_hover_text(hover)
            .on_hover_cursor(CursorIcon::PointingHand);
        if resp.clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(&b.url));
        }
        if dismissable && resp.secondary_clicked() {
            dismissed = Some((b.root.clone(), b.branch.clone()));
        }
        edge = chip.min.x;
    }

    if let Some(g) = git {
        let galley = ui.fonts(|f| f.layout_job(g.chip_job(font, color, &hud)));
        let chip = chip_rect(galley.size(), edge);
        if fits(&chip) {
            if let Some(fill) = hud.chip_fill {
                painter.rect_filled(chip, CornerRadius::same(3), fill);
            }
            painter.galley(chip.min + pad, galley, color);
            ui.interact(chip, ui.id().with(("git-chip", pane)), Sense::hover())
                .on_hover_text(&g.detail);
        }
    }
    dismissed
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
    // False for a peeked archived workspace: panes render as a dimmed,
    // read-only preview - no input, no divider drag, no focus ring.
    interactive: bool,
    // Some = the theme's solid HUD strip reserves this much of each pane.
    bar_h: Option<f32>,
    rects: &mut HashMap<PaneId, Rect>,
    ui_actions: &mut Vec<UiAction>,
) {
    match node {
        Node::Leaf(id) => {
            // The pane's full screen claim (focus ring, neighbor math,
            // washes); grid coordinates must go through split_bar.
            rects.insert(*id, rect);
            let Some(pane) = panes.get_mut(id) else {
                return;
            };
            let split =
                bar_h.and_then(|h| split_bar(rect, ui_theme.bar_edge, h));
            let term_rect = split.map_or(rect, |(_, term)| term);
            let mut child =
                ui.new_child(egui::UiBuilder::new().max_rect(term_rect));
            let view = TerminalView::new(&mut child, &mut pane.backend)
                .set_size(term_rect.size())
                .set_focus(*id == focused)
                .set_font(TerminalFont::new(FontSettings {
                    font_type: font.clone(),
                }))
                .set_theme(term_theme.clone())
                .set_copy_on_select(copy_on_select)
                .set_interactive(interactive);
            let response = child.add(view);
            if !interactive {
                // A peeked archived workspace: wash every pane heavily so the
                // whole thing reads as a parked, read-only preview.
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::ZERO,
                    ui_theme.archived_overlay,
                );
            } else if *id != focused
                && panes.len() > 1
                && ui_theme.dim_overlay.a() > 0
            {
                // Wash unfocused panes toward the background, like iTerm's
                // "dim inactive split panes".
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::ZERO,
                    ui_theme.dim_overlay,
                );
            }
            if interactive && response.clicked() {
                ui_actions.push(UiAction::FocusPane(*id));
            }
            // The strip sits outside the TerminalView's response; a click
            // there must still focus the pane. (The chips' interacts
            // register later in the frame, so they win hits over this.)
            if let Some((bar, _)) = split {
                if interactive
                    && ui
                        .interact(
                            bar,
                            ui.id().with(("pane-bar", *id)),
                            Sense::click(),
                        )
                        .clicked()
                {
                    ui_actions.push(UiAction::FocusPane(*id));
                }
            }
        },
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            // A read-only preview freezes its layout too - no draggable
            // divider (its cursor hint would invite an interaction that does
            // nothing).
            if interactive {
                let (_, divider, _) =
                    layout::split_rect(rect, *axis, *ratio, PANE_GAP);
                let hit = match axis {
                    SplitAxis::SideBySide => {
                        divider.expand2(Vec2::new(2.0, 0.0))
                    },
                    SplitAxis::Stacked => divider.expand2(Vec2::new(0.0, 2.0)),
                };
                let divider_id = ui.id().with(("divider", path));
                let response = ui
                    .interact(hit, divider_id, egui::Sense::drag())
                    .on_hover_cursor(match axis {
                        SplitAxis::SideBySide => {
                            egui::CursorIcon::ResizeHorizontal
                        },
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
                term_theme, ui_theme, copy_on_select, interactive, bar_h,
                rects, ui_actions,
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
                interactive,
                bar_h,
                rects,
                ui_actions,
            );
        },
    }
}

/// Tab label: registered agents show as `● name · role`, everything else
/// falls back to the pane's OSC title.
/// The visible tab to focus after archiving the tab at `removed`: the nearest
/// visible index to its left, else the nearest to its right, else None (it was
/// the last visible tab, so the caller spawns a fresh one). `visible` is the
/// post-archive visible list — ascending and not containing `removed`.
fn nearest_visible(visible: &[usize], removed: usize) -> Option<usize> {
    visible
        .iter()
        .rev()
        .find(|&&v| v < removed)
        .or_else(|| visible.iter().find(|&&v| v > removed))
        .copied()
}

/// The active index after the tab at `removed` is deleted from `tabs` (now
/// `new_len` long). A removal below the active tab shifts it down by one;
/// removing the active tab (or the last one) can leave it past the end, so
/// clamp. Fixes a latent off-by-one that archived background tabs — whose
/// shells can exit at a lower index — would otherwise expose.
fn active_after_removal(active: usize, removed: usize, new_len: usize) -> usize {
    let shifted = if removed < active { active - 1 } else { active };
    shifted.min(new_len.saturating_sub(1))
}

/// The visible index to activate when stepping `delta` (±1) from `active` with
/// wraparound. If `active` isn't in `visible` (peeking an archived tab), a
/// forward step lands on the first entry and a backward step on the last.
/// None when nothing is visible.
fn step_visible_target(
    visible: &[usize],
    active: usize,
    delta: isize,
) -> Option<usize> {
    if visible.is_empty() {
        return None;
    }
    let n = visible.len() as isize;
    let pos = match visible.iter().position(|&i| i == active) {
        Some(p) => (p as isize + delta).rem_euclid(n),
        None if delta > 0 => 0,
        None => n - 1,
    };
    Some(visible[pos as usize])
}

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

    /// Every remembered PR paints its own chip, and a lone pane (empty
    /// label) still gets the chips - just no title box.
    #[test]
    fn pane_hud_stacks_all_pr_chips() {
        let ctx = egui::Context::default();
        // Bind the HUD font family (`th.bar_font`); painting its text panics
        // otherwise. None bar bytes seeds it with the system fallbacks.
        config::install_fonts(&ctx, None, None);
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let badge = |n: u64, kind| pr_status::Badge {
            number: n,
            url: format!("https://github.com/a/b/pull/{n}"),
            kind,
            detail: format!("#{n}"),
            root: "/repo".into(),
            branch: format!("feat-{n}"),
        };
        let badges = vec![
            badge(4, pr_status::Kind::Merged),
            badge(9, pr_status::Kind::Ok),
        ];

        fn collect(shape: &egui::Shape, out: &mut Vec<egui::Shape>) {
            if let egui::Shape::Vec(v) = shape {
                for s in v {
                    collect(s, out);
                }
            } else {
                out.push(shape.clone());
            }
        }

        for (label, expect_title) in [("agent-pane", true), ("", false)] {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(900.0, 700.0),
                )),
                ..Default::default()
            };
            let output = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    draw_pane_title(
                        ui,
                        Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(880.0, 680.0),
                        ),
                        label,
                        None,
                        &badges,
                        PaneId(1),
                        true,
                        &th,
                        None,
                    );
                });
            });
            let mut shapes = Vec::new();
            for clipped in &output.shapes {
                collect(&clipped.shape, &mut shapes);
            }
            let texts: Vec<String> = shapes
                .iter()
                .filter_map(|s| match s {
                    egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            for number in ["#4", "#9"] {
                assert!(
                    texts.iter().any(|t| t == number),
                    "chip {number} missing with label {label:?}: {texts:?}"
                );
            }
            assert_eq!(
                texts.iter().any(|t| t == "agent-pane"),
                expect_title,
                "title box wrong for label {label:?}: {texts:?}"
            );
        }
    }

    #[test]
    fn elide_keeps_the_head() {
        assert_eq!(elide("zsh", 10), "zsh");
        assert_eq!(elide("● alice · reviewer", 8), "● alice…");
        assert_eq!(elide("abcdef", 1), "…");
        assert_eq!(elide("abcdef", 0), "");
    }

    #[test]
    fn nearest_visible_prefers_left_then_right() {
        // tabs 0..4, tab 2 just archived -> visible = [0,1,3,4].
        let visible = [0, 1, 3, 4];
        assert_eq!(nearest_visible(&visible, 2), Some(1)); // left neighbor
        // Archiving the leftmost visible tab -> fall through to the right.
        let visible = [1, 2, 3];
        assert_eq!(nearest_visible(&visible, 0), Some(1));
        // Archiving the rightmost -> left neighbor.
        let visible = [0, 1];
        assert_eq!(nearest_visible(&visible, 2), Some(1));
        // Nothing else visible -> caller must spawn a fresh tab.
        assert_eq!(nearest_visible(&[], 0), None);
    }

    #[test]
    fn active_after_removal_follows_the_active_tab() {
        // Remove a tab below the active one: active shifts down with it.
        assert_eq!(active_after_removal(3, 1, 4), 2);
        // Remove a tab above the active one: active unchanged.
        assert_eq!(active_after_removal(1, 3, 4), 1);
        // Remove the active tab when it was last: clamp to the new end.
        assert_eq!(active_after_removal(4, 4, 4), 3);
        // Remove the active tab mid-list: same index, now the next tab.
        assert_eq!(active_after_removal(2, 2, 4), 2);
        // Last tab overall removed: clamps to 0 (caller then quits).
        assert_eq!(active_after_removal(0, 0, 0), 0);
    }

    #[test]
    fn step_visible_target_wraps_and_handles_peek() {
        let visible = [0, 2, 3]; // tab 1 archived
        // Forward from tab 0 -> 2, wrap from 3 -> 0.
        assert_eq!(step_visible_target(&visible, 0, 1), Some(2));
        assert_eq!(step_visible_target(&visible, 3, 1), Some(0));
        // Backward from tab 0 wraps to the last visible.
        assert_eq!(step_visible_target(&visible, 0, -1), Some(3));
        // Peeking an archived tab (1, not in the list): Next -> first, Prev -> last.
        assert_eq!(step_visible_target(&visible, 1, 1), Some(0));
        assert_eq!(step_visible_target(&visible, 1, -1), Some(3));
        // Nothing visible.
        assert_eq!(step_visible_target(&[], 0, 1), None);
    }

    #[test]
    fn split_bar_reserves_the_edge_and_skips_short_panes() {
        use crate::theme::BarEdge::*;
        let pane = Rect::from_min_max(
            egui::pos2(100.0, 200.0),
            egui::pos2(300.0, 400.0),
        );
        let (bar, term) = split_bar(pane, Top, 24.0).unwrap();
        assert_eq!(bar, Rect::from_min_max(pane.min, egui::pos2(300.0, 224.0)));
        assert_eq!(
            term,
            Rect::from_min_max(egui::pos2(100.0, 224.0), pane.max)
        );
        let (bar, term) = split_bar(pane, Bottom, 24.0).unwrap();
        assert_eq!(bar, Rect::from_min_max(egui::pos2(100.0, 376.0), pane.max));
        assert_eq!(
            term,
            Rect::from_min_max(pane.min, egui::pos2(300.0, 376.0))
        );
        // The two halves partition the pane exactly.
        for edge in [Top, Bottom] {
            let (bar, term) = split_bar(pane, edge, 24.0).unwrap();
            assert_eq!(bar.width(), pane.width());
            assert_eq!(bar.height() + term.height(), pane.height());
        }
        // Too short to give up a strip: the terminal keeps everything.
        let short = Rect::from_min_size(pane.min, Vec2::new(200.0, 100.0));
        assert!(split_bar(short, Top, 24.0).is_none());
    }

    #[test]
    fn hud_line_y_matches_old_corners_and_centers_solid() {
        use crate::theme::BarEdge::*;
        let pane = Rect::from_min_max(
            egui::pos2(100.0, 200.0),
            egui::pos2(300.0, 400.0),
        );
        // Floating chips keep the classic 4pt corner inset.
        assert_eq!(hud_line_y(pane, 20.0, Top, false), 204.0);
        assert_eq!(hud_line_y(pane, 20.0, Bottom, false), 376.0);
        // A solid strip centers the line inside itself.
        let (bar, _) = split_bar(pane, Top, 24.0).unwrap();
        assert_eq!(hud_line_y(bar, 20.0, Top, true), 202.0);
    }

    /// The solid strip fills the pane's full width, and the chips shed
    /// their own translucent boxes (the strip is the background).
    #[test]
    fn solid_bar_paints_strip_without_chip_boxes() {
        let ctx = egui::Context::default();
        // Bind the HUD font family (`th.bar_font`) before painting its text.
        config::install_fonts(&ctx, None, None);
        let preset = theme::preset("bbs").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let badges = vec![pr_status::Badge {
            number: 7,
            url: "https://github.com/a/b/pull/7".into(),
            kind: pr_status::Kind::Ok,
            detail: "#7".into(),
            root: "/repo".into(),
            branch: "feat-7".into(),
        }];
        let pane =
            Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(880.0, 680.0));
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                draw_pane_title(
                    ui,
                    pane,
                    "agent-pane",
                    None,
                    &badges,
                    PaneId(1),
                    true,
                    &th,
                    Some(24.0),
                );
            });
        });
        fn collect(shape: &egui::Shape, out: &mut Vec<egui::Shape>) {
            if let egui::Shape::Vec(v) = shape {
                for s in v {
                    collect(s, out);
                }
            } else {
                out.push(shape.clone());
            }
        }
        let mut shapes = Vec::new();
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut shapes);
        }
        let strip = shapes.iter().any(|s| {
            matches!(s, egui::Shape::Rect(r)
                if r.fill == th.bar_bg && r.rect.width() == pane.width())
        });
        assert!(strip, "full-width strip missing");
        let chip_box = shapes.iter().any(|s| {
            matches!(s, egui::Shape::Rect(r)
                if r.fill == th.bar_bg.gamma_multiply(0.8))
        });
        assert!(!chip_box, "translucent chip box painted on the strip");
        let texts: Vec<String> = shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect();
        for t in ["#7", "agent-pane"] {
            assert!(
                texts.iter().any(|x| x == t),
                "{t} missing on the strip: {texts:?}"
            );
        }
    }
}
