pub mod settings;

use crate::types::Size;
use alacritty_terminal::event::{
    Event, EventListener, Notify, OnResize, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{
    Selection, SelectionRange, SelectionType as AlacrittySelectionType,
};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{
    self,
    cell::{Cell, Flags},
    test::TermSize,
    viewport_to_point, Term, TermMode,
};
use alacritty_terminal::{tty, Grid};
use egui::Modifiers;
use settings::BackendSettings;
use std::borrow::Cow;
use std::cmp::min;
use std::collections::HashSet;
use std::io::Result;
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc};
use std::time::Duration;

pub type TerminalMode = TermMode;
pub type PtyEvent = Event;
pub type SelectionType = AlacrittySelectionType;

#[derive(Debug, Clone)]
pub enum BackendCommand {
    Write(Vec<u8>),
    Scroll(i32),
    Resize(Size, Size),
    SelectStart(SelectionType, f32, f32),
    SelectUpdate(f32, f32),
    ProcessLink(LinkAction, Point),
    MouseReport(MouseButton, Modifiers, Point, bool),
}

/// muxterm patch P21: how eagerly a pane's PTY output may wake the UI.
/// The host app knows which panes are visible and whether the window is
/// focused; it publishes that per pane via `set_repaint_policy`, and the
/// PTY subscription thread reads it per event. Only flood-class events
/// (`Wakeup` and friends) honor the policy - rare events stay immediate.
/// Defaults to `Live`, so a host that never sets a policy keeps
/// upstream's repaint-per-event behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RepaintPolicy {
    /// Rendered with the window focused: repaint immediately per event.
    Live = 0,
    /// Rendered but the window is unfocused: coalesce output to ~4 Hz.
    Throttled = 1,
    /// Not rendered (a background tab): coalesce output to ~2 Hz.
    Background = 2,
}

/// P21: coalesced wake cadence under each non-Live policy. egui keeps
/// only the smallest pending `request_repaint_after`, so concurrent
/// delayed wakes from many panes collapse into one frame.
const THROTTLED_WAKE: Duration = Duration::from_millis(250);
const BACKGROUND_WAKE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub enum MouseMode {
    Sgr,
    Normal(bool),
}

impl From<TermMode> for MouseMode {
    fn from(term_mode: TermMode) -> Self {
        if term_mode.contains(TermMode::SGR_MOUSE) {
            MouseMode::Sgr
        } else if term_mode.contains(TermMode::UTF8_MOUSE) {
            MouseMode::Normal(true)
        } else {
            MouseMode::Normal(false)
        }
    }
}

#[derive(Debug, Clone)]
pub enum MouseButton {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
    Other = 99,
}

#[derive(Debug, Clone)]
pub enum LinkAction {
    Clear,
    Hover,
    Open,
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalSize {
    // Exact font metrics (not truncated to integers): the renderer batches
    // whole runs of text into single galleys, which only lines up with the
    // grid if cell advance and cell width agree to the sub-pixel.
    pub cell_width: f32,
    pub cell_height: f32,
    num_cols: u16,
    num_lines: u16,
    layout_size: Size,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cell_width: 1.0,
            cell_height: 1.0,
            num_cols: 80,
            num_lines: 50,
            layout_size: Size::default(),
        }
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.num_lines as usize
    }

    fn columns(&self) -> usize {
        self.num_cols as usize
    }

    fn last_column(&self) -> Column {
        Column(self.num_cols as usize - 1)
    }

    fn bottommost_line(&self) -> Line {
        Line(self.num_lines as i32 - 1)
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(size: TerminalSize) -> Self {
        Self {
            num_lines: size.num_lines,
            num_cols: size.num_cols,
            cell_width: size.cell_width as u16,
            cell_height: size.cell_height as u16,
        }
    }
}

pub struct TerminalBackend {
    pub id: u64,
    pub url_regex: regex::Regex,
    // muxterm patch P10: file paths are links too (iTerm's semantic
    // history). Kept separate from url_regex so URLs win when both match.
    pub path_regex: regex::Regex,
    /// muxterm patch P24: `#<number>` tokens are links when the number is
    /// in the app-registered set (`set_pr_links`); `None` - the default -
    /// keeps them inert, so hosts that never call the setter see no change.
    pr_regex: regex::Regex,
    pr_links: Option<Arc<HashSet<u64>>>,
    /// muxterm patch P10/P20: where a cmd+clicked link's candidate texts
    /// go, most complete first (P20 rejoins tokens a TUI hard-wrapped, so a
    /// guess and its unjoined fallback travel together). The app decides
    /// what "open" means (resolve relative paths against the pane's cwd,
    /// existence-check candidates in order, pick an opener); without one,
    /// candidates fall back to `open::that` until one succeeds.
    link_opener: Option<Box<dyn Fn(&[String]) + Send + Sync>>,
    term: Arc<FairMutex<Term<EventProxy>>>,
    size: TerminalSize,
    notifier: Notifier,
    last_content: RenderableContent,
    /// Set by the PTY event thread and by grid-mutating commands; cleared
    /// by `sync()`. While clear, frames reuse `last_content` instead of
    /// re-cloning the grid under the terminal lock.
    dirty: Arc<AtomicBool>,
    /// muxterm patch P21: shared with the PTY subscription thread, which
    /// picks immediate vs coalesced wakes from it per event.
    repaint_policy: Arc<AtomicU8>,
    /// muxterm patch P22: bumped by `sync()` whenever it consumes the
    /// dirty flag - the one place fresh content enters `last_content` -
    /// so it versions everything the renderer reads from there.
    generation: u64,
    /// muxterm patch P22: the last frame's built shapes, replayed by
    /// `view::show` while its cache key still matches (see
    /// `view::RenderCache`). Owned here so it dies with the pane.
    pub(crate) render_cache: Option<crate::view::RenderCache>,
}

