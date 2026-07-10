//! Workspaces: a tab's sense of what it is *for*. Every tab carries a
//! `Workspace` - bare for a plain cmd+t shell tab, rich for a cmd+n workspace
//! (a project folder, an optional git worktree, the task prompt, the agent,
//! and a short AI-generated title). The layout's source of truth is
//! `state::WorkspaceState`; this is the live GUI-side value plus the git and
//! title-generation helpers.
//!
//! Worktrees live under `~/.muxterm/worktrees/` - outside their repos - so the
//! repo's own `git status` stays clean and no `.gitignore` handling is needed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::thread;

use muxterm::agent::{self, Agent};
use muxterm::mesh;
use muxterm::state::{self, ProjectState, WorkspaceState, WorktreeState};

#[derive(Clone, Debug)]
pub struct Workspace {
    /// The folder the workspace lives in; None for a bare shell tab with no
    /// chosen folder. Starts as the creation folder, then follows the panes:
    /// when every pane leaves it (and its repo), the App's root sync
    /// repoints it at where they went (`retarget`).
    pub root: Option<PathBuf>,
    /// Display label: a prompt-derived placeholder at first, upgraded to a
    /// short AI title (`spawn_title`), until a human or an agent renames it
    /// (`mux rename`). Bare tabs keep a random codename.
    pub title: String,
    /// One-line summary; only `mux rename --desc` sets it.
    pub description: Option<String>,
    /// The free-text task the workspace was started from (empty for bare).
    pub prompt: String,
    /// The dedicated git worktree, when "create worktree" was used.
    pub worktree: Option<Worktree>,
    /// Agent id (one of agent::AGENTS) launched in the pane; None for bare.
    pub agent: Option<&'static str>,
    pub model: Option<String>,
    pub created_at: u64,
    /// When the workspace was archived (parked in the sidebar's archived pile,
    /// out of the tab bar and cmd+1..9 flow), or None while it's active. The
    /// timestamp orders the pile newest-first. Archiving never touches the
    /// tmux session - it just hides the tab; the tab stays in `App.tabs`.
    pub archived_at: Option<u64>,
    /// The project's setup script (cmd+shift+n), typed into the first pane
    /// before the agent launches. The workspace owns its copy, so editing or
    /// removing the project never touches a live tab. None for cmd+n/cmd+t.
    pub setup: Option<String>,
    /// The project's subfolder (cmd+shift+n, monorepo projects): the panes
    /// cd here - inside the worktree - before the setup script runs. Owned
    /// like `setup`. None for cmd+n/cmd+t.
    pub subdir: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

impl Workspace {
    /// A bare shell workspace (cmd+t / a new plain tab): no prompt, worktree,
    /// or agent - just a folder and a codename.
    pub fn bare(root: Option<PathBuf>) -> Self {
        Self {
            root,
            title: random_title(),
            description: None,
            prompt: String::new(),
            worktree: None,
            agent: None,
            model: None,
            created_at: mesh::now(),
            archived_at: None,
            setup: None,
            subdir: None,
        }
    }

    /// Is this workspace parked in the sidebar's archived pile?
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    pub fn to_state(&self) -> WorkspaceState {
        WorkspaceState {
            root: self.root.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            prompt: self.prompt.clone(),
            worktree: self.worktree.as_ref().map(|w| WorktreeState {
                path: w.path.clone(),
                branch: w.branch.clone(),
            }),
            agent: self.agent.map(str::to_string),
            model: self.model.clone(),
            created_at: self.created_at,
            archived_at: self.archived_at,
            setup: self.setup.clone(),
            subdir: self.subdir.clone(),
        }
    }

    pub fn from_state(s: WorkspaceState) -> Self {
        Self {
            root: s.root,
            title: s.title,
            description: s.description,
            prompt: s.prompt,
            worktree: s.worktree.map(|w| Worktree {
                path: w.path,
                branch: w.branch,
            }),
            // Resolve to a &'static agent id; an unknown id (config changed,
            // agent removed) just drops the association.
            agent: s.agent.as_deref().and_then(agent::by_id).map(|a| a.id),
            model: s.model,
            created_at: s.created_at,
            archived_at: s.archived_at,
            setup: s.setup,
            subdir: s.subdir,
        }
    }
}

/// A saved workspace source (Settings > Projects): the thing cmd+shift+n
/// starts sessions from. Points at a folder on disk (`path`) or a GitHub
/// repo (`repo`) cloned on first use under `~/.muxterm/clones/`, optionally
/// narrowed to a `subdir` (a monorepo app): the worktree is still made for
/// the whole repo, then the panes cd into the subfolder before setup runs.
/// The layout's source of truth is `state::ProjectState`.
#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub name: String,
    /// Local root when set; else the project lives at `local_root()`'s
    /// clone destination.
    pub path: Option<PathBuf>,
    /// GitHub repo ("owner/repo" or a full clone URL); see `clone_url`.
    pub repo: Option<String>,
    /// Bash lines typed into a new workspace's first pane, before the agent
    /// launch (e.g. "direnv allow").
    pub setup: Option<String>,
    /// Repo-relative subfolder the workspace works in.
    pub subdir: Option<String>,
}

impl Project {
    pub fn to_state(&self) -> ProjectState {
        ProjectState {
            name: self.name.clone(),
            path: self.path.clone(),
            repo: self.repo.clone(),
            setup: self.setup.clone(),
            subdir: self.subdir.clone(),
        }
    }

    pub fn from_state(s: ProjectState) -> Self {
        Self {
            name: s.name,
            path: s.path,
            repo: s.repo,
            setup: s.setup,
            subdir: s.subdir,
        }
    }

    /// Where the project lives locally: `path` when set, else the clone
    /// destination under `~/.muxterm/clones/` - named for the *repo*, not
    /// the project, so several projects into one monorepo (different
    /// subdirs) share a single clone.
    pub fn local_root(&self) -> PathBuf {
        match (&self.path, &self.repo) {
            (Some(p), _) => p.clone(),
            (None, Some(repo)) => state::clones_dir().join(clone_dirname(repo)),
            (None, None) => {
                state::clones_dir().join(worktree_dirname(&self.name))
            },
        }
    }

    /// Some(repo) when this is a repo project whose local clone does not
    /// exist yet - the cmd+shift+n submit path then clones (trying
    /// `clone_candidates`) before the worktree checkout.
    pub fn needs_clone(&self) -> Option<String> {
        match (&self.path, &self.repo) {
            (None, Some(repo)) if !self.local_root().exists() => {
                Some(repo.clone())
            },
            _ => None,
        }
    }
}

/// The one `git clone` URL a project's `repo` field means: bare
/// "owner/repo" shorthand expands to the GitHub https form; anything else
/// - full URLs, scp-style remotes, local paths - is used exactly as given.
/// Deliberately no protocol fallbacks: if the URL can't be reached, the
/// popup says so up front (the `ls-remote` preflight) instead of quietly
/// trying variants the user never wrote.
pub fn clone_url(repo: &str) -> String {
    let r = repo.trim();
    if is_github_shorthand(r) {
        return format!(
            "https://github.com/{}.git",
            r.trim_end_matches('/').trim_end_matches(".git")
        );
    }
    r.to_string()
}

/// A repo's directory name under `~/.muxterm/clones/`: the repo value
/// minus scheme/`git@`/`.git`, with path separators flattened to '-'
/// ("https://github.com/a/b" and "a/b" both land on "github.com-a-b" /
/// "a-b" style names, stable across the ways one repo can be written
/// *within* each spelling).
fn clone_dirname(repo: &str) -> String {
    let r = repo.trim().trim_end_matches('/').trim_end_matches(".git");
    let r = r.split("://").last().unwrap_or(r);
    let r = r.strip_prefix("git@").unwrap_or(r);
    r.replace(['/', ':'], "-")
}

