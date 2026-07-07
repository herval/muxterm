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

/// Theme-derived colors for tmux's copy-mode search highlight, built by
/// theme::search_highlight - the one place theme values reach the conf.
/// Hex strings are single-quoted there: an unquoted `#` starts a comment.
#[derive(Debug)]
pub struct SearchStyle {
    pub match_bg: String,
    pub current_bg: String,
    pub current_fg: String,
}

/// Mouse drags inside panes are driven by tmux copy-mode, so copy-on-select
/// for them is a tmux binding, not app code. Both values are spelled out
/// explicitly (`on` is tmux's own default) so that re-sourcing the file
/// flips a running server in either direction:
/// - on: releasing a drag copies the selection (OSC 52 -> clipboard).
/// - off: releasing keeps the selection on screen and copies nothing;
///   cmd+c (App::copy_intercept) does the explicit copy.
fn conf(copy_on_select: bool, search: &SearchStyle) -> String {
    let drag_end = if copy_on_select {
        "bind -T copy-mode MouseDragEnd1Pane send-keys -X copy-selection-and-cancel\n\
         bind -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-selection-and-cancel\n"
    } else {
        "unbind -T copy-mode MouseDragEnd1Pane\n\
         unbind -T copy-mode-vi MouseDragEnd1Pane\n"
    };
    // The cmd+f highlight (tmux >= 3.2 for the match styles).
    let search_style = format!(
        "set -g copy-mode-match-style 'bg={}'\n\
         set -g copy-mode-current-match-style 'bg={},fg={}'\n",
        search.match_bg, search.current_bg, search.current_fg,
    );
    format!("{CONF_BASE}{drag_end}{search_style}")
}

