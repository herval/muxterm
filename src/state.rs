use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::layout::SplitAxis;

pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug)]
pub struct StateFile {
    pub version: u32,
    pub windows: Vec<WindowState>,
    /// Folder the workspace-creation popup pre-fills next time (the last one
    /// a workspace was created in). Absent in pre-workspace state files.
    #[serde(default)]
    pub last_workspace_dir: Option<String>,
    /// Whether the workspace sidebar is shown. Defaults on so the feature is
    /// discoverable; toggled by cmd+\.
    #[serde(default = "default_true")]
    pub sidebar_open: bool,
    /// Whether the sidebar's archived pile is folded to its header. Additive
    /// with `#[serde(default)]` (expanded), so older state files load as-is.
    #[serde(default)]
    pub archived_collapsed: bool,
    /// The saved project registry (Settings > Projects), the sources the
    /// cmd+shift+n popup offers. Additive with `#[serde(default)]` (empty).
    #[serde(default)]
    pub projects: Vec<ProjectState>,
    /// The saved workspace-layout templates (Settings > Templates), offered by
    /// the new-workspace popup's template picker. Additive with
    /// `#[serde(default)]` (empty), so older state files load unchanged.
    #[serde(default)]
    pub templates: Vec<TemplateState>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WindowState {
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TabState {
    /// Stable tab identity (`mux-tab-<8hex>`), used to scope the agent
    /// mesh's per-tab context. Empty in pre-mesh state files; backfilled
    /// on load.
    #[serde(default)]
    pub id: String,
    pub tree: NodeState,
    pub focused_session: String,
    /// Workspace metadata (folder, worktree, prompt, agent, AI title). None
    /// for a bare shell tab (cmd+t) and for pre-workspace state files.
    #[serde(default)]
    pub workspace: Option<WorkspaceState>,
}

/// Serde mirror of the GUI's `workspace::Workspace`; the layout's source of
/// truth for a workspace lives here, alongside the split tree.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorkspaceState {
    #[serde(default)]
    pub root: Option<PathBuf>,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub worktree: Option<WorktreeState>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub created_at: u64,
    /// When the workspace was archived, or None while active. Additive with
    /// `#[serde(default)]`, so pre-archive state files load with all tabs
    /// visible - no VERSION bump needed.
    #[serde(default)]
    pub archived_at: Option<u64>,
    /// The project's setup script, copied in at creation (the workspace owns
    /// its copy so later project edits/removal never touch a live tab, and a
    /// relaunch mid-checkout still knows what to run).
    #[serde(default)]
    pub setup: Option<String>,
    /// The project's subfolder, copied in at creation like `setup`: the
    /// workspace's panes cd here (inside the worktree) before setup runs.
    #[serde(default)]
    pub subdir: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WorktreeState {
    pub path: PathBuf,
    pub branch: String,
}

/// Serde mirror of the GUI's `workspace::Project`: a saved workspace source -
/// a folder on disk (`path`) or a GitHub repo (`repo`, cloned on first use
/// under `~/.muxterm/clones/`), plus an optional subfolder the workspace
/// cd's into and an optional setup script typed into a new workspace's
/// first pane.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProjectState {
    pub name: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub setup: Option<String>,
    #[serde(default)]
    pub subdir: Option<String>,
}

/// Serde mirror of the GUI's `workspace::Template`: a named workspace-layout
/// preset. `panes[0]` is the main pane (the agent/shell the workspace would
/// normally open); each later pane splits the previous one and runs `command`
/// after the shared cd + setup boot.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TemplateState {
    pub name: String,
    #[serde(default)]
    pub panes: Vec<TemplatePaneState>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TemplatePaneState {
    /// Shell command typed after the cd + setup boot; empty leaves a bare
    /// shell. Ignored for `panes[0]` (the main pane runs the agent/shell).
    #[serde(default)]
    pub command: String,
    /// How this pane splits off the previous one; ignored for `panes[0]`.
    #[serde(default = "default_split_axis")]
    pub split: SplitAxis,
    /// This pane's percent of the split region (10..=90); the previous pane
    /// keeps the rest. Ignored for `panes[0]`.
    #[serde(default = "default_split_size")]
    pub size: u8,
}

fn default_split_axis() -> SplitAxis {
    SplitAxis::SideBySide
}

fn default_split_size() -> u8 {
    50
}

#[derive(Serialize, Deserialize, Debug)]
pub enum NodeState {
    Leaf {
        session: String,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<NodeState>,
        second: Box<NodeState>,
    },
}

impl NodeState {
    pub fn sessions(&self, out: &mut HashSet<String>) {
        match self {
            NodeState::Leaf { session } => {
                out.insert(session.clone());
            },
            NodeState::Split { first, second, .. } => {
                first.sessions(out);
                second.sessions(out);
            },
        }
    }

