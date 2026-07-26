use egui::{Event, ImeEvent, Key, Modifiers};

/// The "?" trigger. A '?' typed as the first character at an idle shell
/// prompt never reaches the shell: the app swallows it and types
/// `mux ask -i` in its place, then steps out of the way. What comes up is a
/// real foreground process in the pane (src/ask.rs `repl`), so the question
/// line, its editing, its history and the way out (ctrl+c / ctrl+d) all
/// belong to the tty from that point on - the GUI's whole job is spotting
/// the '?' and handing over.
///
/// Deliberately egui-Context-free so the decision unit-tests with bare
/// Event values.
///
/// Ordering constraint: egui emits Key{pressed} before the companion Text
/// in the same frame, so the trigger decision belongs to the Text event -
/// the Slash press that produced the '?' must pass through undecided.

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
    /// The trigger fired: drop the '?' and hand the pane to `mux ask -i`.
    Enter,
}

/// Drive one event through the trigger. `line` is the focused pane's
/// tracked heuristic (updated in place); `at_shell` is only called when a
/// '?' lands on an empty line, so its subprocess cost is paid once per
/// trigger, not per keystroke.
pub fn on_event(
    event: &Event,
    line: &mut LineTracker,
    at_shell: &mut dyn FnMut() -> bool,
) -> Verdict {
    if let Event::Text(t) = event {
        if t == "?" && line.is_empty() && at_shell() {
            return Verdict::Enter;
        }
    }
    apply_line_effect(event, line);
    Verdict::Pass
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

    fn feed(e: &Event, line: &mut LineTracker, shell: bool) -> Verdict {
        on_event(e, line, &mut || shell)
    }

    #[test]
    fn tracks_line_emptiness() {
        let mut line = empty_line();
        assert_eq!(feed(&text("l"), &mut line, true), Verdict::Pass);
        assert!(!line.is_empty());
        feed(&key(Key::Enter), &mut line, true);
        assert!(line.is_empty());
        feed(&key(Key::ArrowUp), &mut line, true);
        assert!(!line.is_empty());
        feed(&ctrl(Key::C), &mut line, true);
        assert!(line.is_empty());
        feed(&ctrl(Key::A), &mut line, true);
        assert!(line.is_empty()); // cursor movement on an empty line
    }

    /// The reported regression: a typo erased with Backspace must re-arm
    /// the trigger - the line really is empty again.
    #[test]
    fn backspace_walks_line_back_to_empty_and_rearms_trigger() {
        let mut line = empty_line();
        feed(&text("x"), &mut line, true);
        assert!(!line.is_empty());
        feed(&key(Key::Backspace), &mut line, true);
        assert!(line.is_empty());
        assert_eq!(feed(&text("?"), &mut line, true), Verdict::Enter);
    }

    #[test]
    fn extra_backspaces_do_not_undercount() {
        let mut line = empty_line();
        // Held Backspace repeats past the start of the line.
        feed(&text("x"), &mut line, true);
        feed(&key(Key::Backspace), &mut line, true);
        feed(&key(Key::Backspace), &mut line, true);
        feed(&text("y"), &mut line, true);
        assert!(!line.is_empty());
    }

    /// Once the cursor may have left the end of a non-empty line, Backspace
    /// can no longer prove emptiness (at column 0 it erases nothing).
    #[test]
    fn movement_off_line_end_makes_backspace_inconclusive() {
        let mut line = empty_line();
        feed(&text("x"), &mut line, true);
        feed(&key(Key::ArrowLeft), &mut line, true);
        feed(&key(Key::Backspace), &mut line, true);
        assert!(!line.is_empty());
        // Killing the line resets certainty.
        feed(&ctrl(Key::U), &mut line, true);
        assert!(line.is_empty());
    }

    #[test]
    fn question_mark_triggers_only_on_empty_shell_line() {
        let mut line = LineTracker::Dirty;
        assert_eq!(feed(&text("?"), &mut line, true), Verdict::Pass);

        let mut line = empty_line();
        assert_eq!(feed(&text("?"), &mut line, false), Verdict::Pass);
        assert!(!line.is_empty()); // the passed-through '?' dirtied the line

        let mut line = empty_line();
        assert_eq!(feed(&text("?"), &mut line, true), Verdict::Enter);
    }

    /// A '?' anywhere but the first column is just a character - globs,
    /// urls and questions in a commit message must reach the shell.
    #[test]
    fn question_mark_mid_line_is_not_a_trigger() {
        let mut line = empty_line();
        feed(&text("ls *"), &mut line, true);
        assert_eq!(feed(&text("?"), &mut line, true), Verdict::Pass);
    }
}