impl TerminalBackend {
    pub fn new(
        id: u64,
        app_context: egui::Context,
        pty_event_proxy_sender: Sender<(u64, PtyEvent)>,
        settings: BackendSettings,
    ) -> Result<Self> {
        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(settings.shell, settings.args)),
            working_directory: settings.working_directory,
            ..tty::Options::default()
        };
        let config = term::Config {
            // tmux owns scrollback (history-limit in the managed tmux.conf)
            // and the wheel is forwarded to tmux copy-mode while mouse
            // reporting is on, so the local history is only reachable if a
            // user manually turns tmux mouse off. Keeping it tiny matters:
            // sync() deep-clones the whole grid, history included, on every
            // dirty frame — at the default 10k lines that's tens of MB per
            // clone once the buffer fills.
            scrolling_history: 200,
            // muxterm patch P14: double-click selects a whole non-whitespace
            // run. Alacritty's default semantic_escape_chars (",│`|:\"' ()[]{}<>\t")
            // split words at quotes/brackets/colons/slashes' neighbors, so a
            // double-click on foo(bar) or a/b:c only grabbed a fragment. Reducing
            // the boundary set to just whitespace makes Semantic selection cover
            // every contiguous non-whitespace character, matching iTerm/macOS.
            semantic_escape_chars: " \t".to_owned(),
            ..term::Config::default()
        };
        let terminal_size = TerminalSize::default();
        let pty = tty::new(&pty_config, terminal_size.into(), id)?;
        let (event_sender, event_receiver) = mpsc::channel();
        let event_proxy = EventProxy(event_sender);
        let mut term = Term::new(config, &terminal_size, event_proxy.clone());
        let initial_content = RenderableContent {
            grid: term.grid().clone(),
            selectable_range: None,
            terminal_mode: *term.mode(),
            terminal_size,
            cursor: term.grid_mut().cursor_cell().clone(),
            hovered_hyperlink: None,
        };
        let term = Arc::new(FairMutex::new(term));
        let pty_event_loop =
            EventLoop::new(term.clone(), event_proxy, pty, false, false)?;
        let notifier = Notifier(pty_event_loop.channel());
        let url_regex = url_regex();
        let path_regex = path_regex();
        let _pty_event_loop_thread = pty_event_loop.spawn();
        let dirty = Arc::new(AtomicBool::new(true));
        let thread_dirty = dirty.clone();
        let repaint_policy =
            Arc::new(AtomicU8::new(RepaintPolicy::Live as u8));
        let thread_policy = repaint_policy.clone();
        let _pty_event_subscription = std::thread::Builder::new()
            .name(format!("pty_event_subscription_{}", id))
            .spawn(move || loop {
                // muxterm patch P4: break on channel disconnect instead of
                // busy-looping, and don't panic when the app-side receiver
                // is already gone during shutdown.
                match event_receiver.recv() {
                    Ok(event) => {
                        if pty_event_proxy_sender
                            .send((id, event.clone()))
                            .is_err()
                        {
                            break;
                        }
                        // Mark before waking the UI so the repaint that
                        // follows can never observe a clean flag - the
                        // delayed wakes below included, they always fire
                        // after this store.
                        thread_dirty.store(true, Ordering::Release);
                        // muxterm patch P21: only flood-class events honor
                        // the policy; every other event is rare and
                        // latency-sensitive even when hidden (PtyWrite is
                        // a blocked DA/DSR reply that only flushes inside
                        // a frame, Exit/ChildExit tear the pane down, Bell
                        // alerts, Title relabels, ClipboardStore is an
                        // OSC 52 copy), so those wake the UI immediately.
                        // A stale policy read can only misclassify a
                        // delay, never lose an update: some wake is always
                        // scheduled and dirty is already set.
                        let gated = matches!(
                            event,
                            Event::Wakeup
                                | Event::MouseCursorDirty
                                | Event::CursorBlinkingChange
                        );
                        match thread_policy.load(Ordering::Acquire) {
                            _ if !gated => app_context.request_repaint(),
                            p if p == RepaintPolicy::Live as u8 => {
                                app_context.request_repaint()
                            },
                            p if p == RepaintPolicy::Throttled as u8 => {
                                app_context
                                    .request_repaint_after(THROTTLED_WAKE)
                            },
                            _ => app_context
                                .request_repaint_after(BACKGROUND_WAKE),
                        }
                        if let Event::Exit = event {
                            break;
                        }
                    },
                    Err(_) => break,
                }
            })?;

        Ok(Self {
            id,
            url_regex,
            path_regex,
            pr_regex: pr_regex(),
            pr_links: None,
            link_opener: None,
            term: term.clone(),
            size: terminal_size,
            notifier,
            last_content: initial_content,
            dirty,
            repaint_policy,
            generation: 0,
            render_cache: None,
        })
    }

    /// muxterm patch P22: see the `generation` field.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// muxterm patch P21: publish how eagerly this pane's PTY output may
    /// wake the UI. `&self` on purpose - the app sweeps every pane once
    /// per frame, and the store is a single atomic.
    pub fn set_repaint_policy(&self, policy: RepaintPolicy) {
        self.repaint_policy.store(policy as u8, Ordering::Release);
    }

    pub fn process_command(&mut self, cmd: BackendCommand) {
        // muxterm patch P22: the view issues a Resize every frame and the
        // no-op check reads only self.size - bail before taking the
        // terminal lock so frames never contend with a streaming parser.
        if let BackendCommand::Resize(layout_size, font_size) = &cmd {
            if *layout_size == self.size.layout_size
                && font_size.width == self.size.cell_width
                && font_size.height == self.size.cell_height
            {
                return;
            }
        }
        let term = self.term.clone();
        let mut term = term.lock();
        match cmd {
            BackendCommand::Write(input) => {
                self.write(input);
                term.scroll_display(Scroll::Bottom);
                self.dirty.store(true, Ordering::Release);
            },
            BackendCommand::Scroll(delta) => {
                self.scroll(&mut term, delta);
                self.dirty.store(true, Ordering::Release);
            },
            BackendCommand::Resize(layout_size, font_size) => {
                // resize() marks dirty itself, only on an actual change —
                // the view issues this command every frame.
                self.resize(&mut term, layout_size, font_size);
            },
            BackendCommand::SelectStart(selection_type, x, y) => {
                self.start_selection(&mut term, selection_type, x, y);
                self.dirty.store(true, Ordering::Release);
            },
            BackendCommand::SelectUpdate(x, y) => {
                self.update_selection(&mut term, x, y);
                self.dirty.store(true, Ordering::Release);
            },
            BackendCommand::ProcessLink(link_action, point) => {
                self.process_link_action(&term, link_action, point);
            },
            BackendCommand::MouseReport(button, modifiers, point, pressed) => {
                self.process_mouse_report(button, modifiers, point, pressed);
            },
        };
    }

    pub fn selection_point(
        x: f32,
        y: f32,
        terminal_size: &TerminalSize,
        display_offset: usize,
    ) -> Point {
        let col = (x.max(0.0) / terminal_size.cell_width) as usize;
        let col = min(Column(col), Column(terminal_size.num_cols as usize - 1));

        let line = (y.max(0.0) / terminal_size.cell_height) as usize;
        let line = min(line, terminal_size.num_lines as usize - 1);

        viewport_to_point(display_offset, Point::new(line, col))
    }

    pub fn selectable_content(&self) -> String {
        let content = self.last_content();
        let mut result = String::new();
        if let Some(range) = content.selectable_range {
            for indexed in content.grid.display_iter() {
                if range.contains(indexed.point) {
                    result.push(indexed.c);
                }
            }
        }
        result
    }

    /// muxterm patch P8: the selection as text straight from the live
    /// terminal - not the last-rendered frame, so a SelectStart processed
    /// this frame is already included - with line breaks preserved. None
    /// when nothing is effectively selected (a bare click), so callers
    /// can't clobber the clipboard with an empty string.
    pub fn selection_content(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    pub fn sync(&mut self) -> &RenderableContent {
        // Clear-before-clone: a PTY write racing the clone re-marks the
        // flag and the next frame picks it up; the ordering can lose a
        // frame of staleness but never an update.
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return &self.last_content;
        }
        // muxterm patch P22: new content is about to land in last_content;
        // version it for the render cache.
        self.generation = self.generation.wrapping_add(1);
        let term = self.term.clone();
        let mut terminal = term.lock();
        let selectable_range = match &terminal.selection {
            Some(s) => s.to_range(&terminal),
            None => None,
        };

        let cursor = terminal.grid_mut().cursor_cell().clone();
        self.last_content.grid = terminal.grid().clone();
        self.last_content.selectable_range = selectable_range;
        self.last_content.cursor = cursor.clone();
        self.last_content.terminal_mode = *terminal.mode();
        self.last_content.terminal_size = self.size;
        self.last_content()
    }

    pub fn last_content(&self) -> &RenderableContent {
        &self.last_content
    }

    /// muxterm patch P10: register the app's link opener (see the field).
    pub fn set_link_opener(
        &mut self,
        opener: impl Fn(&[String]) + Send + Sync + 'static,
    ) {
        self.link_opener = Some(Box::new(opener));
    }

    /// muxterm patch P24: which PR numbers a `#<number>` token may link
    /// to; `None` turns PR tokens off entirely. Arc'd so the app shares
    /// one set across every pane of a repo.
    pub fn set_pr_links(&mut self, links: Option<Arc<HashSet<u64>>>) {
        self.pr_links = links;
    }

    /// muxterm patch P24: the pr argument `link_match_at` expects.
    fn pr_param(&self) -> Option<(&regex::Regex, &HashSet<u64>)> {
        self.pr_links.as_deref().map(|set| (&self.pr_regex, set))
    }

    /// muxterm patch P10: is there a URL or path-shaped token under this
    /// grid point right now? The view asks at press time to decide whether
    /// a cmd+click bypasses tmux mouse reporting.
    pub fn has_link_at(&self, point: Point) -> bool {
        let terminal = self.term.lock();
        link_match_at(
            &terminal,
            point,
            &self.url_regex,
            &self.path_regex,
            self.pr_param(),
        )
        .is_some()
    }

    fn process_link_action(
        &mut self,
        terminal: &Term<EventProxy>,
        link_action: LinkAction,
        point: Point,
    ) {
        match link_action {
            LinkAction::Hover => {
                // muxterm patch P10: paths hover like URLs, URLs win ties.
                self.last_content.hovered_hyperlink = link_match_at(
                    terminal,
                    point,
                    &self.url_regex,
                    &self.path_regex,
                    self.pr_param(),
                )
                .map(|(_, range)| range);
            },
            LinkAction::Clear => {
                self.last_content.hovered_hyperlink = None;
            },
            LinkAction::Open => {
                // muxterm patch P10: resolve the match at the clicked point
                // from the live terminal instead of trusting the last hover
                // (which may be stale or unset if the mouse never moved),
                // and never panic on a failed open.
                let texts = link_match_at(
                    terminal,
                    point,
                    &self.url_regex,
                    &self.path_regex,
                    self.pr_param(),
                )
                .map(|(texts, _)| texts);
                if let Some(texts) = texts {
                    match &self.link_opener {
                        Some(opener) => opener(&texts),
                        None => {
                            // No app opener means no cwd to existence-check
                            // against; walk the candidates until one opens.
                            let _ =
                                texts.iter().find(|t| open::that(t).is_ok());
                        },
                    }
                }
            },
        };
    }

    fn process_mouse_report(
        &self,
        button: MouseButton,
        modifiers: Modifiers,
        point: Point,
        pressed: bool,
    ) {
        let mut mods = 0;
        if modifiers.contains(Modifiers::SHIFT) {
            mods += 4;
        }
        if modifiers.contains(Modifiers::ALT) {
            mods += 8;
        }
        if modifiers.contains(Modifiers::COMMAND) {
            mods += 16;
        }

        match MouseMode::from(self.last_content().terminal_mode) {
            MouseMode::Sgr => {
                self.sgr_mouse_report(point, button as u8 + mods, pressed)
            },
            MouseMode::Normal(is_utf8) => {
                if pressed {
                    self.normal_mouse_report(
                        point,
                        button as u8 + mods,
                        is_utf8,
                    )
                } else {
                    self.normal_mouse_report(point, 3 + mods, is_utf8)
                }
            },
        }
    }

    fn sgr_mouse_report(&self, point: Point, button: u8, pressed: bool) {
        let c = if pressed { 'M' } else { 'm' };

        let msg = format!(
            "\x1b[<{};{};{}{}",
            button,
            point.column + 1,
            point.line + 1,
            c
        );

        self.notifier.notify(msg.as_bytes().to_vec());
    }

    fn normal_mouse_report(&self, point: Point, button: u8, is_utf8: bool) {
        let Point { line, column } = point;
        let max_point = if is_utf8 { 2015 } else { 223 };

        if line >= max_point || column >= max_point {
            return;
        }

        let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

        let mouse_pos_encode = |pos: usize| -> Vec<u8> {
            let pos = 32 + 1 + pos;
            let first = 0xC0 + pos / 64;
            let second = 0x80 + (pos & 63);
            vec![first as u8, second as u8]
        };

        if is_utf8 && column >= Column(95) {
            msg.append(&mut mouse_pos_encode(column.0));
        } else {
            msg.push(32 + 1 + column.0 as u8);
        }

        if is_utf8 && line >= 95 {
            msg.append(&mut mouse_pos_encode(line.0 as usize));
        } else {
            msg.push(32 + 1 + line.0 as u8);
        }

        self.notifier.notify(msg);
    }

    fn start_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        selection_type: SelectionType,
        x: f32,
        y: f32,
    ) {
        let location = Self::selection_point(
            x,
            y,
            &self.size,
            terminal.grid().display_offset(),
        );
        terminal.selection = Some(Selection::new(
            selection_type,
            location,
            self.selection_side(x),
        ));
    }

    fn update_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        x: f32,
        y: f32,
    ) {
        let display_offset = terminal.grid().display_offset();
        if let Some(ref mut selection) = terminal.selection {
            let location =
                Self::selection_point(x, y, &self.size, display_offset);
            selection.update(location, self.selection_side(x));
        }
    }

    fn selection_side(&self, x: f32) -> Side {
        let cell_x = x.max(0.0) % self.size.cell_width;

        if cell_x > self.size.cell_width / 2.0 {
            Side::Right
        } else {
            Side::Left
        }
    }

    fn resize(
        &mut self,
        terminal: &mut Term<EventProxy>,
        layout_size: Size,
        font_size: Size,
    ) {
        if layout_size == self.size.layout_size
            && font_size.width == self.size.cell_width
            && font_size.height == self.size.cell_height
        {
            return;
        }

        let lines = (layout_size.height / font_size.height) as u16;
        let cols = (layout_size.width / font_size.width) as u16;
        if lines > 0 && cols > 0 {
            self.size = TerminalSize {
                layout_size,
                cell_height: font_size.height,
                cell_width: font_size.width,
                num_lines: lines,
                num_cols: cols,
            };

            self.notifier.on_resize(self.size.into());
            terminal.resize(TermSize::new(
                self.size.num_cols as usize,
                self.size.num_lines as usize,
            ));
            self.dirty.store(true, Ordering::Release);
        }
    }

    fn write<I: Into<Cow<'static, [u8]>>>(&self, input: I) {
        self.notifier.notify(input);
    }

    fn scroll(&mut self, terminal: &mut Term<EventProxy>, delta_value: i32) {
        if delta_value != 0 {
            let scroll = Scroll::Delta(delta_value);
            if terminal
                .mode()
                .contains(TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN)
            {
                let line_cmd = if delta_value > 0 { b'A' } else { b'B' };
                let mut content = vec![];

                for _ in 0..delta_value.abs() {
                    content.push(0x1b);
                    content.push(b'O');
                    content.push(line_cmd);
                }

                self.notifier.notify(content);
            } else {
                terminal.grid_mut().scroll_display(scroll);
            }
        }
    }

}

