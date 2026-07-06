use egui::{Event, ImeEvent, Key};

use muxterm::layout::PaneId;

/// The cmd+f scrollback-search bar. The local grid holds no real history
/// (tmux owns scrollback), so every edit drives tmux copy-mode search
/// server-side; match highlighting and viewport jumps come back through
/// the PTY like any other tmux redraw.
///
/// The machine is fed every input event before any TerminalView clones
/// the frame's list, like the "?" prompt, and is deliberately egui-
/// Context-free so transitions are unit-testable with bare Event values.
#[derive(Debug, Default, PartialEq)]
pub enum State {
    #[default]
    Closed,
    Open {
        query: String,
        /// Read back from tmux after each op; None hides the counter
        /// (empty query, or a server without search_count).
        count: Option<MatchCount>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchCount {
    pub total: u32,
    /// tmux capped the count; render as "N+".
    pub partial: bool,
}

/// What the app must do with the event that was just fed in.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Leave the event in the frame for TerminalView.
    Pass,
    /// Remove the event from the frame.
    Consume,
    /// Run this tmux copy-mode operation on the bound pane.
    Op(SearchOp),
}

/// Every op re-enters copy-mode first (a no-op when already in it), so a
/// drag or click that kicked the pane out of copy-mode self-heals on the
/// next keystroke.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchOp {
    /// Query edited: reset to the bottom of history and re-search, so
    /// the bar always lands on the most recent match (iTerm semantics).
    Search(String),
    /// Enter / cmd+g: continue toward older matches.
    Next,
    /// shift+Enter / cmd+shift+g: back toward newer matches.
    Prev,
    /// Query emptied: drop the highlights and unfreeze the pane.
    Clear,
}

#[derive(Default)]
pub struct SearchBar {
    pub state: State,
    /// Pane the bar is bound to while open.
    pub pane: Option<PaneId>,
}

impl SearchBar {
    /// Close when focus moved off the bound pane or it died. Called once
    /// per frame before events are fed. Teardown is pure app state - a
    /// pane left sitting in copy-mode is indistinguishable from one the
    /// user wheel-scrolled up.
    pub fn sync(&mut self, focused: PaneId, pane_exists: bool) {
        if self.active() && (!pane_exists || self.pane != Some(focused)) {
            self.close();
        }
    }

    pub fn active(&self) -> bool {
        matches!(self.state, State::Open { .. })
    }

    pub fn open(&mut self, pane: PaneId) {
        self.state = State::Open {
            query: String::new(),
            count: None,
        };
        self.pane = Some(pane);
    }

    pub fn close(&mut self) {
        self.state = State::Closed;
        self.pane = None;
    }

    pub fn set_count(&mut self, new: Option<MatchCount>) {
        if let State::Open { count, .. } = &mut self.state {
            *count = new;
        }
    }

    /// Drive one event through the machine.
    pub fn on_event(&mut self, event: &Event) -> Verdict {
        let State::Open { query, .. } = &mut self.state else {
            return Verdict::Pass;
        };
        match event {
            Event::Text(t) | Event::Ime(ImeEvent::Commit(t)) => {
                query.extend(t.chars().filter(|c| !c.is_control()));
                Verdict::Op(SearchOp::Search(query.clone()))
            },
            Event::Paste(t) => {
                query.extend(
                    t.chars().map(|c| if c.is_control() { ' ' } else { c }),
                );
                Verdict::Op(SearchOp::Search(query.clone()))
            },
            Event::Ime(_) => Verdict::Consume,
            Event::Key {
                key: Key::Backspace,
                pressed: true,
                ..
            } => {
                // Erasing past empty keeps the bar open (unlike the "?"
                // prompt) - it has an explicit close in Escape.
                match query.pop() {
                    Some(_) if query.is_empty() => {
                        Verdict::Op(SearchOp::Clear)
                    },
                    Some(_) => Verdict::Op(SearchOp::Search(query.clone())),
                    None => Verdict::Consume,
                }
            },
            Event::Key {
                key: Key::Enter,
                pressed: true,
                modifiers,
                ..
            } => {
                if query.is_empty() {
                    Verdict::Consume
                } else if modifiers.shift {
                    Verdict::Op(SearchOp::Prev)
                } else {
                    Verdict::Op(SearchOp::Next)
                }
            },
            Event::Key {
                key: Key::Escape,
                pressed: true,
                ..
            } => {
                // No tmux op: the pane stays in copy-mode at the match,
                // so the find spot survives the bar (iTerm behavior) and
                // the *next* Escape - now reaching the PTY - is tmux's
                // own cancel, snapping back to live output.
                self.close();
                Verdict::Consume
            },
            // The bar owns the keyboard; nothing else may leak to the
            // PTY. Copy/Cut stay live so cmd+c can still copy a copy-mode
            // selection made around a match (App::copy_intercept).
            Event::Key { .. } => Verdict::Consume,
            _ => Verdict::Pass,
        }
    }
}

