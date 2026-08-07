use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
# muxterm's own client keeps plain left-clicks local (egui_term P16: clicks
# and drags drive the widget's local selection; the wheel is reported for
# scrollback) and sends a left-button report only for a deliberate
# option+click (egui_term P25, modifier bits stripped). Route those by what
# the pane's app asked for: mouse-tracking apps (the agent CLIs) get the
# click via `send -M`, which re-encodes it in the app's own protocol - one
# pane per session, so client and pane coordinates are identical. Everything
# else consumes it: select-pane is a no-op - muxterm is one pane per session
# - and consuming beats `unbind`, which would pass the raw sequence through.
# The consume arm also stays belt-and-braces for *other* clients attached to
# the socket, whose selection clicks would otherwise `send -M` into a
# mouse-mode app and move its cursor.
bind -n MouseDown1Pane if -F '#{mouse_any_flag}' {send -M} {select-pane -t =}
bind -n MouseUp1Pane if -F '#{mouse_any_flag}' {send -M} {select-pane -t =}
# One wheel report scrolls one line. The client already sends exactly the
# number of reports the gesture earned (egui_term P29 measures the delta in
# rendered cell heights), so tmux's default copy-mode step of `-N 5` scaled
# every flick by five and turned scrolling into jumps. The root binding keeps
# the stock guard verbatim - `send -M` is how a pager gets the wheel
# translated into arrow keys (#{alternate_on}) and how a mouse-tracking app
# gets the report at all (#{mouse_any_flag}) - and only changes the arm that
# enters copy-mode, which used to swallow the report that opened it. `-e`
# still auto-exits at the bottom. WheelDownPane needs no root binding: with
# no scrollback to enter, tmux's default already does the right thing.
bind -n WheelUpPane if -F '#{||:#{alternate_on},#{pane_in_mode},#{mouse_any_flag}}' {send -M} {copy-mode -e ; send-keys -X scroll-up}
bind -T copy-mode WheelUpPane send-keys -X scroll-up
bind -T copy-mode WheelDownPane send-keys -X scroll-down
bind -T copy-mode-vi WheelUpPane send-keys -X scroll-up
bind -T copy-mode-vi WheelDownPane send-keys -X scroll-down
# muxterm drives copy-mode selections itself - one chained tmux invocation
# per drag update (TmuxCtl::select_update), because the left button is never
# reported to tmux (egui_term P16). These four settle what a selection looks
# like and how it can be dismissed.
# mode-keys is otherwise guessed from $EDITOR, and the vi table binds Escape
# to clear-selection rather than cancel - which would leave a pane sitting
# frozen in copy-mode after the user tried to dismiss a selection.
setw -g mode-keys emacs
# The selection highlight. `reverse` reaches the client as ESC[7m, which the
# widget renders with the same fg/bg swap it paints its own local selection
# with - so the handoff from the optimistic local highlight to tmux's own is
# invisible.
setw -g mode-style reverse
# Hide copy-mode's [12/340] position readout: entering copy-mode to hold a
# selection would otherwise flash a counter into the pane's top-right corner
# on every drag.
setw -g copy-mode-position-format ''
# Double-click selects a whole non-whitespace run, matching egui_term P14
# (which cut alacritty's semantic boundaries down to whitespace). tmux's
# default separators would stop select-word at the first slash or colon.
set -g word-separators ' '
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

    /// Where the shell's prompt ends and how big the pane is:
    /// `(cursor_col, width, height)` in cells. The "?" prompt's inline
    /// erase needs all three to work out how many rows the command it types
    /// will occupy once the shell echoes it. Read from tmux rather than the
    /// local grid because tmux's is the copy the shell actually wrote to -
    /// and through `list-panes`, since `display-message -t` resolves pane
    /// fields empty.
    pub fn cursor_and_size(&self, session: &str) -> Option<(u16, u16, u16)> {
        let out = Command::new(&self.bin)
            .args([
                "-L",
                SOCKET,
                "list-panes",
                "-t",
                &format!("={session}"),
                "-F",
                "#{cursor_x} #{pane_width} #{pane_height}",
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut fields = stdout.lines().next()?.split_whitespace();
        let mut next = || fields.next()?.parse::<u16>().ok();
        Some((next()?, next()?, next()?))
    }

    /// Foreground process + pid + cwd of every session's active pane in one
    /// tmux round trip. Polled once a second for the sidebar's working-dot
    /// ("is something other than a shell running?"), the workspace-root sync
    /// ("did every pane leave the workspace's folder?"), and the
    /// background-job scan's walk roots (bg_jobs), so one subprocess
    /// covering all panes matters - the per-session getters would be N.
    pub fn pane_snapshot(&self) -> HashMap<String, PaneSnap> {
        let out = Command::new(&self.bin)
            .args([
                "-L",
                SOCKET,
                "list-panes",
                "-a",
                "-F",
                "#{session_name}\t#{pane_pid}\t#{pane_current_command}\t#{window_activity}\t#{pane_current_path}",
            ])
            .output();
        match out {
            Ok(out) if out.status.success() => {
                parse_pane_snapshot(&String::from_utf8_lossy(&out.stdout))
            },
            _ => HashMap::new(),
        }
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

    /// Drive one step of a pane's copy-mode selection (see `select_argv`).
    /// muxterm never forwards the left button (egui_term P16), so a drag is
    /// mirrored into tmux from the app side: this is how a selection comes to
    /// live somewhere that survives the pane's repaints and reaches into
    /// scrollback.
    ///
    /// Runs on a detached thread - one chained fork measures ~4ms, which is a
    /// whole frame at the moment the user is dragging. The returned flag
    /// flips once `output()` returns, i.e. once the server has *executed* the
    /// sequence; the caller keeps at most one update in flight and lets every
    /// later one supersede it (each carries absolute coordinates, so a
    /// skipped update costs nothing).
    pub fn select_update(
        &self,
        session: &str,
        anchor: Option<(usize, usize)>,
        cursor: (usize, usize),
        scroll: i32,
        finish: Finish,
    ) -> Arc<AtomicBool> {
        self.spawn_argv(select_argv(session, anchor, cursor, scroll, finish))
    }

    /// Freeze the pane's view by entering copy-mode, without touching the
    /// cursor or the selection. This is the whole of what a drag does when
    /// it arms: the freeze is what stops the pane's repaints from wiping the
    /// widget's local highlight, and the local highlight is what draws the
    /// drag - at sixty frames a second, with no forks and nothing to blink.
    /// tmux only learns the selection when the drag ends.
    pub fn enter_copy_mode(&self, session: &str) -> Arc<AtomicBool> {
        self.spawn_argv(
            ["-L", SOCKET, "copy-mode", "-t", &format!("={session}:")]
                .map(String::from)
                .to_vec(),
        )
    }

    /// Scroll a pane already in copy-mode, leaving the cursor on its screen
    /// row so the selection extends with the viewport. One command, so the
    /// client gets one redraw - repositioning the cursor absolutely would
    /// send it via `top-line`, and the trip through the top of the pane is
    /// visible.
    pub fn scroll_copy_mode(&self, session: &str, lines: i32) -> Arc<AtomicBool> {
        let cmd = if lines > 0 { "scroll-up" } else { "scroll-down" };
        self.spawn_argv(
            [
                "-L",
                SOCKET,
                "send-keys",
                "-t",
                &format!("={session}:"),
                "-X",
                "-N",
                &lines.unsigned_abs().to_string(),
                cmd,
            ]
            .map(String::from)
            .to_vec(),
        )
    }

    /// Leave copy-mode, dropping any selection with it - the pane goes back
    /// to following its program's live output. A no-op when the pane isn't in
    /// a mode, so it is always safe to send.
    pub fn cancel_copy_mode(&self, session: &str) -> Arc<AtomicBool> {
        self.spawn_argv(
            ["-L", SOCKET, "copy-mode", "-q", "-t", &format!("={session}:")]
                .map(String::from)
                .to_vec(),
        )
    }

    /// Drop the selection but stay in copy-mode (the pane keeps its scroll
    /// position) - what cmd+f wants before it moves the copy-mode cursor.
    pub fn clear_copy_selection(&self, session: &str) {
        let bin = self.bin.clone();
        let t = format!("={session}:");
        std::thread::spawn(move || {
            let _ = Command::new(&bin)
                .args(["-L", SOCKET, "send-keys", "-t", &t, "-X"])
                .arg("clear-selection")
                .output();
        });
    }

    /// Run a prepared argv off the UI thread, flagging completion.
    fn spawn_argv(&self, argv: Vec<String>) -> Arc<AtomicBool> {
        let done = Arc::new(AtomicBool::new(false));
        let bin = self.bin.clone();
        let flag = done.clone();
        std::thread::spawn(move || {
            let _ = Command::new(&bin).args(argv).output();
            flag.store(true, Ordering::Release);
        });
        done
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

/// The per-second pane snapshot, shared with the poller threads
/// (pr_status/git_status): the GUI already pays one `list-panes -a` per
/// tick for the sidebar dots and workspace-root sync, so the pollers read
/// this instead of each spawning their own tmux query.
pub type SharedPanes =
    std::sync::Arc<std::sync::Mutex<HashMap<String, PaneSnap>>>;

/// One row of the per-second `list-panes -a` snapshot: the foreground
/// process, root pid, and cwd of a session's active pane.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneSnap {
    pub cmd: String,
    /// None when tmux reported no path (a dying pane).
    pub cwd: Option<PathBuf>,
    /// #{pane_pid}: the pane's root process (the shell tmux spawned), the
    /// walk root for the background-job scan (bg_jobs). None if the field
    /// failed to parse - a torn line must not drop the row's cmd/cwd.
    pub pid: Option<u32>,
    /// #{window_activity}: unix seconds of the pane's last terminal activity
    /// (each muxterm pane is its own tmux session/window, so it's per-pane).
    /// Lets the poll tick clear a stuck "attention" whose pane kept producing
    /// output after the permission fired. None if the field failed to parse.
    pub activity: Option<u64>,
}

/// Parse `list-panes -a` output shaped
/// `#{session_name}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}`.
/// Tab-separated: the command keeps any spaces a process title may carry,
/// and paths routinely contain spaces; neither plausibly carries a tab.
fn parse_pane_snapshot(text: &str) -> HashMap<String, PaneSnap> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(5, '\t');
            let session = fields.next()?;
            let pid = fields.next()?.trim().parse::<u32>().ok();
            let cmd = fields.next()?.trim();
            let activity = fields.next()?.trim().parse::<u64>().ok();
            let cwd = fields.next().map(str::trim).filter(|p| !p.is_empty());
            (!session.is_empty() && !cmd.is_empty()).then(|| {
                (
                    session.to_string(),
                    PaneSnap {
                        cmd: cmd.to_string(),
                        cwd: cwd.map(PathBuf::from),
                        pid,
                        activity,
                    },
                )
            })
        })
        .collect()
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

/// The argv (after `tmux`) that recreates a visible-pane selection in
/// copy-mode and scrolls it: `copy-mode ; top-line ; start-of-line ; [down
/// sr] ; [right sc] ; begin-selection ; [down er-sr] ; start-of-line ;
/// [right ec] ; [scroll]`. `top-line`+`start-of-line` give a stable anchor
/// (top-left of the visible screen); zero-count motions are skipped. Pure so
/// it unit-tests without a tmux server (the risky copy-mode motion sequence).
/// What the last step of a selection update does with the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    /// Leave the selection standing (cmd+c copies it later).
    Keep,
    /// Copy and leave copy-mode, unfreezing the pane.
    CopyAndCancel,
}

