//! Workspaces: a tab's sense of what it is *for*. Every tab carries a
//! `Workspace` - bare for a plain cmd+t shell tab, rich for a cmd+n workspace
//! (a project folder, an optional git worktree, the task prompt, the agent,
//! and a random two-word codename as its title). The layout's source of truth
//! is `state::WorkspaceState`; this is the live GUI-side value plus the git
//! and naming helpers.
//!
//! Worktrees live under `~/.muxterm/worktrees/` - outside their repos - so the
//! repo's own `git status` stays clean and no `.gitignore` handling is needed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::thread;

use muxterm::agent;
use muxterm::mesh;
use muxterm::state::{self, WorkspaceState, WorktreeState};

#[derive(Clone, Debug)]
pub struct Workspace {
    /// The folder the workspace lives in; None for a bare shell tab with no
    /// chosen folder. Starts as the creation folder, then follows the panes:
    /// when every pane leaves it (and its repo), the App's root sync
    /// repoints it at where they went (`retarget`).
    pub root: Option<PathBuf>,
    /// Display label: a random two-word codename, until a human or an agent
    /// renames it (`mux rename`).
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
        }
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
        }
    }
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

/// A random adjective-animal codename ("brisk-otter"): every workspace's
/// birth name. Random rather than AI-generated: instant, needs no agent CLI,
/// and two words tell tabs apart - the sidebar subtitle carries the
/// folder/branch. Randomness comes from uuid v4 bytes (the crate's one
/// existing entropy source; no rand dependency).
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

/// A git-branch-safe slug from the task prompt: the first few words,
/// lowercased with non-alphanumerics stripped, joined by '-'. May be empty
/// (all punctuation / no prompt) - the caller supplies the fallback.
pub fn slug(prompt: &str) -> String {
    prompt
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .map(|c| c.to_ascii_lowercase())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-")
}

/// Reserve a worktree for `root` under `~/.muxterm/worktrees/`: pick a free
/// name derived from the prompt (a numeric suffix on a branch or directory
/// collision) and create the - still empty - directory. Synchronous and
/// cheap, so the pane can open directly inside it and never has to `cd`;
/// `spawn_worktree` then runs the slow checkout into it off the UI thread
/// (`git worktree add` accepts an existing empty directory). The mkdir *is*
/// the claim: it atomically settles racing name picks.
pub fn claim_worktree(root: &Path, prompt: &str) -> anyhow::Result<Worktree> {
    let base = {
        let s = slug(prompt);
        if s.is_empty() {
            "workspace".to_string()
        } else {
            s
        }
    };
    let dir = state::worktrees_dir();
    fs::create_dir_all(&dir)?;

    for attempt in 0..100 {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        if branch_exists(root, &name) {
            continue;
        }
        let path = dir.join(&name);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(Worktree { path, branch: name }),
            // A taken directory loses to the next suffix.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("no free worktree name for {base}")
}

/// The slow half of `claim_worktree`: check `root` out into the claimed
/// directory, on the claimed branch.
fn populate_worktree(root: &Path, wt: &Worktree) -> anyhow::Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "add"])
        .arg(&wt.path)
        .args(["-b", &wt.branch])
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git worktree add failed: {}", err.trim());
    }
    Ok(())
}

/// Run `populate_worktree` off the UI thread - the checkout can be slow on a
/// large or lfs-heavy repo - and stream the result back to the App by tab id.
/// A failed checkout gives the claimed directory back (`remove_dir` refuses
/// non-empty, so a partial checkout is never deleted). Same channel + repaint
/// wiring as the pr_status/git_status pollers.
pub fn spawn_worktree(
    tab_id: String,
    root: PathBuf,
    worktree: Worktree,
    tx: Sender<(String, Result<Worktree, String>)>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let res = match populate_worktree(&root, &worktree) {
            Ok(()) => Ok(worktree),
            Err(e) => {
                let _ = fs::remove_dir(&worktree.path);
                Err(format!("{e:#}"))
            },
        };
        let _ = tx.send((tab_id, res));
        ctx.request_repaint();
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_from_prompt() {
        assert_eq!(slug("Wire up OAuth login"), "wire-up-oauth-login");
        assert_eq!(slug("  fix   the build!! "), "fix-the-build");
        assert_eq!(slug("C++ & Rust: interop"), "c-rust-interop");
        assert_eq!(slug(""), "");
        assert_eq!(slug("---"), "");
        // Long prompts are capped without a trailing dash.
        assert!(slug(&"word ".repeat(40)).len() <= 40);
        assert!(!slug(&"word ".repeat(40)).ends_with('-'));
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
        };
        let back = Workspace::from_state(ws.to_state());
        assert_eq!(back.agent, Some("claude"));
        assert_eq!(back.worktree.unwrap().branch, "b");
        // An unknown agent id resolves to None rather than panicking.
        let mut st = ws.to_state();
        st.agent = Some("nope".into());
        assert_eq!(Workspace::from_state(st).agent, None);
    }
}
