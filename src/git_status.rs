//! Git working-tree badges (config `git_status`): a background thread maps
//! every pane's cwd to its repo's branch plus dirty/ahead-behind counts and
//! streams `session -> Git` snapshots to the UI. Parsing `git status`
//! porcelain v2 into a `Git` is a pure function, fixture-tested like the PR
//! rollup in `pr_status`.
//!
//! Unlike the PR poller this is all local (`git status`), so it just ticks
//! on a slow cadence with no network and no per-key TTL games; a clean tab
//! costs one cheap subprocess per unique cwd every few seconds.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use egui::text::LayoutJob;
use egui::{Color32, FontId, TextFormat};

use muxterm::mesh;

use crate::theme::UiTheme;

/// Local scan cadence (tmux cwds + `git status`; no network).
const TICK: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq)]
pub struct Git {
    pub branch: String,
    /// Count of changed + untracked entries (0 == clean).
    pub dirty: u32,
    pub ahead: u32,
    pub behind: u32,
    /// Tooltip: "main\n2 changed · 1 ahead".
    pub detail: String,
}

impl Git {
    /// The dot's color: yellow when the tree is dirty, green when clean.
    fn dot(&self, th: &UiTheme) -> Color32 {
        if self.dirty > 0 {
            th.status_warn
        } else {
            th.status_ok
        }
    }

    /// The chip as a colored run: a state dot, the branch, then compact
    /// `*dirty`/`↑ahead`/`↓behind` markers (zero parts omitted). `base` is
    /// the branch/marker text color (active vs. dim tab).
    pub fn chip_job(&self, font: FontId, base: Color32, th: &UiTheme) -> LayoutJob {
        let mut job = LayoutJob::default();
        let seg = |job: &mut LayoutJob, s: &str, c: Color32| {
            job.append(s, 0.0, TextFormat::simple(font.clone(), c));
        };
        seg(&mut job, "\u{25CF} ", self.dot(th));
        seg(&mut job, &self.branch, base);
        if self.dirty > 0 {
            seg(&mut job, &format!("  *{}", self.dirty), th.status_warn);
        }
        if self.ahead > 0 {
            seg(&mut job, &format!("  \u{2191}{}", self.ahead), base);
        }
        if self.behind > 0 {
            seg(&mut job, &format!("  \u{2193}{}", self.behind), base);
        }
        job
    }
}

/// Spawn the poller; it idles (and sends one clearing snapshot) whenever
/// `enabled` is off, so the config toggle applies live without thread
/// lifecycle juggling. Mirrors `pr_status::spawn`.
pub fn spawn(
    ctx: egui::Context,
    tx: Sender<HashMap<String, Git>>,
    enabled: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("git-status".into())
        .spawn(move || run(ctx, tx, enabled))
        .expect("spawn git-status thread");
}

fn run(
    ctx: egui::Context,
    tx: Sender<HashMap<String, Git>>,
    enabled: Arc<AtomicBool>,
) {
    let Ok(tmux) = mesh::find_tmux() else {
        return; // the app itself can't run without tmux either
    };
    let mut last_sent: Option<HashMap<String, Git>> = None;

    loop {
        if !enabled.load(Ordering::Relaxed) {
            if last_sent.as_ref().is_some_and(|s| !s.is_empty()) {
                let _ = tx.send(HashMap::new());
                ctx.request_repaint();
            }
            last_sent = None;
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        // One `git status` per unique cwd, fanned back out to every pane
        // sharing it, so a tab full of panes in one checkout is one call.
        let mut by_cwd: HashMap<String, Option<Git>> = HashMap::new();
        let mut snapshot: HashMap<String, Git> = HashMap::new();
        for (session, cwd) in pane_cwds(&tmux) {
            let git = by_cwd
                .entry(cwd.clone())
                .or_insert_with(|| status(&cwd))
                .clone();
            if let Some(g) = git {
                snapshot.insert(session, g);
            }
        }

        if last_sent.as_ref() != Some(&snapshot) {
            log::debug!("git_status: {snapshot:?}");
            last_sent = Some(snapshot.clone());
            let _ = tx.send(snapshot);
            ctx.request_repaint();
        }

        std::thread::sleep(TICK);
    }
}

fn pane_cwds(tmux: &Path) -> Vec<(String, String)> {
    let out = Command::new(tmux)
        .args([
            "-L",
            mesh::SOCKET,
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_current_path}",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (session, cwd) = line.split_once('\t')?;
            (!cwd.is_empty()).then(|| (session.to_string(), cwd.to_string()))
        })
        .collect()
}

