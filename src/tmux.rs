use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};
use egui_term::BackendSettings;

/// Dedicated tmux server socket: muxterm sessions never touch the user's
/// default tmux server, which also makes the startup GC safe.
pub const SOCKET: &str = "muxterm";
const SESSION_PREFIX: &str = "mux-";

/// Regenerated on every launch (it only applies when the server starts).
/// `status off` makes sessions look like a plain terminal; the `Ms` override
/// makes tmux emit OSC 52 on copy, which surfaces as PtyEvent::ClipboardStore.
const CONF: &str = r##"# managed by muxterm - regenerated at every launch
set -g status off
set -g mouse on
set -s escape-time 0
set -g history-limit 100000
set -g default-terminal "tmux-256color"
set -g set-titles on
set -g set-titles-string "#{pane_current_command}"
set -s set-clipboard on
set -as terminal-overrides ',xterm*:Ms=\E]52;%p1%s;%p2%s\007'
set -g focus-events on
setw -g aggressive-resize on
bind -n S-PPage copy-mode -u
"##;

pub struct TmuxCtl {
    bin: PathBuf,
    conf: PathBuf,
}

impl TmuxCtl {
    /// PATH is not guaranteed when launched outside a shell, so probe the
    /// usual install locations before falling back to `which`.
    pub fn discover(config_dir: &Path) -> Result<Self> {
        let candidates =
            ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux", "/usr/bin/tmux"];
        let bin = candidates
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .or_else(|| {
                let out = Command::new("which").arg("tmux").output().ok()?;
                if !out.status.success() {
                    return None;
                }
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                (!path.is_empty()).then(|| PathBuf::from(path))
            })
            .context(
                "tmux not found - install it with `brew install tmux` and relaunch muxterm",
            )?;
        Ok(Self {
            bin,
            conf: config_dir.join("tmux.conf"),
        })
    }

    pub fn write_conf(&self) -> Result<()> {
        if let Some(parent) = self.conf.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.conf, CONF)?;
        Ok(())
    }

    pub fn new_session_name() -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        format!("{SESSION_PREFIX}{}", &id[..8])
    }

    /// The whole trick of muxterm: the pane's PTY runs a tmux client.
    /// `-A` attaches if the session exists and creates it otherwise, so
    /// restore-after-relaunch and fresh spawn are the same code path.
    /// `-D` kicks any stale client so pane sizing is never fought over.
    /// `-c` sets the new shell's start directory (ignored on attach).
    pub fn spawn_settings(
        &self,
        session: &str,
        start_dir: Option<String>,
    ) -> BackendSettings {
        let mut args = vec![
            "-L".into(),
            SOCKET.into(),
            "-f".into(),
            self.conf.display().to_string(),
            "new-session".into(),
            "-A".into(),
            "-D".into(),
            "-s".into(),
            session.into(),
        ];
        if let Some(dir) = start_dir {
            args.push("-c".into());
            args.push(dir);
        }
        BackendSettings {
            shell: self.bin.display().to_string(),
            args,
            working_directory: None,
        }
    }

    /// Current working directory of a session's active pane, so splits and
    /// new tabs can start where the user is.
    pub fn pane_current_path(&self, session: &str) -> Option<String> {
        let out = Command::new(&self.bin)
            .args([
                "-L",
                SOCKET,
                "list-panes",
                "-t",
                &format!("={session}"),
                "-F",
                "#{pane_current_path}",
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let path = stdout.lines().next().unwrap_or("").trim().to_string();
        (!path.is_empty()).then_some(path)
    }

    /// `=` forces an exact match; `-t name` alone prefix-matches.
    pub fn kill_session(&self, session: &str) {
        let _ = Command::new(&self.bin)
            .args(["-L", SOCKET, "kill-session", "-t", &format!("={session}")])
            .output();
    }

    pub fn list_sessions(&self) -> Vec<String> {
        match Command::new(&self.bin)
            .args(["-L", SOCKET, "list-sessions", "-F", "#{session_name}"])
            .output()
        {
            // A non-zero exit just means no server is running on the socket.
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::to_owned)
                    .collect()
            },
            _ => Vec::new(),
        }
    }

    /// Kill muxterm-owned sessions that no saved pane references (panes whose
    /// Exit event raced an app crash, etc.). Never called when the state file
    /// failed to parse - a corrupt state must not cost live sessions.
    pub fn gc(&self, referenced: &HashSet<String>) {
        for session in self.list_sessions() {
            if session.starts_with(SESSION_PREFIX)
                && !referenced.contains(&session)
            {
                log::info!("gc: killing unreferenced session {session}");
                self.kill_session(&session);
            }
        }
    }
}
