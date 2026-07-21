//! The cmd+n workspace-creation popup, painted in the same terminal-panel
//! idiom as the settings window: a hairline-framed box of monospace rows with
//! `[ bracketed ]` section rules, an `[x]` toggle, and `>` selection markers.
//! Unlike settings (a pure painter grid) this hosts real text entry, so the
//! folder and task fields stay egui `TextEdit`s - restyled to monospace with a
//! hairline border so they sit inside the same grid.
//!
//! cmd+shift+n opens the same form in *project mode* (`for_project`): the
//! folder field gives way to rows over the saved projects (Settings >
//! Projects), and the worktree toggle disappears (always on). A repo project
//! that hasn't been cloned yet feeds the same branch typeahead from
//! `git ls-remote` (async; `poll_remote_branches`), falling back to a plain
//! field with deferred resolution when the probe fails.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use egui::text::LayoutJob;
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Key, Pos2, Response,
    Sense, Shadow, Stroke, TextFormat, Vec2,
};

use muxterm::agent::{self, Agent};

use crate::theme::{self, UiTheme};
use crate::workspace::{Branch, BranchChoice, Project, Template};

/// Live state of the open popup, owned by the App as `Option<NewWorkspaceForm>`.
pub struct NewWorkspaceForm {
    pub folder: String,
    pub create_worktree: bool,
    /// The branch typeahead's text: empty = a fresh codename branch; an
    /// existing branch checks it out; an unknown name creates it. Resolved
    /// against `branches` by `branch_choice`.
    pub branch: String,
    pub prompt: String,
    pub agent: &'static str,
    pub model: String,
    /// Project mode (cmd+shift+n): the saved projects to pick from,
    /// snapshotted at open. Empty = the plain cmd+n folder form.
    pub projects: Vec<Project>,
    /// Index into `projects` of the picked one.
    pub project: usize,
    /// The saved layout templates to pick from, snapshotted at open. Empty
    /// hides the picker (single pane, the default).
    pub templates: Vec<Template>,
    /// The picked template, or None for the default single-pane layout.
    pub template: Option<usize>,
    /// Cached `is_git_repo` for `folder`, refreshed only when the folder text
    /// settles on an existing directory (so we don't spawn `git` per keystroke).
    is_repo: bool,
    /// Branches of `folder`'s repo, newest commit first; empty when not a
    /// repo. Refreshed together with `is_repo` (same `checked` guard), so it
    /// can never go stale against the folder. For an un-cloned repo project
    /// this instead carries the `ls-remote` list (`poll_remote_branches`),
    /// so the typeahead is one mechanism either way.
    branches: Vec<Branch>,
    checked: String,
    /// In-flight `ls-remote` probe for an un-cloned repo project, keyed by
    /// repo so a stale answer can't paint another project's list.
    remote_rx: Option<(String, Receiver<Option<Vec<Branch>>>)>,
    /// repo -> preflight result: Some(list) = reachable (list feeds the
    /// typeahead), None = unreachable (submit refuses - a clone would only
    /// fail later and messier). Cached across project-row clicks.
    remote_branches: HashMap<String, Option<Vec<Branch>>>,
}

impl NewWorkspaceForm {
    pub fn new(folder: String, agent: &'static str, model: String) -> Self {
        let mut form = Self {
            folder,
            create_worktree: false,
            branch: String::new(),
            prompt: String::new(),
            agent,
            model,
            projects: Vec::new(),
            project: 0,
            templates: Vec::new(),
            template: None,
            is_repo: false,
            branches: Vec::new(),
            checked: String::from("\0"), // force a first check
            remote_rx: None,
            remote_branches: HashMap::new(),
        };
        form.refresh_repo();
        // A git repo defaults the checkbox on - the common case for cmd+n.
        form.create_worktree = form.is_repo;
        form
    }

    /// cmd+shift+n: the same form in project mode, seeded on the first
    /// saved project. The folder tracks the picked project; a worktree is
    /// always wanted (that's the point of a project session), even when
    /// the checkbox logic would say no because the clone doesn't exist yet.
    pub fn for_project(
        projects: Vec<Project>,
        agent: &'static str,
        model: String,
    ) -> Self {
        let folder = projects
            .first()
            .map(|p| p.local_root().display().to_string())
            .unwrap_or_default();
        let mut form = Self::new(folder, agent, model);
        form.projects = projects;
        form
    }

    /// The picked project, in project mode.
    pub fn selected_project(&self) -> Option<&Project> {
        self.projects.get(self.project)
    }

    /// The picked layout template, or None for the default single-pane layout.
    pub fn selected_template(&self) -> Option<&Template> {
        self.template.and_then(|i| self.templates.get(i))
    }

    /// Some(clone URL) when the picked project is a repo without its local
    /// clone yet - submit must clone before the worktree checkout.
    pub fn clone_needed(&self) -> Option<String> {
        self.selected_project().and_then(|p| p.needs_clone())
    }

    /// Re-run the git-repo probe (and the branch listing) when the folder
    /// changed to an existing dir. Non-dirs (mid-typing paths) skip the
    /// subprocess and read as non-repos.
    fn refresh_repo(&mut self) {
        if self.folder == self.checked {
            return;
        }
        self.checked = self.folder.clone();
        let path = Path::new(self.folder.trim());
        self.is_repo = path.is_dir() && crate::workspace::is_git_repo(path);
        if !self.is_repo {
            self.create_worktree = false;
        }
        self.branches = if self.is_repo {
            crate::workspace::list_branches(path)
        } else {
            Vec::new()
        };
    }

    /// What the branch field means, resolved against the enumerated list.
    pub fn branch_choice(&self) -> BranchChoice {
        crate::workspace::resolve_branch(&self.branch, &self.branches)
    }

