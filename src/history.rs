//! Session history: what muxterm had open, kept after it's gone, so a lost
//! layout can be inspected and brought back (`mux history`).
//!
//! `state.json` is the *current* layout and nothing else - every save
//! overwrites it, so one bad cascade (every pane exiting at once) or a
//! relaunch over a bad state leaves no trace of what was there before. This
//! module keeps two things beside it under `~/.muxterm/history/`:
//!
//! - `snapshots/<unix>-<reason>.json`: full copies of the state file. The GUI
//!   writes one at launch (what it loaded), when a tab or pane is about to be
//!   lost (the *outgoing* layout, debounced so a burst of closes keeps the
//!   state from before the burst), periodically while the layout moves, and
//!   right before recovering from a dead tmux server. The policy is the pure
//!   `Journal`, so it unit-tests without a clock.
//! - `scrollback/<session>.<unix>.txt`: the scrollback poller's captures of
//!   sessions that went away, archived instead of deleted
//!   (`scrollback::prune` / `rotate`).
//!
//! Both are pruned by age and count. The GUI owns writing snapshots; the CLI
//! only reads them, and asks the GUI to reopen tabs from one through a spool
//! (`RestoreRequest` under `history-req/`) - the GUI owns `state.json`.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::{self, StateFile};

/// Snapshots kept regardless of age, and the most kept at all.
const KEEP_MIN: usize = 20;
const KEEP_MAX: usize = 300;
/// Snapshots older than this are pruned (above KEEP_MIN).
const SNAPSHOT_MAX_AGE: u64 = 30 * 24 * 3600;
/// Archived scrollback older than this is pruned.
const SCROLLBACK_MAX_AGE: u64 = 14 * 24 * 3600;
/// A burst of closes inside this window is one event: only the layout from
/// before the first close is kept.
pub const LOSS_DEBOUNCE: u64 = 60;
/// A layout that keeps changing without losing anything is still snapshot
/// this often, so the history isn't only ever "right before a close".
pub const PERIODIC: u64 = 10 * 60;

pub fn dir() -> PathBuf {
    state::config_dir().join("history")
}

pub fn snapshots_dir() -> PathBuf {
    dir().join("snapshots")
}

pub fn scrollback_dir() -> PathBuf {
    dir().join("scrollback")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ------------------------------------------------------------- snapshots

/// What identifies a layout for the journal: every tab id and every pane
/// session. Titles, cwds and agents moving are not structural - they ride
/// along in whatever snapshot is taken next.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Shape {
    tabs: BTreeSet<String>,
    sessions: BTreeSet<String>,
}

impl Shape {
    pub fn of(state: &StateFile) -> Shape {
        let mut shape = Shape::default();
        for w in &state.windows {
            for t in &w.tabs {
                shape.tabs.insert(t.id.clone());
                let mut s = HashSet::new();
                t.tree.sessions(&mut s);
                shape.sessions.extend(s);
            }
        }
        shape
    }

