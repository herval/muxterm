use egui::{Event, ImeEvent, Key, Modifiers};

use muxterm::layout::PaneId;

/// The "? " prompt line: a '?' typed as the first character at an idle shell
/// prompt swallows subsequent keystrokes into an AI query instead of the PTY.
///
/// The machine is fed every input event before any TerminalView clones the
/// frame's list, and its verdict decides whether the event stays in the
/// frame. It is deliberately egui-Context-free so transitions are unit-
/// testable with bare Event values.
///
/// Ordering constraint that shapes Pending: egui emits Key{pressed} before
/// the companion Text in the same frame, so printable Key presses must never
/// decide anything - only the Text/Paste/Ime event that follows them does.
#[derive(Debug, Default, PartialEq)]
pub enum State {
    #[default]
    Inactive,
    /// A '?' typed on an empty line was swallowed; the next content event
    /// decides between Compose and replaying the '?' to the shell.
    Pending,
    Compose {
        buffer: String,
        error: Option<String>,
    },
}

#[derive(Default)]
pub struct PromptMachine {
    pub state: State,
    /// Pane the machine is bound to while Pending/Compose.
    pub pane: Option<PaneId>,
}

/// Best-effort model of the shell's input line, gating the '?' trigger.
/// `Known(n)` means the line holds n characters with the cursor at the
/// end - the invariant that lets Backspace walk the count back to empty.
/// Anything the count can't model (history recall, paste, completion,
/// movement off the end) soils the tracker until the line is killed.
/// Wrong guesses must err toward Dirty: a missed trigger is harmless
/// (the '?' just reaches the shell), a false one would intercept real
/// typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTracker {
    Known(usize),
    Dirty,
}

impl LineTracker {
    pub fn is_empty(self) -> bool {
        self == LineTracker::Known(0)
    }

    fn add(&mut self, n: usize) {
        if let LineTracker::Known(count) = self {
            *count += n;
        }
    }

    /// One Backspace: erases one char with the cursor at the end; extra
    /// presses on an already-empty line are shell no-ops.
    fn erase(&mut self) {
        if let LineTracker::Known(count) = self {
            *count = count.saturating_sub(1);
        }
    }

    fn clear(&mut self) {
        *self = LineTracker::Known(0);
    }

    fn soil(&mut self) {
        *self = LineTracker::Dirty;
    }

    /// Cursor movement toward the start of a non-empty line breaks the
    /// cursor-at-end invariant (a later Backspace at column 0 erases
    /// nothing, so the count would undercount); on an empty line it is
    /// a no-op.
    fn drift(&mut self) {
        if !self.is_empty() {
            self.soil();
        }
    }
}

/// What the app must do with the event that was just fed in.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Leave the event in the frame for TerminalView.
    Pass,
    /// Remove the event from the frame.
    Consume,
    /// Write these bytes to the PTY first, then leave the event in the frame.
    PassAndWrite(Vec<u8>),
    /// Enter pressed in Compose: run this query.
    Submit(String),
}

impl PromptMachine {
    /// Cancel Pending/Compose when focus moved away from the bound pane or
    /// that pane is gone. Called once per frame before events are fed.
    pub fn sync(&mut self, focused: PaneId, pane_exists: bool) {
        if self.state != State::Inactive
            && (!pane_exists || self.pane != Some(focused))
        {
            self.cancel();
        }
    }

    pub fn composing(&self) -> bool {
        matches!(self.state, State::Compose { .. })
    }

    pub fn cancel(&mut self) {
        self.state = State::Inactive;
        self.pane = None;
    }

    /// A submit was rejected (agent binary missing): stay in Compose so the
    /// query isn't lost, and surface the reason in the overlay.
    pub fn set_error(&mut self, msg: String) {
        if let State::Compose { error, .. } = &mut self.state {
            *error = Some(msg);
        }
    }

