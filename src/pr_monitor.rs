//! The user's own open GitHub PRs, listed in the sidebar (config
//! `monitor_prs`).
//!
//! A sibling of `pr_status`, and deliberately a separate poller rather than an
//! extension of it: `pr_status` answers "does *this checkout's* branch have a
//! PR" and is keyed by (repo root, branch), discovering the repo from a pane's
//! cwd. This answers "what am I working on across every repo", which needs a
//! GitHub identity and a cross-repo search - two things `pr_status` has no
//! notion of.
//!
//! One `gh search prs` call covers every repo the token can see. That search
//! cannot return a PR's head branch (the search API has no such field), so the
//! branch is fetched lazily by `head_branch`, once, on the click that checks a
//! PR out - which keeps the list to a single call and puts the per-PR cost only
//! where it is needed.
//!
//! Unlike `pr_status`, a failure is *surfaced*: a chip that quietly fails to
//! appear is a small mystery, but a whole section that renders empty reads as
//! broken, so the reason travels with the snapshot.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use muxterm::agent;
use serde::Deserialize;

/// How often the list is refreshed while the window is focused. PRs are not
/// fast-moving and this is a network round trip, so it is minutes, not seconds.
const REFRESH: Duration = Duration::from_secs(300);
/// Back-off after a failed search (offline, unauthenticated, rate-limited).
const ERROR_TTL: Duration = Duration::from_secs(300);
/// How long the loop sleeps between wake-ups; the TTLs above do the pacing.
const TICK: Duration = Duration::from_secs(5);
/// Nobody reads the sidebar while the window is unfocused.
const UNFOCUSED_EVERY: u32 = 12;
/// A hung `gh` must not pin the thread forever.
const GH_TIMEOUT: Duration = Duration::from_secs(30);
/// More than this and the section stops being a list and becomes a haystack.
const LIMIT: &str = "50";

/// One open pull request authored by the user.
///
/// No `Instant` here: the derived `PartialEq` is what lets the poller send only
/// on change, the same trick `pr_status::Badge` uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrItem {
    pub number: u64,
    /// `owner/name`, as `gh` spells it - this is what `--repo` wants back.
    pub repo: String,
    pub title: String,
    pub url: String,
    pub draft: bool,
}

/// What the sidebar shows. An empty list with a `note` is a section that can
/// explain itself; an empty list without one simply means no open PRs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub items: Vec<PrItem>,
    /// Why the list is empty, when it is empty for a reason worth saying.
    pub note: Option<String>,
}

/// Spawn the poller; like `pr_status::spawn` it idles (and sends one clearing
/// snapshot) whenever `enabled` is off, so the toggle applies live without
/// thread lifecycle juggling.
pub fn spawn(
    ctx: egui::Context,
    tx: Sender<Snapshot>,
    enabled: Arc<AtomicBool>,
    focused: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("pr-monitor".into())
        .spawn(move || run(ctx, tx, enabled, focused))
        .expect("spawn pr-monitor thread");
}