    /// Did going from `self` to `next` drop a tab or a pane?
    fn loses(&self, next: &Shape) -> bool {
        !self.tabs.is_subset(&next.tabs)
            || !self.sessions.is_subset(&next.sessions)
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// When to snapshot, decided from successive saves. Holds the previous
/// save's serialized state so a loss can keep the layout *before* it.
#[derive(Default)]
pub struct Journal {
    prev: Option<(Shape, String)>,
    last_write: Option<u64>,
    last_written: Option<Shape>,
}

impl Journal {
    /// Record that a snapshot was just written out of band (the launch one,
    /// the pre-recovery one), so the debounce and dedupe see it.
    pub fn wrote(&mut self, shape: Shape, now: u64) {
        self.last_write = Some(now);
        self.last_written = Some(shape);
    }

    /// A save of `json` (shaped `shape`) is happening at `now`: returns the
    /// snapshot to write, if any, as (json, reason).
    ///
    /// - A tab or pane disappeared: keep the *previous* layout, unless a
    ///   snapshot was taken within `LOSS_DEBOUNCE` (then this is the tail of
    ///   a burst whose start is already kept).
    /// - Otherwise, once per `PERIODIC`, keep the current layout if its
    ///   shape isn't the one last kept.
    pub fn observe(
        &mut self,
        shape: Shape,
        json: String,
        now: u64,
    ) -> Option<(String, &'static str)> {
        let since = self.last_write.map(|t| now.saturating_sub(t));
        let mut out = None;
        if let Some((prev_shape, prev_json)) = &self.prev {
            if prev_shape.loses(&shape)
                && !prev_shape.is_empty()
                && since.is_none_or(|s| s >= LOSS_DEBOUNCE)
            {
                self.last_written = Some(prev_shape.clone());
                out = Some((prev_json.clone(), "before-close"));
            }
        }
        if out.is_none()
            && !shape.is_empty()
            && self.last_written.as_ref() != Some(&shape)
            && since.is_none_or(|s| s >= PERIODIC)
        {
            self.last_written = Some(shape.clone());
            out = Some((json.clone(), "periodic"));
        }
        if out.is_some() {
            self.last_write = Some(now);
        }
        self.prev = Some((shape, json));
        out
    }
}

/// One snapshot on disk.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// File stem, `<unix>-<reason>` - what the CLI accepts to name it.
    pub id: String,
    pub ts: u64,
    pub reason: String,
    pub path: PathBuf,
}

/// Write a serialized state as a snapshot (then prune). Returns its id.
pub fn write_snapshot_json(json: &str, reason: &str) -> anyhow::Result<String> {
    let dir = snapshots_dir();
    fs::create_dir_all(&dir)?;
    let ts = now();
    let mut id = format!("{ts}-{reason}");
    let mut n = 1;
    while dir.join(format!("{id}.json")).exists() {
        n += 1;
        id = format!("{ts}-{reason}-{n}");
    }
    let tmp = dir.join(format!(".{id}.tmp"));
    fs::write(&tmp, json)?;
    fs::rename(&tmp, dir.join(format!("{id}.json")))?;
    prune_snapshots(now());
    Ok(id)
}

pub fn write_snapshot(state: &StateFile, reason: &str) -> anyhow::Result<String> {
    write_snapshot_json(&serde_json::to_string_pretty(state)?, reason)
}

fn parse_id(stem: &str) -> Option<(u64, String)> {
    let (ts, reason) = stem.split_once('-')?;
    Some((ts.parse().ok()?, reason.to_string()))
}

/// Every snapshot, newest first.
pub fn list_snapshots() -> Vec<Snapshot> {
    let Ok(entries) = fs::read_dir(snapshots_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<Snapshot> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            let (ts, reason) = parse_id(&id)?;
            Some(Snapshot { id, ts, reason, path })
        })
        .collect();
    out.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.id.cmp(&a.id)));
    out
}

/// Name a snapshot the way the CLI lets you: `latest` (or nothing), a list
/// index as `mux history list` prints it (`1` = newest), or an id / id
/// prefix. An ambiguous prefix names nothing.
pub fn resolve<'a>(snaps: &'a [Snapshot], spec: Option<&str>) -> Option<&'a Snapshot> {
    let spec = spec.map(str::trim).unwrap_or("latest");
    if spec.is_empty() || spec == "latest" {
        return snaps.first();
    }
    if spec.len() < 6 {
        if let Ok(i) = spec.parse::<usize>() {
            return i.checked_sub(1).and_then(|i| snaps.get(i));
        }
    }
    let mut hits = snaps.iter().filter(|s| s.id.starts_with(spec));
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

pub fn load(snap: &Snapshot) -> anyhow::Result<StateFile> {
    let text = fs::read_to_string(&snap.path)?;
    Ok(serde_json::from_str(&text)?)
}

/// Keep the newest KEEP_MIN no matter what; beyond that drop anything older
/// than SNAPSHOT_MAX_AGE, and never keep more than KEEP_MAX.
fn prune_snapshots(now: u64) {
    for (i, snap) in list_snapshots().iter().enumerate() {
        let old = now.saturating_sub(snap.ts) > SNAPSHOT_MAX_AGE;
        if i >= KEEP_MAX || (i >= KEEP_MIN && old) {
            let _ = fs::remove_file(&snap.path);
        }
    }
}