    /// Drive one event through the machine. `line` is the focused pane's
    /// tracked heuristic (updated in place); `at_shell` is only called
    /// when a '?' lands on an empty line, so its subprocess cost is paid
    /// once per trigger, not per keystroke.
    pub fn on_event(
        &mut self,
        event: &Event,
        focused: PaneId,
        line: &mut LineTracker,
        at_shell: &mut dyn FnMut() -> bool,
    ) -> Verdict {
        match &mut self.state {
            State::Inactive => {
                if let Event::Text(t) = event {
                    if t == "?" && line.is_empty() && at_shell() {
                        self.state = State::Pending;
                        self.pane = Some(focused);
                        return Verdict::Consume;
                    }
                }
                apply_line_effect(event, line);
                Verdict::Pass
            },
            State::Pending => match event {
                Event::Text(t) if t == " " => {
                    self.state = State::Compose {
                        buffer: String::new(),
                        error: None,
                    };
                    Verdict::Consume
                },
                Event::Text(_)
                | Event::Paste(_)
                | Event::Ime(ImeEvent::Commit(_)) => {
                    self.cancel();
                    line.add(1); // the replayed '?'
                    apply_line_effect(event, line);
                    Verdict::PassAndWrite(b"?".to_vec())
                },
                Event::Key {
                    key: Key::Escape | Key::Backspace,
                    pressed: true,
                    ..
                } => {
                    // The shell never saw the '?', so cancel silently. Esc
                    // must not leak: it could flip zsh's vi-mode.
                    self.cancel();
                    Verdict::Consume
                },
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if is_control_key(*key) || chorded(modifiers) => {
                    // Keys that hit the shell without a companion Text
                    // event: replay the '?' then let the key through.
                    self.cancel();
                    line.add(1); // the replayed '?'
                    apply_line_effect(event, line);
                    Verdict::PassAndWrite(b"?".to_vec())
                },
                // Printable presses (their Text follows this frame) and all
                // releases - e.g. the shift release from typing '?' - wait.
                _ => Verdict::Pass,
            },
            State::Compose { buffer, error } => match event {
                Event::Text(t) | Event::Ime(ImeEvent::Commit(t)) => {
                    buffer.extend(t.chars().filter(|c| !c.is_control()));
                    *error = None;
                    Verdict::Consume
                },
                Event::Paste(t) => {
                    // A raw newline must never ride into the composed
                    // command line.
                    buffer.extend(
                        t.chars().map(|c| if c.is_control() { ' ' } else { c }),
                    );
                    *error = None;
                    Verdict::Consume
                },
                Event::Ime(_) => Verdict::Consume,
                Event::Key {
                    key: Key::Backspace,
                    pressed: true,
                    ..
                } => {
                    // Erasing past "? " leaves compose entirely.
                    if buffer.pop().is_none() {
                        self.cancel();
                    }
                    Verdict::Consume
                },
                Event::Key {
                    key: Key::Escape,
                    pressed: true,
                    ..
                } => {
                    self.cancel();
                    Verdict::Consume
                },
                Event::Key {
                    key: Key::Enter,
                    pressed: true,
                    ..
                } => {
                    let query = buffer.trim().to_string();
                    if query.is_empty() {
                        self.cancel();
                        Verdict::Consume
                    } else {
                        // Stay in Compose: the app cancels on success or
                        // calls set_error, keeping the query editable.
                        Verdict::Submit(query)
                    }
                },
                Event::Key { .. } | Event::Copy | Event::Cut => {
                    Verdict::Consume
                },
                // Pointer and window events don't belong to the compose
                // line; a click that moves focus cancels via sync().
                _ => Verdict::Pass,
            },
        }
    }
}

fn chorded(m: &Modifiers) -> bool {
    // Shift is deliberately not a chord: typing capitals must stay a plain
    // content decision for the Text event that follows.
    m.ctrl || m.alt || m.command || m.mac_cmd
}

/// Keys that act on the shell without a companion Text event.
fn is_control_key(key: Key) -> bool {
    use Key::*;
    matches!(
        key,
        Enter
            | Tab
            | Delete
            | Insert
            | Home
            | End
            | PageUp
            | PageDown
            | ArrowUp
            | ArrowDown
            | ArrowLeft
            | ArrowRight
            | F1
            | F2
            | F3
            | F4
            | F5
            | F6
            | F7
            | F8
            | F9
            | F10
            | F11
            | F12
            | F13
            | F14
            | F15
            | F16
            | F17
            | F18
            | F19
            | F20
    )
}