fn run(
    ctx: egui::Context,
    tx: Sender<Snapshot>,
    enabled: Arc<AtomicBool>,
    focused: Arc<AtomicBool>,
) {
    let mut gh: Option<PathBuf> = None;
    let mut last_sent: Option<Snapshot> = None;
    let mut next_at: Option<Instant> = None;
    let mut skipped = 0u32;

    loop {
        if !enabled.load(Ordering::Relaxed) {
            if last_sent.as_ref().is_some_and(|s| *s != Snapshot::default()) {
                let _ = tx.send(Snapshot::default());
                ctx.request_repaint();
            }
            last_sent = None;
            next_at = None;
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        if !focused.load(Ordering::Relaxed) && skipped + 1 < UNFOCUSED_EVERY {
            skipped += 1;
            std::thread::sleep(TICK);
            continue;
        }
        skipped = 0;

        if next_at.is_some_and(|at| Instant::now() < at) {
            std::thread::sleep(TICK);
            continue;
        }

        if gh.is_none() {
            gh = crate::pr_status::find_gh();
        }
        let snapshot = match &gh {
            None => Snapshot {
                items: Vec::new(),
                note: Some("gh not found".into()),
            },
            Some(bin) => search(bin),
        };
        // A failure backs off harder than a success, so a broken auth costs
        // one call per five minutes rather than one per tick.
        next_at = Some(
            Instant::now()
                + if snapshot.note.is_some() { ERROR_TTL } else { REFRESH },
        );
        if last_sent.as_ref() != Some(&snapshot) {
            let _ = tx.send(snapshot.clone());
            ctx.request_repaint();
            last_sent = Some(snapshot);
        }
        std::thread::sleep(TICK);
    }
}

/// One cross-repo search for the user's own open PRs.
fn search(gh: &PathBuf) -> Snapshot {
    let mut cmd = Command::new(gh);
    cmd.env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .args([
            "search",
            "prs",
            "--author=@me",
            "--state=open",
            "--limit",
            LIMIT,
            "--json",
            "number,title,repository,url,isDraft",
        ]);
    let out = match agent::output_with_timeout(&mut cmd, GH_TIMEOUT) {
        Ok(Some(out)) => out,
        Ok(None) => {
            return Snapshot {
                items: Vec::new(),
                note: Some("gh timed out".into()),
            }
        },
        Err(e) => {
            log::debug!("gh did not run: {e}");
            return Snapshot {
                items: Vec::new(),
                note: Some("gh did not run".into()),
            };
        },
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        log::debug!("gh search prs failed: {}", err.trim());
        return Snapshot {
            items: Vec::new(),
            note: Some(gh_reason(&err)),
        };
    }
    match parse_search(&String::from_utf8_lossy(&out.stdout)) {
        Some(items) => Snapshot { items, note: None },
        None => Snapshot {
            items: Vec::new(),
            note: Some("could not read gh output".into()),
        },
    }
}

/// A short, human reason for a failed search - the section says this instead
/// of rendering empty and looking broken.
fn gh_reason(stderr: &str) -> String {
    let e = stderr.to_ascii_lowercase();
    // gh says this several ways: "not logged into any GitHub hosts", "run gh
    // auth login", a bare 401.
    if e.contains("auth")
        || e.contains("login")
        || e.contains("logged in")
        || e.contains("logged into")
        || e.contains("401")
    {
        "gh is not authenticated".into()
    } else if e.contains("rate limit") {
        "GitHub rate limit reached".into()
    } else if e.contains("network") || e.contains("dial tcp") || e.contains("dns")
    {
        "offline".into()
    } else {
        "gh search failed".into()
    }
}

#[derive(Deserialize)]
struct SearchRepo {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Deserialize)]
struct SearchRow {
    number: u64,
    title: String,
    url: String,
    repository: SearchRepo,
    #[serde(rename = "isDraft")]
    is_draft: bool,
}

/// `gh search prs --json` output into items. Pure; fixture-tested. None means
/// the payload was not the shape we asked for, which is worth saying out loud
/// rather than showing as "no open PRs".
pub fn parse_search(json: &str) -> Option<Vec<PrItem>> {
    let rows: Vec<SearchRow> = serde_json::from_str(json).ok()?;
    Some(
        rows.into_iter()
            .map(|r| PrItem {
                number: r.number,
                repo: r.repository.name_with_owner,
                title: r.title,
                url: r.url,
                draft: r.is_draft,
            })
            .collect(),
    )
}

/// The head branch of one PR, and whether it lives in a fork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub branch: String,
    /// A fork PR's branch is in another repo, so a plain `--track` against
    /// origin cannot reach it.
    pub cross_repo: bool,
}

#[derive(Deserialize)]
struct HeadJson {
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "isCrossRepository")]
    is_cross_repository: bool,
}

/// Parse `gh pr view --json headRefName,isCrossRepository`. Pure.
pub fn parse_head(json: &str) -> Option<Head> {
    let h: HeadJson = serde_json::from_str(json).ok()?;
    (!h.head_ref_name.is_empty()).then(|| Head {
        branch: h.head_ref_name,
        cross_repo: h.is_cross_repository,
    })
}