/// Clone: pane link-openers each carry one onto their worker thread.
#[derive(Clone)]
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

    /// Returns whether the on-disk conf actually changed, so callers know
    /// to re-source a running server (copy_on_select or theme changes).
    pub fn write_conf(
        &self,
        copy_on_select: bool,
        search: &SearchStyle,
    ) -> Result<bool> {
        let content = conf(copy_on_select, search);
        if fs::read_to_string(&self.conf).ok().as_deref()
            == Some(content.as_str())
        {
            return Ok(false);
        }
        if let Some(parent) = self.conf.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.conf, &content)?;
        Ok(true)
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
    /// `-u` declares the client terminal UTF-8 capable. tmux otherwise
    /// guesses from LC_ALL/LC_CTYPE/LANG, which are all unset when the
    /// app is launched from Finder/Dock - and a non-UTF-8 client gets
    /// every non-Latin-1 glyph redrawn as `_` (block-art logos and
    /// spinners turn into rows of underscores).
    /// `-A` attaches if the session exists and creates it otherwise, so
    /// restore-after-relaunch and fresh spawn are the same code path.
    /// `-D` kicks any stale client so pane sizing is never fought over.
    /// `-c` sets the new shell's start directory (ignored on attach).
    /// `-e` seeds the pane environment: `MUXTERM*` for agent-mesh
    /// detection, and `COLORFGBG` so terminal-background sniffers (Claude
    /// Code's `auto` theme, vim, bat, delta) match muxterm's own theme
    /// rather than the stale value inherited from whatever launched the app
    /// - macOS hands Finder/Dock launches a `0;15` (light) COLORFGBG that
    /// otherwise leaks into every pane. All `-e` vars are ignored on attach,
    /// so pre-existing sessions keep the environment they first spawned with.
    pub fn spawn_settings(
        &self,
        session: &str,
        start_dir: Option<String>,
        dark: bool,
    ) -> BackendSettings {
        let mut args = vec![
            "-u".into(),
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
            "-e".into(),
            // Claude Code's `auto` theme reads only COLORFGBG's last field
            // (<=6 or ==8 => dark); the canonical fg;bg pair also steers
            // other background sniffers the same way.
            format!("COLORFGBG={}", if dark { "15;0" } else { "0;15" }),
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
    /// so the "?" prompt only ever triggers at a shell.
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

    /// iTerm-style cmd+k: clear the pane's visible screen and its scrollback.
    /// Ctrl-L makes the shell clear and redraw its prompt at the top; tmux
    /// scrolls the cleared screen into its history, so a beat later
    /// `clear-history` wipes that. The short delay makes the ordering
    /// deterministic - run before C-l's history push settles, clear-history
    /// leaves the pushed lines behind - so it runs on a detached thread rather
    /// than blocking the UI. Whatever the pane runs, C-l is just a redraw.
    pub fn clear(&self, session: &str) {
        let bin = self.bin.clone();
        let target = format!("={session}:");
        std::thread::spawn(move || {
            let run = |args: &[&str]| {
                let _ = Command::new(&bin).args(args).output();
            };
            run(&["-L", SOCKET, "send-keys", "-t", target.as_str(), "C-l"]);
            std::thread::sleep(std::time::Duration::from_millis(200));
            run(&["-L", SOCKET, "clear-history", "-t", target.as_str()]);
        });
    }

    /// One tmux invocation per cmd+f edit: (re)enter copy-mode, jump to
    /// the bottom of history so the newest match wins, run the plain-text
    /// search, and read the match counters back on the same round trip.
    /// The `--` belongs to the copy-mode command's own argument parser -
    /// without it a query starting with `-` is rejected as a flag.
    pub fn search_text(
        &self,
        session: &str,
        query: &str,
    ) -> Option<SearchStatus> {
        let target = format!("={session}:");
        let query = escape_semi(query);
        self.search_op(session, &[
            "send-keys",
            "-t",
            &target,
            "-X",
            "history-bottom",
            ";",
            "send-keys",
            "-t",
            &target,
            "-X",
            "search-backward-text",
            "--",
            &query,
        ])
    }

    /// Enter / cmd+g: continue toward older matches. Works even after a
    /// click or drag dropped the pane out of copy-mode - tmux keeps the
    /// pane's last search string across copy-mode instances.
    pub fn search_next(&self, session: &str) -> Option<SearchStatus> {
        let target = format!("={session}:");
        self.search_op(session, &[
            "send-keys",
            "-t",
            &target,
            "-X",
            "search-again",
        ])
    }

    /// shift+Enter / cmd+shift+g: back toward newer matches.
    pub fn search_prev(&self, session: &str) -> Option<SearchStatus> {
        let target = format!("={session}:");
        self.search_op(session, &[
            "send-keys",
            "-t",
            &target,
            "-X",
            "search-reverse",
        ])
    }

    /// Query emptied: leave copy-mode entirely, which drops the match
    /// highlights and unfreezes the pane. `-q` is a no-op outside a mode,
    /// so no `#{pane_in_mode}` guard is needed.
    pub fn search_clear(&self, session: &str) {
        let _ = Command::new(&self.bin)
            .args([
                "-L",
                SOCKET,
                "copy-mode",
                "-q",
                "-t",
                &format!("={session}:"),
            ])
            .output();
    }

    /// `copy-mode ; <steps> ; display-message`, sequenced by lone `;`
    /// argv elements so the whole op is a single fork + server round
    /// trip. copy-mode goes first because it is a no-op when the pane is
    /// already in it: any interaction that knocked the pane out of
    /// copy-mode (drag-copy, click) self-heals on the next op.
    /// display-message wants the bare session name (it rejects `=`).
    fn search_op(&self, session: &str, steps: &[&str]) -> Option<SearchStatus> {
        let target = format!("={session}:");
        let out = Command::new(&self.bin)
            .args(["-L", SOCKET, "copy-mode", "-t", &target, ";"])
            .args(steps)
            .args([
                ";",
                "display-message",
                "-p",
                "-t",
                session,
                "#{search_present} #{search_count} #{search_count_partial}",
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_search_status(&String::from_utf8_lossy(&out.stdout))
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

/// What a search op reads back from tmux.
#[derive(Debug)]
pub struct SearchStatus {
    /// #{search_count}: total matches. None when the server predates the
    /// format variable (tmux < 3.5) - the bar hides its counter but the
    /// search itself still works.
    pub total: Option<u32>,
    /// #{search_count_partial}: tmux capped the count; render "N+".
    pub partial: bool,
}

/// display-message output "1 17 0" -> 17 matches; "1 120 1" -> capped;
/// "1  " -> matched but no search_count (tmux < 3.5); "0  " -> the search
/// ran and found nothing (a no-match search leaves search_present unset,
/// verified against tmux 3.7); "" -> the sequence aborted before
/// display-message ran (no search at all).
fn parse_search_status(stdout: &str) -> Option<SearchStatus> {
    let mut fields = stdout.split_whitespace();
    if fields.next()? != "1" {
        return Some(SearchStatus {
            total: Some(0),
            partial: false,
        });
    }
    let total = fields.next().and_then(|f| f.parse().ok());
    let partial = fields.next() == Some("1");
    Some(SearchStatus { total, partial })
}

/// tmux re-parses argv words: one that is `;` or ends with an unescaped
/// `;` splits the command sequence, and unescaping eats one trailing
/// backslash. Guarding the final character is sufficient - mid-string
/// semicolons are already literal.
fn escape_semi(query: &str) -> String {
    match query.strip_suffix(';') {
        Some(head) => format!("{head}\\;"),
        None => query.to_string(),
    }
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
    fn spawn_forces_utf8_client() {
        // Finder-launched apps have no locale env, and without -u tmux
        // draws every non-Latin-1 glyph on the client as '_'.
        let ctl = TmuxCtl {
            bin: PathBuf::from("/usr/bin/tmux"),
            conf: PathBuf::from("/tmp/tmux.conf"),
        };
        let settings = ctl.spawn_settings("mux-abcd1234", None, true);
        assert_eq!(settings.args.first().map(String::as_str), Some("-u"));
        let new_session =
            settings.args.iter().position(|a| a == "new-session");
        assert!(new_session.is_some(), "client must open a session");
    }

    #[test]
    fn spawn_advertises_theme_background() {
        // Claude Code's `auto` theme (and vim/bat/delta) read COLORFGBG's
        // last field for light/dark; muxterm must overwrite the value the
        // OS leaked in so panes match the app's own theme, not the launcher.
        let ctl = TmuxCtl {
            bin: PathBuf::from("/usr/bin/tmux"),
            conf: PathBuf::from("/tmp/tmux.conf"),
        };
        let dark = ctl.spawn_settings("mux-abcd1234", None, true);
        assert!(dark.args.iter().any(|a| a == "COLORFGBG=15;0"));
        let light = ctl.spawn_settings("mux-abcd1234", None, false);
        assert!(light.args.iter().any(|a| a == "COLORFGBG=0;15"));
    }

    fn style() -> SearchStyle {
        SearchStyle {
            match_bg: "#46648b".into(),
            current_bg: "#4a90d9".into(),
            current_fg: "#1d1e23".into(),
        }
    }

    #[test]
    fn conf_flips_drag_end_bindings() {
        let on = conf(true, &style());
        assert!(on.contains(
            "bind -T copy-mode MouseDragEnd1Pane send-keys -X copy-selection-and-cancel"
        ));
        assert!(on.contains(
            "bind -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-selection-and-cancel"
        ));
        assert!(!on.contains("unbind"));
        let off = conf(false, &style());
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
    fn conf_injects_search_match_styles() {
        let text = conf(true, &style());
        assert!(text
            .contains("set -g copy-mode-match-style 'bg=#46648b'"));
        assert!(text.contains(
            "set -g copy-mode-current-match-style 'bg=#4a90d9,fg=#1d1e23'"
        ));
    }

    #[test]
    fn escape_semi_protects_only_a_trailing_semicolon() {
        assert_eq!(escape_semi("foo"), "foo");
        assert_eq!(escape_semi("a;b"), "a;b");
        assert_eq!(escape_semi("foo;"), "foo\\;");
        assert_eq!(escape_semi(";"), "\\;");
        // tmux's unescape eats one trailing backslash, so a query ending
        // in `\;` needs the extra layer to round-trip literally.
        assert_eq!(escape_semi("foo\\;"), "foo\\\\;");
    }

    #[test]
    fn search_status_parses_and_degrades() {
        let s = parse_search_status("1 17 0\n").unwrap();
        assert_eq!(s.total, Some(17));
        assert!(!s.partial);
        let s = parse_search_status("1 120 1\n").unwrap();
        assert_eq!(s.total, Some(120));
        assert!(s.partial);
        // tmux < 3.5: search_count expands to nothing.
        let s = parse_search_status("1  \n").unwrap();
        assert_eq!(s.total, None);
        assert!(!s.partial);
        // The search ran and found nothing.
        let s = parse_search_status("0  \n").unwrap();
        assert_eq!(s.total, Some(0));
        // The command sequence aborted early (no search at all).
        assert!(parse_search_status("").is_none());
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