/// The URL detector. Upstream's pattern, `\u{..}` rewritten as `\x{..}` for
/// the `regex` crate (P19).
fn url_regex() -> regex::Regex {
    regex::Regex::new(r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\x{0}-\x{1F}\x{7F}-\x{9F}<>"\s{-}\^⟨⟩`]+"#).unwrap()
}

/// muxterm patch P23: strip the sentence punctuation a URL match swallowed
/// - `.,;:!?'()` are all legal URL chars, so prose like
/// "(https://ex.com/tokens/)." matches through the close-paren and dot.
/// Only the trailing run is shed (a mid-URL `?query` is untouched), and a
/// closing bracket only while unbalanced, keeping Wikipedia-style
/// "..._(disambiguation)" URLs whole.
fn trim_url_punct(text: &str) -> &str {
    let mut s = text;
    loop {
        let Some(c) = s.chars().last() else { return s };
        let trim = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' => true,
            ')' => s.matches('(').count() < s.matches(')').count(),
            ']' => s.matches('[').count() < s.matches(']').count(),
            _ => false,
        };
        if !trim {
            return s;
        }
        s = &s[..s.len() - c.len_utf8()];
    }
}

/// muxterm patch P10: path-shaped tokens - absolute (`/x/y`), homedir
/// (`~/x`), dot-relative (`./x`, `../x`), and bare-relative with at least
/// one slash (`src/app.rs`, as rustc and grep print), optionally carrying a
/// `:line` or `:line:col` suffix. Existence is deliberately not checked
/// here (the grid has no cwd); the app-side opener filters false positives
/// like `and/or` by only opening paths that exist.
fn path_regex() -> regex::Regex {
    regex::Regex::new(
        r#"(~|\.\.?|[A-Za-z0-9._@%+-]+)?(/[A-Za-z0-9._@%+-]+)+/?(:\d+(:\d+)?)?"#,
    )
    .unwrap()
}