/// Bare "owner/repo": exactly one interior slash and no path-ish spelling
/// (mirrors the settings form's location classification - anything that
/// reads as a filesystem path is not shorthand).
fn is_github_shorthand(s: &str) -> bool {
    if s.contains("://")
        || s.starts_with("git@")
        || s.contains(char::is_whitespace)
        || s.starts_with('/')
        || s.starts_with('~')
        || s.starts_with('.')
    {
        return false;
    }
    match s.split_once('/') {
        Some((owner, name)) => {
            !owner.is_empty()
                && !name.trim_end_matches(".git").is_empty()
                && !name.contains('/')
        },
        None => false,
    }
}

/// Resolve typed folder text into a path: trim, expand a leading `~`, treat
/// empty as "no folder". Existence isn't checked here - the git and spawn
/// steps handle a bad path.
pub fn expand_dir(input: &str) -> Option<PathBuf> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            if let Some(home) = dirs::home_dir() {
                return Some(home.join(rest.trim_start_matches('/')));
            }
        }
    }
    Some(PathBuf::from(s))
}

const ADJECTIVES: &[&str] = &[
    "amber", "bold", "brave", "breezy", "bright", "brisk", "calm", "candid",
    "cheery", "civil", "clever", "cosmic", "cozy", "crisp", "daring", "deft",
    "dapper", "eager", "fabled", "fancy", "fleet", "gentle", "gilded", "glad",
    "golden", "hardy", "hazel", "humble", "jaunty", "jolly", "keen", "limber",
    "lively", "lucid", "lunar", "mellow", "merry", "mighty", "nimble", "noble",
    "peppy", "perky", "placid", "plucky", "proud", "quiet", "rapid", "regal",
    "rosy", "rustic", "sage", "sandy", "serene", "sleek", "snappy", "solar",
    "spry", "stout", "sunny", "swift", "tidy", "vivid", "wry", "zesty",
];

const ANIMALS: &[&str] = &[
    "badger", "bat", "bear", "beaver", "bee", "bison", "camel", "cat",
    "cheetah", "crab", "crane", "crow", "deer", "dingo", "dolphin", "dove",
    "eagle", "egret", "falcon", "ferret", "finch", "fox", "gecko", "gibbon",
    "hare", "hawk", "heron", "hound", "ibex", "impala", "jackal", "koala",
    "lemur", "lion", "llama", "lynx", "manatee", "marmot", "mole", "moose",
    "narwhal", "newt", "ocelot", "orca", "osprey", "otter", "owl", "panda",
    "pelican", "pony", "puffin", "quail", "rabbit", "raven", "seal", "shrew",
    "sparrow", "stoat", "swan", "tapir", "toucan", "walrus", "wombat", "wren",
];

/// A random adjective-animal codename ("brisk-otter"): the name of a
/// workspace's git worktree/branch, and a bare cmd+t tab's title. Random
/// rather than derived from the prompt: instant, needs no agent CLI, git-safe,
/// and keeps the task's words out of the branch. Randomness comes from uuid v4
/// bytes (the crate's one existing entropy source; no rand dependency).
pub fn random_title() -> String {
    let b = uuid::Uuid::new_v4().into_bytes();
    let adj = ADJECTIVES[usize::from(b[0]) % ADJECTIVES.len()];
    let animal = ANIMALS[usize::from(b[1]) % ANIMALS.len()];
    format!("{adj}-{animal}")
}

/// Is `root` inside a git work tree? Gates the "create worktree" checkbox and
/// the worktree step. `git` lives in /usr/bin (on PATH even for Finder
/// launches), so a bare Command is enough - unlike `claude`/`codex`.
pub fn is_git_repo(root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).trim() == "true"
        })
        .unwrap_or(false)
}

/// The git work-tree root containing `path`, if any (`git rev-parse
/// --show-toplevel`). Same bare-`git` reasoning as `is_git_repo`.
pub fn repo_toplevel(path: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let top = stdout.trim();
    (!top.is_empty()).then(|| PathBuf::from(top))
}

/// The `origin` remote URL of `path`'s repo, if any (`git remote get-url
/// origin`). Same bare-`git` reasoning as `is_git_repo`.
pub fn origin_url(path: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// The setup script a plain cmd+n worktree inherits from the saved
/// projects, if an unambiguous one exists. A path project matches when its
/// path equals the picked folder; a repo project when its repo and the
/// folder's `origin` URL name the same repo (both funneled through
/// `clone_url` then `clone_dirname`, so "owner/repo", the https URL and
/// the scp-style remote all converge). All matches must carry the
/// identical non-empty setup - disagreement or a setup-less match
/// inherits nothing. Pure: the caller injects the origin URL, so tests
/// never shell git.
pub fn inherited_setup<'a>(
    projects: &'a [Project],
    root: &Path,
    origin: Option<&str>,
) -> Option<&'a str> {
    let repo_key = |spelling: &str| {
        clone_dirname(&clone_url(spelling)).to_ascii_lowercase()
    };
    let origin_key = origin.map(repo_key);
    // The (Some, Some) shape is load-bearing: a repo-less project must not
    // "match" a folder with no origin (None == None).
    let matches = |p: &&Project| {
        p.path.as_deref() == Some(root)
            || match (&p.repo, &origin_key) {
                (Some(repo), Some(key)) => repo_key(repo) == *key,
                _ => false,
            }
    };
    let mut matched = projects.iter().filter(matches);
    let setup = matched.next()?.setup.as_deref()?;
    matched
        .all(|p| p.setup.as_deref() == Some(setup))
        .then_some(setup)
}

/// The workspace-root sync's decision: given where the sidebar says a
/// workspace lives (`homes`: the root, plus the worktree when there is one)
/// and where its panes actually are (`cwds`, focused pane first), is the
/// displayed reference stale? A single pane still under any home - or
/// anywhere in the same git repo, per `toplevel` (from a root that is a
/// repo subfolder, `cd ..` within the repo is not leaving) - pins it and
/// nothing changes. Only when *every* pane has left does this return the
/// new root: the focused pane's repo toplevel, or its bare cwd outside a
/// repo. Pure; the caller injects `toplevel` (the App memoizes
/// `repo_toplevel`), which also keeps the common pinned-by-path case free
/// of git calls.
pub fn retarget(
    homes: &[&Path],
    cwds: &[&Path],
    mut toplevel: impl FnMut(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    if homes.is_empty() || cwds.is_empty() {
        return None;
    }
    if cwds
        .iter()
        .any(|cwd| homes.iter().any(|home| cwd.starts_with(home)))
    {
        return None;
    }
    // No pane sits under a home by path; ask git before concluding - a cwd
    // elsewhere in the same repo still counts as "on it".
    let home_tops: Vec<PathBuf> =
        homes.iter().filter_map(|home| toplevel(home)).collect();
    if cwds
        .iter()
        .any(|cwd| toplevel(cwd).is_some_and(|top| home_tops.contains(&top)))
    {
        return None;
    }
    Some(toplevel(cwds[0]).unwrap_or_else(|| cwds[0].to_path_buf()))
}

/// Where a new tab/workspace opened from this one should start: normally the
/// focused pane's `cwd`, but when that sits inside the tab's own `worktree`,
/// its parent repo (`root`) instead - so new work opens in the real project,
/// not nested in the throwaway checkout. Everything else passes through. No
/// canonicalization: a muxterm worktree path is always machine-generated under
/// the real home, and tmux reports real cwds, so neither side has the
/// `/tmp`-vs-`/private/tmp` divergence `retarget` has to guard against.
pub fn escape_worktree(
    cwd: &Path,
    worktree: Option<&Path>,
    root: Option<&Path>,
) -> PathBuf {
    if let (Some(wt), Some(root)) = (worktree, root) {
        if cwd.starts_with(wt) {
            return root.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

/// Where a workspace's panes should cd once its checkout has landed, if
/// anywhere: the subdir joined under the worktree (else the root). Panes
/// spawn *in* the worktree/root, so without a subdir there is nothing to
/// type. Pure; the caller types the `cd`, so a missing subfolder fails
/// visibly in the shell instead of silently landing elsewhere.
pub fn boot_cd(
    worktree: Option<&Path>,
    root: Option<&Path>,
    subdir: Option<&str>,
) -> Option<PathBuf> {
    let sub = subdir?.trim().trim_matches('/');
    if sub.is_empty() {
        return None;
    }
    worktree.or(root).map(|base| base.join(sub))
}

/// The label shown before the AI title lands - and the fallback if it never
/// does (no agent CLI, offline): the first words of the task prompt, else
/// "workspace".
pub fn placeholder_title(prompt: &str) -> String {
    let p = prompt.trim();
    if p.is_empty() {
        return "workspace".to_string();
    }
    p.split_whitespace().take(6).collect::<Vec<_>>().join(" ")
}

/// One pickable branch in the cmd+n popup's branch typeahead.
#[derive(Clone, Debug, PartialEq)]
pub struct Branch {
    /// Short name with any remote prefix stripped ("feat/x").
    pub name: String,
    /// Some("origin") when the branch exists only on that remote.
    pub remote: Option<String>,
    /// A local branch already checked out somewhere (the main repo checkout
    /// counts, so does any worktree); git refuses a second checkout, so the
    /// popup dims it.
    pub in_use: bool,
}

/// Every branch of `root`'s repo, local and remote, newest commit first.
/// One ref walk, no network (refs/remotes is local state).
pub fn list_branches(root: &Path) -> Vec<Branch> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)%09%(worktreepath)",
            "refs/heads",
            "refs/remotes",
        ])
        .output();
    match out {
        Ok(out) if out.status.success() => {
            parse_branches(&String::from_utf8_lossy(&out.stdout))
        },
        _ => Vec::new(),
    }
}