    /// Keep the preflight fresh for an un-cloned repo project: kick one
    /// `ls-remote` probe per repo (off-thread - network), drain its answer,
    /// and surface a reachable repo's branch list through `branches` so
    /// `branch_picker` works exactly as it does against a local repo. The
    /// clone-time resolution doesn't change - `clone_and_populate` still
    /// re-resolves the typed text against the fresh clone; this only makes
    /// the picker see the same names sooner (and unreachable repos fail
    /// *now*, in the popup, instead of after a workspace exists).
    fn poll_remote_branches(&mut self, ctx: &egui::Context) {
        if let Some((repo, rx)) = &self.remote_rx {
            match rx.try_recv() {
                Ok(res) => {
                    self.remote_branches.insert(repo.clone(), res);
                    self.remote_rx = None;
                },
                Err(TryRecvError::Empty) => {},
                Err(TryRecvError::Disconnected) => self.remote_rx = None,
            }
        }
        let Some(repo) = self.clone_needed() else {
            return;
        };
        let probing = self
            .remote_rx
            .as_ref()
            .is_some_and(|(r, _)| *r == repo);
        if !self.remote_branches.contains_key(&repo) && !probing {
            let (tx, rx) = mpsc::channel();
            crate::workspace::spawn_remote_branches(
                repo.clone(),
                tx,
                ctx.clone(),
            );
            self.remote_rx = Some((repo.clone(), rx));
        }
        if self.branches.is_empty() {
            if let Some(Some(bs)) = self.remote_branches.get(&repo) {
                self.branches = bs.clone();
            }
        }
    }

    /// Why Create must not fire yet, if anything: an un-cloned repo
    /// project only proceeds once the `ls-remote` preflight has proven the
    /// URL reachable. Everything else (local projects, cloned repos,
    /// cmd+n mode) is never blocked.
    pub fn submit_blocker(&self) -> Option<Blocker> {
        let repo = self.clone_needed()?;
        match self.remote_branches.get(&repo) {
            None => Some(Blocker::Probing),
            Some(None) => Some(Blocker::Unreachable),
            Some(Some(_)) => None,
        }
    }
}

/// What stands between an un-cloned repo project and Create (`submit_blocker`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Blocker {
    /// The `ls-remote` preflight is still in flight.
    Probing,
    /// The preflight failed: this URL can't be cloned from here.
    Unreachable,
}

pub enum Outcome {
    None,
    Cancel,
    Create,
}

pub enum ConfirmOutcome {
    None,
    Delete,
    Keep,
}

pub fn show(
    ctx: &egui::Context,
    form: &mut NewWorkspaceForm,
    agents: &[&'static Agent],
    th: &UiTheme,
    font: &FontId,
) -> Outcome {
    form.refresh_repo();
    if !form.projects.is_empty() {
        form.poll_remote_branches(ctx);
    }
    let mut outcome = Outcome::None;

    let panel = Panel {
        font: font.clone(),
        char_w: ctx.fonts(|f| f.glyph_width(font, ' ')),
        row_h: ctx.fonts(|f| f.row_height(font)),
        th,
    };

    egui::Window::new("New Workspace")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .default_width(480.0)
        .frame(
            egui::Frame::new()
                .fill(th.bg)
                .inner_margin(16.0)
                .stroke(panel.hairline())
                .shadow(Shadow {
                    offset: [0, 6],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(100),
                }),
        )
        .show(ctx, |ui| {
            ui.set_width(452.0);
            ui.spacing_mut().item_spacing = Vec2::new(0.0, 6.0);
            // The two TextEdits are the only built-in widgets; drag them into
            // the grid's look (monospace, hairline border, accent on focus).
            panel.style_inputs(ui);

            let project_mode = !form.projects.is_empty();
            let title = if project_mode {
                "[ New workspace from project ]"
            } else {
                "[ New workspace ]"
            };
            panel.divider(ui, title, th.accent);
            ui.add_space(2.0);

            if project_mode {
                project_picker(ui, form, &panel, th);
            } else {
                panel.divider(ui, "Folder", th.accent);
                ui.add_space(2.0);
                ui.add(
                    egui::TextEdit::singleline(&mut form.folder)
                        .hint_text("~/path/to/project")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(2.0);
                if form.is_repo {
                    if panel
                        .toggle(ui, form.create_worktree, "Create git worktree", true)
                        .clicked()
                    {
                        form.create_worktree = !form.create_worktree;
                    }
                } else {
                    panel.toggle(ui, false, "Create git worktree", false);
                    if !form.folder.trim().is_empty() {
                        panel.row(
                            ui,
                            vec![(
                                "not a git repo - worktree off".into(),
                                th.text_dim,
                            )],
                            false,
                        );
                    }
                }
                if form.is_repo && form.create_worktree {
                    branch_picker(ui, form, &panel, th);
                }
            }
            ui.add_space(6.0);

            panel.divider(ui, "What do you want to work on?", th.accent);
            ui.add_space(2.0);
            ui.add(
                egui::TextEdit::multiline(&mut form.prompt)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(6.0);

            panel.divider(ui, "Agent", th.accent);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                panel.seg(ui, " ", th.text_dim, false, false); // 1-cell indent
                // Pre-filtered by the caller to agents whose CLI is installed.
                for a in agents {
                    let selected = form.agent == a.id;
                    let color = if selected { th.accent } else { th.text };
                    let marker = if selected { "> " } else { "  " };
                    let label = format!("{marker}{}   ", a.label);
                    if panel.seg(ui, &label, color, true, selected).clicked()
                        && !selected
                    {
                        form.agent = a.id;
                        // Keep the model valid for the newly-picked agent.
                        if !current_agent(form.agent)
                            .models
                            .contains(&form.model.as_str())
                        {
                            form.model = default_model(form.agent);
                        }
                    }
                }
            });

            panel.divider(ui, "Model", th.accent);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                panel.seg(ui, " ", th.text_dim, false, false); // 1-cell indent
                for m in current_agent(form.agent).models {
                    let selected = form.model == *m;
                    let color = if selected { th.accent } else { th.text };
                    let marker = if selected { "> " } else { "  " };
                    let label = format!("{marker}{m}   ");
                    if panel.seg(ui, &label, color, true, selected).clicked() {
                        form.model = m.to_string();
                    }
                }
            });
            ui.add_space(8.0);

            // Layout template: single pane (default) or a saved multi-pane
            // preset. Hidden when none are saved (Settings > Templates).
            if !form.templates.is_empty() {
                panel.divider(ui, "Layout", th.accent);
                ui.add_space(2.0);
                // Collect the click and apply after the row - the seg loop
                // borrows form.templates, so it can't also write form.template.
                let mut pick: Option<Option<usize>> = None;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    panel.seg(ui, " ", th.text_dim, false, false); // indent
                    let selected = form.template.is_none();
                    let color = if selected { th.accent } else { th.text };
                    let marker = if selected { "> " } else { "  " };
                    if panel
                        .seg(ui, &format!("{marker}single   "), color, true, selected)
                        .clicked()
                    {
                        pick = Some(None);
                    }
                    for (i, t) in form.templates.iter().enumerate() {
                        let selected = form.template == Some(i);
                        let color = if selected { th.accent } else { th.text };
                        let marker = if selected { "> " } else { "  " };
                        let label = format!("{marker}{}   ", t.name);
                        if panel.seg(ui, &label, color, true, selected).clicked() {
                            pick = Some(Some(i));
                        }
                    }
                });
                if let Some(sel) = pick {
                    form.template = sel;
                }
                ui.add_space(8.0);
            }

            panel.divider(ui, "", th.text_dim);
            ui.add_space(6.0);
            let blocked = form.submit_blocker().is_some();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                if panel.button(ui, "[ Cancel ]", false, true).clicked() {
                    outcome = Outcome::Cancel;
                }
                // Right-align the primary action, like the settings config row.
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if panel
                            .button(ui, "[ Create ]", true, !blocked)
                            .clicked()
                            && !blocked
                        {
                            outcome = Outcome::Create;
                        }
                    },
                );
            });
            ui.add_space(4.0);
            panel.divider(ui, "esc cancels - cmd+enter creates", th.text_dim);
        });

    // cmd+Enter submits from anywhere in the form (Enter alone is a newline in
    // the prompt field). Esc is handled by the App, like the settings window.
    // The preflight blocker gates it exactly like the Create button.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::Enter))
        && form.submit_blocker().is_none()
    {
        outcome = Outcome::Create;
    }
    outcome
}

