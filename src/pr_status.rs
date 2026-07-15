//! GitHub PR status badges (config `pr_status`): a background thread maps
//! every pane's cwd to (repo, branch), asks `gh` about the PRs that
//! checkout has touched, and streams `session -> Vec<Badge>` snapshots
//! back to the UI. The rollup of gh's JSON into a Badge is a pure function
//! so it unit-tests on fixture strings, like the "?" prompt machine.
//!
//! The poller remembers every branch that had a PR, per repo root
//! (`pr_memory.json` under ~/.muxterm, so chips survive a relaunch):
//! moving off a branch keeps its chip alive and refreshing until the
//! checkout goes away or the user right-clicks the (merged/closed) chip
//! off. Remembered PRs refresh by number - `gh pr view <n>` survives the
//! branch being deleted after merge.
//!
//! Cost discipline: pane cwds come from the GUI's shared per-second
//! snapshot (no tmux spawn here), git branches are one local subprocess
//! per unique cwd on a slow tick - stretched further while the window is
//! unfocused and nobody reads the chips; the network (`gh pr view`) runs
//! at most once per unique (repo, branch) per TTL - merged/closed PRs,
//! the ones that accumulate, on a much longer one.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, Pos2, Rect, Stroke, Vec2};

use muxterm::state;

use crate::theme::HudColors;
use crate::tmux;

/// Local scan cadence (pane cwds + git branches; no network).
const LOCAL_TICK: Duration = Duration::from_secs(5);
/// While the window is unfocused nobody reads the chips: scan every Nth
/// tick (30s), and pick a refocus back up within one tick.
const UNFOCUSED_EVERY: u32 = 6;
/// Re-fetch cadence per (repo, branch) with a PR or none found.
const FETCH_TTL: Duration = Duration::from_secs(60);
/// Re-fetch cadence for a merged/closed PR: it barely changes, and dead
/// PRs are the ones the memory accumulates.
const CLOSED_TTL: Duration = Duration::from_secs(600);
/// Back-off after a gh failure (unauthenticated, offline, rate-limited).
const ERROR_TTL: Duration = Duration::from_secs(300);

/// Chips right-clicked away, (root, branch)-keyed: the App inserts, the
/// poller drains each tick and forgets them from its memory.
pub type Dismissed = Arc<Mutex<HashSet<(String, String)>>>;

#[derive(Clone, Debug, PartialEq)]
pub struct Badge {
    pub number: u64,
    pub url: String,
    pub kind: Kind,
    /// Tooltip: "#12 title\nfeat-x · checks pending · approved".
    pub detail: String,
    /// The (root, branch) this badge is remembered under - the dismissal
    /// key, and how the App tells a chip's branch from the pane's. No
    /// Instants here: derived PartialEq drives the poller's send-on-change.
    pub root: String,
    pub branch: String,
}

/// Visual severity of the chip's icon; the words live in `detail`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Checks failing or changes requested.
    Err,
    /// Checks pending.
    Warn,
    /// Open and healthy but not yet approved: checks green (or none), not
    /// draft, no approving review. The "awaiting review" state.
    Ok,
    /// Approved: open, checks green (or none), not draft, review APPROVED.
    /// Split out from `Ok` so a signed-off PR reads distinctly from one
    /// still waiting on a reviewer.
    Approved,
    Merged,
    /// Draft.
    Draft,
    /// Closed without merging.
    Neutral,
}

impl Kind {
    pub fn color(self, hud: &HudColors) -> Color32 {
        match self {
            Kind::Err => hud.err,
            Kind::Warn => hud.warn,
            Kind::Ok => hud.ok,
            Kind::Approved => hud.ok,
            Kind::Merged => hud.merged,
            Kind::Draft => hud.fg_dim,
            Kind::Neutral => hud.fg_dim,
        }
    }