/// muxterm patch P24: `#<digits>` ending at a word boundary - the boundary
/// is what keeps a hex color like `#0044aa` from matching (backtracking
/// cannot shorten its way to a hit: every digit prefix still ends
/// digit-to-word). Which numbers actually link is the app's call
/// (`set_pr_links`); the grid knows nothing about PRs.
fn pr_regex() -> regex::Regex {
    regex::Regex::new(r"#\d+\b").unwrap()
}

/// muxterm patch P19: a grid row wraps into the next when it carries the
/// WRAPLINE flag (native alacritty soft-wrap) *or* its last column holds a
/// glyph (a full row). tmux repaints soft-wrapped output as discrete
/// cursor-positioned rows that drop WRAPLINE, so the full-row heuristic is
/// what stitches a URL that ran off the right edge back together.
fn row_wraps(grid: &Grid<Cell>, line: Line, last_col: Column) -> bool {
    let cell = &grid[line][last_col];
    cell.flags.contains(Flags::WRAPLINE) || cell.c != ' '
}

/// muxterm patch P19: the logical line through `line` - the run of visually
/// continuous rows around it (`row_wraps`), reconstructed as a string with a
/// parallel grid `Point` per char (wide-char spacer cells skipped). Link
/// detection runs the regexes over this so a match spans row boundaries even
/// when tmux stripped WRAPLINE.
fn logical_line<T: EventListener>(
    term: &Term<T>,
    line: Line,
) -> (String, Vec<Point>) {
    if term.columns() == 0 {
        return (String::new(), Vec::new());
    }
    let grid = term.grid();
    let last_col = Column(term.columns() - 1);
    let top_bound = term.topmost_line();
    let bottom_bound = term.bottommost_line();

    // Walk up while the row above wraps into this one, down while this row
    // wraps into the one below.
    let mut top = line;
    while top > top_bound && row_wraps(grid,top - 1i32, last_col) {
        top -= 1i32;
    }
    let mut bottom = line;
    while bottom < bottom_bound && row_wraps(grid,bottom, last_col) {
        bottom += 1i32;
    }

    let mut text = String::new();
    let mut points = Vec::new();
    let mut l = top;
    while l <= bottom {
        for col in 0..term.columns() {
            let point = Point::new(l, Column(col));
            let cell = &grid[point];
            // Skip the padding cells of a wide char so the string stays
            // aligned with the columns the chars actually occupy.
            if cell.flags.intersects(
                Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
            ) {
                continue;
            }
            text.push(cell.c);
            points.push(point);
        }
        l += 1i32;
    }
    (text, points)
}

