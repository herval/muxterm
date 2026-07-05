//! GitHub PR status badges (config `pr_status`): a background thread maps
//! every pane's cwd to (repo, branch), asks `gh` about that branch's PR,
//! and streams `session -> Badge` snapshots back to the UI. The rollup of
//! gh's JSON into a Badge is a pure function so it unit-tests on fixture
//! strings, like the "?" prompt machine.
//!
//! Cost discipline: pane cwds and git branches are local subprocesses on a
//! slow tick; the network (`gh pr view`) runs at most once per unique
//! (repo, branch) per TTL, so a tab full of panes in one checkout costs
//! one API call a minute.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::Color32;

use muxterm::mesh;

use crate::theme::UiTheme;

/// Local scan cadence (tmux cwds + git branches; no network).
const LOCAL_TICK: Duration = Duration::from_secs(5);
/// Re-fetch cadence per (repo, branch) with a PR or none found.
const FETCH_TTL: Duration = Duration::from_secs(60);
/// Back-off after a gh failure (unauthenticated, offline, rate-limited).
const ERROR_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq)]
pub struct Badge {
    pub number: u64,
    pub url: String,
    pub kind: Kind,
    /// Tooltip: "#12 title\nchecks pending · approved".
    pub detail: String,
}

/// Visual severity of the chip's dot; the words live in `detail`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Err,
    Warn,
    Ok,
    Merged,
    Neutral,
}

impl Kind {
    pub fn color(self, th: &UiTheme) -> Color32 {
        match self {
            Kind::Err => th.status_err,
            Kind::Warn => th.status_warn,
            Kind::Ok => th.status_ok,
            Kind::Merged => th.status_merged,
            Kind::Neutral => th.text_dim,
        }
    }
}

/// Spawn the poller; it idles (and sends one clearing snapshot) whenever
/// `enabled` is off, so the config toggle applies live without thread
/// lifecycle juggling.
pub fn spawn(
    ctx: egui::Context,
    tx: Sender<HashMap<String, Badge>>,
    enabled: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("pr-status".into())
        .spawn(move || run(ctx, tx, enabled))
        .expect("spawn pr-status thread");
}

enum Fetched {
    Badge(Badge),
    NoPr,
    Error,
}

impl Fetched {
    fn ttl(&self) -> Duration {
        match self {
            Fetched::Error => ERROR_TTL,
            _ => FETCH_TTL,
        }
    }
}

