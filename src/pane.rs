use std::path::PathBuf;
use std::time::Instant;

use egui_term::TerminalBackend;

use crate::ai_prompt::LineTracker;
use crate::attention;
use muxterm::layout::PaneId;
use muxterm::state::AgentResume;

/// One terminal pane. Dropping it shuts the PTY down, which only detaches
/// the tmux client - killing the session is an explicit, separate step.
pub struct Pane {
    pub id: PaneId,
    pub session: String,
    pub backend: TerminalBackend,
    pub title: String,
    /// A short, durable, human-friendly codename (an animal, e.g. "otter"):
    /// shown on the pane's HUD bar and how the user / teammates refer to it
    /// (`mux tell/post <name>`). Auto-assigned at spawn, persisted in
    /// state.json, and overridden for display by a `mux join` agent name.
    pub name: String,
    /// Heuristic model of the shell's input line; gates the "?" prompt.
    pub line: LineTracker,
    /// Pending activity/attention badge, rolled up per-tab in the tab bar.
    pub attn: attention::Cell,
    /// muxterm parked this pane in tmux copy-mode to hold a text selection.
    /// While set, the pane's view is frozen (that freeze is what stops its
    /// repaints from wiping the selection) and keystrokes would reach
    /// copy-mode rather than the program - so typing, a plain click, or cmd+c
    /// all have to take the pane back out. Not set by a pane the *user*
    /// scrolled into copy-mode with the wheel, which is theirs to leave.
    pub copy_sel: bool,
    /// Last working directory tmux *reported* for this pane (the poll
    /// tick's `pane_snap`), seeded from the saved leaf on restore. Only ever
    /// overwritten by a real observation: a session missing from the
    /// snapshot - the server just died - must not erase where the pane was,
    /// because that is exactly when recovery needs it. `to_state` persists
    /// this, not the snapshot.
    pub cwd: Option<PathBuf>,
    /// The agent conversation last seen running here (hook-reported), for
    /// `--resume` after the session dies. Same observation-only rule as
    /// `cwd`: cleared when the pane is seen back at a shell, never because
    /// it vanished.
    pub agent: Option<AgentResume>,
    /// When this backend was spawned. A pane recovered with a `--resume`
    /// typed into it sits at a shell prompt for a moment before the CLI
    /// starts; `agent` isn't cleared by a shell sighting that early.
    pub spawned: Instant,
}