    /// In-order session names (pane order within the tab).
    pub fn session_list(&self, out: &mut Vec<String>) {
        match self {
            NodeState::Leaf { session } => out.push(session.clone()),
            NodeState::Split { first, second, .. } => {
                first.session_list(out);
                second.session_list(out);
            },
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .expect("no home directory on this platform")
        .join(".muxterm")
}

/// One-time move of muxterm's state out of the old
/// `~/Library/Application Support/muxterm/` into `~/.muxterm/`. Idempotent -
/// a no-op once the new dir exists - so it is safe to call at the top of
/// every launch of either binary (whichever runs first migrates). A rename is
/// atomic when both paths share a volume (the common case on macOS); the
/// recursive-copy fallback covers a cross-volume move. The tmux server is
/// unaffected: its socket is name-keyed (`-L muxterm`), and tmux.conf is
/// regenerated at launch regardless.
pub fn migrate_config_dir() {
    let new = config_dir();
    if new.exists() {
        return;
    }
    let Some(old) = dirs::config_dir().map(|d| d.join("muxterm")) else {
        return;
    };
    if !old.exists() {
        return;
    }
    if let Some(parent) = new.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::rename(&old, &new).is_ok() {
        log::info!("migrated config dir to {}", new.display());
        return;
    }
    match copy_dir_recursive(&old, &new) {
        Ok(()) => {
            let _ = fs::remove_dir_all(&old);
            log::info!("migrated config dir (copy) to {}", new.display());
        },
        Err(e) => log::warn!("could not migrate config dir: {e:#}"),
    }
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Directory holding all workspace git worktrees (`~/.muxterm/worktrees/`).
/// Worktrees live outside their repos so the repo's own `git status` stays
/// clean and no `.gitignore` handling is needed.
pub fn worktrees_dir() -> PathBuf {
    config_dir().join("worktrees")
}

/// Directory holding the local clones of GitHub-repo projects
/// (`~/.muxterm/clones/`), made on first use. Never GC'd - same doctrine as
/// worktrees; removing a project keeps its clone.
pub fn clones_dir() -> PathBuf {
    config_dir().join("clones")
}

pub fn state_path() -> PathBuf {
    config_dir().join("state.json")
}

pub enum LoadResult {
    Loaded(StateFile),
    FirstRun,
    /// Present but unreadable. The caller must skip session GC in this case.
    Corrupt,
}

/// Read-only load for external tools (the `mux` CLI): no `.bak` renaming,
/// no side effects. Returns None when missing or unreadable.
pub fn peek() -> Option<StateFile> {
    let text = fs::read_to_string(state_path()).ok()?;
    serde_json::from_str::<StateFile>(&text)
        .ok()
        .filter(|s| s.version == VERSION)
}

pub fn load() -> LoadResult {
    let path = state_path();
    match fs::read_to_string(&path) {
        Err(_) => LoadResult::FirstRun,
        Ok(text) => match serde_json::from_str::<StateFile>(&text) {
            Ok(s) if s.version == VERSION => LoadResult::Loaded(s),
            _ => {
                log::warn!("unreadable state file, moving it to state.json.bak");
                let _ = fs::rename(&path, path.with_extension("json.bak"));
                LoadResult::Corrupt
            },
        },
    }
}

pub fn save(state: &StateFile) -> anyhow::Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// The codename vocabulary: `workspace::random_title` composes a random
/// `<adjective>-<animal>` from these lists, and `is_codename` recognizes one.
/// They live here (a lib module the `mux` CLI can reach) so both the GUI's
/// namer and the CLI's rename nudge share one source of truth.
pub const ADJECTIVES: &[&str] = &[
    "amber", "bold", "brave", "breezy", "bright", "brisk", "calm", "candid",
    "cheery", "civil", "clever", "cosmic", "cozy", "crisp", "daring", "deft",
    "dapper", "eager", "fabled", "fancy", "fleet", "gentle", "gilded", "glad",
    "golden", "hardy", "hazel", "humble", "jaunty", "jolly", "keen", "limber",
    "lively", "lucid", "lunar", "mellow", "merry", "mighty", "nimble", "noble",
    "peppy", "perky", "placid", "plucky", "proud", "quiet", "rapid", "regal",
    "rosy", "rustic", "sage", "sandy", "serene", "sleek", "snappy", "solar",
    "spry", "stout", "sunny", "swift", "tidy", "vivid", "wry", "zesty",
];

pub const ANIMALS: &[&str] = &[
    "badger", "bat", "bear", "beaver", "bee", "bison", "camel", "cat",
    "cheetah", "crab", "crane", "crow", "deer", "dingo", "dolphin", "dove",
    "eagle", "egret", "falcon", "ferret", "finch", "fox", "gecko", "gibbon",
    "hare", "hawk", "heron", "hound", "ibex", "impala", "jackal", "koala",
    "lemur", "lion", "llama", "lynx", "manatee", "marmot", "mole", "moose",
    "narwhal", "newt", "ocelot", "orca", "osprey", "otter", "owl", "panda",
    "pelican", "pony", "puffin", "quail", "rabbit", "raven", "seal", "shrew",
    "sparrow", "stoat", "swan", "tapir", "toucan", "walrus", "wombat", "wren",
];

/// Is `title` still an auto-generated `<adjective>-<animal>` codename
/// (`workspace::random_title`)? True only for the exact codename shape - both
/// halves in the lists, no extra segments - so a real name like `fix-auth` or
/// `auth` reads as deliberate. The nudge to `mux rename` fires while, and only
/// while, this holds, so it self-suppresses the moment the tab gets a name.
pub fn is_codename(title: &str) -> bool {
    match title.split_once('-') {
        Some((adj, animal)) => {
            ADJECTIVES.contains(&adj) && ANIMALS.contains(&animal)
        },
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip() {
        let state = StateFile {
            version: VERSION,
            last_workspace_dir: Some("/tmp/proj".into()),
            sidebar_open: true,
            archived_collapsed: false,
            projects: vec![
                ProjectState {
                    name: "muxterm".into(),
                    path: Some("/tmp/proj".into()),
                    repo: None,
                    setup: Some("direnv allow".into()),
                    subdir: None,
                },
                ProjectState {
                    name: "dotfiles/nvim".into(),
                    path: None,
                    repo: Some("herval/dotfiles".into()),
                    setup: None,
                    subdir: Some("nvim".into()),
                },
            ],
            templates: vec![TemplateState {
                name: "dev".into(),
                panes: vec![
                    TemplatePaneState {
                        command: String::new(),
                        split: SplitAxis::SideBySide,
                        size: 50,
                    },
                    TemplatePaneState {
                        command: "gitwatch".into(),
                        split: SplitAxis::Stacked,
                        size: 35,
                    },
                ],
            }],
            windows: vec![WindowState {
                active_tab: 1,
                tabs: vec![
                    TabState {
                        id: "mux-tab-1111".into(),
                        tree: NodeState::Leaf {
                            session: "mux-aaaa".into(),
                        },
                        focused_session: "mux-aaaa".into(),
                        workspace: None,
                    },
                    TabState {
                        id: "mux-tab-2222".into(),
                        tree: NodeState::Split {
                            axis: SplitAxis::SideBySide,
                            ratio: 0.3,
                            first: Box::new(NodeState::Leaf {
                                session: "mux-bbbb".into(),
                            }),
                            second: Box::new(NodeState::Leaf {
                                session: "mux-cccc".into(),
                            }),
                        },
                        focused_session: "mux-cccc".into(),
                        workspace: Some(WorkspaceState {
                            root: Some("/tmp/proj".into()),
                            title: "wire up auth".into(),
                            description: None,
                            prompt: "wire up auth".into(),
                            worktree: Some(WorktreeState {
                                path: "/home/u/.muxterm/worktrees/wire-up-auth"
                                    .into(),
                                branch: "wire-up-auth".into(),
                            }),
                            agent: Some("claude".into()),
                            model: Some("sonnet".into()),
                            created_at: 123,
                            archived_at: None,
                            setup: Some("direnv allow".into()),
                            subdir: Some("apps/web".into()),
                        }),
                    },
                ],
            }],
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let back: StateFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, VERSION);
        assert_eq!(back.windows[0].active_tab, 1);
        assert_eq!(back.windows[0].tabs.len(), 2);
        assert_eq!(back.last_workspace_dir.as_deref(), Some("/tmp/proj"));
        let ws = back.windows[0].tabs[1].workspace.as_ref().unwrap();
        assert_eq!(ws.title, "wire up auth");
        assert_eq!(ws.worktree.as_ref().unwrap().branch, "wire-up-auth");
        assert_eq!(ws.setup.as_deref(), Some("direnv allow"));
        assert_eq!(back.projects.len(), 2);
        assert_eq!(back.projects[0].path.as_deref(), Some(Path::new("/tmp/proj")));
        assert_eq!(back.projects[1].repo.as_deref(), Some("herval/dotfiles"));
        assert_eq!(back.projects[1].subdir.as_deref(), Some("nvim"));
        assert_eq!(back.templates.len(), 1);
        assert_eq!(back.templates[0].name, "dev");
        assert_eq!(back.templates[0].panes.len(), 2);
        assert_eq!(back.templates[0].panes[1].command, "gitwatch");
        assert_eq!(back.templates[0].panes[1].split, SplitAxis::Stacked);
        assert_eq!(back.templates[0].panes[1].size, 35);

        let mut sessions = HashSet::new();
        for tab in &back.windows[0].tabs {
            tab.tree.sessions(&mut sessions);
        }
        assert_eq!(sessions.len(), 3);
        assert!(sessions.contains("mux-cccc"));
    }

    // A pre-workspace state file has neither `workspace` on tabs nor the
    // top-level `last_workspace_dir`/`sidebar_open`; serde defaults must fill
    // them so an upgrade never drops a saved layout.
    #[test]
    fn pre_workspace_state_loads() {
        let json = r#"{
            "version": 1,
            "windows": [{
                "active_tab": 0,
                "tabs": [{
                    "id": "mux-tab-1111",
                    "tree": {"Leaf": {"session": "mux-aaaa"}},
                    "focused_session": "mux-aaaa"
                }]
            }]
        }"#;
        let s: StateFile = serde_json::from_str(json).unwrap();
        assert_eq!(s.windows[0].tabs.len(), 1);
        assert!(s.windows[0].tabs[0].workspace.is_none());
        assert!(s.last_workspace_dir.is_none());
        // Sidebar defaults on for discoverability, archived pile expanded.
        assert!(s.sidebar_open);
        assert!(!s.archived_collapsed);
        // No saved projects or templates in an older file.
        assert!(s.projects.is_empty());
        assert!(s.templates.is_empty());
    }

    #[test]
    fn is_codename_recognizes_only_the_codename_shape() {
        // A live codename off `random_title`: both halves are in the lists.
        assert!(is_codename("brisk-otter"));
        assert!(is_codename("amber-owl"));
        // Deliberate names, even hyphenated, are not codenames.
        assert!(!is_codename("fix-auth"));
        assert!(!is_codename("auth"));
        assert!(!is_codename("workspace"));
        assert!(!is_codename(""));
        // The right shape but an unknown half, or an extra segment, is out.
        assert!(!is_codename("brisk-widget"));
        assert!(!is_codename("otter"));
        assert!(!is_codename("brisk-otter-x"));
    }
}