    /// The chip's state icon, painter-drawn like `sidebar::status_icon`
    /// (terminal fonts gamble on ✗/✓ glyphs) and a distinct shape per
    /// state - color alone must never be the only signal: red ✗ failing,
    /// yellow dot pending, green ✓ open/awaiting review, green sealed-✓
    /// approved, hollow ring draft, magenta merge-glyph merged, dim minus
    /// closed.
    pub fn draw_icon(
        self,
        painter: &egui::Painter,
        center: Pos2,
        font_size: f32,
        hud: &HudColors,
    ) {
        let r = font_size * 0.30;
        let stroke =
            Stroke::new((font_size * 0.12).max(1.0), self.color(hud));
        match self {
            Kind::Err => {
                let d = r * 0.8;
                painter.line_segment(
                    [
                        Pos2::new(center.x - d, center.y - d),
                        Pos2::new(center.x + d, center.y + d),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        Pos2::new(center.x - d, center.y + d),
                        Pos2::new(center.x + d, center.y - d),
                    ],
                    stroke,
                );
            },
            Kind::Warn => {
                painter.circle_filled(center, r * 0.62, self.color(hud));
            },
            Kind::Ok => {
                let mid = Pos2::new(center.x - r * 0.25, center.y + r * 0.8);
                painter.line_segment(
                    [Pos2::new(center.x - r, center.y + r * 0.1), mid],
                    stroke,
                );
                painter.line_segment(
                    [mid, Pos2::new(center.x + r, center.y - r * 0.7)],
                    stroke,
                );
            },
            Kind::Approved => {
                // The ready check, sealed in a ring: a stronger "signed off"
                // than the bare open-PR check, and shape-distinct from it and
                // from the empty (dim) draft ring.
                painter.circle_stroke(center, r, stroke);
                let cr = r * 0.5;
                let mid =
                    Pos2::new(center.x - cr * 0.25, center.y + cr * 0.85);
                painter.line_segment(
                    [Pos2::new(center.x - cr, center.y + cr * 0.1), mid],
                    stroke,
                );
                painter.line_segment(
                    [mid, Pos2::new(center.x + cr, center.y - cr * 0.75)],
                    stroke,
                );
            },
            Kind::Merged => {
                let x = center.x - r * 0.45;
                let dot = Pos2::new(center.x + r * 0.75, center.y);
                painter.line_segment(
                    [Pos2::new(x, center.y - r), Pos2::new(x, center.y + r)],
                    stroke,
                );
                painter.line_segment([Pos2::new(x, center.y), dot], stroke);
                for p in [
                    Pos2::new(x, center.y - r),
                    Pos2::new(x, center.y + r),
                    dot,
                ] {
                    painter.circle_filled(p, stroke.width * 1.1, stroke.color);
                }
            },
            Kind::Draft => {
                painter.circle_stroke(center, r * 0.62, stroke);
            },
            Kind::Neutral => {
                painter.rect_filled(
                    Rect::from_center_size(
                        center,
                        Vec2::new(r * 1.6, stroke.width * 1.6),
                    ),
                    CornerRadius::ZERO,
                    stroke.color,
                );
            },
        }
    }
}

/// Spawn the poller; it idles (and sends one clearing snapshot) whenever
/// `enabled` is off, so the config toggle applies live without thread
/// lifecycle juggling.
pub fn spawn(
    ctx: egui::Context,
    tx: Sender<HashMap<String, Vec<Badge>>>,
    enabled: Arc<AtomicBool>,
    panes: tmux::SharedPanes,
    focused: Arc<AtomicBool>,
    dismissed: Dismissed,
) {
    std::thread::Builder::new()
        .name("pr-status".into())
        .spawn(move || run(ctx, tx, enabled, panes, focused, dismissed))
        .expect("spawn pr-status thread");
}

enum Fetched {
    Badge(Badge),
    NoPr,
    Error,
    /// Restored from the memory file, not fetched yet: retained like a
    /// Badge, stale immediately, invisible to snapshots.
    Seed,
}

/// One cached fetch per (root, branch).
struct Entry {
    at: Instant,
    fetched: Fetched,
    /// Pinned on the first Badge (or seeded from the memory file);
    /// refreshes then go by `gh pr view <number>`, which survives branch
    /// deletion after merge and branch-name reuse.
    number: Option<u64>,
}

impl Entry {
    fn ttl(&self) -> Duration {
        match &self.fetched {
            Fetched::Error => ERROR_TTL,
            Fetched::Seed => Duration::ZERO,
            Fetched::Badge(b) => match b.kind {
                Kind::Merged | Kind::Neutral => CLOSED_TTL,
                _ => FETCH_TTL,
            },
            Fetched::NoPr => FETCH_TTL,
        }
    }
}

