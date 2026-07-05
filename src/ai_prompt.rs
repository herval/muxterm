use egui::{Event, ImeEvent, Key, Modifiers};

use muxterm::layout::PaneId;

/// The "?" prompt line: a '?' typed as the first character at an idle shell
/// prompt swallows subsequent keystrokes into an AI query instead of the PTY.
///
/// The machine is fed every input event before any TerminalView clones the
/// frame's list, and its verdict decides whether the event stays in the
/// frame. It is deliberately egui-Context-free so transitions are unit-
/// testable with bare Event values.
///
/// Ordering constraint: egui emits Key{pressed} before the companion Text
/// in the same frame, so the trigger decision belongs to the Text event -
/// the Slash press that produced the '?' must pass through undecided.
#[derive(Debug, Default, PartialEq)]
pub enum State {
    #[default]
    Inactive,
    Compose {
        buffer: String,
        error: Option<String>,
    },
}

#[derive(Default)]
pub struct PromptMachine {
    pub state: State,
    /// Pane the machine is bound to while composing.
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
    /// Enter pressed in Compose: run this query.
    Submit(String),
}

impl PromptMachine {
    /// Cancel Compose when focus moved away from the bound pane or that
    /// pane is gone. Called once per frame before events are fed.
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
                        self.state = State::Compose {
                            buffer: String::new(),
                            error: None,
                        };
                        self.pane = Some(focused);
                        return Verdict::Consume;
                    }
                }
                apply_line_effect(event, line);
                Verdict::Pass
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
                    // Erasing past the '?' leaves compose entirely; the
                    // shell never saw the '?', so nothing to clean up.
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

    /// A machine driven through '?' into Compose.
    fn compose() -> PromptMachine {
        let mut m = PromptMachine::default();
        let mut line = empty_line();
        assert_eq!(feed(&mut m, &text("?"), &mut line, true), Verdict::Consume);
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
        assert!(m.composing());
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
        assert!(m.composing());
    }

    /// The shift/slash release from typing '?' itself lands in Compose and
    /// must not disturb the empty buffer.
    #[test]
    fn trigger_keys_own_release_is_harmless() {
        let mut m = compose();
        let mut line = empty_line();
        feed(&mut m, &release(Key::Slash), &mut line, true);
        feed(&mut m, &text("hi"), &mut line, true);
        assert_eq!(
            feed(&mut m, &key(Key::Enter), &mut line, true),
            Verdict::Submit("hi".into())
        );
    }

    /// Muscle memory from the old "? " trigger: a space typed after the
    /// '?' is just leading whitespace, trimmed away on submit.
    #[test]
    fn space_after_question_mark_still_composes() {
        let mut m = compose();
        let mut line = empty_line();
        feed(&mut m, &text(" "), &mut line, true);
        feed(&mut m, &text("hi"), &mut line, true);
        assert_eq!(
            feed(&mut m, &key(Key::Enter), &mut line, true),
            Verdict::Submit("hi".into())
        );
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
        // and '?' can trigger again immediately.
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
