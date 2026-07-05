use egui_term::TerminalBackend;

use crate::layout::PaneId;

/// One terminal pane. Dropping it shuts the PTY down, which only detaches
/// the tmux client - killing the session is an explicit, separate step.
pub struct Pane {
    pub id: PaneId,
    pub session: String,
    pub backend: TerminalBackend,
    pub title: String,
    /// Heuristic: is the shell's input line empty? Gates the "? " prompt.
    pub line_empty: bool,
}