/// The argv (after `tmux`) for one copy-mode selection update. `row` is
/// 0-based within the *visible* pane; the second element is a count of
/// `cursor-right` presses - characters, not columns, which is what
/// egui_term's `copy_target` counts and clamps so the cursor can never wrap
/// onto the next row.
///
/// `anchor` is Some while the drag origin is still on screen. tmux pins the
/// anchor to the *content*, so re-issuing `begin-selection` there every
/// update is self-healing; once autoscroll has pushed the origin past an
/// edge, leaving it alone is the only way to keep it - a screen-relative
/// motion cannot address an off-screen row.
///
/// `scroll` comes last, and that ordering is the whole anchor rule:
/// scrolling moves the viewport but leaves the *cursor* on its screen row,
/// so the cursor end travels into older lines while the anchor stays put.
/// Anchoring the moving end instead collapses the selection to nothing and
/// then inverts it (measured: a 3-row selection + `scroll-up -N 3` left
/// selection_present=0).
///
/// No `start-of-line` anywhere: on the continuation row of a soft-wrapped
/// line it jumps *up* to the logical line's start (measured), which is most
/// of an agent CLI's output. `top-line` already parks the cursor at column 0
/// of the viewport's top row, and `cursor-down` counts screen rows whether
/// they are wrapped or not.
fn select_argv(
    session: &str,
    anchor: Option<(usize, usize)>,
    cursor: (usize, usize),
    scroll: i32,
    finish: Finish,
) -> Vec<String> {
    // A `; send-keys -t <target> -X <args...>` step (a nested fn, not a
    // closure, so it doesn't borrow `argv` for the whole build).
    fn step(argv: &mut Vec<String>, t: &str, args: &[&str]) {
        argv.push(";".into());
        argv.extend(["send-keys", "-t", t, "-X"].map(String::from));
        argv.extend(args.iter().map(|s| s.to_string()));
    }
    fn seek(argv: &mut Vec<String>, t: &str, (row, col): (usize, usize)) {
        step(argv, t, &["top-line"]);
        if row > 0 {
            step(argv, t, &["-N", &row.to_string(), "cursor-down"]);
        }
        if col > 0 {
            step(argv, t, &["-N", &col.to_string(), "cursor-right"]);
        }
    }
    let t = format!("={session}:");
    // copy-mode first, and *bare*: it is a true no-op on a pane already in
    // it, so every update self-heals a pane something else knocked out. `-e`
    // is deliberately not used here - it would cancel the mode, and the
    // selection with it, the moment a scroll reached the bottom.
    let mut argv: Vec<String> = ["-L", SOCKET, "copy-mode", "-t", t.as_str()]
        .map(String::from)
        .to_vec();
    if let Some(a) = anchor {
        seek(&mut argv, &t, a);
        step(&mut argv, &t, &["begin-selection"]);
    }
    seek(&mut argv, &t, cursor);
    if scroll != 0 {
        let cmd = if scroll > 0 { "scroll-up" } else { "scroll-down" };
        step(&mut argv, &t, &["-N", &scroll.unsigned_abs().to_string(), cmd]);
    }
    match finish {
        Finish::Keep => {},
        Finish::CopyAndCancel => {
            step(&mut argv, &t, &["copy-selection-and-cancel"])
        },
    }
    argv
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

    /// The ordered copy-mode command names (the arg after each `-X`, past an
    /// optional `-N <count>`).
    fn x_cmds(argv: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < argv.len() {
            if argv[i] == "-X" {
                if argv.get(i + 1).map(String::as_str) == Some("-N") {
                    out.push(argv[i + 3].clone());
                    i += 4;
                } else {
                    out.push(argv[i + 1].clone());
                    i += 2;
                }
            } else {
                i += 1;
            }
        }
        out
    }

    #[test]
    fn select_argv_anchors_then_moves_then_scrolls() {
        // Anchor at (row 2, 3 presses), cursor at (row 4, 10), scroll up 3.
        let a = select_argv("mux-aaaa", Some((2, 3)), (4, 10), 3, Finish::Keep);
        assert_eq!(a[..5], ["-L", SOCKET, "copy-mode", "-t", "=mux-aaaa:"]);
        assert_eq!(x_cmds(&a), [
            "top-line",
            "cursor-down",  // to the anchor row
            "cursor-right", // to the anchor column
            "begin-selection",
            "top-line",
            "cursor-down",  // to the cursor row
            "cursor-right", // to the cursor column
            "scroll-up",    // scroll > 0
        ]);
        let joined = a.join(" ");
        assert!(joined.contains("-X -N 2 cursor-down"), "{joined}");
        assert!(joined.contains("-X -N 10 cursor-right"), "{joined}");
        assert!(joined.contains("-X -N 3 scroll-up"), "{joined}");
        // `start-of-line` walks *up* to the start of a soft-wrapped logical
        // line, which is most of an agent CLI's output - it must never
        // appear between an absolute seek's steps.
        assert!(!joined.contains("start-of-line"), "{joined}");

        // The scroll always comes last: that ordering is what leaves the
        // cursor end riding the viewport while the anchor stays put.
        assert_eq!(x_cmds(&a).last().unwrap(), "scroll-up");

        // Zero row/column offsets emit no motion; a negative scroll goes the
        // other way.
        let b = select_argv("mux-bbbb", None, (0, 0), -2, Finish::Keep);
        assert_eq!(x_cmds(&b), ["top-line", "scroll-down"]);
        assert!(b.join(" ").contains("-X -N 2 scroll-down"));
    }

    #[test]
    fn select_argv_without_an_anchor_only_moves_the_cursor() {
        // Once autoscroll has pushed the origin off screen there is no row to
        // re-anchor to, and re-issuing begin-selection would land it on the
        // wrong line - so the update carries the cursor alone.
        let a = select_argv("mux-aaaa", None, (4, 10), 0, Finish::Keep);
        assert_eq!(x_cmds(&a), ["top-line", "cursor-down", "cursor-right"]);
        assert_eq!(a.iter().filter(|s| *s == "top-line").count(), 1);
        assert!(!a.iter().any(|s| s == "begin-selection"));
    }

    #[test]
    fn select_argv_finish_variants() {
        let keep = select_argv("s", None, (1, 1), 0, Finish::Keep);
        assert!(!keep.iter().any(|s| s.starts_with("copy-selection")));

        // copy_on_select copies and hands the pane straight back, so the
        // copy step is always last and always the cancelling one.
        let cancel = select_argv("s", None, (1, 1), 0, Finish::CopyAndCancel);
        assert_eq!(
            x_cmds(&cancel).last().unwrap(),
            "copy-selection-and-cancel",
        );
    }

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
    fn pane_snapshot_parse() {
        // Claude Code's process title is its version string; commands and
        // paths may carry spaces; blank/malformed lines are dropped and a
        // missing path becomes None rather than an empty cwd. A garbage pid
        // or window_activity field costs only that field, never the row; cwd
        // stays the greedy last field so a tab in it (never seen in practice)
        // could not shift the columns.
        let map = parse_pane_snapshot(
            "mux-aaaa1111\t81234\tzsh\t1784119626\t/Users/u/dev\n\
             mux-bbbb2222\t81235\t2.1.202\t1784119177\t/Users/u/my repo\n\
             mux-cccc3333\t81236\tgit log\t1784119000\t\n\
             mux-dddd4444\tnope\tvim\tnope\t/Users/u/dev\n\
             \nbroken\n",
        );
        assert_eq!(map.len(), 4);
        assert_eq!(map["mux-aaaa1111"].cmd, "zsh");
        assert_eq!(map["mux-aaaa1111"].pid, Some(81234));
        assert_eq!(map["mux-aaaa1111"].activity, Some(1784119626));
        assert_eq!(
            map["mux-aaaa1111"].cwd.as_deref(),
            Some(Path::new("/Users/u/dev"))
        );
        assert_eq!(map["mux-bbbb2222"].cmd, "2.1.202");
        assert_eq!(map["mux-bbbb2222"].activity, Some(1784119177));
        assert_eq!(
            map["mux-bbbb2222"].cwd.as_deref(),
            Some(Path::new("/Users/u/my repo"))
        );
        assert_eq!(map["mux-cccc3333"].cmd, "git log");
        assert!(map["mux-cccc3333"].cwd.is_none());
        assert_eq!(map["mux-dddd4444"].cmd, "vim");
        assert!(map["mux-dddd4444"].pid.is_none());
        // A garbage activity field degrades to None, keeping cmd/cwd.
        assert!(map["mux-dddd4444"].activity.is_none());
        assert_eq!(
            map["mux-dddd4444"].cwd.as_deref(),
            Some(Path::new("/Users/u/dev"))
        );
        assert!(is_shell(&map["mux-aaaa1111"].cmd));
        assert!(!is_shell(&map["mux-bbbb2222"].cmd));
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
        assert!(!on.contains("unbind -T copy-mode MouseDragEnd1Pane"));
        let off = conf(false, &style());
        assert!(off.contains("unbind -T copy-mode MouseDragEnd1Pane"));
        assert!(off.contains("unbind -T copy-mode-vi MouseDragEnd1Pane"));
        assert!(!off.contains("copy-selection-and-cancel"));
        // The shared base must survive in both variants.
        for text in [&on, &off] {
            assert!(text.contains("set -g mouse on"));
            assert!(text.contains("set -s set-clipboard on"));
            // Left-clicks route by whether the pane's app asked for the
            // mouse: relayed option+clicks (egui_term P25) reach tracking
            // apps via send -M, everything else is consumed (not unbound).
            assert!(text.contains(
                "bind -n MouseDown1Pane if -F '#{mouse_any_flag}' {send -M} {select-pane -t =}"
            ));
            assert!(text.contains(
                "bind -n MouseUp1Pane if -F '#{mouse_any_flag}' {send -M} {select-pane -t =}"
            ));
        }
    }

    /// One wheel report = one line. tmux's default copy-mode step is `-N 5`,
    /// which multiplied every gesture by five on top of a client that already
    /// sends one report per line earned (egui_term P29).
    #[test]
    fn conf_binds_one_line_wheel_steps() {
        for text in [conf(true, &style()), conf(false, &style())] {
            for table in ["copy-mode", "copy-mode-vi"] {
                for (key, cmd) in [
                    ("WheelUpPane", "scroll-up"),
                    ("WheelDownPane", "scroll-down"),
                ] {
                    assert!(
                        text.contains(&format!(
                            "bind -T {table} {key} send-keys -X {cmd}\n"
                        )),
                        "{table}/{key} must scroll exactly one line",
                    );
                }
            }
            // The root binding's guard is load-bearing: `alternate_on` is what
            // gives pagers wheel->arrow translation and `mouse_any_flag` is
            // what lets a tracking app see the wheel at all. Only the
            // copy-mode-entering arm may differ from tmux's default.
            assert!(text.contains(
                "bind -n WheelUpPane if -F '#{||:#{alternate_on},#{pane_in_mode},#{mouse_any_flag}}' {send -M} {copy-mode -e ; send-keys -X scroll-up}"
            ));
            // Selections are driven by muxterm, so the conf has to settle
            // what one looks like and how it can be dismissed.
            for line in [
                "setw -g mode-keys emacs",
                "setw -g mode-style reverse",
                "setw -g copy-mode-position-format ''",
                "set -g word-separators ' '",
            ] {
                assert!(text.contains(line), "missing: {line}");
            }
            // No *binding* may reintroduce a multi-line wheel step (the
            // comment above them names tmux's default, so skip comments).
            let directives = text
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .collect::<Vec<_>>();
            assert!(
                !directives.iter().any(|l| l.contains("-X -N")),
                "a counted wheel step came back: {directives:?}",
            );
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
