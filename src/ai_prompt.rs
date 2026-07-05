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

    /// Drive one event through the machine. `line_empty` is the focused
    /// pane's tracked heuristic (updated in place); `at_shell` is only
    /// called when a '?' lands on an empty line, so its subprocess cost is
    /// paid once per trigger, not per keystroke.
    pub fn on_event(
        &mut self,
        event: &Event,
        focused: PaneId,
        line_empty: &mut bool,
        at_shell: &mut dyn FnMut() -> bool,
    ) -> Verdict {
        match &mut self.state {
            State::Inactive => {
                if let Event::Text(t) = event {
                    if t == "?" && *line_empty && at_shell() {
                        self.state = State::Pending;
                        self.pane = Some(focused);
                        return Verdict::Consume;
                    }
                }
                apply_line_effect(event, line_empty);
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
                    *line_empty = false;
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
                    apply_line_effect(event, line_empty);
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

/// Best-effort tracking of "is the shell's input line empty". Wrong guesses
/// must err toward dirty: a missed trigger is harmless (the '?' just reaches
/// the shell), a false one would intercept real typing.
fn apply_line_effect(event: &Event, line_empty: &mut bool) {
    match event {
        Event::Text(_) | Event::Paste(_) | Event::Ime(ImeEvent::Commit(_)) => {
            *line_empty = false;
        },
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            if chorded(modifiers) {
                match key {
                    // These kill the input line outright.
                    Key::C | Key::U if modifiers.ctrl => *line_empty = true,
                    // Pure cursor/screen movement leaves the line alone.
                    Key::A | Key::E | Key::B | Key::F | Key::L
                        if modifiers.ctrl => {},
                    // Everything else may edit or recall history.
                    _ => *line_empty = false,
                }
            } else {
                match key {
                    Key::Enter => *line_empty = true,
                    // History recall, completion, and edits.
                    Key::ArrowUp
                    | Key::ArrowDown
                    | Key::Tab
                    | Key::Backspace
                    | Key::Delete => *line_empty = false,
                    // Movement and scrollback keys leave the line alone.
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

    fn feed(
        m: &mut PromptMachine,
        e: &Event,
        empty: &mut bool,
        shell: bool,
    ) -> Verdict {
        m.on_event(e, PANE, empty, &mut || shell)
    }

    /// A machine driven through "? " into Compose.
    fn compose() -> PromptMachine {
        let mut m = PromptMachine::default();
        let mut empty = true;
        assert_eq!(feed(&mut m, &text("?"), &mut empty, true), Verdict::Consume);
        assert_eq!(feed(&mut m, &text(" "), &mut empty, true), Verdict::Consume);
        assert!(m.composing());
        m
    }

    #[test]
    fn tracks_line_emptiness() {
        let mut m = PromptMachine::default();
        let mut empty = true;
        assert_eq!(feed(&mut m, &text("l"), &mut empty, true), Verdict::Pass);
        assert!(!empty);
        feed(&mut m, &key(Key::Enter), &mut empty, true);
        assert!(empty);
        feed(&mut m, &key(Key::ArrowUp), &mut empty, true);
        assert!(!empty);
        feed(&mut m, &ctrl(Key::C), &mut empty, true);
        assert!(empty);
        feed(&mut m, &ctrl(Key::A), &mut empty, true);
        assert!(empty); // cursor movement is not an edit
    }

    #[test]
    fn question_mark_triggers_only_on_empty_shell_line() {
        let mut m = PromptMachine::default();
        let mut empty = false;
        assert_eq!(feed(&mut m, &text("?"), &mut empty, true), Verdict::Pass);
        assert_eq!(m.state, State::Inactive);

        let mut empty = true;
        assert_eq!(feed(&mut m, &text("?"), &mut empty, false), Verdict::Pass);
        assert_eq!(m.state, State::Inactive);
        assert!(!empty); // the passed-through '?' dirtied the line

        let mut empty = true;
        assert_eq!(feed(&mut m, &text("?"), &mut empty, true), Verdict::Consume);
        assert_eq!(m.state, State::Pending);
    }

    /// egui delivers Key{Space, pressed} before Text(" ") in the same
    /// frame; the press must not decide Pending.
    #[test]
    fn pending_ignores_space_key_press_and_waits_for_its_text() {
        let mut m = PromptMachine::default();
        let mut empty = true;
        feed(&mut m, &text("?"), &mut empty, true);
        assert_eq!(
            feed(&mut m, &key(Key::Space), &mut empty, true),
            Verdict::Pass
        );
        assert_eq!(m.state, State::Pending);
        assert_eq!(feed(&mut m, &text(" "), &mut empty, true), Verdict::Consume);
        assert!(m.composing());
    }

    #[test]
    fn pending_replays_question_mark_when_not_followed_by_space() {
        let mut m = PromptMachine::default();
        let mut empty = true;
        feed(&mut m, &text("?"), &mut empty, true);
        // The release of shift/slash from typing '?' itself keeps waiting.
        assert_eq!(
            feed(&mut m, &release(Key::Slash), &mut empty, true),
            Verdict::Pass
        );
        assert_eq!(
            feed(&mut m, &text("x"), &mut empty, true),
            Verdict::PassAndWrite(b"?".to_vec())
        );
        assert_eq!(m.state, State::Inactive);
        assert!(!empty);
    }

    #[test]
    fn pending_enter_replays_and_leaves_line_empty() {
        let mut m = PromptMachine::default();
        let mut empty = true;
        feed(&mut m, &text("?"), &mut empty, true);
        assert_eq!(
            feed(&mut m, &key(Key::Enter), &mut empty, true),
            Verdict::PassAndWrite(b"?".to_vec())
        );
        assert_eq!(m.state, State::Inactive);
        assert!(empty);
    }

    #[test]
    fn pending_escape_and_backspace_cancel_silently() {
        for k in [Key::Escape, Key::Backspace] {
            let mut m = PromptMachine::default();
            let mut empty = true;
            feed(&mut m, &text("?"), &mut empty, true);
            assert_eq!(feed(&mut m, &key(k), &mut empty, true), Verdict::Consume);
            assert_eq!(m.state, State::Inactive);
            assert!(empty); // the shell never saw anything
        }
    }

    #[test]
    fn compose_edits_and_submits() {
        let mut m = compose();
        let mut empty = true;
        feed(&mut m, &text("fix"), &mut empty, true);
        feed(&mut m, &text(" it!"), &mut empty, true);
        feed(&mut m, &key(Key::Backspace), &mut empty, true);
        assert_eq!(
            feed(&mut m, &key(Key::Enter), &mut empty, true),
            Verdict::Submit("fix it".into())
        );
        assert!(m.composing()); // app decides: cancel or set_error
    }

    #[test]
    fn compose_paste_flattens_control_characters() {
        let mut m = compose();
        let mut empty = true;
        feed(&mut m, &Event::Paste("a\nb\tc".into()), &mut empty, true);
        assert_eq!(
            feed(&mut m, &key(Key::Enter), &mut empty, true),
            Verdict::Submit("a b c".into())
        );
    }

    #[test]
    fn compose_backspace_past_start_exits() {
        let mut m = compose();
        let mut empty = true;
        feed(&mut m, &text("a"), &mut empty, true);
        feed(&mut m, &key(Key::Backspace), &mut empty, true);
        assert!(m.composing());
        assert_eq!(
            feed(&mut m, &key(Key::Backspace), &mut empty, true),
            Verdict::Consume
        );
        assert_eq!(m.state, State::Inactive);
    }

    #[test]
    fn compose_escape_cancels_and_empty_submit_cancels() {
        let mut m = compose();
        let mut empty = true;
        feed(&mut m, &key(Key::Escape), &mut empty, true);
        assert_eq!(m.state, State::Inactive);

        let mut m = compose();
        feed(&mut m, &text("   "), &mut empty, true);
        assert_eq!(
            feed(&mut m, &key(Key::Enter), &mut empty, true),
            Verdict::Consume
        );
        assert_eq!(m.state, State::Inactive);
    }

    #[test]
    fn error_shows_and_clears_on_next_keystroke() {
        let mut m = compose();
        let mut empty = true;
        feed(&mut m, &text("hi"), &mut empty, true);
        m.set_error("claude not found in PATH".into());
        assert!(matches!(&m.state, State::Compose { error: Some(_), .. }));
        feed(&mut m, &text("!"), &mut empty, true);
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