// ------------------------------------------------------------- scrollback

/// Move a capture file of a session that went away into the archive
/// (`<session>.<unix>.txt`, stamped with the capture's mtime so re-used
/// session names keep every version). Falls back to a copy across volumes.
pub fn archive_scrollback(src: &Path, session: &str) {
    let dir = scrollback_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = fs::metadata(src)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or_else(now);
    let dst = dir.join(format!("{session}.{ts}.txt"));
    if fs::rename(src, &dst).is_err() && fs::copy(src, &dst).is_ok() {
        let _ = fs::remove_file(src);
    }
    prune_scrollback(now());
}

/// Every archived capture of `session`, newest first.
pub fn archived_scrollback(session: &str) -> Vec<(u64, PathBuf)> {
    let Ok(entries) = fs::read_dir(scrollback_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?;
            let ts = name
                .strip_suffix(".txt")?
                .strip_prefix(session)?
                .strip_prefix('.')?
                .parse()
                .ok()?;
            Some((ts, path))
        })
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

fn prune_scrollback(now: u64) {
    let Ok(entries) = fs::read_dir(scrollback_dir()) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let ts = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.rsplit_once('.'))
            .and_then(|(_, ts)| ts.parse::<u64>().ok());
        if ts.is_some_and(|ts| now.saturating_sub(ts) > SCROLLBACK_MAX_AGE) {
            let _ = fs::remove_file(&path);
        }
    }
}

// ------------------------------------------------------------- restore spool

/// `mux history restore` -> GUI: reopen these tabs from this snapshot. The
/// GUI re-reads the snapshot itself (the spool carries only names) and skips
/// any tab already open or whose sessions an open pane holds.
#[derive(Serialize, Deserialize, Debug)]
pub struct RestoreRequest {
    pub v: u32,
    pub ts: u64,
    /// Request id; the GUI answers in `<id>.done` (a JSON `RestoreResult`).
    pub id: String,
    pub snapshot: String,
    /// Tab ids to reopen.
    pub tabs: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct RestoreResult {
    pub restored: Vec<String>,
    /// (tab id, why it was skipped).
    pub skipped: Vec<(String, String)>,
}

pub fn restore_spool() -> PathBuf {
    state::config_dir().join("history-req")
}

pub fn write_restore_request(req: &RestoreRequest) -> anyhow::Result<()> {
    let dir = restore_spool();
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{}.tmp", req.id));
    fs::write(&tmp, serde_json::to_string(req)?)?;
    fs::rename(&tmp, dir.join(format!("{}.json", req.id)))?;
    Ok(())
}

/// Drain pending requests (the GUI's poll tick). Unparseable files are
/// dropped: nothing waits on a request it couldn't have written.
pub fn take_restore_requests() -> Vec<RestoreRequest> {
    let Ok(entries) = fs::read_dir(restore_spool()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Ok(req) = fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|t| Ok(serde_json::from_str::<RestoreRequest>(&t)?))
        {
            out.push(req);
        }
        let _ = fs::remove_file(&path);
    }
    out
}

pub fn write_restore_result(id: &str, result: &RestoreResult) {
    let path = restore_spool().join(format!("{id}.done"));
    if let Ok(text) = serde_json::to_string(result) {
        let _ = fs::write(path, text);
    }
}

