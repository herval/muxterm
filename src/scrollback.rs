//! Periodic pane-scrollback capture for reboot recovery (config
//! `restore_scrollback`). muxterm's persistence trick only survives an app
//! *quit* - which merely detaches the tmux clients, leaving the `-L muxterm`
//! server (and every session's scrollback) alive. A machine reboot kills that
//! server outright, so scrollback that lived only in it is gone.
//!
//! This background thread snapshots each live pane's recent scrollback to
//! `~/.muxterm/scrollback/<session>.txt` on a slow cadence, so a post-reboot
//! restore can replay it back into the fresh pane (`app::restore_tab`).
//! Cosmetic and best-effort: it captures plain text (no ANSI), never blocks
//! the UI (own thread, reading the shared pane snapshot like `git_status`
//! rather than spawning its own `list-panes`), and prunes files whose session
//! is gone.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use muxterm::state;

use crate::tmux::{SharedPanes, TmuxCtl};

/// Capture cadence. Slow on purpose: scrollback is only ever read after a
/// reboot, and a reboot loses at most this much of each pane's tail.
const TICK: Duration = Duration::from_secs(30);
/// Lines of tail kept per pane. tmux holds up to its history-limit (100k); a
/// couple thousand is ample replay context and bounds each file's size.
const LINES: u32 = 2000;

/// `~/.muxterm/scrollback` - the live captures the poller writes.
pub fn dir() -> PathBuf {
    state::config_dir().join("scrollback")
}

/// `~/.muxterm/scrollback-prev` - last run's captures, rotated aside at launch
/// so the restarting poller's fresh (empty-shell) writes can't clobber them
/// before a restore replays them.
fn prev_dir() -> PathBuf {
    state::config_dir().join("scrollback-prev")
}

/// Rotate the previous run's captures aside (`scrollback` -> `scrollback-prev`,
/// discarding any older rotation). Called once at launch, before the poller
/// starts, and only when a cold restore may want to replay. The poller
/// recreates `scrollback/` on its first write; `scrollback-prev` then sits
/// untouched for the whole session, so replaying from it never races a write.
pub fn rotate() {
    let (dir, prev) = (dir(), prev_dir());
    if !dir.exists() {
        return;
    }
    let _ = fs::remove_dir_all(&prev);
    let _ = fs::rename(&dir, &prev);
}

/// The rotated-aside capture file for a session, if one exists - the source
/// `app::restore_tab` replays on a cold restore (via `cat`, so no scrollback
/// text is ever fed to the shell as a command).
pub fn recovered_file(session: &str) -> Option<PathBuf> {
    let p = prev_dir().join(format!("{session}.txt"));
    p.is_file().then_some(p)
}

/// Spawn the capture poller. Idles when `enabled` is off so the config toggle
/// applies live, and sleeps one TICK before its first capture so a just-
/// rotated `scrollback-prev` is never shadowed by an immediate empty-shell
/// snapshot. Mirrors `git_status::spawn`.
pub fn spawn(tmux: TmuxCtl, enabled: Arc<AtomicBool>, panes: SharedPanes) {
    std::thread::Builder::new()
        .name("scrollback".into())
        .spawn(move || run(tmux, enabled, panes))
        .expect("spawn scrollback thread");
}

fn run(tmux: TmuxCtl, enabled: Arc<AtomicBool>, panes: SharedPanes) {
    loop {
        std::thread::sleep(TICK);
        if !enabled.load(Ordering::Relaxed) {
            continue;
        }
        let sessions = live_sessions(&panes);
        if sessions.is_empty() {
            continue;
        }
        if let Err(e) = fs::create_dir_all(dir()) {
            log::warn!("scrollback: cannot create capture dir: {e:#}");
            continue;
        }
        let mut kept: HashSet<String> = HashSet::new();
        for session in sessions {
            if let Some(text) = tmux.capture_pane(&session, LINES) {
                let path = dir().join(format!("{session}.txt"));
                if let Err(e) = fs::write(&path, text) {
                    log::warn!("scrollback: write {} failed: {e:#}", path.display());
                } else {
                    kept.insert(session);
                }
            }
        }
        prune(&kept);
    }
}

/// Live `mux-*` session names from the app's shared per-second snapshot (no
/// tmux spawn here), collected under a short lock (same as `git_status`).
fn live_sessions(panes: &SharedPanes) -> Vec<String> {
    panes.lock().unwrap().keys().cloned().collect()
}

/// Drop capture files whose session is no longer live, so a killed pane's
/// scrollback can't linger and be replayed into an unrelated future pane.
fn prune(kept: &HashSet<String>) {
    let Ok(entries) = fs::read_dir(dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(session) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".txt"))
        else {
            continue;
        };
        if !kept.contains(session) {
            let _ = fs::remove_file(&path);
        }
    }
}