/// A small modal asking whether to delete a just-closed tab's worktree that
/// still holds uncommitted work, or keep it on disk. Painted in the same
/// panel idiom as the creation popup. Deleting is the deliberate click and is
/// never the keyboard default; Esc keeps (wired by the App), so the safe
/// choice needs no press. `git worktree remove` keeps the branch - only the
/// working dir and its uncommitted changes go.
pub fn confirm_worktree_delete(
    ctx: &egui::Context,
    title: &str,
    path: &str,
    reason: &str,
    th: &UiTheme,
    font: &FontId,
) -> ConfirmOutcome {
    let mut outcome = ConfirmOutcome::None;
    let panel = Panel {
        font: font.clone(),
        char_w: ctx.fonts(|f| f.glyph_width(font, ' ')),
        row_h: ctx.fonts(|f| f.row_height(font)),
        th,
    };

    egui::Window::new("Delete worktree?")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .default_width(460.0)
        .frame(
            egui::Frame::new()
                .fill(th.bg)
                .inner_margin(16.0)
                .stroke(panel.hairline())
                .shadow(Shadow {
                    offset: [0, 6],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(100),
                }),
        )
        .show(ctx, |ui| {
            ui.set_width(432.0);
            ui.spacing_mut().item_spacing = Vec2::new(0.0, 6.0);
            panel.divider(ui, "[ Keep or delete worktree? ]", th.accent);
            ui.add_space(2.0);
            panel.row(
                ui,
                vec![
                    ("workspace ".into(), th.text_dim),
                    (title.to_string(), th.text),
                ],
                false,
            );
            panel.row(ui, vec![(home_abbrev(path), th.text_dim)], false);
            panel.row(ui, vec![(reason.to_string(), th.status_err)], false);
            ui.add_space(2.0);
            panel.row(
                ui,
                vec![(
                    "Deleting discards those uncommitted changes.".into(),
                    th.text_dim,
                )],
                false,
            );
            ui.add_space(6.0);
            panel.divider(ui, "", th.text_dim);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                if panel
                    .button(ui, "[ Delete worktree ]", false, true)
                    .clicked()
                {
                    outcome = ConfirmOutcome::Delete;
                }
                // The safe choice is the primary, right-aligned like Create.
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if panel.button(ui, "[ Keep ]", true, true).clicked() {
                            outcome = ConfirmOutcome::Keep;
                        }
                    },
                );
            });
            ui.add_space(4.0);
            panel.divider(ui, "esc keeps it on disk", th.text_dim);
        });
    outcome
}