/// muxterm patch P20: cap on how many whitespace-delimited runs a guessed
/// wrap join may chain together (a path rarely spans more than three rows).
const JOIN_CAP: usize = 6;

/// muxterm patch P20: the [start, end) char spans of the maximal
/// non-whitespace runs of `chars`.
fn runs_of(chars: &[char]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, c) in chars.iter().enumerate() {
        match (start, c.is_whitespace()) {
            (None, false) => start = Some(i),
            (Some(s), true) => {
                runs.push((s, i));
                start = None;
            },
            _ => {},
        }
    }
    if let Some(s) = start {
        runs.push((s, chars.len()));
    }
    runs
}

/// muxterm patch P10/P19/P20: the link candidates under a grid point -
/// their texts ordered most-complete-first plus the grid span of the best
/// one - URLs winning over paths (every URL tail is also a path-shaped
/// token). Matches run over the clicked point's reconstructed logical line,
/// so soft-wrapped links resolve whole (P19). A TUI that hard-wraps a long
/// token at its own layout edge (Claude Code indents the continuation, so
/// the rows are not visually continuous and the wrap whitespace splits the
/// token) is handled speculatively (P20): a clicked run that starts its
/// line may continue the previous line's trailing run, one that ends its
/// line may continue onto the next line's leading run - but a run is only
/// taken as a continuation when it is indented, which is what separates a
/// wrapped box (TUIs indent continuation rows) from a flush-left column of
/// distinct paths. The emitter's wrap point is invisible, so a joining
/// that does happen is still a guess: each plausible one is a
/// candidate, longest match first, the plain unjoined match last - the
/// app-side opener existence-checks them in order, which is what filters
/// bad guesses (prose gluing `src/app.rs` + `and` loses to `src/app.rs`).
/// URLs never match across a guessed join: they open without an existence
/// check, and gluing the next line's word onto a URL would open a wrong
/// address. Within one logical line (P19) the text really is contiguous,
/// so URLs still span soft wraps.
fn link_match_at<T: EventListener>(
    term: &Term<T>,
    point: Point,
    url_regex: &regex::Regex,
    path_regex: &regex::Regex,
    pr: Option<(&regex::Regex, &HashSet<u64>)>,
) -> Option<(Vec<String>, RangeInclusive<Point>)> {
    let (ltext, lpoints) = logical_line(term, point.line);
    let lchars: Vec<char> = ltext.chars().collect();
    // Char index of the clicked cell within the reconstructed line.
    let click = lpoints.iter().position(|p| *p == point)?;
    let runs = runs_of(&lchars);
    let ci = runs.iter().position(|&(s, e)| (s..e).contains(&click))?;

    let run = |chars: &[char], points: &[Point], (s, e): (usize, usize)| {
        (chars[s..e].iter().collect::<String>(), points[s..e].to_vec())
    };

    // The chain of runs the clicked token may span. Whitespace between
    // runs is dropped: a join models the wrap gap (trailing pad + newline
    // + continuation indent) that the emitter inserted mid-token.
    let mut chain = vec![run(&lchars, &lpoints, runs[ci])];
    let mut clicked = 0usize; // index of the clicked run within the chain
    let click_in_run = click - runs[ci].0;

    // A run only counts as a wrap *continuation* when it is indented: a
    // boxed TUI indents the rows it wraps onto, while flush-left runs are
    // separate items (a find/ls column of paths must not glue).

    // Backward: an indented run that starts its line may continue the
    // previous line's trailing run. Keep walking only through lines that
    // are themselves a lone indented run (a pure middle segment); the
    // head line ends the walk.
    let mut continues = ci == 0 && runs[ci].0 > 0;
    let mut top = lpoints[0].line;
    while continues && chain.len() < JOIN_CAP && top > term.topmost_line() {
        let (ptext, ppoints) = logical_line(term, top - 1i32);
        let pchars: Vec<char> = ptext.chars().collect();
        let pruns = runs_of(&pchars);
        let Some(&last) = pruns.last() else { break };
        top = ppoints[0].line;
        chain.insert(0, run(&pchars, &ppoints, last));
        clicked += 1;
        continues = pruns.len() == 1 && last.0 > 0;
    }

    // Forward: a run that ends its line may wrap onto the next line's
    // leading run, if that run is indented.
    let mut continues = ci == runs.len() - 1;
    let mut bottom = lpoints[lpoints.len() - 1].line;
    while continues && chain.len() < JOIN_CAP && bottom < term.bottommost_line()
    {
        let (ntext, npoints) = logical_line(term, bottom + 1i32);
        let nchars: Vec<char> = ntext.chars().collect();
        let nruns = runs_of(&nchars);
        let Some(&first) = nruns.first() else { break };
        if first.0 == 0 {
            break;
        }
        bottom = npoints[npoints.len() - 1].line;
        chain.push(run(&nchars, &npoints, first));
        continues = nruns.len() == 1;
    }

    // Every contiguous sub-chain around the clicked run is a candidate
    // joining; a match must cover the clicked cell. Sub-chains that cross
    // a guessed join match paths only (see above).
    let mut cands: Vec<(String, RangeInclusive<Point>)> = Vec::new();
    for i in 0..=clicked {
        for j in clicked..chain.len() {
            let text: String =
                chain[i..=j].iter().map(|(t, _)| t.as_str()).collect();
            let points: Vec<Point> = chain[i..=j]
                .iter()
                .flat_map(|(_, p)| p.iter().copied())
                .collect();
            let click_at = click_in_run
                + chain[i..clicked]
                    .iter()
                    .map(|(t, _)| t.chars().count())
                    .sum::<usize>();
            // URL matches shed trailing sentence punctuation (P23) before
            // the click test, so the shed chars neither highlight nor open.
            let hit = |re: &regex::Regex, trim: bool| {
                for m in re.find_iter(&text) {
                    let s =
                        if trim { trim_url_punct(m.as_str()) } else { m.as_str() };
                    let start = text[..m.start()].chars().count();
                    let end = start + s.chars().count(); // exclusive
                    if (start..end).contains(&click_at) {
                        return Some((
                            s.to_string(),
                            points[start]..=points[end - 1],
                        ));
                    }
                }
                None
            };
            // muxterm patch P24: a `#<number>` token is a link only when
            // the app registered the number as a known PR. Below URLs (the
            // URL char class allows `#`, so a fragment must not hijack its
            // URL), above paths; never joined across guessed wraps.
            let hit_pr = || {
                let (re, set) = pr?;
                for m in re.find_iter(&text) {
                    let known = m.as_str()[1..]
                        .parse::<u64>()
                        .is_ok_and(|n| set.contains(&n));
                    if !known {
                        continue;
                    }
                    let start = text[..m.start()].chars().count();
                    let end = text[..m.end()].chars().count(); // exclusive
                    if (start..end).contains(&click_at) {
                        return Some((
                            m.as_str().to_string(),
                            points[start]..=points[end - 1],
                        ));
                    }
                }
                None
            };
            let found = if i == j {
                hit(url_regex, true)
                    .or_else(hit_pr)
                    .or_else(|| hit(path_regex, false))
            } else {
                hit(path_regex, false)
            };
            if let Some(cand) = found {
                cands.push(cand);
            }
        }
    }

    // Longest match first; hover shows the best candidate's span.
    cands.sort_by_key(|(t, _)| std::cmp::Reverse(t.chars().count()));
    let mut texts: Vec<String> = Vec::new();
    let mut span = None;
    for (text, s) in cands {
        if !texts.contains(&text) {
            span.get_or_insert(s);
            texts.push(text);
        }
    }
    Some((texts, span?))
}