/// Fold `for-each-ref` output (`refname TAB worktreepath`, newest-first)
/// into the popup's branch list. Full `%(refname)` rather than `:short`:
/// `:short` disambiguates against same-named tags by printing "heads/x",
/// which would leak into the list; stripping the prefix ourselves always
/// yields the plain branch name. Remote entries drop the symbolic HEAD
/// pointer and dedupe against local names (local wins - that is what a
/// checkout would use) and against earlier, newer-tipped remotes. Pure;
/// fixture-tested.
pub fn parse_branches(out: &str) -> Vec<Branch> {
    let locals: Vec<&str> = out
        .lines()
        .filter_map(|l| l.split('\t').next())
        .filter_map(|r| r.strip_prefix("refs/heads/"))
        .collect();
    let mut seen_remote: Vec<&str> = Vec::new();
    let mut branches = Vec::new();
    for line in out.lines() {
        let (refname, worktreepath) = line.split_once('\t').unwrap_or((line, ""));
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            if name.is_empty() {
                continue;
            }
            branches.push(Branch {
                name: name.to_string(),
                remote: None,
                in_use: !worktreepath.is_empty(),
            });
        } else if let Some(rest) = refname.strip_prefix("refs/remotes/") {
            let Some((remote, name)) = rest.split_once('/') else {
                continue;
            };
            if name.is_empty()
                || name == "HEAD"
                || locals.contains(&name)
                || seen_remote.contains(&name)
            {
                continue;
            }
            seen_remote.push(name);
            branches.push(Branch {
                name: name.to_string(),
                remote: Some(remote.to_string()),
                in_use: false,
            });
        }
    }
    branches
}

/// Branch names straight off an un-cloned remote (`git ls-remote --heads`),
/// for the cmd+shift+n typeahead when a repo project has no clone yet - so
/// picking a base branch works the same as against a local repo. This
/// doubles as the *preflight*: None means the remote can't be reached with
/// this URL (offline, bad URL, private repo the machine can't auth to),
/// and the popup refuses to create the workspace, saying why - a clone
/// would only fail later and messier. Network; run via
/// `spawn_remote_branches`, never on the UI thread.
pub fn list_remote_branches(repo: &str) -> Option<Vec<Branch>> {
    let out = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["ls-remote", "--heads"])
        .arg(clone_url(repo))
        .output();
    match out {
        Ok(out) if out.status.success() => {
            Some(parse_ls_remote(&String::from_utf8_lossy(&out.stdout)))
        },
        _ => None,
    }
}

/// Fold `ls-remote --heads` output (`sha TAB refs/heads/name`) into the
/// popup's branch list. Every entry is remote-only ("origin" - what the
/// clone will name it), none in use; alphabetical, as ls-remote sorts (no
/// commit dates without a clone). Pure; fixture-tested.
pub fn parse_ls_remote(out: &str) -> Vec<Branch> {
    out.lines()
        .filter_map(|l| l.split('\t').nth(1))
        .filter_map(|r| r.strip_prefix("refs/heads/"))
        .filter(|name| !name.is_empty())
        .map(|name| Branch {
            name: name.to_string(),
            remote: Some("origin".to_string()),
            in_use: false,
        })
        .collect()
}

/// `list_remote_branches` off-thread, streamed back to the popup's poll.
/// Same channel + repaint wiring as spawn_worktree.
pub fn spawn_remote_branches(
    repo: String,
    tx: Sender<Option<Vec<Branch>>>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let _ = tx.send(list_remote_branches(&repo));
        ctx.request_repaint();
    });
}

/// What the popup's branch field means, decided against the enumerated
/// branch list (`resolve_branch`).
#[derive(Clone, Debug, PartialEq)]
pub enum BranchChoice {
    /// Empty field: a fresh branch named by a random codename (may suffix).
    Codename,
    /// An existing local branch: check it out, no `-b`.
    Existing(String),
    /// A remote-only branch: create a local branch tracking it.
    Track { name: String, remote: String },
    /// A name that exists nowhere: a fresh branch with that name.
    New(String),
}

/// Classify the branch field's text. Exact-match only - a substring that
/// matches nothing exactly is a `New` branch request (the popup's caption
/// row spells out which of these will happen before submit). An in-use
/// local branch still resolves `Existing`: git refuses the checkout and the
/// App's failed-worktree fallback handles it, rather than silently doing
/// something else with the name the user typed.
pub fn resolve_branch(input: &str, branches: &[Branch]) -> BranchChoice {
    let text = input.trim();
    if text.is_empty() {
        return BranchChoice::Codename;
    }
    for b in branches {
        match &b.remote {
            None if b.name == text => {
                return BranchChoice::Existing(b.name.clone());
            },
            // Users type remote branches both bare and qualified.
            Some(remote)
                if b.name == text
                    || format!("{remote}/{}", b.name) == text =>
            {
                return BranchChoice::Track {
                    name: b.name.clone(),
                    remote: remote.clone(),
                };
            },
            _ => {},
        }
    }
    BranchChoice::New(text.to_string())
}

/// Directory name for a worktree of `branch`: '/' - the one character valid
/// in a ref name but not a path segment - becomes '-' ("feat/x" ->
/// "feat-x"). Anything else passes through (a leading-dot branch yields a
/// hidden dir; accepted quirk).
fn worktree_dirname(branch: &str) -> String {
    branch.replace('/', "-")
}

