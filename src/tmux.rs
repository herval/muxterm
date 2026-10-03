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

/// The answer to `TmuxCtl::probe_server`.
#[derive(Debug, PartialEq, Eq)]
pub enum ServerProbe {
    /// Up, holding these sessions (possibly none).
    Alive(HashSet<String>),
    /// Nothing listening on the socket: the server died.
    Dead,
    /// The probe failed some other way - don't conclude anything.
    Unknown,
}

/// `list-sessions`' outcome -> ServerProbe. Only tmux's two "nobody home"
/// messages count as dead: a missing socket file ("no server running on"
/// / "error connecting to ... (No such file or directory)") or a stale one
/// nobody accepts on ("Connection refused"). Pure so the wording is tested.
fn classify_probe(ok: bool, stdout: &str, stderr: &str) -> ServerProbe {
    if ok {
        return ServerProbe::Alive(
            stdout
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
        );
    }
    let dead = stderr.contains("no server running on")
        || (stderr.contains("error connecting to")
            && (stderr.contains("No such file or directory")
                || stderr.contains("Connection refused")));
    if dead {
        ServerProbe::Dead
    } else {
        ServerProbe::Unknown
    }
}

/// Regenerated on every launch (it only applies when the server starts) and
/// re-sourced into a running server when copy_on_select changes.
/// `status off` makes sessions look like a plain terminal; the `Ms` override
/// lets tmux pass a program's clipboard escape (OSC 52) on to the client,
/// where it surfaces as PtyEvent::ClipboardStore. muxterm's own copies don't
/// take that route (`TmuxCtl::take_selection`).
const CONF_BASE: &str = r##"# managed by muxterm - regenerated at every launch
set -g status off
set -g mouse on
set -s escape-time 0
# Keep the server up with zero sessions: then "the server is gone" can only
# mean it died (crash, kill-server, reboot), which is what lets a pane exit
# tell a shell exiting from the server dying under every pane at once
# (app::settle_exits) - the latter must recover, never close the tabs.
set -s exit-empty off
set -g history-limit 100000
set -g default-terminal "tmux-256color"
set -g set-titles on
set -g set-titles-string "#{pane_current_command}"
set -s set-clipboard on
set -as terminal-overrides ',xterm*:Ms=\E]52;%p1%s;%p2%s\007'
# tmux keeps OSC 8 hyperlinks in its grid but forwards them only to clients
# whose terminal claims the feature; without it the URL behind Claude Code's
# `⧉ mockups` is dropped and the click has nothing to open (egui_term P37
# reads it off the cell). Read at client attach, which follows the launch
# re-source.
set -as terminal-features ',xterm*:hyperlinks'
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
# muxterm's own drags and multi-clicks reach tmux as meta-tagged mouse
# reports with bindings of their own (tmux::Gesture, gesture_bindings), so
# tmux draws and holds every selection. These four settle what a selection
# looks like and how it can be dismissed.
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
# default separators would stop select-word at the first slash or colon. The
# tab is there because tmux keeps one as a single character of its own.
set -g word-separators " \t"
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

/// copy_on_select for tmux's own mouse drags - those of another client
/// attached to the socket, since muxterm's arrive as `Gesture`s and the app
/// copies them itself. Both values are spelled out explicitly (`on` is
/// tmux's own default) so that re-sourcing the file flips a running server
/// in either direction:
/// - on: releasing a drag copies the selection (OSC 52 -> clipboard).
/// - off: releasing keeps the selection on screen and copies nothing.
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
    let gestures = gesture_bindings();
    format!("{CONF_BASE}{drag_end}{gestures}{search_style}")
}