/// root -> branch -> PR number: every PR a checkout has touched. The
/// durable half of the poller (`pr_memory.json`); the cache is rebuilt
/// from it after a relaunch. Entries leave on dismissal or when the root
/// path itself is gone (worktree removed = task over).
type Memory = HashMap<String, HashMap<String, u64>>;

fn run(
    ctx: egui::Context,
    tx: Sender<HashMap<String, Vec<Badge>>>,
    enabled: Arc<AtomicBool>,
    panes: tmux::SharedPanes,
    focused: Arc<AtomicBool>,
    dismissed: Dismissed,
) {
    let mut gh: Option<PathBuf> = None;
    let mut warned_no_gh = false;
    let mut cache: HashMap<(String, String), Entry> = HashMap::new();
    let memory_path = state::config_dir().join("pr_memory.json");
    let mut memory = load_memory(&memory_path);
    let mut last_sent: Option<HashMap<String, Vec<Badge>>> = None;
    let mut skipped = 0u32;

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

        if !focused.load(Ordering::Relaxed) && skipped + 1 < UNFOCUSED_EVERY {
            skipped += 1;
            std::thread::sleep(LOCAL_TICK);
            continue;
        }
        skipped = 0;

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

        // Chips right-clicked away since last tick: forgetting the branch
        // here (memory + cache) is what makes them stay gone.
        let mut dirty = false;
        for key in std::mem::take(&mut *dismissed.lock().unwrap()) {
            cache.remove(&key);
            if let Some(branches) = memory.get_mut(&key.0) {
                dirty |= branches.remove(&key.1).is_some();
                if branches.is_empty() {
                    memory.remove(&key.0);
                }
            }
        }

        // session -> (repo root, branch) for every pane in a checkout; one
        // `git rev-parse` per unique cwd, fanned back out to the sessions
        // sharing it, so a tab of panes in one checkout is one subprocess.
        // `roots` keeps every work-tree session, detached HEAD included:
        // remembered chips outlive the branch pointer.
        let mut keys: HashMap<String, (String, String)> = HashMap::new();
        let mut roots: HashMap<String, String> = HashMap::new();
        let mut by_cwd: HashMap<PathBuf, Option<(String, Option<String>)>> =
            HashMap::new();
        for (session, cwd) in pane_cwds(&panes) {
            let key = by_cwd
                .entry(cwd)
                .or_insert_with_key(|cwd| repo_branch(cwd))
                .clone();
            if let Some((root, branch)) = key {
                roots.insert(session.clone(), root.clone());
                if let Some(branch) = branch {
                    keys.insert(session, (root, branch));
                }
            }
        }
        let live_keys: HashSet<(String, String)> =
            keys.values().cloned().collect();
        let live_roots: HashSet<String> = roots.values().cloned().collect();

        // Remembered branches of live roots re-enter the cache as seeds
        // (lazily, so a slow tmux attach at startup can't wipe anything,
        // and closed tabs' PRs are never polled).
        for root in &live_roots {
            for (branch, number) in
                memory.get(root).into_iter().flatten()
            {
                cache
                    .entry((root.clone(), branch.clone()))
                    .or_insert_with(|| Entry {
                        at: Instant::now(),
                        fetched: Fetched::Seed,
                        number: Some(*number),
                    });
            }
        }

        cache.retain(|k, e| retain_entry(k, e, &live_keys, &live_roots));

        let mut targets = live_keys.clone();
        targets.extend(cache.keys().cloned());
        for key in targets {
            let stale = cache
                .get(&key)
                .is_none_or(|e| e.at.elapsed() > e.ttl());
            if !stale {
                continue;
            }
            let by_number =
                fetch_target(cache.get(&key), live_keys.contains(&key));
            let old = cache.remove(&key);
            let old_number = old.as_ref().and_then(|e| e.number);
            let fetched =
                match (fetch(gh_bin, &key.0, &key.1, by_number), old) {
                    // A branch lookup that found nothing must not erase a
                    // remembered PR (the rediscovery probe on a live
                    // branch whose remote was deleted after merge lands
                    // here): keep the last badge.
                    (Fetched::NoPr, Some(old)) if old.number.is_some() => {
                        old.fetched
                    },
                    (fetched, _) => fetched,
                };
            let number = match &fetched {
                Fetched::Badge(b) => {
                    let branches = memory.entry(key.0.clone()).or_default();
                    if branches.get(&key.1) != Some(&b.number) {
                        branches.insert(key.1.clone(), b.number);
                        dirty = true;
                    }
                    Some(b.number)
                },
                _ => old_number,
            };
            cache.insert(
                key,
                Entry { at: Instant::now(), fetched, number },
            );
        }

        // A root whose directory is gone (worktree removed) takes its
        // remembered PRs with it.
        memory.retain(|root, _| {
            let keep = Path::new(root).exists();
            dirty |= !keep;
            keep
        });
        if dirty {
            save_memory(&memory_path, &memory);
        }

        let snapshot = snapshot(&roots, &cache);
        if last_sent.as_ref() != Some(&snapshot) {
            log::debug!("pr_status: {snapshot:?}");
            last_sent = Some(snapshot.clone());
            let _ = tx.send(snapshot);
            ctx.request_repaint();
        }

        std::thread::sleep(LOCAL_TICK);
    }
}