/// Fold one event into the tracked line model (see [`LineTracker`]).
fn apply_line_effect(event: &Event, line: &mut LineTracker) {
    match event {
        Event::Text(t) | Event::Ime(ImeEvent::Commit(t)) => {
            line.add(t.chars().count());
        },
        // Paste may carry newlines or land mid-line; don't try to count.
        Event::Paste(_) => line.soil(),
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            if chorded(modifiers) {
                match key {
                    // These kill the input line outright.
                    Key::C | Key::U if modifiers.ctrl => line.clear(),
                    // Screen repaint and end-of-line movement keep both
                    // the line and the cursor-at-end invariant.
                    Key::L | Key::E | Key::F if modifiers.ctrl => {},
                    // Movement toward the start of the line.
                    Key::A | Key::B if modifiers.ctrl => line.drift(),
                    // Everything else may edit or recall history.
                    _ => line.soil(),
                }
            } else {
                match key {
                    Key::Enter => line.clear(),
                    // The one edit the count can model.
                    Key::Backspace => line.erase(),
                    // History recall and completion rewrite the line.
                    Key::ArrowUp | Key::ArrowDown | Key::Tab => line.soil(),
                    // Movement toward the start of the line.
                    Key::ArrowLeft | Key::Home => line.drift(),
                    // ArrowRight/End/Delete are no-ops with the cursor at
                    // the end; scrollback keys don't touch the line.
                    _ => {},
                }
            }
        },
        _ => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANE: PaneId = PaneId(1);
    const OTHER: PaneId = PaneId(2);

    fn text(s: &str) -> Event {
        Event::Text(s.into())
    }

    fn key(k: Key) -> Event {
        Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn release(k: Key) -> Event {
        Event::Key {
            key: k,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn ctrl(k: Key) -> Event {
        Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        }
    }

    fn empty_line() -> LineTracker {
        LineTracker::Known(0)
    }

    fn feed(
        m: &mut PromptMachine,
        e: &Event,
        line: &mut LineTracker,
        shell: bool,
    ) -> Verdict {
        m.on_event(e, PANE, line, &mut || shell)
    }

    /// A machine driven through "? " into Compose.
    fn compose() -> PromptMachine {
        let mut m = PromptMachine::default();
        let mut line = empty_line();
        assert_eq!(feed(&mut m, &text("?"), &mut line, true), Verdict::Consume);
        assert_eq!(feed(&mut m, &text(" "), &mut line, true), Verdict::Consume);
        assert!(m.composing());
        m
    }

    #[test]
    fn tracks_line_emptiness() {
        let mut m = PromptMachine::default();
        let mut line = empty_line();
        assert_eq!(feed(&mut m, &text("l"), &mut line, true), Verdict::Pass);
        assert!(!line.is_empty());
        feed(&mut m, &key(Key::Enter), &mut line, true);
        assert!(line.is_empty());
        feed(&mut m, &key(Key::ArrowUp), &mut line, true);
        assert!(!line.is_empty());
        feed(&mut m, &ctrl(Key::C), &mut line, true);
        assert!(line.is_empty());
        feed(&mut m, &ctrl(Key::A), &mut line, true);
        assert!(line.is_empty()); // cursor movement on an empty line
    }

    /// The reported regression: a typo erased with Backspace must re-arm
    /// the trigger - the line really is empty again.
    #[test]
    fn backspace_walks_line_back_to_empty_and_rearms_trigger() {
        let mut m = PromptMachine::default();
        let mut line = empty_line();
        feed(&mut m, &text("x"), &mut line, true);
        assert!(!line.is_empty());
        feed(&mut m, &key(Key::Backspace), &mut line, true);
        assert!(line.is_empty());
        assert_eq!(feed(&mut m, &text("?"), &mut line, true), Verdict::Consume);
        assert_eq!(m.state, State::Pending);
    }

    #[test]
    fn extra_backspaces_do_not_undercount() {
        let mut m = PromptMachine::default();
        let mut line = empty_line();
        // Held Backspace repeats past the start of the line.
        feed(&mut m, &text("x"), &mut line, true);
        feed(&mut m, &key(Key::Backspace), &mut line, true);
        feed(&mut m, &key(Key::Backspace), &mut line, true);
        feed(&mut m, &text("y"), &mut line, true);
        assert!(!line.is_empty());
    }

    /// Once the cursor may have left the end of a non-empty line, Backspace
    /// can no longer prove emptiness (at column 0 it erases nothing).
    #[test]
    fn movement_off_line_end_makes_backspace_inconclusive() {
        let mut m = PromptMachine::default();
        let mut line = empty_line();
        feed(&mut m, &text("x"), &mut line, true);
        feed(&mut m, &key(Key::ArrowLeft), &mut line, true);
        feed(&mut m, &key(Key::Backspace), &mut line, true);
        assert!(!line.is_empty());
        // Killing the line resets certainty.
        feed(&mut m, &ctrl(Key::U), &mut line, true);
        assert!(line.is_empty());
    }

    #[test]
    fn question_mark_triggers_only_on_empty_shell_line() {
        let mut m = PromptMachine::default();
        let mut line = LineTracker::Dirty;
        assert_eq!(feed(&mut m, &text("?"), &mut line, true), Verdict::Pass);
        assert_eq!(m.state, State::Inactive);

        let mut line = empty_line();
        assert_eq!(feed(&mut m, &text("?"), &mut line, false), Verdict::Pass);
        assert_eq!(m.state, State::Inactive);
        assert!(!line.is_empty()); // the passed-through '?' dirtied the line

        let mut line = empty_line();
        assert_eq!(feed(&mut m, &text("?"), &mut line, true), Verdict::Consume);
        assert_eq!(m.state, State::Pending);
    }

    /// egui delivers Key{Space, pressed} before Text(" ") in the same
    /// frame; the press must not decide Pending.
    #[test]
    fn pending_ignores_space_key_press_and_waits_for_its_text() {
        let mut m = PromptMachine::default();
        let mut line = empty_line();
        feed(&mut m, &text("?"), &mut line, true);
        assert_eq!(
            feed(&mut m, &key(Key::Space), &mut line, true),
            Verdict::Pass
        );
        assert_eq!(m.state, State::Pending);
        assert_eq!(feed(&mut m, &text(" "), &mut line, true), Verdict::Consume);
        assert!(m.composing());
    }

    #[test]
    fn pending_replays_question_mark_when_not_followed_by_space() {
        let mut m = PromptMachine::default();
        let mut line = empty_line();
        feed(&mut m, &text("?"), &mut line, true);
        // The release of shift/slash from typing '?' itself keeps waiting.
        assert_eq!(
            feed(&mut m, &release(Key::Slash), &mut line, true),
            Verdict::Pass
        );
        assert_eq!(
            feed(&mut m, &text("x"), &mut line, true),
            Verdict::PassAndWrite(b"?".to_vec())
        );
        assert_eq!(m.state, State::Inactive);
        // The line now holds "?x"; two Backspaces re-arm the trigger.
        assert_eq!(line, LineTracker::Known(2));
    }

    #[test]
    fn pending_enter_replays_and_leaves_line_empty() {
        let mut m = PromptMachine::default();
        let mut line = empty_line();
        feed(&mut m, &text("?"), &mut line, true);
        assert_eq!(
            feed(&mut m, &key(Key::Enter), &mut line, true),
            Verdict::PassAndWrite(b"?".to_vec())
        );
        assert_eq!(m.state, State::Inactive);
        assert!(line.is_empty());
    }

    #[test]
    fn pending_escape_and_backspace_cancel_silently() {
        for k in [Key::Escape, Key::Backspace] {
            let mut m = PromptMachine::default();
            let mut line = empty_line();
            feed(&mut m, &text("?"), &mut line, true);
            assert_eq!(feed(&mut m, &key(k), &mut line, true), Verdict::Consume);
            assert_eq!(m.state, State::Inactive);
            assert!(line.is_empty()); // the shell never saw anything
        }
    }

    #[test]
    fn compose_edits_and_submits() {
        let mut m = compose();
        let mut line = empty_line();
        feed(&mut m, &text("fix"), &mut line, true);
        feed(&mut m, &text(" it!"), &mut line, true);
        feed(&mut m, &key(Key::Backspace), &mut line, true);
        assert_eq!(
            feed(&mut m, &key(Key::Enter), &mut line, true),
            Verdict::Submit("fix it".into())
        );
        assert!(m.composing()); // app decides: cancel or set_error
    }

    #[test]
    fn compose_paste_flattens_control_characters() {
        let mut m = compose();
        let mut line = empty_line();
        feed(&mut m, &Event::Paste("a\nb\tc".into()), &mut line, true);
        assert_eq!(
            feed(&mut m, &key(Key::Enter), &mut line, true),
            Verdict::Submit("a b c".into())
        );
    }

    #[test]
    fn compose_backspace_past_start_exits() {
        let mut m = compose();
        let mut line = empty_line();
        feed(&mut m, &text("a"), &mut line, true);
        feed(&mut m, &key(Key::Backspace), &mut line, true);
        assert!(m.composing());
        assert_eq!(
            feed(&mut m, &key(Key::Backspace), &mut line, true),
            Verdict::Consume
        );
        assert_eq!(m.state, State::Inactive);
        // Compose consumed everything, so the shell line is still empty
        // and "? " can trigger again immediately.
        assert!(line.is_empty());
    }

    #[test]
    fn compose_escape_cancels_and_empty_submit_cancels() {
        let mut m = compose();
        let mut line = empty_line();
        feed(&mut m, &key(Key::Escape), &mut line, true);
        assert_eq!(m.state, State::Inactive);

        let mut m = compose();
        feed(&mut m, &text("   "), &mut line, true);
        assert_eq!(
            feed(&mut m, &key(Key::Enter), &mut line, true),
            Verdict::Consume
        );
        assert_eq!(m.state, State::Inactive);
    }

    #[test]
    fn error_shows_and_clears_on_next_keystroke() {
        let mut m = compose();
        let mut line = empty_line();
        feed(&mut m, &text("hi"), &mut line, true);
        m.set_error("claude not found in PATH".into());
        assert!(matches!(&m.state, State::Compose { error: Some(_), .. }));
        feed(&mut m, &text("!"), &mut line, true);
        assert!(matches!(&m.state, State::Compose { error: None, .. }));
    }

    #[test]
    fn sync_cancels_when_focus_moves_or_pane_dies() {
        let mut m = compose();
        m.sync(OTHER, true);
        assert_eq!(m.state, State::Inactive);

        let mut m = compose();
        m.sync(PANE, false);
        assert_eq!(m.state, State::Inactive);

        let mut m = compose();
        m.sync(PANE, true);
        assert!(m.composing());
    }
}