/// Reserve a worktree for `root` under `~/.muxterm/worktrees/`: pick a free
/// directory name and create the - still empty - directory. Synchronous and
/// cheap, so the pane can open directly inside it and never has to `cd`;
/// `spawn_worktree` then runs the slow checkout into it off the UI thread
/// (`git worktree add` accepts an existing empty directory). The mkdir *is*
/// the claim: it atomically settles racing name picks.
///
/// The directory is named for the branch: a random codename for `Codename`
/// (where a collision suffixes branch and dir together, gated by
/// `branch_exists`), the sanitized branch name otherwise (where only the
/// *directory* may suffix - a user-chosen branch name is never quietly
/// renamed; a stale-list race just makes `git worktree add` fail into the
/// App's existing recovery path).
pub fn claim_worktree(root: &Path, choice: &BranchChoice) -> anyhow::Result<Worktree> {
    let fixed: Option<&str> = match choice {
        BranchChoice::Codename => None,
        BranchChoice::Existing(n) | BranchChoice::New(n) => Some(n),
        BranchChoice::Track { name, .. } => Some(name),
    };
    let base = fixed.map(worktree_dirname).unwrap_or_else(random_title);
    let dir = state::worktrees_dir();
    fs::create_dir_all(&dir)?;

    for attempt in 0..100 {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        if fixed.is_none() && branch_exists(root, &name) {
            continue;
        }
        let path = dir.join(&name);
        match fs::create_dir(&path) {
            Ok(()) => {
                return Ok(Worktree {
                    path,
                    branch: fixed.unwrap_or(&name).to_string(),
                })
            },
            // A taken directory loses to the next suffix.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("no free worktree name for {base}")
}

/// How long a background fetch may hold up a checkout before it is killed
/// and the checkout proceeds on local state.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// One targeted `git fetch`, capped at FETCH_TIMEOUT so a hung remote
/// degrades to "proceed on local state". GIT_TERMINAL_PROMPT=0 keeps a
/// credential prompt from silently eating the whole budget.
fn fetch(root: &Path, remote: &str, refspec: &str) -> bool {
    let mut cmd = Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(root)
        .args(["fetch", remote, refspec]);
    agent::output_with_timeout(&mut cmd, FETCH_TIMEOUT)
        .ok()
        .flatten()
        .is_some_and(|o| o.status.success())
}

/// The repo's checked-out branch name, if HEAD is on one.
fn head_branch(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty() && name != "HEAD").then_some(name)
}

/// Bring the base of a project-mode worktree up to date with its remote
/// before the checkout, so "continue branch x" continues x's *pushed* work.
/// Fetches only what the choice consumes, each under FETCH_TIMEOUT -
/// best-effort throughout (offline, no remote, a hung ssh, a diverged
/// branch: every step may fail or be killed and the checkout proceeds on
/// local state). `progress` narrates the fetch when one actually happens.
///
/// - `Existing`: `fetch name:name` fast-forwards the local ref in place
///   without touching any working tree (a checked-out or diverged branch
///   just refuses).
/// - `Track`: a bare `fetch <remote> <name>` refreshes the remote-tracking
///   ref `--track -b` will base on.
/// - `Codename`/`New`: only inside a muxterm-owned clone (always clean,
///   nobody works in it), fetch the clone's checked-out branch and
///   fast-forward it so new branches base on the fresh default; a user
///   repo's HEAD is never moved - and never fetched, nothing would consume
///   the refs.
fn refresh_base(
    root: &Path,
    choice: &BranchChoice,
    progress: impl Fn(String),
) {
    match choice {
        BranchChoice::Existing(name) => {
            progress(format!("fetching origin/{name}…"));
            let _ = fetch(root, "origin", &format!("{name}:{name}"));
        },
        BranchChoice::Track { name, remote } => {
            progress(format!("fetching {remote}/{name}…"));
            let _ = fetch(root, remote, name);
        },
        BranchChoice::Codename | BranchChoice::New(_)
            if root.starts_with(state::clones_dir()) =>
        {
            let Some(branch) = head_branch(root) else {
                return;
            };
            progress(format!("fetching origin/{branch}…"));
            if fetch(root, "origin", &branch) {
                let _ = Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(["merge", "--ff-only", "@{u}"])
                    .output();
            }
        },
        _ => {},
    }
}

/// The slow half of `claim_worktree`: check `root` out into the claimed
/// directory, on the claimed branch - created (`-b`), plain-checked-out, or
/// created tracking its remote, per the popup's choice.
fn populate_worktree(
    root: &Path,
    wt: &Worktree,
    choice: &BranchChoice,
) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(["worktree", "add"]);
    match choice {
        BranchChoice::Codename | BranchChoice::New(_) => {
            cmd.arg(&wt.path).args(["-b", &wt.branch]);
        },
        BranchChoice::Existing(name) => {
            cmd.arg(&wt.path).arg(name);
        },
        BranchChoice::Track { name, remote } => {
            cmd.args(["--track", "-b", name])
                .arg(&wt.path)
                .arg(format!("{remote}/{name}"));
        },
    }
    let out = cmd.output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git worktree add failed: {}", err.trim());
    }
    Ok(())
}

/// One message from a worktree worker thread, keyed by tab id: any number
/// of Progress lines (what the thread is doing right now, floated over the
/// pending tab's agent pane by the App), then exactly one Done.
pub enum WorktreeMsg {
    Progress(String),
    Done(Result<Worktree, String>),
}

/// Run `populate_worktree` off the UI thread - the checkout can be slow on a
/// large or lfs-heavy repo - and stream progress + the result back to the
/// App by tab id (WorktreeMsg). Same channel + repaint wiring as the
/// pr_status/git_status pollers.
/// `fresh_base` (project mode) updates the base branch from its remote first
/// - network latency lands on this thread, never the UI. A failed checkout
/// leaves the claimed directory alone (the workspace's shells are booting
/// inside it; deleting a live shell's cwd showers it with getcwd errors) -
/// the App's walk-back cd's them out, and `sweep_stale_claims` reclaims the
/// empty directory at the next launch.
pub fn spawn_worktree(
    tab_id: String,
    root: PathBuf,
    worktree: Worktree,
    choice: BranchChoice,
    fresh_base: bool,
    tx: Sender<(String, WorktreeMsg)>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let progress = |line: String| {
            let _ = tx.send((tab_id.clone(), WorktreeMsg::Progress(line)));
            ctx.request_repaint();
        };
        if fresh_base {
            refresh_base(&root, &choice, &progress);
        }
        progress("checking out worktree…".into());
        let res = match populate_worktree(&root, &worktree, &choice) {
            Ok(()) => Ok(worktree),
            Err(e) => Err(format!("{e:#}")),
        };
        let _ = tx.send((tab_id, WorktreeMsg::Done(res)));
        ctx.request_repaint();
    });
}

/// `spawn_worktree`'s sibling for a repo project with no local clone yet:
/// clone first, then resolve the typed branch text against the *fresh*
/// clone's branches, then populate. The popup's `ls-remote` preflight
/// already proved the URL reachable before any of this ran, so a failure
/// here is the rare kind (disk, races), not a bad URL. Same channel
/// protocol. On failure the claimed directory is deliberately left in
/// place: the workspace's shells are booting inside it, and deleting a
/// live shell's cwd showers it with getcwd errors - the App's walk-back
/// cd's them out instead, and stray empty claims are swept at the next
/// launch.
pub fn spawn_clone_worktree(
    tab_id: String,
    repo: String,
    root: PathBuf,
    worktree: Worktree,
    branch_input: String,
    tx: Sender<(String, WorktreeMsg)>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let progress = |line: String| {
            let _ = tx.send((tab_id.clone(), WorktreeMsg::Progress(line)));
            ctx.request_repaint();
        };
        progress(format!("cloning {repo}…"));
        let res = clone_and_populate(
            &repo,
            &root,
            worktree,
            &branch_input,
            &progress,
        )
        .map_err(|(_, e)| format!("{e:#}"));
        let _ = tx.send((tab_id, WorktreeMsg::Done(res)));
        ctx.request_repaint();
    });
}