/// Whether a cache entry survives this tick. A live (currently checked
/// out) key always does; a remembered PR (pinned number - badges and
/// seeds alike, errors included so back-off holds) lives as long as its
/// root does; scan by-products (NoPr/Error on a branch nobody is on) go.
fn retain_entry(
    key: &(String, String),
    entry: &Entry,
    live_keys: &HashSet<(String, String)>,
    live_roots: &HashSet<String>,
) -> bool {
    if live_keys.contains(key) {
        return true;
    }
    live_roots.contains(&key.0) && entry.number.is_some()
}

/// The PR number a refresh should ask gh for, or None for a branch
/// lookup. Pinned numbers win (they survive branch deletion after merge)
/// - except on a live branch whose PR already merged/closed: there the
/// branch lookup runs again, so a reused branch name finds its new PR.
fn fetch_target(entry: Option<&Entry>, live: bool) -> Option<u64> {
    let entry = entry?;
    let number = entry.number?;
    if live {
        if let Fetched::Badge(b) = &entry.fetched {
            if matches!(b.kind, Kind::Merged | Kind::Neutral) {
                return None;
            }
        }
    }
    Some(number)
}

/// session -> every remembered badge under that session's root, PR number
/// ascending. The order is load-bearing: `last_sent` diffs snapshots for
/// send-on-change, which only works when equal state renders equally.
fn snapshot(
    roots: &HashMap<String, String>,
    cache: &HashMap<(String, String), Entry>,
) -> HashMap<String, Vec<Badge>> {
    roots
        .iter()
        .filter_map(|(session, root)| {
            let mut badges: Vec<Badge> = cache
                .iter()
                .filter(|((r, _), _)| r == root)
                .filter_map(|(_, e)| match &e.fetched {
                    Fetched::Badge(b) => Some(b.clone()),
                    _ => None,
                })
                .collect();
            badges.sort_by_key(|b| b.number);
            badges.dedup_by_key(|b| b.number);
            (!badges.is_empty()).then(|| (session.clone(), badges))
        })
        .collect()
}

