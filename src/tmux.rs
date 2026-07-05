use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use egui_term::BackendSettings;
// Dedicated tmux server socket: muxterm sessions never touch the user's
// default tmux server, which also makes the startup GC safe. Constants and
// binary discovery are shared with the `mux` agent-mesh CLI.
use muxterm::mesh::{find_tmux, SESSION_PREFIX, SOCKET};

/// Regenerated on every launch (it only applies when the server starts) and
/// re-sourced into a running server when copy_on_select changes.
/// `status off` makes sessions look like a plain terminal; the `Ms` override
/// makes tmux emit OSC 52 on copy, which surfaces as PtyEvent::ClipboardStore.
const CONF_BASE: &str = r##"# managed by muxterm - regenerated at every launch
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

/// Mouse drags inside panes are driven by tmux copy-mode, so copy-on-select
/// for them is a tmux binding, not app code. Both values are spelled out
/// explicitly (`on` is tmux's own default) so that re-sourcing the file
/// flips a running server in either direction:
/// - on: releasing a drag copies the selection (OSC 52 -> clipboard).
/// - off: releasing keeps the selection on screen and copies nothing;
///   cmd+c (App::copy_intercept) does the explicit copy.
fn conf(copy_on_select: bool) -> String {
    let drag_end = if copy_on_select {
        "bind -T copy-mode MouseDragEnd1Pane send-keys -X copy-selection-and-cancel\n\
         bind -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-selection-and-cancel\n"
    } else {
        "unbind -T copy-mode MouseDragEnd1Pane\n\
         unbind -T copy-mode-vi MouseDragEnd1Pane\n"
    };
    format!("{CONF_BASE}{drag_end}")
}

pub struct TmuxCtl {
    bin: PathBuf,
    conf: PathBuf,
}

impl TmuxCtl {
    pub fn discover(config_dir: &Path) -> Result<Self> {
        Ok(Self {
            bin: find_tmux()?,
            conf: config_dir.join("tmux.conf"),
        })
    }

    pub fn write_conf(&self, copy_on_select: bool) -> Result<()> {
        if let Some(parent) = self.conf.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.conf, conf(copy_on_select))?;
        Ok(())
    }

    /// Apply the conf to an already-running server (config files are only
    /// read at server start). Silently a no-op when no server is up.
    pub fn source_conf(&self) {
        let _ = Command::new(&self.bin)
            .args(["-L", SOCKET, "source-file"])
            .arg(&self.conf)
            .output();
    }

    pub fn new_session_name() -> String {
        muxterm::mesh::new_session_name()
    }

    /// The whole trick of muxterm: the pane's PTY runs a tmux client.
    /// `-A` attaches if the session exists and creates it otherwise, so
    /// restore-after-relaunch and fresh spawn are the same code path.
    /// `-D` kicks any stale client so pane sizing is never fought over.
    /// `-c` sets the new shell's start directory (ignored on attach).
    /// `-e` marks the pane environment for agent-mesh detection (also
    /// ignored on attach - pre-existing sessions keep their environment).
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
            "-e".into(),
            "MUXTERM=1".into(),
            "-e".into(),
            format!("MUXTERM_SESSION={session}"),
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

    /// Foreground process of the session's active pane ("zsh", "vim", ...),
    /// so the "? " prompt only ever triggers at a shell.
    pub fn pane_current_command(&self, session: &str) -> Option<String> {
        let out = Command::new(&self.bin)
            .args([
                "-L",
                SOCKET,
                "list-panes",
                "-t",
                &format!("={session}"),
                "-F",
                "#{pane_current_command}",
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let cmd = stdout.lines().next().unwrap_or("").trim().to_string();
        (!cmd.is_empty()).then_some(cmd)
    }

    /// Last `lines` of the pane's content including scrollback, as plain
    /// text (`-J` rejoins wrapped lines), for the AI agent's context.
    /// Pane-scoped commands need the `=name:` target form (tmux >= 3.7
    /// rejects a bare `=name` here, unlike list-panes).
    pub fn capture_pane(&self, session: &str, lines: u32) -> Option<String> {
        let out = Command::new(&self.bin)
            .args([
                "-L",
                SOCKET,
                "capture-pane",
                "-p",
                "-J",
                "-S",
                &format!("-{lines}"),
                "-t",
                &format!("={session}:"),
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = trim_capture(&String::from_utf8_lossy(&out.stdout));
        (!text.is_empty()).then_some(text)
    }

    /// Is the session's active pane sitting in copy-mode with a selection?
    /// (`display-message` rejects the `=` target prefix; session names are
    /// fixed-length uuids, so prefix ambiguity can't bite.)
    pub fn selection_present(&self, session: &str) -> bool {
        let out = Command::new(&self.bin)
            .args([
                "-L",
                SOCKET,
                "display-message",
                "-p",
                "-t",
                session,
                "#{selection_present}",
            ])
            .output();
        match out {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim() == "1"
            },
            _ => false,
        }
    }

    /// Copy the active copy-mode selection, exactly like the default
    /// drag-end binding would: the text reaches the clipboard through the
    /// OSC 52 round trip (PtyEvent::ClipboardStore).
    pub fn copy_selection(&self, session: &str) {
        let _ = Command::new(&self.bin)
            .args([
                "-L",
                SOCKET,
                "send-keys",
                "-t",
                &format!("={session}:"),
                "-X",
                "copy-selection-and-cancel",
            ])
            .output();
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

/// Is this pane_current_command a shell sitting at a prompt? Login shells
/// report themselves with a leading dash ("-zsh").
pub fn is_shell(cmd: &str) -> bool {
    matches!(
        cmd.trim_start_matches('-'),
        "zsh" | "bash" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "nu"
    )
}

/// capture-pane pads the visible region with blank lines; strip them (and
/// per-line trailing whitespace) so the context file ends at real content.
fn trim_capture(text: &str) -> String {
    let mut lines: Vec<&str> =
        text.lines().map(|l| l.trim_end()).collect();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_are_recognized() {
        for cmd in ["zsh", "-zsh", "bash", "fish", "-bash"] {
            assert!(is_shell(cmd), "{cmd} should count as a shell");
        }
        for cmd in ["vim", "node", "claude", "ssh", ""] {
            assert!(!is_shell(cmd), "{cmd} should not count as a shell");
        }
    }

    #[test]
    fn conf_flips_drag_end_bindings() {
        let on = conf(true);
        assert!(on.contains(
            "bind -T copy-mode MouseDragEnd1Pane send-keys -X copy-selection-and-cancel"
        ));
        assert!(on.contains(
            "bind -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-selection-and-cancel"
        ));
        assert!(!on.contains("unbind"));
        let off = conf(false);
        assert!(off.contains("unbind -T copy-mode MouseDragEnd1Pane"));
        assert!(off.contains("unbind -T copy-mode-vi MouseDragEnd1Pane"));
        assert!(!off.contains("copy-selection-and-cancel"));
        // The shared base must survive in both variants.
        for text in [&on, &off] {
            assert!(text.contains("set -g mouse on"));
            assert!(text.contains("set -s set-clipboard on"));
        }
    }

    #[test]
    fn capture_trimming_strips_trailing_blanks_only() {
        assert_eq!(
            trim_capture("$ ls  \nfoo bar\n\n\n\n"),
            "$ ls\nfoo bar"
        );
        assert_eq!(trim_capture("\n\n"), "");
        assert_eq!(trim_capture("a\n\nb\n"), "a\n\nb");
    }
}
