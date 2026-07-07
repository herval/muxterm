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
use muxterm::state::{self, WorkspaceState, WorktreeState};

#[derive(Clone, Debug)]
pub struct Workspace {
    /// The folder the workspace was created in; None for a bare shell tab
    /// with no chosen folder.
    pub root: Option<PathBuf>,
    /// Display label: prompt-derived at first, upgraded to the AI title.
    pub title: String,
    /// AI-generated one-line summary (fills in asynchronously).
    pub description: Option<String>,
    /// The free-text task the workspace was started from (empty for bare).
    pub prompt: String,
    /// The dedicated git worktree, when "create worktree" was used.
    pub worktree: Option<Worktree>,
    /// Agent id ("claude" | "codex") launched in the pane; None for bare.
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
    /// or agent - just a folder and a name.
    pub fn bare(root: Option<PathBuf>) -> Self {
        let title = default_title(root.as_deref(), "");
        Self {
            root,
            title,
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

/// The label shown before the AI title lands: the first words of the task
/// prompt, else the folder name, else "workspace".
pub fn default_title(root: Option<&Path>, prompt: &str) -> String {
    let p = prompt.trim();
    if !p.is_empty() {
        return p.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
    }
    root.and_then(|r| r.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
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

/// Create a git worktree for `root` under `~/.muxterm/worktrees/`, on a fresh
/// branch derived from the prompt. On a name collision (branch or directory
/// already taken) a numeric suffix is appended until one is free. Returns the
/// worktree so the pane can start there.
pub fn create_worktree(root: &Path, prompt: &str) -> anyhow::Result<Worktree> {
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
        let path = dir.join(&name);
        if path.exists() || branch_exists(root, &name) {
            continue;
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["worktree", "add"])
            .arg(&path)
            .args(["-b", &name])
            .output()?;
        if out.status.success() {
            return Ok(Worktree { path, branch: name });
        }
        let err = String::from_utf8_lossy(&out.stderr);
        // A racing name loses to the next suffix; any other failure is real.
        if !err.contains("already exists") {
            anyhow::bail!("git worktree add failed: {}", err.trim());
        }
    }
    anyhow::bail!("no free worktree name for {base}")
}

/// Run `create_worktree` off the UI thread - the checkout can be slow on a
/// large or lfs-heavy repo - and stream the result back to the App by tab id.
/// Same channel + repaint wiring as the title/name generators.
pub fn spawn_worktree(
    tab_id: String,
    root: PathBuf,
    prompt: String,
    tx: Sender<(String, Result<Worktree, String>)>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let res = create_worktree(&root, &prompt).map_err(|e| format!("{e:#}"));
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

const TITLE_INSTRUCTION: &str =
    "Summarize the following coding task as a terse title of at most 6 words. \
     Reply with only the title: no quotes, no trailing punctuation.";

const NAME_INSTRUCTION: &str =
    "Below is a snapshot of a terminal session (its working directory and \
     recent output). Give a terse 2-4 word name for what this workspace is \
     about. Reply with only the name: no quotes, no trailing punctuation.";

/// Kick off a background one-shot small-model call that turns the task prompt
/// into a short title, streamed back to the App keyed by tab id. Mirrors the
/// pr_status/git_status poller wiring (an mpsc Sender plus an egui::Context to
/// wake the UI). Best-effort: on any failure the workspace keeps its
/// prompt-derived title.
pub fn spawn_title(
    tab_id: String,
    prompt: String,
    agent: &'static Agent,
    tx: Sender<(String, String)>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        if let Some(title) =
            generate(agent, TITLE_INSTRUCTION, &format!("Task: {prompt}"))
        {
            if tx.send((tab_id, title)).is_ok() {
                ctx.request_repaint();
            }
        }
    });
}

/// Like `spawn_title`, but for a workspace with no task prompt (a bare cmd+t
/// tab): name it from a snapshot of its terminal instead. Same channel + UI
/// wiring, same best-effort contract.
pub fn spawn_name(
    tab_id: String,
    context: String,
    agent: &'static Agent,
    tx: Sender<(String, String)>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        if let Some(name) = generate(agent, NAME_INSTRUCTION, &context) {
            if tx.send((tab_id, name)).is_ok() {
                ctx.request_repaint();
            }
        }
    });
}

fn generate(agent: &Agent, instruction: &str, body: &str) -> Option<String> {
    let full = format!("{instruction}\n\n{body}");
    let cmdline = match agent.id {
        // codex exec streams its own progress; the final assistant line is
        // last, which `clean_title` picks up.
        "codex" => format!("codex exec {}", agent::shell_quote(&full)),
        _ => {
            let model = agent.fast_model.unwrap_or("haiku");
            format!("claude -p --model {model} {}", agent::shell_quote(&full))
        },
    };
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
    fn default_title_prefers_prompt_then_folder() {
        assert_eq!(
            default_title(None, "add a settings sidebar to the app now"),
            "add a settings sidebar to the"
        );
        assert_eq!(
            default_title(Some(Path::new("/home/u/myproj")), ""),
            "myproj"
        );
        assert_eq!(default_title(None, "  "), "workspace");
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
    fn bare_workspace_names_from_folder() {
        let ws = Workspace::bare(Some(PathBuf::from("/home/u/thing")));
        assert_eq!(ws.title, "thing");
        assert!(ws.worktree.is_none());
        assert!(ws.agent.is_none());
        assert_eq!(Workspace::bare(None).title, "workspace");
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