/// The lazy second call: which branch a PR is on. Network; run off the UI
/// thread. `gh search prs` cannot answer this - its JSON has no head fields -
/// so it is asked once, for the PR being checked out.
pub fn head_branch(repo: &str, number: u64) -> Option<Head> {
    let gh = crate::pr_status::find_gh()?;
    let mut cmd = Command::new(gh);
    cmd.env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .args(["pr", "view", &number.to_string(), "--repo", repo, "--json"])
        .arg("headRefName,isCrossRepository");
    let out = agent::output_with_timeout(&mut cmd, GH_TIMEOUT).ok()??;
    if !out.status.success() {
        log::debug!(
            "gh pr view {repo}#{number} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    parse_head(&String::from_utf8_lossy(&out.stdout))
}

/// The PR as text to read: the overview `gh pr view` prints, then its diff.
///
/// Both are plain when gh is not writing to a terminal - no ANSI to parse -
/// so the viewer colours the diff itself from the line prefixes.
pub fn fetch_text(repo: &str, number: u64) -> Result<String, String> {
    let gh = crate::pr_status::find_gh().ok_or("gh not found")?;
    let run = |args: &[&str]| -> Result<String, String> {
        let mut cmd = Command::new(&gh);
        cmd.env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .args(args);
        let out = agent::output_with_timeout(&mut cmd, GH_TIMEOUT)
            .map_err(|e| format!("gh did not run: {e}"))?
            .ok_or("gh timed out")?;
        if !out.status.success() {
            return Err(gh_reason(&String::from_utf8_lossy(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let n = number.to_string();
    let view = run(&["pr", "view", &n, "--repo", repo])?;
    // A diff that fails (a huge PR, a timeout) still leaves the overview
    // worth reading, so it degrades rather than failing the whole view.
    let diff = run(&["pr", "diff", &n, "--repo", repo])
        .unwrap_or_else(|e| format!("\n(could not load diff: {e})\n"));
    Ok(format!("{view}\n{diff}"))
}

/// `fetch_text` off-thread, streamed back to the App's preview overlay.
pub fn spawn_text(
    repo: String,
    number: u64,
    tx: Sender<(String, u64, Result<String, String>)>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let text = fetch_text(&repo, number);
        let _ = tx.send((repo, number, text));
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a real `gh search prs --author=@me --state=open` run.
    const SEARCH: &str = r#"[
      {"isDraft":false,"number":9645,
       "repository":{"name":"monobloco","nameWithOwner":"Telepatia-AI/monobloco"},
       "title":"feat(canario): institution-ramp canary rollback platform",
       "updatedAt":"2026-08-12T17:04:43Z",
       "url":"https://github.com/Telepatia-AI/monobloco/pull/9645"},
      {"isDraft":true,"number":12,
       "repository":{"name":"muxterm","nameWithOwner":"herval/muxterm"},
       "title":"wip: something",
       "updatedAt":"2026-08-11T09:00:00Z",
       "url":"https://github.com/herval/muxterm/pull/12"}
    ]"#;

    #[test]
    fn parses_a_multi_repo_search() {
        let items = parse_search(SEARCH).expect("valid payload");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], PrItem {
            number: 9645,
            repo: "Telepatia-AI/monobloco".into(),
            title: "feat(canario): institution-ramp canary rollback platform"
                .into(),
            url: "https://github.com/Telepatia-AI/monobloco/pull/9645".into(),
            draft: false,
        });
        // The repo is carried as owner/name, which is what `--repo` wants back
        // when the head branch is fetched later.
        assert_eq!(items[1].repo, "herval/muxterm");
        assert!(items[1].draft, "a draft PR is marked as one");
    }

    #[test]
    fn an_empty_search_is_not_an_error() {
        assert_eq!(parse_search("[]"), Some(Vec::new()));
    }

    /// A payload we can't read must be distinguishable from "no open PRs" -
    /// the section says which.
    #[test]
    fn unreadable_output_is_none_not_empty() {
        assert_eq!(parse_search("not json"), None);
        assert_eq!(parse_search(""), None);
    }

    #[test]
    fn head_branch_json_carries_the_fork_flag() {
        let same = r#"{"headRefName":"snappy-dove","isCrossRepository":false}"#;
        assert_eq!(parse_head(same), Some(Head {
            branch: "snappy-dove".into(),
            cross_repo: false,
        }));
        let fork = r#"{"headRefName":"patch-1","isCrossRepository":true}"#;
        assert!(parse_head(fork).unwrap().cross_repo);
        // A PR with no head branch is not something we can check out.
        assert_eq!(
            parse_head(r#"{"headRefName":"","isCrossRepository":false}"#),
            None,
        );
    }

    /// The reason shown when a search fails should name the likely cause.
    #[test]
    fn gh_failures_get_a_human_reason() {
        assert_eq!(
            gh_reason("error: not logged into any GitHub hosts"),
            "gh is not authenticated",
        );
        assert_eq!(gh_reason("API rate limit exceeded"), "GitHub rate limit reached");
        assert_eq!(gh_reason("dial tcp: lookup api.github.com"), "offline");
        assert_eq!(gh_reason("something else entirely"), "gh search failed");
    }
}