/// The bindings that turn a `Gesture` into a selection - never a copy:
/// muxterm makes those itself (`TmuxCtl::take_selection`), copy_on_select
/// included, so the text comes back to it rather than depending on tmux's
/// clipboard escape getting through.
///
/// Root table first, for a pane not in copy-mode yet: a drag's first report
/// enters it and starts the drag in one step (`copy-mode -M`, anchored where
/// the press went down), and a word or line click enters it before
/// selecting. Every other key a gesture can produce - a press's release,
/// tmux's own second/triple-click and delayed double-click events - is
/// consumed rather than left unbound, because an unbound mouse key is
/// forwarded to the pane, and a program that turned on mouse tracking (every
/// agent CLI does) would see a click it never got from the user.
///
/// In copy-mode, keys the mode's table lacks fall back to root, so only the
/// drag needs bindings there: it continues the selection instead of
/// re-entering the mode, and its end leaves the selection standing.
/// `send-keys -X` run from a mouse binding moves tmux's cursor to the mouse
/// first, which is what puts `select-word` and `select-line` under the
/// click.
fn gesture_bindings() -> String {
    let mut out = String::from("bind -n M-MouseDrag1Pane copy-mode -M\n");
    for key in [
        "MouseDown1Pane",
        "MouseUp1Pane",
        "SecondClick1Pane",
        "DoubleClick1Pane",
        "TripleClick1Pane",
        "MouseDragEnd1Pane",
        "MouseUp2Pane",
        "DoubleClick2Pane",
        "MouseUp3Pane",
        "DoubleClick3Pane",
    ] {
        out += &format!("bind -n M-{key} select-pane -t =\n");
    }
    // A click repeated inside tmux's 300ms window arrives as a second or
    // triple click rather than a press, so all three select.
    for (button, cmd) in [(3, "select-word"), (2, "select-line")] {
        for click in ["MouseDown", "SecondClick", "TripleClick"] {
            out += &format!(
                "bind -n M-{click}{button}Pane {{ select-pane -t = ; \
                 copy-mode ; send-keys -X {cmd} }}\n"
            );
        }
    }
    for table in ["copy-mode", "copy-mode-vi"] {
        out += &format!(
            "bind -T {table} M-MouseDrag1Pane \
             {{ select-pane ; send-keys -X begin-selection }}\n\
             bind -T {table} M-MouseDragEnd1Pane select-pane\n"
        );
    }
    out
}

/// One of muxterm's selection gestures, relayed to tmux as an SGR mouse
/// report written to the pane's own PTY - the same channel as a wheel
/// report, so tmux reads them in the order they happened.
///
/// They carry the meta bit, which nothing else muxterm reports does (egui_term
/// P25/P30 strip modifiers), so they get tmux.conf bindings of their own
/// (`gesture_bindings`) and can never be mistaken for a click meant for the
/// pane's program. Word and line ride on the right and middle buttons:
/// single presses bound to `select-word`/`select-line`, rather than tmux's
/// own double/triple-click detection, which only fires 300ms after the click
/// and keeps state between gestures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    /// The left button went down here: a drag's anchor.
    Press,
    /// The held button moved to this cell.
    Drag,
    /// The button came up, ending the drag.
    Release,
    /// Select the word at this cell (a double-click).
    Word,
    /// Select the line at this cell (a triple-click).
    Line,
    /// One line of copy-mode scroll. A plain wheel report, *not* meta-tagged:
    /// it is copy-mode's own wheel binding that it is meant to reach.
    ScrollUp,
    ScrollDown,
}

impl Gesture {
    /// The report bytes for this gesture at a visible (row, column) of the
    /// pane.
    pub fn report(self, (row, col): (usize, usize)) -> Vec<u8> {
        const META: u8 = 8;
        let sgr = |code: u8, press: bool| {
            let end = if press { 'M' } else { 'm' };
            format!("\x1b[<{code};{};{}{end}", col + 1, row + 1)
        };
        let report = match self {
            Gesture::Press => sgr(META, true),
            Gesture::Drag => sgr(32 | META, true),
            Gesture::Release => sgr(META, false),
            Gesture::Word => sgr(2 | META, true) + &sgr(2 | META, false),
            Gesture::Line => sgr(1 | META, true) + &sgr(1 | META, false),
            Gesture::ScrollUp => sgr(64, true),
            Gesture::ScrollDown => sgr(65, true),
        };
        report.into_bytes()
    }
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