/// Key repeat can land several edits in one frame; queries are cumulative
/// snapshots, so only the last Search matters. Next/Prev/Clear all keep
/// their meaning and stay in order.
pub fn coalesce(ops: Vec<SearchOp>) -> Vec<SearchOp> {
    let last_search = ops
        .iter()
        .rposition(|op| matches!(op, SearchOp::Search(_)));
    ops.into_iter()
        .enumerate()
        .filter(|(i, op)| match op {
            SearchOp::Search(_) => Some(*i) == last_search,
            _ => true,
        })
        .map(|(_, op)| op)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Modifiers;

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

    fn shift_key(k: Key) -> Event {
        Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::SHIFT,
        }
    }

    fn open() -> SearchBar {
        let mut bar = SearchBar::default();
        bar.open(PANE);
        bar
    }

    fn search(q: &str) -> Verdict {
        Verdict::Op(SearchOp::Search(q.into()))
    }

    #[test]
    fn closed_state_passes_everything() {
        let mut bar = SearchBar::default();
        for event in [text("x"), key(Key::Enter), key(Key::Escape)] {
            assert_eq!(bar.on_event(&event), Verdict::Pass);
        }
        assert!(!bar.active());
    }

    #[test]
    fn typing_searches_incrementally() {
        let mut bar = open();
        assert_eq!(bar.on_event(&text("a")), search("a"));
        assert_eq!(bar.on_event(&text("b")), search("ab"));
    }

    #[test]
    fn backspace_edits_then_clears_then_noops() {
        let mut bar = open();
        bar.on_event(&text("ab"));
        assert_eq!(bar.on_event(&key(Key::Backspace)), search("a"));
        assert_eq!(
            bar.on_event(&key(Key::Backspace)),
            Verdict::Op(SearchOp::Clear)
        );
        assert_eq!(bar.on_event(&key(Key::Backspace)), Verdict::Consume);
        assert!(bar.active()); // the bar has an explicit close: Escape
    }

    #[test]
    fn enter_walks_matches_and_noops_when_empty() {
        let mut bar = open();
        assert_eq!(bar.on_event(&key(Key::Enter)), Verdict::Consume);
        bar.on_event(&text("x"));
        assert_eq!(
            bar.on_event(&key(Key::Enter)),
            Verdict::Op(SearchOp::Next)
        );
        assert_eq!(
            bar.on_event(&shift_key(Key::Enter)),
            Verdict::Op(SearchOp::Prev)
        );
    }

    #[test]
    fn escape_closes_without_an_op() {
        let mut bar = open();
        bar.on_event(&text("x"));
        assert_eq!(bar.on_event(&key(Key::Escape)), Verdict::Consume);
        assert!(!bar.active());
        assert_eq!(bar.pane, None);
    }

    #[test]
    fn paste_flattens_control_characters() {
        let mut bar = open();
        assert_eq!(
            bar.on_event(&Event::Paste("a\nb".into())),
            search("a b")
        );
    }

    #[test]
    fn copy_and_pointer_pass_other_keys_are_consumed() {
        let mut bar = open();
        bar.on_event(&text("x"));
        assert_eq!(bar.on_event(&Event::Copy), Verdict::Pass);
        assert_eq!(
            bar.on_event(&Event::PointerGone),
            Verdict::Pass
        );
        assert_eq!(bar.on_event(&key(Key::A)), Verdict::Consume);
        assert_eq!(bar.on_event(&key(Key::Tab)), Verdict::Consume);
    }

    #[test]
    fn sync_closes_on_focus_move_or_pane_death() {
        let mut bar = open();
        bar.sync(OTHER, true);
        assert!(!bar.active());

        let mut bar = open();
        bar.sync(PANE, false);
        assert!(!bar.active());

        let mut bar = open();
        bar.sync(PANE, true);
        assert!(bar.active());
    }

    #[test]
    fn set_count_only_applies_while_open() {
        let mut bar = open();
        let count = MatchCount { total: 3, partial: false };
        bar.set_count(Some(count));
        assert_eq!(
            bar.state,
            State::Open { query: String::new(), count: Some(count) }
        );
        bar.close();
        bar.set_count(Some(count));
        assert_eq!(bar.state, State::Closed);
    }

    #[test]
    fn coalesce_keeps_last_search_and_all_steps() {
        let ops = vec![
            SearchOp::Search("a".into()),
            SearchOp::Search("ab".into()),
            SearchOp::Next,
            SearchOp::Search("abc".into()),
            SearchOp::Prev,
        ];
        assert_eq!(
            coalesce(ops),
            vec![
                SearchOp::Next,
                SearchOp::Search("abc".into()),
                SearchOp::Prev,
            ]
        );
        assert_eq!(coalesce(vec![SearchOp::Clear]), vec![SearchOp::Clear]);
    }
}