pub struct RenderableContent {
    pub grid: Grid<Cell>,
    pub hovered_hyperlink: Option<RangeInclusive<Point>>,
    pub selectable_range: Option<SelectionRange>,
    pub cursor: Cell,
    pub terminal_mode: TermMode,
    pub terminal_size: TerminalSize,
}

impl Default for RenderableContent {
    fn default() -> Self {
        Self {
            grid: Grid::new(0, 0, 0),
            hovered_hyperlink: None,
            selectable_range: None,
            cursor: Cell::default(),
            terminal_mode: TermMode::empty(),
            terminal_size: TerminalSize::default(),
        }
    }
}

impl Drop for TerminalBackend {
    fn drop(&mut self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

#[derive(Clone)]
pub struct EventProxy(mpsc::Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event.clone());
    }
}

// muxterm patch P10: link detection tests over a mock terminal.
#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::test::mock_term;

    /// The link candidates found when clicking (line, col) of `content`,
    /// most complete first (P20).
    fn link_candidates(
        content: &str,
        line: i32,
        col: usize,
    ) -> Option<Vec<String>> {
        let term = mock_term(content);
        link_match_at(
            &term,
            Point::new(Line(line), Column(col)),
            &url_regex(),
            &path_regex(),
            None,
        )
        .map(|(texts, _)| texts)
    }

    /// The best link text found when clicking (line, col) of `content`.
    fn link_at(content: &str, line: i32, col: usize) -> Option<String> {
        link_candidates(content, line, col).map(|mut texts| texts.remove(0))
    }

    /// The best link text at (line, col) with `prs` registered as the
    /// known PR numbers (P24).
    fn pr_link_at(
        content: &str,
        line: i32,
        col: usize,
        prs: &[u64],
    ) -> Option<String> {
        let term = mock_term(content);
        let set: HashSet<u64> = prs.iter().copied().collect();
        link_match_at(
            &term,
            Point::new(Line(line), Column(col)),
            &url_regex(),
            &path_regex(),
            Some((&pr_regex(), &set)),
        )
        .map(|(mut texts, _)| texts.remove(0))
    }

    #[test]
    fn urls_and_paths_are_found_under_the_point() {
        let line = "see https://example.com/a?b=1 and /tmp/out.png here";
        assert_eq!(
            link_at(line, 0, 10),
            Some("https://example.com/a?b=1".into())
        );
        assert_eq!(link_at(line, 0, 36), Some("/tmp/out.png".into()));
        // Plain prose around them is not a link.
        assert_eq!(link_at(line, 0, 0), None);
        assert_eq!(link_at(line, 0, 30), None);
    }

    #[test]
    fn url_wins_over_its_own_path_tail() {
        // The point sits in the path part of the URL; the whole URL must
        // come back, not the /a/b tail.
        let text = link_at("https://example.com/a/b", 0, 21).unwrap();
        assert_eq!(text, "https://example.com/a/b");
    }

    // muxterm patch P23: trailing sentence punctuation is not part of a URL.
    #[test]
    fn urls_shed_trailing_punctuation() {
        let line =
            "docs (https://docs.slack.dev/authentication/tokens/). here";
        assert_eq!(
            link_at(line, 0, 12),
            Some("https://docs.slack.dev/authentication/tokens/".into())
        );
        // The shed close-paren itself is not clickable.
        assert_eq!(link_at(line, 0, 51), None);
    }

    #[test]
    fn balanced_parens_stay_in_the_url() {
        let line = "see https://en.wikipedia.org/wiki/Rust_(language), ok";
        assert_eq!(
            link_at(line, 0, 10),
            Some("https://en.wikipedia.org/wiki/Rust_(language)".into())
        );
    }

    #[test]
    fn url_punct_trim_edges() {
        for (raw, trimmed) in [
            ("https://x.com/a).", "https://x.com/a"),
            ("https://x.com/docs?", "https://x.com/docs"),
            ("https://x.com/search?q=foo", "https://x.com/search?q=foo"),
            ("https://x.com/a_(b)_(c))", "https://x.com/a_(b)_(c)"),
            ("https://x.com/a],;!'", "https://x.com/a"),
            ("https://x.com/a:8080", "https://x.com/a:8080"),
        ] {
            assert_eq!(trim_url_punct(raw), trimmed, "for {raw:?}");
        }
    }

    #[test]
    fn path_shapes_match() {
        for (line, col, expected) in [
            ("--> src/tmux.rs:57:9", 6, "src/tmux.rs:57:9"),
            ("cat ~/dev/muxterm/README.md", 6, "~/dev/muxterm/README.md"),
            ("run ./target/debug/mux now", 8, "./target/debug/mux"),
            ("cd ../crates/egui_term", 5, "../crates/egui_term"),
            ("grep: src/app.rs:12: match", 8, "src/app.rs:12"),
        ] {
            assert_eq!(
                link_at(line, 0, col).as_deref(),
                Some(expected),
                "in {line:?}"
            );
        }
    }

    #[test]
    fn bare_words_do_not_match() {
        assert_eq!(link_at("just some words", 0, 6), None);
        assert_eq!(link_at("Makefile", 0, 3), None);
    }

    // muxterm patch P24: `#<number>` tokens link to known PRs.
    #[test]
    fn known_pr_numbers_link() {
        let line = "Merge pull request #123 from x (#45)";
        assert_eq!(pr_link_at(line, 0, 20, &[123, 45]), Some("#123".into()));
        // Inside punctuation too, and the token comes back bare.
        assert_eq!(pr_link_at(line, 0, 33, &[123, 45]), Some("#45".into()));
        // Prose around them is not a link.
        assert_eq!(pr_link_at(line, 0, 25, &[123, 45]), None);
    }

    #[test]
    fn unknown_pr_numbers_stay_inert() {
        assert_eq!(pr_link_at("see #999 here", 0, 5, &[123]), None);
        // No registered set at all (the plain helper passes None).
        assert_eq!(link_at("see #123 here", 0, 5), None);
    }

    #[test]
    fn hex_colors_and_spaced_hashes_are_not_prs() {
        // No word boundary splits #0044aa into a match, even with 44
        // registered.
        assert_eq!(pr_link_at("color #0044aa set", 0, 8, &[44, 4]), None);
        assert_eq!(pr_link_at("# 123 heading", 0, 0, &[123]), None);
        assert_eq!(pr_link_at("# 123 heading", 0, 2, &[123]), None);
    }

    #[test]
    fn url_fragment_beats_pr_number() {
        let line = "open https://x.com/a#123 now";
        assert_eq!(
            pr_link_at(line, 0, 21, &[123]),
            Some("https://x.com/a#123".into())
        );
    }

    // muxterm patch P11: the view's cursor gate rests on SHOW_CURSOR
    // tracking DECTCEM; pin that here where a mock term can see it.
    #[test]
    fn dectcem_drives_show_cursor_mode() {
        let mut term = mock_term("some content");
        let mut parser =
            alacritty_terminal::vte::ansi::Processor::<
                alacritty_terminal::vte::ansi::StdSyncHandler,
            >::default();
        assert!(term.mode().contains(TermMode::SHOW_CURSOR));
        parser.advance(&mut term, b"\x1b[?25l");
        assert!(!term.mode().contains(TermMode::SHOW_CURSOR));
        parser.advance(&mut term, b"\x1b[?25h");
        assert!(term.mode().contains(TermMode::SHOW_CURSOR));
    }

    #[test]
    fn wrapped_paths_come_back_joined() {
        // mock_term: "\n" wraps (WRAPLINE), so this is one logical line
        // split across two rows; the match must span the wrap and the
        // extracted text must not contain a newline.
        let text = link_at("ls /private/tmp/some\nthing/deep.png", 1, 3);
        assert_eq!(text, Some("/private/tmp/something/deep.png".into()));
    }

    // muxterm patch P19: the real bug - tmux repaints a soft-wrapped URL as
    // two discrete rows and drops WRAPLINE, so alacritty's search truncated
    // the match at the row edge. mock_term's `\r` ends a row WITHOUT
    // WRAPLINE; the first row still fills the width (it is the longest), so
    // the full-row heuristic stitches the rows back into one URL.
    #[test]
    fn urls_wrap_across_rows_without_wrapline() {
        let content = "https://ex.com/aaaaaaaa\r\nbb?y=2";
        let whole = Some("https://ex.com/aaaaaaaabb?y=2".to_string());
        // Clicking the first row (previously truncated at the edge)...
        assert_eq!(link_at(content, 0, 5), whole);
        // ...and clicking the continuation on the second row.
        assert_eq!(link_at(content, 1, 1), whole);
    }

    // muxterm patch P20: Claude-Code-style hard wrap - the TUI broke a long
    // path at its own layout edge and indented the continuation, so row 0
    // stops short of the grid width (row 1 is longer) and no soft-wrap
    // heuristic applies. The clicked run rejoins with the adjacent line's
    // edge run; the unjoined run stays as the opener's fallback candidate.
    #[test]
    fn indent_wrapped_paths_join_across_lines() {
        let content =
            "a [img/tmp/deft-swan/fa76\r\n    db05/badges.png (21.6KB) x";
        let joined = "img/tmp/deft-swan/fa76db05/badges.png";
        // Clicking the head on row 0...
        assert_eq!(
            link_candidates(content, 0, 10),
            Some(vec![joined.into(), "img/tmp/deft-swan/fa76".into()])
        );
        // ...and the tail on row 1.
        assert_eq!(
            link_candidates(content, 1, 6),
            Some(vec![joined.into(), "db05/badges.png".into()])
        );
    }

    // muxterm patch P20: a middle row that is a lone indented run chains
    // both ways; alone it is not even path-shaped, so only joins make it
    // clickable at all.
    #[test]
    fn paths_wrapped_across_three_lines_join_whole() {
        let content =
            "x /aa/bb-cc\r\n  dd-ee\r\n  ff/gg.png with trailing words";
        assert_eq!(
            link_candidates(content, 1, 4),
            Some(vec![
                "/aa/bb-ccdd-eeff/gg.png".into(),
                "/aa/bb-ccdd-ee".into(),
                "dd-eeff/gg.png".into(),
            ])
        );
    }

    // muxterm patch P20: joining is a guess, so indented prose that merely
    // ends a line with a path keeps the plain match as a fallback - the
    // app-side existence check is what makes `src/app.rsand` lose to
    // `src/app.rs`.
    #[test]
    fn prose_wrap_keeps_the_unjoined_fallback() {
        let content = "see src/app.rs\r\n  and some more words";
        assert_eq!(
            link_candidates(content, 0, 6),
            Some(vec!["src/app.rsand".into(), "src/app.rs".into()])
        );
    }

    // muxterm patch P20: only indented runs count as continuations - a
    // flush-left column of paths (find/ls output) is distinct items, and
    // each row must come back alone.
    #[test]
    fn flush_left_columns_do_not_glue() {
        let content = "src/a.rs\r\nsrc/b.rs\r\nsomething-longer-here x";
        assert_eq!(
            link_candidates(content, 0, 3),
            Some(vec!["src/a.rs".into()])
        );
        assert_eq!(
            link_candidates(content, 1, 3),
            Some(vec!["src/b.rs".into()])
        );
    }

    // muxterm patch P20: URLs open without an existence check, so they must
    // never glue across a guessed wrap - the plain URL stays the best
    // candidate (any joined form may only be path-shaped, which the opener
    // existence-checks and discards).
    #[test]
    fn urls_do_not_glue_across_a_guessed_wrap() {
        let content = "go https://x.com/a\r\n  and some more words";
        let cands = link_candidates(content, 0, 14).unwrap();
        assert_eq!(cands[0], "https://x.com/a");
        assert!(!cands.contains(&"https://x.com/aand".to_string()));
    }
}
