use egui::{Key, KeyboardShortcut, Modifiers};

use muxterm::layout::{Dir, SplitAxis};

#[derive(Debug, Clone, Copy)]
pub enum Action {
    NewTab,
    /// cmd+n: open the workspace-creation popup (folder + worktree + prompt +
    /// agent). cmd+t (NewTab) stays the shortcut for a bare shell workspace.
    NewWorkspace,
    /// cmd+\: show/collapse the workspace sidebar.
    ToggleSidebar,
    /// cmd+k: clear the focused pane's screen and scrollback (iTerm-style).
    ClearScreen,
    ClosePane,
    Split(SplitAxis),
    PrevTab,
    NextTab,
    /// Activate a tab by its raw index in `App.tabs` (sidebar clicks, which
    /// carry the real index so display order can differ from tab order).
    GotoTab(usize),
    /// Activate the Nth *visible* (non-archived) tab: cmd+1..9 and tab-bar
    /// clicks, which count only the tabs shown in the active flow.
    GotoVisibleTab(usize),
    /// Park a workspace in the sidebar's archived pile (raw `App.tabs` index).
    Archive(usize),
    /// Pull a workspace back out of the archived pile (raw `App.tabs` index).
    Unarchive(usize),
    Focus(Dir),
    /// Cycle focus through the tab's panes in tree order (+1 / -1).
    /// Window managers like Rectangle often own cmd+opt+arrows globally,
    /// so directional nav alone can't be relied on.
    CyclePane(isize),
    ToggleSettings,
    /// cmd+f: open/close the scrollback-search bar on the focused pane.
    ToggleSearch,
    /// cmd+g / cmd+shift+g: walk matches while the bar is open. Consumed
    /// unconditionally like every chord here; the "is search active"
    /// gate lives in App::apply_action, which owns that state.
    SearchNext,
    SearchPrev,
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
        consume(cmd, Key::N, Action::NewWorkspace);
        consume(cmd, Key::Backslash, Action::ToggleSidebar);
        consume(cmd, Key::K, Action::ClearScreen);
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
        consume(cmd, Key::F, Action::ToggleSearch);
        consume(cmd | Modifiers::SHIFT, Key::G, Action::SearchPrev);
        consume(cmd, Key::G, Action::SearchNext);
        for (n, key) in TAB_KEYS.iter().enumerate() {
            consume(cmd, *key, Action::GotoVisibleTab(n));
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