/// The clone chain's body: `git clone` into the project's local root, then
/// re-resolve the branch text and check out the worktree. Errors carry the
/// claimed worktree back so callers can still name the directory. The
/// clone runs with GIT_TERMINAL_PROMPT=0 - a private repo without ambient
/// credentials fails fast instead of hanging a headless prompt.
fn clone_and_populate(
    repo: &str,
    root: &Path,
    worktree: Worktree,
    branch_input: &str,
    progress: impl Fn(String),
) -> Result<Worktree, (Worktree, anyhow::Error)> {
    if let Some(parent) = root.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let url = clone_url(repo);
    let out = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("clone")
        .arg(&url)
        .arg(root)
        .output();
    match out {
        Ok(o) if o.status.success() => {},
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            return Err((
                worktree,
                anyhow::anyhow!("git clone failed: {url}: {}", err.trim()),
            ));
        },
        Err(e) => return Err((worktree, e.into())),
    }
    // The popup resolved against an empty branch list; now the branches
    // exist. A typed name may turn out Existing/Track - repoint the claimed
    // branch at the real resolution (the *directory* keeps its claimed name;
    // dirs never rename branches).
    let choice = resolve_branch(branch_input, &list_branches(root));
    let mut wt = worktree;
    wt.branch = match &choice {
        BranchChoice::Codename => wt.branch,
        BranchChoice::Existing(n) | BranchChoice::New(n) => n.clone(),
        BranchChoice::Track { name, .. } => name.clone(),
    };
    progress("checking out worktree…".into());
    match populate_worktree(root, &wt, &choice) {
        Ok(()) => Ok(wt),
        Err(e) => Err((wt, e)),
    }
}

/// Launch-time reclamation of stray worktree *claims*: empty directories
/// under `~/.muxterm/worktrees/` that no workspace references and no live
/// pane sits in. Failed checkouts leave their claimed dir behind rather
/// than deleting it under a booting shell (see spawn_worktree); this is
/// where those come back. `remove_dir` refuses non-empty directories, so a
/// real checkout can never be lost here; the pane-cwd guard keeps a claim
/// alive when a relaunch interrupted its checkout and the restored panes
/// still sit inside.
pub fn sweep_stale_claims(referenced: &[&Path], pane_cwds: &[&Path]) {
    let Ok(entries) = fs::read_dir(state::worktrees_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if referenced.contains(&p.as_path())
            || pane_cwds.iter().any(|c| c.starts_with(&p))
        {
            continue;
        }
        if fs::remove_dir(&p).is_ok() {
            log::info!("swept stale worktree claim {}", p.display());
        }
    }
}

fn branch_exists(root: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

const TITLE_INSTRUCTION: &str =
    "Summarize this coding task as a short, descriptive title of 2 to 5 words \
     capturing the intent. Reply with only the title: no quotes, no trailing \
     punctuation.";

/// Kick off a background one-shot small-model call that turns the task prompt
/// into a short title, streamed back to the App keyed by tab id. Mirrors the
/// pr_status/git_status poller wiring (an mpsc Sender plus an egui::Context to
/// wake the UI). Best-effort: on any failure the workspace keeps its
/// prompt-derived placeholder title.
pub fn spawn_title(
    tab_id: String,
    prompt: String,
    agent: &'static Agent,
    tx: Sender<(String, String)>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let started = std::time::Instant::now();
        let title =
            generate(agent, TITLE_INSTRUCTION, &format!("Task: {prompt}"));
        log::info!(
            "title one-shot ({}) took {:.1}s (ok={})",
            agent.id,
            started.elapsed().as_secs_f32(),
            title.is_some()
        );
        if let Some(title) = title {
            if tx.send((tab_id, title)).is_ok() {
                ctx.request_repaint();
            }
        }
    });
}