/// The GUI's answer to a request, consumed (read once, then removed).
pub fn take_restore_result(id: &str) -> Option<RestoreResult> {
    let path = restore_spool().join(format!("{id}.done"));
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{NodeState, TabState, WindowState};

    fn state_with(tabs: &[(&str, &[&str])]) -> StateFile {
        let tabs = tabs
            .iter()
            .map(|(id, sessions)| {
                let mut leaves = sessions.iter().map(|s| NodeState::Leaf {
                    session: s.to_string(),
                    cwd: None,
                    name: String::new(),
                    agent: None,
                });
                let first = leaves.next().unwrap();
                let tree = leaves.fold(first, |acc, leaf| NodeState::Split {
                    axis: crate::layout::SplitAxis::SideBySide,
                    ratio: 0.5,
                    first: Box::new(acc),
                    second: Box::new(leaf),
                });
                TabState {
                    id: id.to_string(),
                    tree,
                    focused_session: sessions[0].to_string(),
                    workspace: None,
                }
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "version": state::VERSION,
            "windows": [],
        }))
        .map(|mut s: StateFile| {
            s.windows = vec![WindowState { tabs, active_tab: 0 }];
            s
        })
        .unwrap()
    }

    fn observe(j: &mut Journal, s: &StateFile, now: u64) -> Option<(String, &'static str)> {
        j.observe(Shape::of(s), serde_json::to_string(s).unwrap(), now)
    }

    #[test]
    fn a_close_keeps_the_layout_from_before_it() {
        let mut j = Journal::default();
        let full = state_with(&[("t1", &["a", "b"]), ("t2", &["c"])]);
        let first = observe(&mut j, &full, 1000);
        assert_eq!(first.map(|(_, r)| r), Some("periodic"));
        // Nothing lost, same shape: nothing to keep.
        assert!(observe(&mut j, &full, 1100).is_none());
        // A pane closes after the debounce: the *previous* layout is kept.
        let fewer = state_with(&[("t1", &["a"]), ("t2", &["c"])]);
        let (json, reason) = observe(&mut j, &fewer, 1200).unwrap();
        assert_eq!(reason, "before-close");
        assert!(json.contains("\"b\""));
    }

    #[test]
    fn a_burst_of_closes_keeps_only_the_state_before_the_burst() {
        // The cascade that lost every workspace: all panes exit within a
        // second. The first close keeps the full layout; the rest are its
        // tail and keep nothing - in particular not the empty end state.
        let mut j = Journal::default();
        let full = state_with(&[("t1", &["a"]), ("t2", &["b"]), ("t3", &["c"])]);
        observe(&mut j, &full, 1000);
        let (json, _) = observe(&mut j, &state_with(&[("t2", &["b"]), ("t3", &["c"])]), 2000).unwrap();
        assert!(json.contains("\"t1\""));
        assert!(observe(&mut j, &state_with(&[("t3", &["c"])]), 2000).is_none());
        let empty = state_with(&[("t9", &["z"])]);
        assert!(observe(&mut j, &empty, 2001).is_none());
    }

    #[test]
    fn growth_is_snapshot_periodically_not_on_every_save() {
        let mut j = Journal::default();
        let one = state_with(&[("t1", &["a"])]);
        j.wrote(Shape::of(&one), 1000);
        observe(&mut j, &one, 1000);
        let two = state_with(&[("t1", &["a"]), ("t2", &["b"])]);
        assert!(observe(&mut j, &two, 1000 + 60).is_none());
        let (_, reason) = observe(&mut j, &two, 1000 + PERIODIC).unwrap();
        assert_eq!(reason, "periodic");
        // Kept already: the same shape isn't kept again.
        assert!(observe(&mut j, &two, 1000 + 3 * PERIODIC).is_none());
    }

    #[test]
    fn resolve_by_latest_index_or_prefix() {
        let snap = |id: &str| Snapshot {
            id: id.into(),
            ts: parse_id(id).unwrap().0,
            reason: parse_id(id).unwrap().1,
            path: PathBuf::new(),
        };
        let snaps = vec![
            snap("1790361000-periodic"),
            snap("1790360000-before-close"),
            snap("1790350000-launch"),
        ];
        assert_eq!(resolve(&snaps, None).unwrap().id, "1790361000-periodic");
        assert_eq!(resolve(&snaps, Some("latest")).unwrap().id, "1790361000-periodic");
        assert_eq!(resolve(&snaps, Some("2")).unwrap().id, "1790360000-before-close");
        assert!(resolve(&snaps, Some("9")).is_none());
        assert_eq!(resolve(&snaps, Some("179035")).unwrap().id, "1790350000-launch");
        // Ambiguous prefix names nothing.
        assert!(resolve(&snaps, Some("17903")).is_none());
    }
}
