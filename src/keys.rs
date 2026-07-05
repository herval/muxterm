use egui::{Key, KeyboardShortcut, Modifiers};

use muxterm::layout::{Dir, SplitAxis};

#[derive(Debug, Clone, Copy)]
pub enum Action {
    NewTab,
    ClosePane,
    Split(SplitAxis),
    PrevTab,
    NextTab,
    GotoTab(usize),
    Focus(Dir),
    /// Cycle focus through the tab's panes in tree order (+1 / -1).
    /// Window managers like Rectangle often own cmd+opt+arrows globally,
    /// so directional nav alone can't be relied on.
    CyclePane(isize),
    ToggleSettings,
}

const TAB_KEYS: [Key; 9] = [
    Key::Num1,
    Key::Num2,
    Key::Num3,
    Key::Num4,
    Key::Num5,
    Key::Num6,
    Key::Num7,
    Key::Num8,
    Key::Num9,
];

/// Runs at the very top of App::update, before any widget. consume_shortcut
/// physically removes the matching key events from the frame's input, and
/// TerminalView reads events from that same list - so app chords can never
/// leak into a PTY. cmd+c/cmd+v are deliberately not consumed: egui turns
/// them into Event::Copy/Event::Paste, which the terminal widget handles.
pub fn drain_shortcuts(ctx: &egui::Context) -> Vec<Action> {
    let cmd = Modifiers::COMMAND;
    let mut out = Vec::new();
    ctx.input_mut(|i| {
        let mut consume = |m: Modifiers, k: Key, action: Action| {
            while i.consume_shortcut(&KeyboardShortcut::new(m, k)) {
                out.push(action);
            }
        };

        consume(cmd | Modifiers::SHIFT, Key::D, Action::Split(SplitAxis::Stacked));
        consume(cmd, Key::D, Action::Split(SplitAxis::SideBySide));
        consume(cmd, Key::T, Action::NewTab);
        consume(cmd, Key::W, Action::ClosePane);
        // shift+[ arrives as the logical key `{` on US-like layouts, but as
        // `[` wherever shift+[ produces something else — bind both.
        consume(cmd | Modifiers::SHIFT, Key::OpenCurlyBracket, Action::PrevTab);
        consume(cmd | Modifiers::SHIFT, Key::CloseCurlyBracket, Action::NextTab);
        consume(cmd | Modifiers::SHIFT, Key::OpenBracket, Action::PrevTab);
        consume(cmd | Modifiers::SHIFT, Key::CloseBracket, Action::NextTab);
        consume(cmd, Key::OpenBracket, Action::CyclePane(-1));
        consume(cmd, Key::CloseBracket, Action::CyclePane(1));
        consume(cmd, Key::Comma, Action::ToggleSettings);
        for (n, key) in TAB_KEYS.iter().enumerate() {
            consume(cmd, *key, Action::GotoTab(n));
        }
        for (key, dir) in [
            (Key::ArrowLeft, Dir::Left),
            (Key::ArrowRight, Dir::Right),
            (Key::ArrowUp, Dir::Up),
            (Key::ArrowDown, Dir::Down),
        ] {
            consume(cmd | Modifiers::ALT, key, Action::Focus(dir));
        }
    });
    out
}