fn run(
    ctx: egui::Context,
    tx: Sender<HashMap<String, Badge>>,
    enabled: Arc<AtomicBool>,
) {
    let Ok(tmux) = mesh::find_tmux() else {
        return; // the app itself can't run without tmux either
    };
    let mut gh: Option<PathBuf> = None;
    let mut warned_no_gh = false;
    let mut cache: HashMap<(String, String), (Instant, Fetched)> =
        HashMap::new();
    let mut last_sent: Option<HashMap<String, Badge>> = None;

    loop {
        if !enabled.load(Ordering::Relaxed) {
            if last_sent.as_ref().is_some_and(|s| !s.is_empty()) {
                let _ = tx.send(HashMap::new());
                ctx.request_repaint();
            }
            last_sent = None;
            cache.clear();
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        if gh.is_none() {
            gh = find_gh();
            if gh.is_none() {
                if !warned_no_gh {
                    log::warn!("pr_status is on but gh was not found");
                    warned_no_gh = true;
                }
                std::thread::sleep(FETCH_TTL);
                continue;
            }
        }
        let gh_bin = gh.as_ref().unwrap();

        // session -> (repo root, branch) for every pane in a checkout.
        let mut keys: HashMap<String, (String, String)> = HashMap::new();
        for (session, cwd) in pane_cwds(&tmux) {
            if let Some(key) = repo_branch(&cwd) {
                keys.insert(session, key);
            }
        }

        let live: HashSet<&(String, String)> = keys.values().collect();
        cache.retain(|k, _| live.contains(k));
        for key in live {
            let stale = cache
                .get(key)
                .is_none_or(|(at, f)| at.elapsed() > f.ttl());
            if stale {
                let fetched = fetch(gh_bin, &key.0, &key.1);
                cache.insert(key.clone(), (Instant::now(), fetched));
            }
        }

        let snapshot: HashMap<String, Badge> = keys
            .iter()
            .filter_map(|(session, key)| match cache.get(key) {
                Some((_, Fetched::Badge(b))) => {
                    Some((session.clone(), b.clone()))
                },
                _ => None,
            })
            .collect();
        if last_sent.as_ref() != Some(&snapshot) {
            log::debug!("pr_status: {snapshot:?}");
            last_sent = Some(snapshot.clone());
            let _ = tx.send(snapshot);
            ctx.request_repaint();
        }

        std::thread::sleep(LOCAL_TICK);
    }
}

/// PATH is bare under Finder launches; probe like `mesh::find_tmux`, but
/// PATH (`which`) first so shells and tests can interpose their own gh.
fn find_gh() -> Option<PathBuf> {
    if let Ok(out) = Command::new("which").arg("gh").output() {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

fn pane_cwds(tmux: &std::path::Path) -> Vec<(String, String)> {
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
            (!cwd.is_empty())
                .then(|| (session.to_string(), cwd.to_string()))
        })
        .collect()
}

/// (repo root, branch) of a directory; None outside a work tree or on a
/// detached HEAD. /usr/bin/git is the macOS shim and always present.
fn repo_branch(cwd: &str) -> Option<(String, String)> {
    let out = Command::new("/usr/bin/git")
        .args(["-C", cwd, "rev-parse", "--show-toplevel", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let root = lines.next()?.trim().to_string();
    let branch = lines.next()?.trim().to_string();
    (!root.is_empty() && !branch.is_empty() && branch != "HEAD")
        .then_some((root, branch))
}

fn fetch(gh: &std::path::Path, root: &str, branch: &str) -> Fetched {
    let out = Command::new(gh)
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .args([
            "pr",
            "view",
            branch,
            "--json",
            "number,title,url,state,isDraft,reviewDecision,statusCheckRollup",
        ])
        .output();
    match out {
        Ok(out) if out.status.success() => {
            match rollup(&String::from_utf8_lossy(&out.stdout)) {
                Some(badge) => Fetched::Badge(badge),
                None => Fetched::Error,
            }
        },
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("no pull requests found") {
                Fetched::NoPr
            } else {
                log::debug!("gh pr view failed for {root}@{branch}: {}", err.trim());
                Fetched::Error
            }
        },
        Err(e) => {
            log::debug!("gh did not run: {e}");
            Fetched::Error
        },
    }
}

/// Fold gh's JSON into one badge. Pure; fixture-tested below.
pub fn rollup(json: &str) -> Option<Badge> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let number = v.get("number")?.as_u64()?;
    let url = v.get("url")?.as_str()?.to_string();
    let title = v.get("title").and_then(|t| t.as_str()).unwrap_or("");
    let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("OPEN");
    let draft = v.get("isDraft").and_then(|d| d.as_bool()).unwrap_or(false);
    let review = v.get("reviewDecision").and_then(|r| r.as_str());

    let checks = v
        .get("statusCheckRollup")
        .and_then(|c| c.as_array())
        .map(|items| {
            let mut failing = false;
            let mut pending = false;
            for item in items {
                match check_class(item) {
                    CheckClass::Fail => failing = true,
                    CheckClass::Pending => pending = true,
                    CheckClass::Ok => {},
                }
            }
            (failing, pending, !items.is_empty())
        })
        .unwrap_or((false, false, false));
    let (failing, pending, has_checks) = checks;

    let mut phrases: Vec<&str> = Vec::new();
    let kind = if state == "MERGED" {
        phrases.push("merged");
        Kind::Merged
    } else if state == "CLOSED" {
        phrases.push("closed");
        Kind::Neutral
    } else {
        if failing {
            phrases.push("checks failing");
        } else if pending {
            phrases.push("checks pending");
        } else if has_checks {
            phrases.push("checks passing");
        }
        match review {
            Some("CHANGES_REQUESTED") => phrases.push("changes requested"),
            Some("APPROVED") => phrases.push("approved"),
            _ => {},
        }
        if draft {
            phrases.push("draft");
        }
        if failing || review == Some("CHANGES_REQUESTED") {
            Kind::Err
        } else if pending {
            Kind::Warn
        } else if draft {
            Kind::Neutral
        } else {
            Kind::Ok
        }
    };
    if phrases.is_empty() {
        phrases.push("open");
    }

    Some(Badge {
        number,
        url,
        kind,
        detail: format!("#{number} {title}\n{}", phrases.join(" · ")),
    })
}

enum CheckClass {
    Ok,
    Pending,
    Fail,
}

/// gh mixes two node shapes in statusCheckRollup: CheckRun
/// (status/conclusion) and StatusContext (state).
fn check_class(item: &serde_json::Value) -> CheckClass {
    if let Some(state) = item.get("state").and_then(|s| s.as_str()) {
        return match state {
            "SUCCESS" | "NEUTRAL" => CheckClass::Ok,
            "PENDING" | "EXPECTED" => CheckClass::Pending,
            _ => CheckClass::Fail,
        };
    }
    match item.get("status").and_then(|s| s.as_str()) {
        Some("COMPLETED") => {
            match item.get("conclusion").and_then(|c| c.as_str()) {
                Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => CheckClass::Ok,
                _ => CheckClass::Fail,
            }
        },
        _ => CheckClass::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(fields: &str) -> String {
        format!(
            r#"{{"number":12,"title":"Fix parser","url":"https://github.com/a/b/pull/12",{fields}}}"#
        )
    }

    #[test]
    fn merged_and_closed_win_over_checks() {
        let b = rollup(&pr(r#""state":"MERGED","statusCheckRollup":[{"status":"COMPLETED","conclusion":"FAILURE"}]"#)).unwrap();
        assert_eq!(b.kind, Kind::Merged);
        assert!(b.detail.contains("merged"));
        let b = rollup(&pr(r#""state":"CLOSED""#)).unwrap();
        assert_eq!(b.kind, Kind::Neutral);
    }

    #[test]
    fn failing_checks_and_changes_requested_are_red() {
        let b = rollup(&pr(r#""state":"OPEN","statusCheckRollup":[{"status":"COMPLETED","conclusion":"FAILURE"},{"status":"COMPLETED","conclusion":"SUCCESS"}]"#)).unwrap();
        assert_eq!(b.kind, Kind::Err);
        assert!(b.detail.contains("checks failing"));
        let b = rollup(&pr(r#""state":"OPEN","reviewDecision":"CHANGES_REQUESTED","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]"#)).unwrap();
        assert_eq!(b.kind, Kind::Err);
        assert!(b.detail.contains("changes requested"));
    }

    #[test]
    fn pending_checks_are_yellow_both_node_shapes() {
        for checks in [
            r#"[{"status":"IN_PROGRESS"}]"#,
            r#"[{"status":"QUEUED"}]"#,
            r#"[{"state":"PENDING"}]"#,
        ] {
            let b = rollup(&pr(&format!(
                r#""state":"OPEN","statusCheckRollup":{checks}"#
            )))
            .unwrap();
            assert_eq!(b.kind, Kind::Warn, "for {checks}");
        }
    }

    #[test]
    fn green_paths_and_draft() {
        let b = rollup(&pr(r#""state":"OPEN","reviewDecision":"APPROVED","statusCheckRollup":[{"state":"SUCCESS"}]"#)).unwrap();
        assert_eq!(b.kind, Kind::Ok);
        assert!(b.detail.contains("approved"));
        // no checks at all: an open healthy PR is still green
        let b = rollup(&pr(r#""state":"OPEN""#)).unwrap();
        assert_eq!(b.kind, Kind::Ok);
        assert!(b.detail.contains("open"));
        let b = rollup(&pr(r#""state":"OPEN","isDraft":true,"statusCheckRollup":[{"state":"SUCCESS"}]"#)).unwrap();
        assert_eq!(b.kind, Kind::Neutral);
        assert!(b.detail.contains("draft"));
        // ...but a draft with failing checks still alarms
        let b = rollup(&pr(r#""state":"OPEN","isDraft":true,"statusCheckRollup":[{"state":"FAILURE"}]"#)).unwrap();
        assert_eq!(b.kind, Kind::Err);
    }

    #[test]
    fn tooltip_carries_number_and_title() {
        let b = rollup(&pr(r#""state":"OPEN""#)).unwrap();
        assert_eq!(b.number, 12);
        assert_eq!(b.url, "https://github.com/a/b/pull/12");
        assert!(b.detail.starts_with("#12 Fix parser\n"));
    }

    #[test]
    fn garbage_json_is_none() {
        assert!(rollup("").is_none());
        assert!(rollup("{}").is_none());
        assert!(rollup(r#"{"number":"nan"}"#).is_none());
    }
}