/// Project mode's replacement for the Folder section: one row per saved
/// project, then the worktree area for the picked one - the branch picker
/// against a local repo, or the clone notice for a repo that hasn't been
/// cloned yet (same picker once `ls-remote` answers; a plain deferred field
/// until then). There is no worktree toggle in this mode: a project session
/// always wants one.
fn project_picker(
    ui: &mut egui::Ui,
    form: &mut NewWorkspaceForm,
    panel: &Panel<'_>,
    th: &UiTheme,
) {
    panel.divider(ui, "Project", th.accent);
    ui.add_space(2.0);
    let mut pick: Option<usize> = None;
    for (i, p) in form.projects.iter().enumerate() {
        let base = match (&p.path, &p.repo) {
            (Some(path), _) => home_abbrev(&path.display().to_string()),
            (None, Some(repo)) => format!("github: {repo}"),
            (None, None) => String::new(),
        };
        let note = match &p.subdir {
            Some(sub) => format!("{base} /{sub}"),
            None => base,
        };
        if panel
            .option(ui, &p.name, &note, true, i == form.project)
            .clicked()
        {
            pick = Some(i);
        }
    }
    if let Some(i) = pick {
        form.project = i;
        form.folder = form.projects[i].local_root().display().to_string();
        form.refresh_repo();
        // refresh_repo only ever clears the flag; project mode wants the
        // worktree whenever the folder really is a repo.
        form.create_worktree = form.is_repo;
    }
    ui.add_space(2.0);

    if form.clone_needed().is_some() {
        // The home-relative clone destination fits the row; the full URL
        // often wouldn't.
        panel.row(
            ui,
            vec![(
                format!("will clone into {}", home_abbrev(&form.folder)),
                th.text_dim,
            )],
            false,
        );
        match form.submit_blocker() {
            None if !form.branches.is_empty() => {
                // The preflight answered: the same typeahead as against a
                // local repo, every suggestion a to-be-tracked remote branch.
                branch_picker(ui, form, panel, th);
            },
            // Reachable but branchless (a freshly created repo): nothing
            // to suggest, but nothing to block either.
            None => {
                ui.add(
                    egui::TextEdit::singleline(&mut form.branch)
                        .hint_text(
                            "branch: random codename - type a name to use",
                        )
                        .desired_width(f32::INFINITY),
                );
                panel.row(
                    ui,
                    vec![(
                        "-> resolved after clone (empty = codename)".into(),
                        th.text_dim,
                    )],
                    false,
                );
            },
            Some(Blocker::Probing) => {
                panel.row(
                    ui,
                    vec![("checking the remote...".into(), th.text_dim)],
                    false,
                );
            },
            // The preflight failed: creating a workspace would only hit
            // the same wall at clone time - refuse, and say why here.
            Some(Blocker::Unreachable) => {
                panel.row(
                    ui,
                    vec![(
                        "can't reach the remote with this URL".into(),
                        th.status_warn,
                    )],
                    false,
                );
                panel.row(
                    ui,
                    vec![(
                        "check access - private repos may need git@".into(),
                        th.text_dim,
                    )],
                    false,
                );
            },
        }
    } else if form.is_repo {
        form.create_worktree = true;
        branch_picker(ui, form, panel, th);
    } else {
        panel.row(
            ui,
            vec![("not a git repo - worktree off".into(), th.text_dim)],
            false,
        );
    }
}

/// Shorten a home-prefixed path for a row ("/Users/u/dev/x" -> "~/dev/x");
/// anything else passes through.
fn home_abbrev(path: &str) -> String {
    match dirs::home_dir() {
        Some(home) => path
            .strip_prefix(&home.display().to_string())
            .map(|rest| format!("~{rest}"))
            .unwrap_or_else(|| path.to_string()),
        None => path.to_string(),
    }
}

/// The suggestion list's slot count cap: enough to surface the newest
/// branches without dwarfing the rest of the form.
const MAX_SUGGESTIONS: usize = 6;

/// The worktree branch typeahead: a text field over a short suggestion list,
/// closed by a caption row spelling out what Create will do with the field
/// as it stands. The slot count depends only on the enumerated list - never
/// on the filter - so typing cannot resize the auto-sized, center-anchored
/// window under the cursor; filtered-out slots paint as blank rows.
fn branch_picker(
    ui: &mut egui::Ui,
    form: &mut NewWorkspaceForm,
    panel: &Panel<'_>,
    th: &UiTheme,
) {
    ui.add(
        egui::TextEdit::singleline(&mut form.branch)
            .hint_text("branch: random codename - type to search or create")
            .desired_width(f32::INFINITY),
    );
    let slots = form.branches.len().min(MAX_SUGGESTIONS);
    let matches = filter_branches(&form.branches, &form.branch);
    let mut pick: Option<String> = None;
    for i in 0..slots {
        match matches.get(i) {
            Some(b) => {
                let note = match (&b.remote, b.in_use) {
                    (Some(remote), _) => format!("({remote})"),
                    (None, true) => "(checked out)".to_string(),
                    (None, false) => String::new(),
                };
                let selected = form.branch.trim() == b.name;
                if panel
                    .option(ui, &b.name, &note, !b.in_use, selected)
                    .clicked()
                {
                    pick = Some(b.name.clone());
                }
            },
            None => {
                panel.row(ui, vec![], false);
            },
        }
    }
    if let Some(name) = pick {
        form.branch = name;
    }
    let (caption, color) = match form.branch_choice() {
        BranchChoice::Codename => (
            "-> new branch, named by codename".to_string(),
            th.text_dim,
        ),
        BranchChoice::Existing(name) => {
            let in_use = form
                .branches
                .iter()
                .any(|b| b.remote.is_none() && b.in_use && b.name == name);
            if in_use {
                // Pickable rows are already dimmed; this catches a typed-in
                // name. Creation will fail into the walk-back-to-root path.
                (
                    format!("-> '{name}' is checked out elsewhere"),
                    th.status_warn,
                )
            } else {
                ("-> check out existing branch".to_string(), th.text_dim)
            }
        },
        BranchChoice::Track { name, remote } => {
            (format!("-> track {remote}/{name}"), th.text_dim)
        },
        BranchChoice::New(name) => {
            (format!("-> create branch '{name}'"), th.text_dim)
        },
    };
    panel.row(ui, vec![(caption, color)], false);
}

