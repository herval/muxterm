//! Per-pane activity/attention badges: a background pane's new output puts
//! a dim dot on its tab, a bell or an explicit `mux notify` escalates to a
//! warn dot plus (window unfocused) a dock bounce and banner. The cell is
//! deliberately Instant-injected and egui-free so transitions unit-test
//! without sleeping, like the "?" prompt machine.

use std::time::{Duration, Instant};

/// Output within this window of pane creation is the tmux attach redraw
/// (restore/split repainting the whole screen), not real activity.
pub const STARTUP_GRACE: Duration = Duration::from_millis(1500);

/// Badge severity; the tab shows the max over its panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Activity,
    Attention,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reason {
    /// New content appeared.
    Output,
    /// The pane rang BEL.
    Bell,
    /// An agent raised its hand via `mux notify`, with its message.
    Notify(Option<String>),
}

/// One pane's pending badge. Mutators that can escalate return true only
/// on the rise into Attention, so the caller alerts once per episode and
/// a bell loop cannot spam notifications.
pub struct Cell {
    created: Instant,
    state: Option<(Level, Reason)>,
}

impl Cell {
    pub fn new(now: Instant) -> Self {
        Cell {
            created: now,
            state: None,
        }
    }

    /// New output. Never escalates: an existing Attention badge (and its
    /// reason) outranks plain activity until the tab is viewed.
    pub fn output(&mut self, now: Instant) {
        if now.duration_since(self.created) < STARTUP_GRACE {
            return;
        }
        if self.level() < Some(Level::Activity) {
            self.state = Some((Level::Activity, Reason::Output));
        }
    }

    /// BEL. Honored even during the startup grace (attach redraws repaint
    /// content but do not re-ring the bell, so this is always deliberate).
    /// An agent's Notify message is stickier than a subsequent bell.
    pub fn bell(&mut self) -> bool {
        let rose = self.level() < Some(Level::Attention);
        if !matches!(self.state, Some((_, Reason::Notify(_)))) {
            self.state = Some((Level::Attention, Reason::Bell));
        }
        rose
    }

    /// Explicit `mux notify`: always replaces the reason and always
    /// re-alerts - raising a hand twice means it twice.
    pub fn notify(&mut self, message: Option<String>) -> bool {
        self.state = Some((Level::Attention, Reason::Notify(message)));
        true
    }

    /// The tab was looked at while the window had focus.
    pub fn viewed(&mut self) {
        self.state = None;
    }

    pub fn indicator(&self) -> Option<(Level, &Reason)> {
        self.state.as_ref().map(|(level, reason)| (*level, reason))
    }

    fn level(&self) -> Option<Level> {
        self.state.as_ref().map(|(level, _)| *level)
    }
}

/// Best-effort macOS banner via osascript. The text travels through env
/// vars, not the script or argv, so agent-controlled strings have no
/// AppleScript-quoting/injection surface. Caveat: the banner carries
/// osascript's (Script Editor) icon; the title text is what names us.
pub fn banner(title: &str, body: &str) {
    let _ = std::process::Command::new("/usr/bin/osascript")
        .env("MUX_NT", title)
        .env("MUX_NB", body)
        .args([
            "-e",
            "display notification (system attribute \"MUX_NB\") \
             with title (system attribute \"MUX_NT\")",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell() -> (Cell, Instant) {
        let t0 = Instant::now();
        (Cell::new(t0), t0)
    }

    fn after_grace(t0: Instant) -> Instant {
        t0 + STARTUP_GRACE + Duration::from_millis(1)
    }

    #[test]
    fn output_during_grace_is_silent() {
        let (mut c, t0) = cell();
        c.output(t0 + Duration::from_millis(500));
        assert_eq!(c.indicator(), None);
        c.output(after_grace(t0));
        assert_eq!(c.indicator(), Some((Level::Activity, &Reason::Output)));
    }

    #[test]
    fn bell_ignores_grace() {
        let (mut c, _t0) = cell();
        assert!(c.bell());
        assert_eq!(c.indicator(), Some((Level::Attention, &Reason::Bell)));
    }

    #[test]
    fn bell_rise_fires_once() {
        let (mut c, _t0) = cell();
        assert!(c.bell());
        assert!(!c.bell());
    }

    #[test]
    fn output_then_bell_still_rises() {
        let (mut c, t0) = cell();
        c.output(after_grace(t0));
        assert!(c.bell());
    }

    #[test]
    fn attention_not_downgraded_by_output() {
        let (mut c, t0) = cell();
        c.bell();
        c.output(after_grace(t0));
        assert_eq!(c.indicator(), Some((Level::Attention, &Reason::Bell)));
    }

    #[test]
    fn notify_is_sticky_over_bell() {
        let (mut c, _t0) = cell();
        assert!(c.notify(Some("need a review".into())));
        assert!(!c.bell());
        assert_eq!(
            c.indicator(),
            Some((
                Level::Attention,
                &Reason::Notify(Some("need a review".into()))
            ))
        );
    }

    #[test]
    fn notify_always_realerts() {
        let (mut c, _t0) = cell();
        assert!(c.notify(None));
        assert!(c.notify(Some("again".into())));
    }

    #[test]
    fn viewed_clears_and_rearms() {
        let (mut c, t0) = cell();
        c.bell();
        c.viewed();
        assert_eq!(c.indicator(), None);
        c.output(after_grace(t0));
        assert_eq!(c.indicator(), Some((Level::Activity, &Reason::Output)));
        assert!(c.bell());
    }

    #[test]
    fn level_ordering_guards_rollup_max() {
        assert!(Level::Activity < Level::Attention);
    }
}