fn load_memory(path: &Path) -> Memory {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Best-effort, atomic like `state::save`: a torn memory file must never
/// eat the badges (load_memory shrugs it off anyway).
fn save_memory(path: &Path, memory: &Memory) {
    let Ok(json) = serde_json::to_string_pretty(memory) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, json).is_ok() {
        let _ = fs::rename(&tmp, path);
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

/// session -> cwd pairs out of the app's shared per-second pane snapshot.
/// Collected under a short lock so no subprocess ever runs while holding
/// it.
fn pane_cwds(panes: &tmux::SharedPanes) -> Vec<(String, PathBuf)> {
    let snap = panes.lock().unwrap();
    snap.iter()
        .filter_map(|(session, pane)| {
            Some((session.clone(), pane.cwd.clone()?))
        })
        .collect()
}

/// (repo root, branch) of a directory; None outside a work tree, branch
/// None on a detached HEAD (the root still scopes remembered chips).
/// /usr/bin/git is the macOS shim and always present.
fn repo_branch(cwd: &Path) -> Option<(String, Option<String>)> {
    let out = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let root = lines.next()?.trim().to_string();
    let branch = lines.next()?.trim().to_string();
    if root.is_empty() {
        return None;
    }
    let branch = (!branch.is_empty() && branch != "HEAD").then_some(branch);
    Some((root, branch))
}

/// One `gh pr view`, by pinned PR number when given (see `fetch_target`),
/// by branch otherwise. The badge is stamped with the (root, branch) key
/// it is cached and remembered under.
fn fetch(
    gh: &std::path::Path,
    root: &str,
    branch: &str,
    number: Option<u64>,
) -> Fetched {
    let target = match number {
        Some(n) => n.to_string(),
        None => branch.to_string(),
    };
    let out = Command::new(gh)
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .args([
            "pr",
            "view",
            &target,
            "--json",
            "number,title,url,state,isDraft,reviewDecision,statusCheckRollup",
        ])
        .output();
    match out {
        Ok(out) if out.status.success() => {
            match rollup(&String::from_utf8_lossy(&out.stdout), root, branch)
            {
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

/// Fold gh's JSON into one badge, stamped with the (root, branch) it is
/// tracked under. Pure; fixture-tested below.
pub fn rollup(json: &str, root: &str, branch: &str) -> Option<Badge> {
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
            Kind::Draft
        } else if review == Some("APPROVED") {
            Kind::Approved
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
        // The branch leads: with several chips on one pane, the tooltip
        // is what says which is which.
        detail: format!(
            "#{number} {title}\n{branch} · {}",
            phrases.join(" · ")
        ),
        root: root.to_string(),
        branch: branch.to_string(),
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

    fn roll(fields: &str) -> Option<Badge> {
        rollup(&pr(fields), "/repo", "feat-x")
    }

    #[test]
    fn merged_and_closed_win_over_checks() {
        let b = roll(r#""state":"MERGED","statusCheckRollup":[{"status":"COMPLETED","conclusion":"FAILURE"}]"#).unwrap();
        assert_eq!(b.kind, Kind::Merged);
        assert!(b.detail.contains("merged"));
        // closed-unmerged is its own state, not draft's
        let b = roll(r#""state":"CLOSED""#).unwrap();
        assert_eq!(b.kind, Kind::Neutral);
        assert_ne!(b.kind, Kind::Draft);
    }

    #[test]
    fn failing_checks_and_changes_requested_are_red() {
        let b = roll(r#""state":"OPEN","statusCheckRollup":[{"status":"COMPLETED","conclusion":"FAILURE"},{"status":"COMPLETED","conclusion":"SUCCESS"}]"#).unwrap();
        assert_eq!(b.kind, Kind::Err);
        assert!(b.detail.contains("checks failing"));
        let b = roll(r#""state":"OPEN","reviewDecision":"CHANGES_REQUESTED","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]"#).unwrap();
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
            let b = roll(&format!(
                r#""state":"OPEN","statusCheckRollup":{checks}"#
            ))
            .unwrap();
            assert_eq!(b.kind, Kind::Warn, "for {checks}");
        }
    }

    #[test]
    fn green_paths_and_draft() {
        // Approved (checks green) reads distinctly from a plain open PR.
        let b = roll(r#""state":"OPEN","reviewDecision":"APPROVED","statusCheckRollup":[{"state":"SUCCESS"}]"#).unwrap();
        assert_eq!(b.kind, Kind::Approved);
        assert!(b.detail.contains("approved"));
        // Open with passing checks but no approving review yet stays Ok, not
        // Approved - the whole point of the split.
        let b = roll(r#""state":"OPEN","statusCheckRollup":[{"state":"SUCCESS"}]"#).unwrap();
        assert_eq!(b.kind, Kind::Ok);
        assert!(b.detail.contains("checks passing"));
        assert!(!b.detail.contains("approved"));
        // no checks at all: an open healthy PR is still green
        let b = roll(r#""state":"OPEN""#).unwrap();
        assert_eq!(b.kind, Kind::Ok);
        assert!(b.detail.contains("open"));
        let b = roll(r#""state":"OPEN","isDraft":true,"statusCheckRollup":[{"state":"SUCCESS"}]"#).unwrap();
        assert_eq!(b.kind, Kind::Draft);
        assert!(b.detail.contains("draft"));
        // ...but a draft with failing checks still alarms
        let b = roll(r#""state":"OPEN","isDraft":true,"statusCheckRollup":[{"state":"FAILURE"}]"#).unwrap();
        assert_eq!(b.kind, Kind::Err);
    }

    #[test]
    fn tooltip_carries_number_title_and_branch() {
        let b = roll(r#""state":"OPEN""#).unwrap();
        assert_eq!(b.number, 12);
        assert_eq!(b.url, "https://github.com/a/b/pull/12");
        assert_eq!(b.root, "/repo");
        assert_eq!(b.branch, "feat-x");
        assert!(b.detail.starts_with("#12 Fix parser\nfeat-x · "));
    }

    #[test]
    fn garbage_json_is_none() {
        assert!(rollup("", "/repo", "b").is_none());
        assert!(rollup("{}", "/repo", "b").is_none());
        assert!(rollup(r#"{"number":"nan"}"#, "/repo", "b").is_none());
    }

    fn key(root: &str, branch: &str) -> (String, String) {
        (root.to_string(), branch.to_string())
    }

    fn entry(fetched: Fetched, number: Option<u64>) -> Entry {
        Entry { at: Instant::now(), fetched, number }
    }

    fn badge(number: u64, kind: Kind, root: &str, branch: &str) -> Badge {
        Badge {
            number,
            url: format!("https://github.com/a/b/pull/{number}"),
            kind,
            detail: format!("#{number}"),
            root: root.to_string(),
            branch: branch.to_string(),
        }
    }

    /// Live keys always survive; remembered PRs (pinned number, Error
    /// back-offs included) ride their root's liveness; branch-scan
    /// by-products don't outlive their checkout.
    #[test]
    fn retention_follows_pins_and_roots() {
        let live_keys: HashSet<(String, String)> =
            [key("/repo", "main")].into();
        let live_roots: HashSet<String> = ["/repo".to_string()].into();
        let b = badge(7, Kind::Ok, "/repo", "old");
        for (name, key, entry, keep) in [
            ("live NoPr", key("/repo", "main"), entry(Fetched::NoPr, None), true),
            ("off-branch badge", key("/repo", "old"), entry(Fetched::Badge(b.clone()), Some(7)), true),
            ("off-branch seed", key("/repo", "old"), entry(Fetched::Seed, Some(7)), true),
            ("off-branch pinned error", key("/repo", "old"), entry(Fetched::Error, Some(7)), true),
            ("off-branch NoPr", key("/repo", "gone"), entry(Fetched::NoPr, None), false),
            ("dead root badge", key("/other", "x"), entry(Fetched::Badge(b.clone()), Some(7)), false),
        ] {
            assert_eq!(
                retain_entry(&key, &entry, &live_keys, &live_roots),
                keep,
                "{name}"
            );
        }
    }

    /// Pinned numbers drive refreshes, except the reused-branch probe: a
    /// live branch whose PR merged/closed goes back to a branch lookup.
    #[test]
    fn fetch_target_pins_except_closed_live() {
        assert_eq!(fetch_target(None, true), None);
        let open = entry(
            Fetched::Badge(badge(7, Kind::Ok, "/r", "b")),
            Some(7),
        );
        assert_eq!(fetch_target(Some(&open), true), Some(7));
        assert_eq!(fetch_target(Some(&open), false), Some(7));
        let merged = entry(
            Fetched::Badge(badge(7, Kind::Merged, "/r", "b")),
            Some(7),
        );
        assert_eq!(fetch_target(Some(&merged), true), None);
        assert_eq!(fetch_target(Some(&merged), false), Some(7));
        let seed = entry(Fetched::Seed, Some(7));
        assert_eq!(fetch_target(Some(&seed), true), Some(7));
    }

    /// Every remembered badge under a session's root shows, ascending and
    /// deduped; seeds and errors don't; a detached-HEAD session (root
    /// without branch) still gets its root's chips; roots stay isolated.
    #[test]
    fn snapshot_gathers_by_root() {
        let roots: HashMap<String, String> = [
            ("mux-a".to_string(), "/repo".to_string()),
            ("mux-detached".to_string(), "/repo".to_string()),
            ("mux-b".to_string(), "/other".to_string()),
            ("mux-quiet".to_string(), "/quiet".to_string()),
        ]
        .into();
        let cache: HashMap<(String, String), Entry> = [
            (
                key("/repo", "feat-b"),
                entry(
                    Fetched::Badge(badge(9, Kind::Ok, "/repo", "feat-b")),
                    Some(9),
                ),
            ),
            (
                key("/repo", "feat-a"),
                entry(
                    Fetched::Badge(badge(4, Kind::Merged, "/repo", "feat-a")),
                    Some(4),
                ),
            ),
            // renamed branch pinning the same PR: deduped
            (
                key("/repo", "feat-a2"),
                entry(
                    Fetched::Badge(badge(4, Kind::Merged, "/repo", "feat-a2")),
                    Some(4),
                ),
            ),
            (key("/repo", "seeded"), entry(Fetched::Seed, Some(2))),
            (
                key("/other", "main"),
                entry(
                    Fetched::Badge(badge(1, Kind::Err, "/other", "main")),
                    Some(1),
                ),
            ),
            (key("/quiet", "main"), entry(Fetched::NoPr, None)),
        ]
        .into();

        let snap = snapshot(&roots, &cache);
        let nums = |s: &str| -> Vec<u64> {
            snap.get(s)
                .map(|v| v.iter().map(|b| b.number).collect())
                .unwrap_or_default()
        };
        assert_eq!(nums("mux-a"), vec![4, 9]);
        assert_eq!(nums("mux-detached"), vec![4, 9]);
        assert_eq!(nums("mux-b"), vec![1]);
        // all-quiet sessions are absent, not empty (matches the old
        // single-badge semantics the App renders against)
        assert!(!snap.contains_key("mux-quiet"));
    }

    /// Each Kind paints its own shape, not just its own color (the
    /// sidebar's color-blindness contract): ✗ segments red, pending dot
    /// yellow, ✓ segments green, draft ring hollow, merge glyph magenta
    /// dots, closed minus a dim bar.
    #[test]
    fn icons_paint_distinct_shapes() {
        use crate::theme;

        let ctx = egui::Context::default();
        let preset = theme::preset("iterm-dark").unwrap();
        let (_, th) = theme::build(preset, &HashMap::new(), 0.12);
        let hud = theme::hud_colors(&th);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(400.0, 100.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for (i, kind) in [
                    Kind::Err,
                    Kind::Warn,
                    Kind::Ok,
                    Kind::Approved,
                    Kind::Merged,
                    Kind::Draft,
                    Kind::Neutral,
                ]
                .into_iter()
                .enumerate()
                {
                    kind.draw_icon(
                        ui.painter(),
                        Pos2::new(20.0 + 30.0 * i as f32, 50.0),
                        11.0,
                        &hud,
                    );
                }
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

        let segment_in = |color: Color32| {
            shapes.iter().any(|s| {
                matches!(s, egui::Shape::LineSegment { stroke, .. }
                    if stroke.color == color)
            })
        };
        assert!(segment_in(th.status_err), "failing ✗ not painted");
        assert!(segment_in(th.status_ok), "ready ✓ not painted");
        assert!(segment_in(th.status_merged), "merge glyph lines missing");
        assert!(
            shapes.iter().any(|s| matches!(s, egui::Shape::Circle(c)
                if c.fill == th.status_warn)),
            "pending dot not painted"
        );
        assert!(
            shapes.iter().any(|s| matches!(s, egui::Shape::Circle(c)
                if c.fill == th.status_merged)),
            "merge glyph dots missing"
        );
        assert!(
            shapes.iter().any(|s| matches!(s, egui::Shape::Circle(c)
                if c.fill == Color32::TRANSPARENT && c.stroke.width > 0.0)),
            "draft ring not painted"
        );
        assert!(
            shapes.iter().any(|s| matches!(s, egui::Shape::Rect(r)
                if r.fill == th.text_dim)),
            "closed minus not painted"
        );
    }

    #[test]
    fn memory_file_round_trips() {
        let path = std::env::temp_dir().join(format!(
            "muxterm-pr-memory-test-{}.json",
            std::process::id()
        ));
        assert!(load_memory(&path).is_empty());
        let mut memory = Memory::new();
        memory
            .entry("/repo".to_string())
            .or_default()
            .insert("feat-x".to_string(), 12);
        save_memory(&path, &memory);
        assert_eq!(load_memory(&path), memory);
        let _ = fs::remove_file(&path);
    }
}