/// Case-insensitive substring filter over branch names, order-preserving
/// (the list arrives newest-first). An empty query keeps everything: the
/// top slots double as discovery of recent branches.
fn filter_branches<'a>(branches: &'a [Branch], query: &str) -> Vec<&'a Branch> {
    let q = query.trim().to_lowercase();
    branches
        .iter()
        .filter(|b| q.is_empty() || b.name.to_lowercase().contains(&q))
        .collect()
}

/// Character-cell geometry and painters shared by the popup's rows, mirroring
/// `settings::Grid` so the two dialogs read as one family.
struct Panel<'a> {
    font: FontId,
    char_w: f32,
    row_h: f32,
    th: &'a UiTheme,
}

impl Panel<'_> {
    fn hairline(&self) -> Stroke {
        Stroke::new(1.0, self.th.text_dim)
    }

    /// Restyle egui's built-in widgets (the two TextEdits) to sit inside the
    /// monospace grid: monospace text, a subtle fill, square corners, and a
    /// hairline border that turns accent on hover/focus.
    fn style_inputs(&self, ui: &mut egui::Ui) {
        ui.style_mut().override_font_id = Some(self.font.clone());
        let accent = Stroke::new(1.0, self.th.accent);
        let v = ui.visuals_mut();
        v.override_text_color = Some(self.th.text);
        v.extreme_bg_color = theme::blend(self.th.bg, self.th.text, 0.06);
        v.selection.bg_fill = theme::blend(self.th.bg, self.th.accent, 0.35);
        v.selection.stroke = accent; // the border of a focused field
        v.widgets.inactive.corner_radius = CornerRadius::ZERO;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, self.th.text_dim);
        v.widgets.hovered.corner_radius = CornerRadius::ZERO;
        v.widgets.hovered.bg_stroke = accent;
        v.widgets.active.corner_radius = CornerRadius::ZERO;
        v.widgets.active.bg_stroke = accent;
    }

    /// A section rule: a hairline across the full content width, interrupted by
    /// an optional inline title. Mirrors `settings::Grid::divider`.
    fn divider(&self, ui: &mut egui::Ui, title: &str, color: Color32) {
        let w = ui.available_width();
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(w, self.row_h), Sense::hover());
        let y = rect.center().y;
        let (x0, x1) = (rect.min.x, rect.max.x);
        if title.is_empty() {
            ui.painter().hline(x0..=x1, y, self.hairline());
            return;
        }
        let tx0 = x0 + self.char_w;
        let tx1 = tx0 + title.chars().count() as f32 * self.char_w;
        let air = 0.4 * self.char_w;
        ui.painter().hline(x0..=(tx0 - air), y, self.hairline());
        ui.painter().hline((tx1 + air)..=x1, y, self.hairline());
        ui.painter().text(
            Pos2::new(tx0, rect.min.y),
            Align2::LEFT_TOP,
            title,
            self.font.clone(),
            color,
        );
    }

    /// A full-width painted row of colored segments, content indented one cell.
    /// Clickable rows get a faint accent wash on hover.
    fn row(
        &self,
        ui: &mut egui::Ui,
        segs: Vec<(String, Color32)>,
        clickable: bool,
    ) -> Response {
        let w = ui.available_width();
        let sense = if clickable { Sense::click() } else { Sense::hover() };
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(w, self.row_h), sense);
        if clickable && resp.hovered() {
            ui.painter().rect_filled(
                rect,
                CornerRadius::ZERO,
                theme::blend(self.th.bg, self.th.accent, 0.10),
            );
        }
        let mut job = LayoutJob::default();
        for (text, color) in &segs {
            job.append(
                text,
                0.0,
                TextFormat::simple(self.font.clone(), *color),
            );
        }
        let galley = ui.fonts(|f| f.layout_job(job));
        ui.painter().galley(
            rect.min + Vec2::new(self.char_w, 0.0),
            galley,
            self.th.text,
        );
        if clickable {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            resp
        }
    }

    /// One branch-typeahead suggestion row: `> name  (note)`, marker on hover
    /// or selection; dimmed and inert when disabled (the branch is checked
    /// out elsewhere, so picking it could only fail).
    fn option(
        &self,
        ui: &mut egui::Ui,
        name: &str,
        note: &str,
        enabled: bool,
        selected: bool,
    ) -> Response {
        let w = ui.available_width();
        let sense = if enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(w, self.row_h), sense);
        let wash = if selected {
            Some(theme::blend(self.th.bg, self.th.accent, 0.22))
        } else if enabled && resp.hovered() {
            Some(theme::blend(self.th.bg, self.th.accent, 0.10))
        } else {
            None
        };
        if let Some(c) = wash {
            ui.painter().rect_filled(rect, CornerRadius::ZERO, c);
        }
        let marker = if selected || (enabled && resp.hovered()) {
            "> "
        } else {
            "  "
        };
        let name_color = if selected {
            self.th.accent
        } else if enabled {
            self.th.text
        } else {
            self.th.text_dim
        };
        let mut job = LayoutJob::default();
        job.append(
            marker,
            0.0,
            TextFormat::simple(self.font.clone(), self.th.accent),
        );
        job.append(
            name,
            0.0,
            TextFormat::simple(self.font.clone(), name_color),
        );
        if !note.is_empty() {
            job.append(
                &format!("  {note}"),
                0.0,
                TextFormat::simple(self.font.clone(), self.th.text_dim),
            );
        }
        let galley = ui.fonts(|f| f.layout_job(job));
        ui.painter().galley(
            rect.min + Vec2::new(self.char_w, 0.0),
            galley,
            self.th.text,
        );
        if enabled {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            resp
        }
    }

    /// A `[x]`/`[ ]` toggle row: accent bracket, themed label; dimmed and inert
    /// when disabled (the folder isn't a git repo).
    fn toggle(
        &self,
        ui: &mut egui::Ui,
        on: bool,
        label: &str,
        enabled: bool,
    ) -> Response {
        let mark = if on { "[x]" } else { "[ ]" };
        let bracket = if enabled { self.th.accent } else { self.th.text_dim };
        let fg = if enabled { self.th.text } else { self.th.text_dim };
        self.row(
            ui,
            vec![(mark.into(), bracket), (format!(" {label}"), fg)],
            enabled,
        )
    }

    /// One independently-clickable run of cells inside a horizontal row, with a
    /// selection wash / hover wash. Mirrors `settings::Grid::seg`.
    fn seg(
        &self,
        ui: &mut egui::Ui,
        text: &str,
        color: Color32,
        clickable: bool,
        selected: bool,
    ) -> Response {
        let w = text.chars().count() as f32 * self.char_w;
        let sense = if clickable { Sense::click() } else { Sense::hover() };
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(w, self.row_h), sense);
        let wash = if selected {
            Some(theme::blend(self.th.bg, self.th.accent, 0.22))
        } else if clickable && resp.hovered() {
            Some(theme::blend(self.th.bg, self.th.accent, 0.10))
        } else {
            None
        };
        if let Some(c) = wash {
            ui.painter().rect_filled(rect, CornerRadius::ZERO, c);
        }
        ui.painter().text(
            rect.min,
            Align2::LEFT_TOP,
            text,
            self.font.clone(),
            color,
        );
        if clickable {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            resp
        }
    }

    /// A `[ bracketed ]` action button. The primary one carries a standing
    /// accent wash; both deepen on hover.
    /// A disabled button paints dim with no wash and doesn't invite a
    /// click (the popup shows *why* next to the field it gates).
    fn button(
        &self,
        ui: &mut egui::Ui,
        text: &str,
        primary: bool,
        enabled: bool,
    ) -> Response {
        let w = text.chars().count() as f32 * self.char_w;
        let sense = if enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(w, self.row_h), sense);
        let base = if primary && enabled { 0.20 } else { 0.0 };
        let hover = if enabled && resp.hovered() { 0.12 } else { 0.0 };
        if base + hover > 0.0 {
            ui.painter().rect_filled(
                rect,
                CornerRadius::ZERO,
                theme::blend(self.th.bg, self.th.accent, base + hover),
            );
        }
        let color = match (enabled, primary) {
            (false, _) => self.th.text_dim,
            (true, true) => self.th.accent,
            (true, false) => self.th.text,
        };
        ui.painter().text(
            rect.min,
            Align2::LEFT_TOP,
            text,
            self.font.clone(),
            color,
        );
        if enabled {
            resp.on_hover_cursor(CursorIcon::PointingHand)
        } else {
            resp
        }
    }
}

