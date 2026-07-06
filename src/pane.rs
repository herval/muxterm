use egui_term::TerminalBackend;

use crate::ai_prompt::LineTracker;
use crate::attention;
use muxterm::layout::PaneId;

/// One terminal pane. Dropping it shuts the PTY down, which only detaches
/// the tmux client - killing the session is an explicit, separate step.
pub struct Pane {
    pub id: PaneId,
    pub session: String,
    pub backend: TerminalBackend,
    pub title: String,
    /// Heuristic model of the shell's input line; gates the "?" prompt.
    pub line: LineTracker,
    /// Pending activity/attention badge, rolled up per-tab in the tab bar.
    pub attn: attention::Cell,
}