    /// Copy the session's copy-mode selection, leave copy-mode, and hand the
    /// text back - None when there was no selection to copy.
    ///
    /// The text comes back here instead of through the clipboard escape (OSC
    /// 52) tmux would otherwise send the client, which `-C` (tmux >= 3.6)
    /// turns off: tmux skips that escape whenever the pane has a redraw
    /// pending, and entering copy-mode in the same breath as copying is
    /// enough to have one (measured on 3.7b: the paste buffer held the text,
    /// the clipboard never heard of it).
    ///
    /// Two invocations. The first is atomic, so nothing interleaves: the
    /// pane's width, then the copy - only if a selection is standing, or tmux
    /// would copy the search match under the cursor instead - into a buffer
    /// named under a prefix no other copy has used, then that buffer's name.
    /// A selection of nothing but blanks makes no buffer at all, so there is
    /// then no name, rather than whatever was copied last. The second reads
    /// that one buffer. It can't be one invocation: `show-buffer` fails
    /// outright once anything else in the same invocation has printed, and
    /// `list-buffers` would print the text with every newline turned into
    /// `_`. (`display-message` rejects the `=` target prefix; session names
    /// are fixed-length uuids, so prefix ambiguity can't bite.)
    pub fn take_selection(&self, session: &str) -> Option<Copied> {
        let target = format!("={session}:");
        let id = uuid::Uuid::new_v4().simple().to_string();
        let prefix = format!("muxterm-copy-{}-", &id[..8]);
        let out = Command::new(&self.bin)
            .args(["-L", SOCKET, "display-message", "-p", "-t", session])
            .args(["#{pane_width}", ";", "if-shell", "-F", "-t", &target])
            .arg("#{selection_present}")
            .arg(format!(
                "send-keys -t {target} -X copy-selection-and-cancel -C {prefix}"
            ))
            .args([";", "list-buffers", "-F", "#{buffer_name}", "-f"])
            .arg(format!("#{{m:{prefix}*,#{{buffer_name}}}}"))
            .output()
            .ok()?;
        let listed = String::from_utf8_lossy(&out.stdout);
        let (width, buffer) = copied_buffer(&listed, &prefix)?;
        let out = Command::new(&self.bin)
            .args(["-L", SOCKET, "show-buffer", "-b", buffer])
            .output()
            .ok()?;
        out.status.success().then(|| Copied {
            text: String::from_utf8_lossy(&out.stdout).into_owned(),
            width,
        })
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

    /// Is the `-L muxterm` server up, and which sessions does it hold? The
    /// distinction `list_sessions` flattens away: an empty list there means
    /// "no sessions" *or* "no server". With `exit-empty off` in the conf, an
    /// absent server is never the normal aftermath of a last shell exiting -
    /// only of the server dying - so `Dead` is what drives in-place recovery
    /// (`app::settle_exits`). Anything unrecognized is `Unknown`, which the
    /// caller must treat as "decide later", never as dead: recovery types
    /// commands into panes, and doing that to live ones would be a disaster.
    pub fn probe_server(&self) -> ServerProbe {
        match Command::new(&self.bin)
            .args(["-L", SOCKET, "list-sessions", "-F", "#{session_name}"])
            .output()
        {
            Ok(out) => classify_probe(
                out.status.success(),
                &String::from_utf8_lossy(&out.stdout),
                &String::from_utf8_lossy(&out.stderr),
            ),
            Err(_) => ServerProbe::Unknown,
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

/// Text tmux copied out of a pane, and the pane width it was wrapped to.
#[derive(Debug, PartialEq, Eq)]
pub struct Copied {
    pub text: String,
    pub width: u16,
}

/// `take_selection`'s first answer: the pane width on a line of its own,
/// then the name of the buffer the copy made - which must carry the prefix
/// it was asked for - or nothing at all when no copy was made.
fn copied_buffer<'a>(out: &'a str, prefix: &str) -> Option<(u16, &'a str)> {
    let mut lines = out.lines();
    let width = lines.next()?.trim().parse().ok()?;
    let buffer = lines.next().filter(|name| name.starts_with(prefix))?;
    Some((width, buffer))
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
    fn probe_tells_a_dead_server_from_a_failed_probe() {
        let alive = classify_probe(true, "mux-a\nmux-b\n", "");
        assert_eq!(
            alive,
            ServerProbe::Alive(
                ["mux-a", "mux-b"].map(String::from).into_iter().collect()
            )
        );
        // exit-empty off: up with nothing in it is still alive.
        assert_eq!(
            classify_probe(true, "", ""),
            ServerProbe::Alive(HashSet::new())
        );
        // The wording tmux 3.x uses, verified against a scratch socket.
        for stderr in [
            "no server running on /private/tmp/tmux-501/muxterm\n",
            "error connecting to /private/tmp/tmux-501/muxterm (No such file or directory)\n",
            "error connecting to /private/tmp/tmux-501/muxterm (Connection refused)\n",
        ] {
            assert_eq!(classify_probe(false, "", stderr), ServerProbe::Dead);
        }
        // Anything else proves nothing.
        assert_eq!(
            classify_probe(false, "", "error connecting to x (Permission denied)"),
            ServerProbe::Unknown
        );
        assert_eq!(classify_probe(false, "", ""), ServerProbe::Unknown);
    }

    /// No copy, no buffer name - and only a name under this copy's own
    /// prefix will do, so an older buffer can never be read in its place.
    #[test]
    fn only_this_copys_buffer_is_read() {
        let p = "muxterm-copy-0a1b2c3d-";
        assert_eq!(copied_buffer("80\n", p), None);
        assert_eq!(copied_buffer("", p), None);
        assert_eq!(
            copied_buffer("80\nmuxterm-copy-0a1b2c3d-7\n", p),
            Some((80, "muxterm-copy-0a1b2c3d-7"))
        );
        assert_eq!(copied_buffer("80\nbuffer3\n", p), None);
        assert_eq!(copied_buffer("80\nmuxterm-copy-ffffffff-2\n", p), None);
        // A width that isn't one means the output isn't what was asked for.
        assert_eq!(copied_buffer("oops\nmuxterm-copy-0a1b2c3d-7\n", p), None);
    }

    /// SGR reports are 1-based, column first; the meta bit (8) is what
    /// routes a gesture to its own bindings, and the wheel steps carry none.
    #[test]
    fn gestures_encode_as_meta_tagged_sgr_reports() {
        let at = |g: Gesture| String::from_utf8(g.report((4, 9))).unwrap();
        assert_eq!(at(Gesture::Press), "\x1b[<8;10;5M");
        assert_eq!(at(Gesture::Drag), "\x1b[<40;10;5M");
        assert_eq!(at(Gesture::Release), "\x1b[<8;10;5m");
        // Word and line are whole clicks: a press and its release.
        assert_eq!(at(Gesture::Word), "\x1b[<10;10;5M\x1b[<10;10;5m");
        assert_eq!(at(Gesture::Line), "\x1b[<9;10;5M\x1b[<9;10;5m");
        assert_eq!(at(Gesture::ScrollUp), "\x1b[<64;10;5M");
        assert_eq!(at(Gesture::ScrollDown), "\x1b[<65;10;5M");
        assert_eq!(
            String::from_utf8(Gesture::Press.report((0, 0))).unwrap(),
            "\x1b[<8;1;1M"
        );
    }

    /// Every key a gesture can produce is bound in the root table: one left
    /// unbound would be forwarded to the pane's program, which is exactly
    /// the stray click egui_term P16 exists to prevent.
    #[test]
    fn every_gesture_key_is_bound() {
        let mut keys: Vec<String> =
            ["MouseDrag1Pane", "MouseDragEnd1Pane"].map(String::from).to_vec();
        for button in 1..=3 {
            for click in
                ["MouseDown", "MouseUp", "SecondClick", "DoubleClick", "TripleClick"]
            {
                keys.push(format!("{click}{button}Pane"));
            }
        }
        for copy_on_select in [true, false] {
            let text = conf(copy_on_select, &style());
            for key in &keys {
                assert!(
                    text.contains(&format!("bind -n M-{key} ")),
                    "root M-{key} unbound (copy_on_select={copy_on_select})"
                );
            }
            // A drag enters copy-mode from root and continues inside it.
            assert!(text.contains("bind -n M-MouseDrag1Pane copy-mode -M\n"));
            for table in ["copy-mode", "copy-mode-vi"] {
                assert!(text.contains(&format!(
                    "bind -T {table} M-MouseDrag1Pane {{ select-pane ; send-keys -X begin-selection }}"
                )));
            }
            assert!(text.contains(
                "bind -n M-MouseDown3Pane { select-pane -t = ; copy-mode ; send-keys -X select-word"
            ));
            assert!(text.contains(
                "bind -n M-MouseDown2Pane { select-pane -t = ; copy-mode ; send-keys -X select-line"
            ));
        }
    }

    /// A gesture only ever selects. Copying - copy_on_select's too - is
    /// muxterm's own `take_selection`, because a copy tmux makes inside a
    /// binding reaches the clipboard only through an escape tmux drops
    /// whenever a redraw is pending, as it is right after entering copy-mode.
    #[test]
    fn gestures_never_copy() {
        let text = gesture_bindings();
        assert!(!text.contains("copy-selection"), "{text}");
        assert!(!text.contains("copy-pipe"), "{text}");
        assert!(text.contains("bind -T copy-mode M-MouseDragEnd1Pane select-pane\n"));
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
            assert!(text.contains("terminal-features ',xterm*:hyperlinks'"));
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
                "set -g word-separators \" \\t\"",
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