fn generate(agent: &Agent, instruction: &str, body: &str) -> Option<String> {
    let full = format!("{instruction}\n\n{body}");
    // Exec-style CLIs stream their own progress; the final assistant line
    // is last, which `clean_title` picks up.
    let cmdline = agent::oneshot_command(agent, &full);
    // Through the interactive login shell so brew/npm PATH entries resolve the
    // agent binary exactly as a pane's shell would (see agent::binary_available).
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let out = Command::new(shell).args(["-ilc", &cmdline]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let title = clean_title(&String::from_utf8_lossy(&out.stdout));
    (!title.is_empty()).then_some(title)
}

/// Reduce a model reply to one tidy title line: the last non-empty line
/// (codex prints its answer last; claude prints only the answer), unquoted and
/// clipped to a few words.
fn clean_title(raw: &str) -> String {
    let line = raw
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`');
    line.split_whitespace().take(8).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_title_from_prompt() {
        assert_eq!(
            placeholder_title("add a settings sidebar to the app now"),
            "add a settings sidebar to the"
        );
        assert_eq!(placeholder_title("  "), "workspace");
        assert_eq!(placeholder_title(""), "workspace");
    }

    #[test]
    fn clean_title_takes_last_line_unquoted() {
        assert_eq!(clean_title("\"Fix the flaky test\""), "Fix the flaky test");
        assert_eq!(
            clean_title("thinking...\nrunning\nAdd auth to API"),
            "Add auth to API"
        );
        assert_eq!(clean_title("   \n  \n"), "");
    }

    #[test]
    fn random_title_is_adjective_animal() {
        // A byte mod 64 is unbiased only while the lists stay 64 long.
        assert_eq!(ADJECTIVES.len(), 64);
        assert_eq!(ANIMALS.len(), 64);
        for _ in 0..20 {
            let t = random_title();
            let (adj, animal) = t.split_once('-').expect("adj-animal shape");
            assert!(ADJECTIVES.contains(&adj), "unknown adjective in {t}");
            assert!(ANIMALS.contains(&animal), "unknown animal in {t}");
        }
    }

    #[test]
    fn retarget_pinned_by_any_pane_under_a_home() {
        // The second pane never left the root: no repo lookup, no move.
        let homes = [Path::new("/repo")];
        let cwds = [Path::new("/elsewhere"), Path::new("/repo/src/deep")];
        let out = retarget(&homes, &cwds, |_| panic!("path pin needs no git"));
        assert_eq!(out, None);
    }

    #[test]
    fn retarget_pinned_by_worktree_home() {
        let homes =
            [Path::new("/repo"), Path::new("/home/u/.muxterm/worktrees/x")];
        let cwds = [Path::new("/home/u/.muxterm/worktrees/x/src")];
        assert_eq!(retarget(&homes, &cwds, |_: &Path| None), None);
    }

    #[test]
    fn retarget_pinned_by_same_repo() {
        // The root is a subfolder; a pane at the repo top left the folder
        // but not the repo.
        let homes = [Path::new("/repo/crates/sub")];
        let cwds = [Path::new("/repo")];
        let top = |p: &Path| {
            p.starts_with("/repo").then(|| PathBuf::from("/repo"))
        };
        assert_eq!(retarget(&homes, &cwds, top), None);
    }

    #[test]
    fn retarget_follows_the_focused_pane_to_its_repo() {
        // Every pane left /old; the new root is the *focused* (first) cwd's
        // repo toplevel, not the raw subfolder it happens to sit in.
        let homes = [Path::new("/old")];
        let cwds = [Path::new("/new/app/src"), Path::new("/somewhere")];
        let top = |p: &Path| {
            p.starts_with("/new/app").then(|| PathBuf::from("/new/app"))
        };
        assert_eq!(retarget(&homes, &cwds, top), Some(PathBuf::from("/new/app")));
    }

    #[test]
    fn retarget_outside_any_repo_adopts_the_cwd() {
        let homes = [Path::new("/old")];
        let cwds = [Path::new("/home/u/notes")];
        assert_eq!(
            retarget(&homes, &cwds, |_: &Path| None),
            Some(PathBuf::from("/home/u/notes"))
        );
    }

    #[test]
    fn retarget_needs_homes_and_cwds() {
        assert_eq!(retarget(&[], &[Path::new("/x")], |_: &Path| None), None);
        assert_eq!(retarget(&[Path::new("/x")], &[], |_: &Path| None), None);
    }

    #[test]
    fn escape_worktree_redirects_only_from_inside_the_worktree() {
        let wt = Path::new("/home/u/.muxterm/worktrees/brisk-otter");
        let root = Path::new("/home/u/dev/proj");
        // Inside the worktree -> parent repo.
        assert_eq!(escape_worktree(&wt.join("src"), Some(wt), Some(root)), root);
        // Outside it -> unchanged.
        let other = Path::new("/tmp/scratch");
        assert_eq!(escape_worktree(other, Some(wt), Some(root)), other);
        // No worktree link on the tab -> unchanged.
        assert_eq!(escape_worktree(other, None, Some(root)), other);
    }

    #[test]
    fn bare_workspace_gets_a_codename() {
        let ws = Workspace::bare(Some(PathBuf::from("/home/u/thing")));
        assert!(ws.title.contains('-'));
        assert!(ws.worktree.is_none());
        assert!(ws.agent.is_none());
    }

    #[test]
    fn state_round_trip_resolves_agent() {
        let ws = Workspace {
            root: Some(PathBuf::from("/p")),
            title: "t".into(),
            description: Some("d".into()),
            prompt: "do it".into(),
            worktree: Some(Worktree {
                path: PathBuf::from("/w"),
                branch: "b".into(),
            }),
            agent: Some("claude"),
            model: Some("sonnet".into()),
            created_at: 7,
            archived_at: Some(9),
            setup: Some("direnv allow".into()),
            subdir: Some("apps/web".into()),
        };
        let back = Workspace::from_state(ws.to_state());
        assert_eq!(back.agent, Some("claude"));
        assert_eq!(back.worktree.unwrap().branch, "b");
        assert_eq!(back.archived_at, Some(9));
        assert_eq!(back.setup.as_deref(), Some("direnv allow"));
        assert_eq!(back.subdir.as_deref(), Some("apps/web"));
        // An unknown agent id resolves to None rather than panicking.
        let mut st = ws.to_state();
        st.agent = Some("nope".into());
        assert_eq!(Workspace::from_state(st).agent, None);
    }

    fn local(name: &str, in_use: bool) -> Branch {
        Branch { name: name.into(), remote: None, in_use }
    }

    fn remote(name: &str, remote: &str) -> Branch {
        Branch {
            name: name.into(),
            remote: Some(remote.into()),
            in_use: false,
        }
    }

    #[test]
    fn parse_branches_fixture() {
        // Newest-first input order is preserved; worktreepath (main repo or
        // a linked worktree) marks in_use; origin/HEAD is dropped; remote
        // prefixes strip even on slashed branch names; a local name shadows
        // its remote; remote-vs-remote dedupe keeps the earlier (newer) line.
        let out = "\
refs/heads/feat/x\t/Users/u/.muxterm/worktrees/feat-x
refs/remotes/origin/HEAD\t
refs/heads/main\t/Users/u/dev/proj
refs/remotes/origin/main\t
refs/remotes/origin/review/deep/name\t
refs/remotes/upstream/shared\t
refs/remotes/origin/shared\t
refs/heads/quiet\t
";
        let bs = parse_branches(out);
        assert_eq!(
            bs,
            vec![
                local("feat/x", true),
                local("main", true),
                remote("review/deep/name", "origin"),
                remote("shared", "upstream"),
                local("quiet", false),
            ]
        );
        assert!(parse_branches("").is_empty());
    }

    #[test]
    fn resolve_branch_classifies() {
        let bs = vec![
            local("main", true),
            local("feature", false),
            remote("review/x", "origin"),
        ];
        assert_eq!(resolve_branch("  ", &bs), BranchChoice::Codename);
        assert_eq!(
            resolve_branch("feature", &bs),
            BranchChoice::Existing("feature".into())
        );
        // In-use locals still resolve Existing: git refuses the checkout and
        // the App's fallback handles it - the name is never repurposed.
        assert_eq!(
            resolve_branch("main", &bs),
            BranchChoice::Existing("main".into())
        );
        // Remote-only branches match bare and remote-qualified.
        let track = BranchChoice::Track {
            name: "review/x".into(),
            remote: "origin".into(),
        };
        assert_eq!(resolve_branch("review/x", &bs), track);
        assert_eq!(resolve_branch("origin/review/x", &bs), track);
        // A substring that matches nothing exactly is a new-branch request.
        assert_eq!(
            resolve_branch("feat", &bs),
            BranchChoice::New("feat".into())
        );
    }

    #[test]
    fn worktree_dirname_sanitizes_slashes() {
        assert_eq!(worktree_dirname("main"), "main");
        assert_eq!(worktree_dirname("feat/x"), "feat-x");
        assert_eq!(worktree_dirname("a/b/c"), "a-b-c");
    }

    #[test]
    fn parse_ls_remote_fixture() {
        let out = "\
9f3c0a refs/heads/main
aa11bb refs/heads/feat/x
ccddee refs/tags/v1.0
ffeedd refs/heads/
";
        // ls-remote separates with tabs; the fixture's spaces stand in.
        let out = out.replace(' ', "\t");
        let bs = parse_ls_remote(&out);
        assert_eq!(
            bs.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["main", "feat/x"],
            "heads only, empty names dropped"
        );
        assert!(bs.iter().all(|b| b.remote.as_deref() == Some("origin")));
        assert!(bs.iter().all(|b| !b.in_use));
        assert!(parse_ls_remote("").is_empty());
    }

    #[test]
    fn clone_url_is_verbatim_except_github_shorthand() {
        // The one expansion: bare owner/repo shorthand.
        assert_eq!(
            clone_url(" herval/dotfiles.git "),
            "https://github.com/herval/dotfiles.git"
        );
        // Everything the user spells out is used exactly as given - no
        // protocol guessing; an unreachable URL fails the preflight loudly.
        for verbatim in [
            "https://github.com/Telepatia-AI/monobloco",
            "git@github.com:a/b.git",
            "git://host.xz/path/to/repo.git",
            "ssh://git@host.xz:2222/repo.git",
            "https://gitlab.com/a/b.git",
            "/tmp/scratch/origin",
            "~/dev/repo",
            "dev/foo/bar", // deeper slashes are a path, not shorthand
        ] {
            assert_eq!(clone_url(verbatim), verbatim);
        }
    }

    #[test]
    fn expand_dir_handles_tilde_and_empty() {
        assert_eq!(expand_dir("  "), None);
        assert_eq!(expand_dir("/tmp/x"), Some(PathBuf::from("/tmp/x")));
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_dir("~"), Some(home.clone()));
        assert_eq!(expand_dir("~/dev"), Some(home.join("dev")));
        // A ~user path is not expanded, just taken literally.
        assert_eq!(expand_dir("~other"), Some(PathBuf::from("~other")));
    }

    #[test]
    fn project_local_root_and_needs_clone() {
        let local = Project {
            name: "muxterm".into(),
            path: Some("/tmp/muxterm".into()),
            repo: None,
            setup: None,
            subdir: None,
        };
        assert_eq!(local.local_root(), PathBuf::from("/tmp/muxterm"));
        assert_eq!(local.needs_clone(), None);

        // A uuid repo guarantees the clone dest doesn't exist on this
        // machine, whatever ~/.muxterm/clones already holds.
        let repo_val = format!("herval/dot-{}", uuid::Uuid::new_v4());
        let repo = Project {
            name: "dots/nvim".into(),
            path: None,
            repo: Some(repo_val.clone()),
            setup: None,
            subdir: Some("nvim".into()),
        };
        // Clone dest under ~/.muxterm/clones, keyed by the *repo* - two
        // subdir projects into one monorepo share the clone.
        assert_eq!(
            repo.local_root(),
            state::clones_dir().join(repo_val.replace('/', "-"))
        );
        let sibling = Project {
            name: "dots/zsh".into(),
            subdir: Some("zsh".into()),
            ..repo.clone()
        };
        assert_eq!(repo.local_root(), sibling.local_root());
        // An un-cloned repo project asks for a clone, by its raw repo
        // value (`clone_url` decides what git is given).
        assert_eq!(repo.needs_clone().as_deref(), Some(repo_val.as_str()));
    }

    #[test]
    fn clone_dirname_flattens_all_spellings() {
        assert_eq!(clone_dirname("herval/dotfiles"), "herval-dotfiles");
        assert_eq!(
            clone_dirname("https://github.com/Telepatia-AI/monobloco"),
            "github.com-Telepatia-AI-monobloco"
        );
        assert_eq!(
            clone_dirname("git@github.com:a/b.git"),
            "github.com-a-b"
        );
    }

    fn setup_project(
        name: &str,
        path: Option<&str>,
        repo: Option<&str>,
        setup: Option<&str>,
    ) -> Project {
        Project {
            name: name.into(),
            path: path.map(PathBuf::from),
            repo: repo.map(str::to_string),
            setup: setup.map(str::to_string),
            subdir: None,
        }
    }

    #[test]
    fn inherited_setup_matches_by_path() {
        let projects =
            [setup_project("dots", Some("/tmp/dots"), None, Some("mise up"))];
        assert_eq!(
            inherited_setup(&projects, Path::new("/tmp/dots"), None),
            Some("mise up")
        );
        assert_eq!(
            inherited_setup(&projects, Path::new("/tmp/other"), None),
            None
        );
    }

    #[test]
    fn inherited_setup_matches_repo_by_origin_spellings() {
        let projects = [setup_project(
            "mono",
            None,
            Some("Telepatia-AI/monobloco"),
            Some("direnv allow"),
        )];
        // Shorthand project vs the scp-style remote the checkout actually
        // has: both funnel through clone_url + clone_dirname.
        assert_eq!(
            inherited_setup(
                &projects,
                Path::new("/tmp/mono"),
                Some("git@github.com:Telepatia-AI/monobloco.git"),
            ),
            Some("direnv allow")
        );
        assert_eq!(
            inherited_setup(
                &projects,
                Path::new("/tmp/mono"),
                Some("https://github.com/telepatia-ai/monobloco"),
            ),
            Some("direnv allow"),
            "https spelling and case both flatten away"
        );
        assert_eq!(
            inherited_setup(
                &projects,
                Path::new("/tmp/mono"),
                Some("git@github.com:other/repo.git"),
            ),
            None
        );
        // No origin at all must not match a repo project (the None == None
        // trap).
        assert_eq!(
            inherited_setup(&projects, Path::new("/tmp/mono"), None),
            None
        );
    }

    #[test]
    fn inherited_setup_requires_unanimity() {
        let repo = Some("a/mono");
        let origin = Some("git@github.com:a/mono.git");
        // Several projects into one repo (monorepo subdirs), same setup:
        // unanimous, inherits.
        let same = [
            setup_project("mono", None, repo, Some("direnv allow")),
            setup_project("mono/web", None, repo, Some("direnv allow")),
        ];
        assert_eq!(
            inherited_setup(&same, Path::new("/x"), origin),
            Some("direnv allow")
        );
        // Disagreeing setups: ambiguous, inherits nothing.
        let differ = [
            setup_project("mono", None, repo, Some("direnv allow")),
            setup_project("mono/web", None, repo, Some("npm install")),
        ];
        assert_eq!(inherited_setup(&differ, Path::new("/x"), origin), None);
        // A setup-less match: nothing to inherit.
        let none = [setup_project("mono", None, repo, None)];
        assert_eq!(inherited_setup(&none, Path::new("/x"), origin), None);
    }

    #[test]
    fn origin_url_reads_the_remote() {
        let (scratch, git) = scratch_git("origintest");
        let repo = scratch.join("repo");
        seed_repo(&git, &repo);
        assert_eq!(origin_url(&repo), None, "no remote yet");
        git(
            &repo,
            &["remote", "add", "origin", "git@github.com:a/b.git"],
        );
        assert_eq!(
            origin_url(&repo).as_deref(),
            Some("git@github.com:a/b.git"),
            "the verbatim URL, no normalization here"
        );
        fs::remove_dir_all(&scratch).unwrap();
    }

    #[test]
    fn boot_cd_joins_subdir_under_worktree_then_root() {
        let wt = Path::new("/wt");
        let root = Path::new("/root");
        assert_eq!(
            boot_cd(Some(wt), Some(root), Some("apps/web")),
            Some(PathBuf::from("/wt/apps/web"))
        );
        assert_eq!(
            boot_cd(None, Some(root), Some("/apps/web/")),
            Some(PathBuf::from("/root/apps/web")),
            "stray slashes trimmed so the join stays relative"
        );
        // Nothing to type without a subdir (panes spawn in place) or
        // without anywhere to join it under.
        assert_eq!(boot_cd(Some(wt), Some(root), None), None);
        assert_eq!(boot_cd(Some(wt), Some(root), Some("  ")), None);
        assert_eq!(boot_cd(None, None, Some("x")), None);
    }

    /// Real-git round trip for every BranchChoice variant. Git is a hard
    /// runtime dependency of the app, the repo is scratch-built in the temp
    /// dir (no network - the "remote" branch is a fabricated ref), and the
    /// whole thing is sub-second. `claim_worktree` itself is NOT called: it
    /// writes to the real `~/.muxterm`; `Worktree` values are hand-built in
    /// the temp dir instead.
    #[test]
    fn populate_worktree_variants() {
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let scratch = std::env::temp_dir()
            .join(format!("muxterm-brtest-{}", uuid::Uuid::new_v4()));
        let repo = scratch.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["-c", "init.defaultBranch=main", "init"]);
        git(
            &repo,
            &[
                "-c", "user.email=t@t", "-c", "user.name=t",
                "commit", "--allow-empty", "-m", "root",
            ],
        );
        git(&repo, &["branch", "feat/x"]);
        // A remote-only branch without a network: fabricate the remote ref
        // and its remote config (--track reads remote.<name>.fetch).
        git(&repo, &["remote", "add", "origin", "file:///dev/null"]);
        git(&repo, &["update-ref", "refs/remotes/origin/remote-only", "HEAD"]);

        let listed = list_branches(&repo);
        let names: Vec<&str> =
            listed.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"feat/x"));
        assert!(names.contains(&"remote-only"));
        let main = listed.iter().find(|b| b.name == "main").unwrap();
        assert!(main.in_use, "the main checkout counts as in use");
        let ro = listed.iter().find(|b| b.name == "remote-only").unwrap();
        assert_eq!(ro.remote.as_deref(), Some("origin"));

        let claim = |name: &str| {
            let path = scratch.join(name);
            fs::create_dir(&path).unwrap();
            path
        };

        // Existing: checks out feat/x itself, no new branch.
        let wt = Worktree { path: claim("wt-existing"), branch: "feat/x".into() };
        populate_worktree(&repo, &wt, &BranchChoice::Existing("feat/x".into()))
            .unwrap();
        assert_eq!(git(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]), "feat/x");

        // Track: creates a local branch tracking the fabricated remote ref.
        let wt = Worktree { path: claim("wt-track"), branch: "remote-only".into() };
        populate_worktree(
            &repo,
            &wt,
            &BranchChoice::Track {
                name: "remote-only".into(),
                remote: "origin".into(),
            },
        )
        .unwrap();
        assert_eq!(
            git(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "remote-only"
        );
        assert_eq!(
            git(&repo, &["config", "branch.remote-only.remote"]),
            "origin"
        );

        // New: -b with a user-chosen name.
        let wt = Worktree { path: claim("wt-new"), branch: "my-idea".into() };
        populate_worktree(&repo, &wt, &BranchChoice::New("my-idea".into()))
            .unwrap();
        assert_eq!(git(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]), "my-idea");

        // A branch checked out elsewhere (main, in the repo itself) refuses.
        let path = claim("wt-refused");
        let wt = Worktree { path: path.clone(), branch: "main".into() };
        let err = populate_worktree(
            &repo,
            &wt,
            &BranchChoice::Existing("main".into()),
        );
        assert!(err.is_err(), "second checkout of main must fail");

        fs::remove_dir_all(&scratch).unwrap();
    }

    fn scratch_git(
        name: &str,
    ) -> (PathBuf, impl Fn(&Path, &[&str]) -> String) {
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let scratch = std::env::temp_dir()
            .join(format!("muxterm-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&scratch).unwrap();
        (scratch, git)
    }

    fn seed_repo(git: &impl Fn(&Path, &[&str]) -> String, repo: &Path) {
        fs::create_dir_all(repo).unwrap();
        git(repo, &["-c", "init.defaultBranch=main", "init"]);
        git(
            repo,
            &[
                "-c", "user.email=t@t", "-c", "user.name=t",
                "commit", "--allow-empty", "-m", "root",
            ],
        );
    }

    /// Real-git round trip of the cmd+shift+n clone chain: a local-path
    /// "URL" keeps it offline. The typed branch text resolves against the
    /// *clone's* branches (the popup had none to offer) - a name that turns
    /// out to exist on the origin becomes a tracking checkout, not a `-b`.
    #[test]
    fn clone_and_populate_resolves_after_clone() {
        let (scratch, git) = scratch_git("clonetest");
        let origin = scratch.join("origin");
        seed_repo(&git, &origin);
        git(&origin, &["branch", "feat/x"]);

        // Typed "feat/x" pre-clone: in the fresh clone it's a remote-only
        // branch (only main comes out local), so it resolves Track.
        let root = scratch.join("clone");
        let claim = scratch.join("wt-feat");
        fs::create_dir(&claim).unwrap();
        let wt = Worktree { path: claim.clone(), branch: "feat/x".into() };
        let wt = clone_and_populate(
            origin.to_str().unwrap(),
            &root,
            wt,
            "feat/x",
            |_| {},
        )
        .expect("clone + checkout");
        assert_eq!(wt.branch, "feat/x");
        assert_eq!(
            git(&claim, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "feat/x"
        );
        assert!(root.join(".git").exists(), "clone landed");

        // A bad URL: the error carries the claimed dir, which survives
        // (shells may be booting inside; the launch sweep reclaims it).
        let root2 = scratch.join("clone2");
        let claim2 = scratch.join("wt-bad");
        fs::create_dir(&claim2).unwrap();
        let wt = Worktree { path: claim2.clone(), branch: "x".into() };
        let (back, _) = clone_and_populate(
            scratch.join("no-such-origin").to_str().unwrap(),
            &root2,
            wt,
            "x",
            |_| {},
        )
        .expect_err("clone must fail");
        assert_eq!(back.path, claim2);
        assert!(claim2.exists(), "the claim outlives the failure");

        fs::remove_dir_all(&scratch).unwrap();
    }

    /// `refresh_base` fast-forwards a chosen existing branch to its remote
    /// tip without touching any working tree - the "continue branch x"
    /// promise of project mode.
    #[test]
    fn refresh_base_fast_forwards_existing() {
        let (scratch, git) = scratch_git("freshtest");
        let origin = scratch.join("origin");
        seed_repo(&git, &origin);
        git(&origin, &["branch", "feat"]);

        let clone = scratch.join("clone");
        git(
            &scratch,
            &[
                "clone",
                origin.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        // A local feat lagging its remote: create it at the current tip,
        // then advance the origin.
        git(&clone, &["branch", "feat", "origin/feat"]);
        git(&origin, &["checkout", "feat"]);
        git(
            &origin,
            &[
                "-c", "user.email=t@t", "-c", "user.name=t",
                "commit", "--allow-empty", "-m", "ahead",
            ],
        );
        let new_tip = git(&origin, &["rev-parse", "feat"]);
        assert_ne!(git(&clone, &["rev-parse", "feat"]), new_tip);

        refresh_base(&clone, &BranchChoice::Existing("feat".into()), |_| {});
        assert_eq!(
            git(&clone, &["rev-parse", "feat"]),
            new_tip,
            "local feat fast-forwarded to the fetched remote tip"
        );
        // The clone's checked-out branch (main) was left alone.
        assert_eq!(
            git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );

        fs::remove_dir_all(&scratch).unwrap();
    }

    /// A Track choice refreshes the remote-tracking ref `--track -b` will
    /// base on, without creating the local branch or moving HEAD.
    #[test]
    fn refresh_base_track_updates_remote_ref() {
        let (scratch, git) = scratch_git("tracktest");
        let origin = scratch.join("origin");
        seed_repo(&git, &origin);
        git(&origin, &["branch", "feat"]);

        let clone = scratch.join("clone");
        git(
            &scratch,
            &[
                "clone",
                origin.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git(&origin, &["checkout", "feat"]);
        git(
            &origin,
            &[
                "-c", "user.email=t@t", "-c", "user.name=t",
                "commit", "--allow-empty", "-m", "ahead",
            ],
        );
        let new_tip = git(&origin, &["rev-parse", "feat"]);
        assert_ne!(git(&clone, &["rev-parse", "origin/feat"]), new_tip);

        refresh_base(
            &clone,
            &BranchChoice::Track {
                name: "feat".into(),
                remote: "origin".into(),
            },
            |_| {},
        );
        assert_eq!(
            git(&clone, &["rev-parse", "origin/feat"]),
            new_tip,
            "remote-tracking ref refreshed"
        );
        // No local feat was created, and HEAD stayed on main.
        assert!(git(&clone, &["branch", "--list", "feat"]).is_empty());
        assert_eq!(
            git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );

        fs::remove_dir_all(&scratch).unwrap();
    }

    /// Codename/New on a user repo (not under clones_dir) fetches nothing:
    /// the branch bases on local state and the refs would go unread. The
    /// unmoved remote-tracking ref is the proof no blanket fetch ran.
    #[test]
    fn refresh_base_skips_user_repos() {
        let (scratch, git) = scratch_git("skiptest");
        let origin = scratch.join("origin");
        seed_repo(&git, &origin);

        let clone = scratch.join("clone");
        git(
            &scratch,
            &[
                "clone",
                origin.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git(
            &origin,
            &[
                "-c", "user.email=t@t", "-c", "user.name=t",
                "commit", "--allow-empty", "-m", "ahead",
            ],
        );
        let stale = git(&clone, &["rev-parse", "origin/main"]);
        assert_ne!(git(&origin, &["rev-parse", "main"]), stale);

        let lines = std::cell::RefCell::new(Vec::new());
        refresh_base(&clone, &BranchChoice::Codename, |l| {
            lines.borrow_mut().push(l)
        });
        assert_eq!(
            git(&clone, &["rev-parse", "origin/main"]),
            stale,
            "no fetch happened for a user repo's codename branch"
        );
        let lines = lines.into_inner();
        assert!(lines.is_empty(), "and none was narrated: {lines:?}");

        fs::remove_dir_all(&scratch).unwrap();
    }

    /// The worktree worker narrates before it works: Progress lines stream
    /// ahead of the one Done, so the App has something to float over the
    /// pane while the checkout runs.
    #[test]
    fn spawn_worktree_streams_progress_then_done() {
        let (scratch, git) = scratch_git("progresstest");
        let repo = scratch.join("repo");
        seed_repo(&git, &repo);
        let claim = scratch.join("wt");
        fs::create_dir(&claim).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        spawn_worktree(
            "mux-tab-test".into(),
            repo.clone(),
            Worktree { path: claim.clone(), branch: "wt".into() },
            BranchChoice::Codename,
            false,
            tx,
            egui::Context::default(),
        );
        let mut msgs = Vec::new();
        while let Ok((tab, msg)) =
            rx.recv_timeout(std::time::Duration::from_secs(10))
        {
            assert_eq!(tab, "mux-tab-test");
            let done = matches!(msg, WorktreeMsg::Done(_));
            msgs.push(msg);
            if done {
                break;
            }
        }
        match msgs.as_slice() {
            [WorktreeMsg::Progress(line), WorktreeMsg::Done(Ok(wt))] => {
                assert!(line.contains("checking out"), "{line}");
                assert_eq!(wt.branch, "wt");
            },
            other => panic!("expected [Progress, Done(Ok)], got {} msgs", other.len()),
        }

        fs::remove_dir_all(&scratch).unwrap();
    }
}