/// `/usr/bin/git` is the macOS shim and always present (same as pr_status).
fn status(cwd: &str) -> Option<Git> {
    let out = Command::new("/usr/bin/git")
        .args(["-C", cwd, "status", "--porcelain=v2", "--branch"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // not a work tree
    }
    parse_status(&String::from_utf8_lossy(&out.stdout))
}

/// Fold `git status --porcelain=v2 --branch` output into one `Git`. Pure;
/// fixture-tested below. None when there is no branch header (not a repo).
///
/// The header lines we read:
///   `# branch.oid <sha>`         (or `(initial)` on an unborn branch)
///   `# branch.head <name>`       (or `(detached)`)
///   `# branch.ab +<ahead> -<behind>`  (absent without an upstream)
/// Every non-`#` line is a changed/untracked entry (types 1, 2, u, ?).
pub fn parse_status(out: &str) -> Option<Git> {
    let mut head: Option<String> = None;
    let mut oid: Option<String> = None;
    let mut ahead = 0u32;
    let mut behind = 0u32;
    let mut dirty = 0u32;

    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            head = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.oid ") {
            oid = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for tok in rest.split_whitespace() {
                if let Some(n) = tok.strip_prefix('+') {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = tok.strip_prefix('-') {
                    behind = n.parse().unwrap_or(0);
                }
            }
        } else if matches!(
            line.as_bytes().first(),
            Some(b'1' | b'2' | b'u' | b'?')
        ) {
            dirty += 1;
        }
    }

    let head = head?;
    let branch = if head == "(detached)" {
        // Show the short commit instead of a useless "(detached)".
        oid.as_deref()
            .filter(|o| *o != "(initial)")
            .map(|o| o.chars().take(7).collect::<String>())
            .unwrap_or_else(|| "detached".to_string())
    } else {
        head
    };

    let mut phrases: Vec<String> = Vec::new();
    phrases.push(if dirty > 0 {
        format!("{dirty} changed")
    } else {
        "clean".to_string()
    });
    if ahead > 0 {
        phrases.push(format!("{ahead} ahead"));
    }
    if behind > 0 {
        phrases.push(format!("{behind} behind"));
    }

    Some(Git {
        detail: format!("{branch}\n{}", phrases.join(" \u{00b7} ")),
        branch,
        dirty,
        ahead,
        behind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "# branch.oid 1a2b3c4d5e6f7a8b\n";

    #[test]
    fn clean_synced_branch() {
        let g = parse_status(&format!(
            "{OID}# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0\n"
        ))
        .unwrap();
        assert_eq!(g.branch, "main");
        assert_eq!((g.dirty, g.ahead, g.behind), (0, 0, 0));
        assert_eq!(g.detail, "main\nclean");
    }

    #[test]
    fn dirty_counts_all_entry_kinds() {
        // one staged (1), one renamed (2), one unmerged (u), one untracked (?)
        let g = parse_status(&format!(
            "{OID}# branch.head feat/x\n\
             1 M. N... 100644 100644 100644 aaa bbb src/a.rs\n\
             2 R. N... 100644 100644 100644 ccc ddd R100 new.rs\told.rs\n\
             u UU N... 100644 100644 100644 100644 eee fff ggg conflict.rs\n\
             ? scratch.txt\n"
        ))
        .unwrap();
        assert_eq!(g.branch, "feat/x");
        assert_eq!(g.dirty, 4);
        assert!(g.detail.contains("4 changed"));
    }

    #[test]
    fn ahead_behind_parsed() {
        let g = parse_status(&format!(
            "{OID}# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -3\n"
        ))
        .unwrap();
        assert_eq!((g.ahead, g.behind), (2, 3));
        assert!(g.detail.contains("2 ahead"));
        assert!(g.detail.contains("3 behind"));
    }

    #[test]
    fn detached_head_shows_short_sha() {
        let g = parse_status(&format!("{OID}# branch.head (detached)\n"))
            .unwrap();
        assert_eq!(g.branch, "1a2b3c4");
        assert_eq!((g.ahead, g.behind), (0, 0));
    }

    #[test]
    fn no_upstream_means_no_ab_line() {
        let g =
            parse_status(&format!("{OID}# branch.head solo\n? new.rs\n"))
                .unwrap();
        assert_eq!(g.branch, "solo");
        assert_eq!((g.dirty, g.ahead, g.behind), (1, 0, 0));
    }

    #[test]
    fn not_a_repo_is_none() {
        assert!(parse_status("").is_none());
        assert!(parse_status("fatal: not a git repository").is_none());
    }
}