fn current_agent(id: &str) -> &'static Agent {
    agent::by_id(id).unwrap_or_else(agent::default_agent)
}

/// The dropdown's default selection for an agent: its first curated model.
pub fn default_model(id: &str) -> String {
    current_agent(id)
        .models
        .first()
        .map(|m| m.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Render the popup headless and check that its labels appear, that the
    /// hand-painted title/marker/button runs are pure ASCII (fallback-font
    /// glyphs carry foreign advance widths that break the char-cell math), and
    /// that the whole thing lays out without panicking. An empty folder keeps
    /// `refresh_repo` from spawning `git`, so the test stays subprocess-free.
    #[test]
    fn popup_renders_ascii_and_labelled() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, ui_theme) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let mut form =
            NewWorkspaceForm::new(String::new(), "claude", "opus".into());

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let agents: Vec<_> = agent::AGENTS.iter().collect();
        let mut frame = |ctx: &egui::Context| {
            let _ = show(ctx, &mut form, &agents, &ui_theme, &font);
        };
        // First frame sizes the window invisibly; the second paints for real.
        let _ = ctx.run(input.clone(), &mut frame);
        let output = ctx.run(input, &mut frame);

        let mut texts: Vec<String> = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        for run in &texts {
            assert!(run.is_ascii(), "non-ASCII painted run: {run:?}");
        }
        let joined = texts.join("\u{1}");
        for needle in [
            "[ New workspace ]",
            "Folder",
            "What do you want to work on?",
            "Agent",
            "Model",
            "Claude Code",
            "[ Cancel ]",
            "[ Create ]",
        ] {
            assert!(
                joined.contains(needle),
                "missing {needle:?} in painted runs: {texts:?}"
            );
        }
    }

    /// The dirty-worktree confirmation renders its labels, stays ASCII (its
    /// button/divider runs feed the same char-cell math as the creation
    /// popup), lays out without panicking, and returns None with no input.
    #[test]
    fn confirm_worktree_renders_ascii_and_labelled() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, ui_theme) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        // Seeded non-None so the assert proves it was overwritten.
        let mut outcome = ConfirmOutcome::Delete;
        let mut frame = |ctx: &egui::Context| {
            outcome = confirm_worktree_delete(
                ctx,
                "brisk-otter",
                "~/.muxterm/worktrees/brisk-otter",
                "uncommitted changes (3 entries)",
                &ui_theme,
                &font,
            );
        };
        let _ = ctx.run(input.clone(), &mut frame);
        let output = ctx.run(input, &mut frame);

        assert!(
            matches!(outcome, ConfirmOutcome::None),
            "an untouched modal must return None"
        );

        let mut texts: Vec<String> = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        for run in &texts {
            assert!(run.is_ascii(), "non-ASCII painted run: {run:?}");
        }
        let joined = texts.join("\u{1}");
        for needle in [
            "[ Keep or delete worktree? ]",
            "brisk-otter",
            "uncommitted changes (3 entries)",
            "[ Delete worktree ]",
            "[ Keep ]",
            "esc keeps it on disk",
        ] {
            assert!(
                joined.contains(needle),
                "missing {needle:?} in painted runs: {texts:?}"
            );
        }
    }

    fn collect_texts(shape: &egui::Shape, texts: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => texts.push(t.galley.text().to_string()),
            egui::Shape::Vec(v) => {
                for s in v {
                    collect_texts(s, texts);
                }
            },
            _ => {},
        }
    }

    fn branch(name: &str, remote: Option<&str>, in_use: bool) -> Branch {
        Branch {
            name: name.into(),
            remote: remote.map(str::to_string),
            in_use,
        }
    }

    #[test]
    fn filter_branches_substring_case_insensitive() {
        let bs = vec![
            branch("Feature/Login", None, false),
            branch("main", None, true),
            branch("fix-log-rotation", Some("origin"), false),
        ];
        let names = |v: Vec<&Branch>| {
            v.iter().map(|b| b.name.clone()).collect::<Vec<_>>()
        };
        // Empty keeps everything, order preserved.
        assert_eq!(
            names(filter_branches(&bs, "")),
            vec!["Feature/Login", "main", "fix-log-rotation"]
        );
        assert_eq!(
            names(filter_branches(&bs, "LOG")),
            vec!["Feature/Login", "fix-log-rotation"]
        );
        assert!(filter_branches(&bs, "nope").is_empty());
    }

    /// The branch typeahead renders: field, suggestions with their notes,
    /// and the caption row - all ASCII (same constraint as the base render
    /// test: fallback-font glyphs break the char-cell math).
    #[test]
    fn popup_renders_branch_picker() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, ui_theme) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let mut form =
            NewWorkspaceForm::new(String::new(), "claude", "opus".into());
        // Poke the private fields directly: an empty folder keeps
        // refresh_repo subprocess-free, and the fixture stands in for a repo.
        form.is_repo = true;
        form.create_worktree = true;
        form.branches = vec![
            branch("main", None, true),
            branch("feat/api-gateway", None, false),
            branch("review/x", Some("origin"), false),
        ];

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let agents: Vec<_> = agent::AGENTS.iter().collect();
        let mut render = |form: &mut NewWorkspaceForm| {
            let mut frame = |ctx: &egui::Context| {
                let _ = show(ctx, form, &agents, &ui_theme, &font);
            };
            let _ = ctx.run(input.clone(), &mut frame);
            let output = ctx.run(input.clone(), &mut frame);
            let mut texts: Vec<String> = Vec::new();
            for clipped in &output.shapes {
                collect_texts(&clipped.shape, &mut texts);
            }
            for run in &texts {
                assert!(run.is_ascii(), "non-ASCII painted run: {run:?}");
            }
            texts
        };

        // Empty field: every branch is suggested, notes and all, and the
        // caption promises the codename default.
        let texts = render(&mut form);
        let joined = texts.join("\u{1}");
        for needle in [
            "main",
            "(checked out)",
            "feat/api-gateway",
            "review/x",
            "(origin)",
            "-> new branch, named by codename",
        ] {
            assert!(
                joined.contains(needle),
                "missing {needle:?} in painted runs: {texts:?}"
            );
        }

        // Typing filters the list and the caption tracks the resolution.
        form.branch = "review/x".into();
        let texts = render(&mut form);
        let joined = texts.join("\u{1}");
        assert!(joined.contains("-> track origin/review/x"));
        assert!(
            !joined.contains("feat/api-gateway"),
            "filtered-out branch still painted: {texts:?}"
        );

        // cmd+Enter submits, and the form hands create_workspace the same
        // resolution the caption promised.
        let mut submit = input.clone();
        submit.events.push(egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        let mut outcome = Outcome::None;
        let _ = ctx.run(submit, |ctx| {
            outcome = show(ctx, &mut form, &agents, &ui_theme, &font);
        });
        assert!(matches!(outcome, Outcome::Create));
        assert_eq!(
            form.branch_choice(),
            BranchChoice::Track {
                name: "review/x".into(),
                remote: "origin".into(),
            }
        );
    }

    /// The Layout picker lists "single" plus each saved template when the
    /// form carries any, and vanishes when it doesn't - all ASCII, like
    /// every row.
    #[test]
    fn popup_renders_layout_picker() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, ui_theme) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        let mut form =
            NewWorkspaceForm::new(String::new(), "claude", "opus".into());

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let agents: Vec<_> = agent::AGENTS.iter().collect();
        let render = |form: &mut NewWorkspaceForm| {
            let mut frame = |ctx: &egui::Context| {
                let _ = show(ctx, form, &agents, &ui_theme, &font);
            };
            let _ = ctx.run(input.clone(), &mut frame);
            let output = ctx.run(input.clone(), &mut frame);
            let mut texts: Vec<String> = Vec::new();
            for clipped in &output.shapes {
                collect_texts(&clipped.shape, &mut texts);
            }
            for run in &texts {
                assert!(run.is_ascii(), "non-ASCII painted run: {run:?}");
            }
            texts
        };

        // No templates: the Layout section is absent (single pane, silently).
        let joined = render(&mut form).join("\u{1}");
        assert!(!joined.contains("Layout"), "picker shown with no templates");

        // With templates: the "single" default plus each name are painted.
        form.templates = vec![Template {
            name: "dev-split".into(),
            panes: vec![crate::workspace::TemplatePane {
                command: String::new(),
                split: muxterm::layout::SplitAxis::SideBySide,
                size: 50,
            }],
        }];
        let texts = render(&mut form);
        let joined = texts.join("\u{1}");
        for needle in ["Layout", "single", "dev-split"] {
            assert!(joined.contains(needle), "missing {needle:?} in {texts:?}");
        }

        // Selection is reflected by selected_template() (default = none).
        assert!(form.selected_template().is_none());
        form.template = Some(0);
        assert_eq!(
            form.selected_template().map(|t| t.name.as_str()),
            Some("dev-split"),
        );
    }

    /// Project mode renders project rows instead of the folder field, hides
    /// the worktree toggle, and shows the clone notice with the deferred
    /// branch caption for a repo project that has no local clone yet.
    #[test]
    fn popup_renders_project_mode() {
        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, ui_theme) = theme::build(preset, &HashMap::new(), 0.12);
        let font = FontId::monospace(14.0);
        // Non-existent paths keep refresh_repo subprocess-free; the uuid
        // repo guarantees the clone dest (keyed by repo) is absent.
        let repo = format!("herval/dots-{}", uuid::Uuid::new_v4());
        let projects = vec![
            Project {
                name: "local-proj".into(),
                path: Some("/no/such/dir/local-proj".into()),
                repo: None,
                setup: None,
                subdir: None,
            },
            Project {
                name: "dots/nvim".into(),
                path: None,
                repo: Some(repo.clone()),
                setup: Some("direnv allow".into()),
                subdir: Some("nvim".into()),
            },
        ];
        let mut form = NewWorkspaceForm::for_project(
            projects,
            "claude",
            "opus".into(),
        );

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(900.0, 700.0),
            )),
            ..Default::default()
        };
        let agents: Vec<_> = agent::AGENTS.iter().collect();
        let mut render = |form: &mut NewWorkspaceForm| {
            let mut frame = |ctx: &egui::Context| {
                let _ = show(ctx, form, &agents, &ui_theme, &font);
            };
            let _ = ctx.run(input.clone(), &mut frame);
            let output = ctx.run(input.clone(), &mut frame);
            let mut texts: Vec<String> = Vec::new();
            for clipped in &output.shapes {
                collect_texts(&clipped.shape, &mut texts);
            }
            for run in &texts {
                assert!(run.is_ascii(), "non-ASCII painted run: {run:?}");
            }
            texts.join("\u{1}")
        };

        // First project (a plain folder, not a repo): rows for both
        // projects, no worktree toggle, the non-repo note.
        let joined = render(&mut form);
        assert!(joined.contains("[ New workspace from project ]"));
        assert!(joined.contains("local-proj"));
        assert!(
            joined.contains(&format!("github: {repo} /nvim")),
            "repo note carries the subfolder: {joined}"
        );
        assert!(
            !joined.contains("Create git worktree"),
            "project mode must not offer the toggle: {joined}"
        );
        assert!(joined.contains("not a git repo - worktree off"));

        // Pick the un-cloned repo project (as project_picker's click does).
        // Seed the preflight cache first - a probe must never hit the
        // network in a test. `None` = unreachable: the popup must say so
        // and refuse to create.
        form.project = 1;
        form.folder =
            form.projects[1].local_root().display().to_string();
        let repo = form.clone_needed().expect("repo project needs a clone");
        form.remote_branches.insert(repo.clone(), None);
        let joined = render(&mut form);
        assert!(
            joined.contains("will clone into "),
            "clone notice missing: {joined}"
        );
        assert!(
            joined.contains("can't reach the remote with this URL"),
            "unreachable warning missing: {joined}"
        );
        assert_eq!(form.submit_blocker(), Some(Blocker::Unreachable));

        // cmd+Enter must NOT submit while the preflight says unreachable.
        let mut submit = input.clone();
        submit.events.push(egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        let mut outcome = Outcome::None;
        let _ = ctx.run(submit.clone(), |ctx| {
            outcome = show(ctx, &mut form, &agents, &ui_theme, &font);
        });
        assert!(
            matches!(outcome, Outcome::None),
            "an unreachable remote must block Create"
        );

        // A reachable preflight feeds the same typeahead, every suggestion
        // a to-be-tracked remote branch - and Create unblocks.
        form.remote_branches.insert(
            repo,
            Some(vec![
                branch("main", Some("origin"), false),
                branch("feat/api", Some("origin"), false),
            ]),
        );
        let joined = render(&mut form);
        assert!(joined.contains("feat/api"), "remote suggestion: {joined}");
        assert!(joined.contains("(origin)"), "remote note: {joined}");
        form.branch = "feat/api".into();
        let joined = render(&mut form);
        assert!(
            joined.contains("-> track origin/feat/api"),
            "typed name resolves as tracking: {joined}"
        );
        assert_eq!(form.submit_blocker(), None);

        let mut outcome = Outcome::None;
        let _ = ctx.run(submit, |ctx| {
            outcome = show(ctx, &mut form, &agents, &ui_theme, &font);
        });
        assert!(matches!(outcome, Outcome::Create));
    }
}

