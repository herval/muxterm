//! Detecting an idle agent's still-running background shells.
//!
//! Claude Code can end its turn with a `run_in_background` Bash job still
//! alive ("1 shell still running") - but its Stop hook fires anyway, no hook
//! exists for background-shell start/end, and the Stop payload carries no
//! bg-task field. So the sidebar's only way to know is to poll the process
//! tree: every Claude tool shell - foreground or background - is spawned as
//! `/bin/zsh -c source ~/.claude/shell-snapshots/snapshot-...`, a marker no
//! other child carries (the `claude daemon run` / `bg-pty-host` / `bg-spare`
//! subtree never matches). Foreground tool shells look identical, which is
//! why the App only feeds *idle*-state sessions here: a marker hit under an
//! idle agent is unambiguously a background job. codex/pi never match the
//! marker, so this is claude-specific by construction - extensible by adding
//! markers.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::mpsc::Sender;

/// The argv marker of a Claude Code tool shell: every one sources a snapshot
/// from ~/.claude/shell-snapshots/. Matches zsh and bash snapshots alike.
const MARKER: &str = "shell-snapshots/snapshot-";

/// One-shot scan off the UI thread: a full `ps` is tens of milliseconds on a
/// busy mac - too slow for the synchronous poll tick. One snapshot, the pure
/// walk, one send. The App keeps at most one scan in flight.
pub fn spawn_scan(
    roots: Vec<(String, u32)>,
    tx: Sender<HashSet<String>>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let found = ps_snapshot()
            .map(|ps| detect(&ps, &roots))
            .unwrap_or_default();
        if tx.send(found).is_ok() {
            ctx.request_repaint();
        }
    });
}

/// `/bin/ps` is the macOS system binary (like git_status's /usr/bin/git).
/// `=` suppresses headers; `-ww` unbounds args so the marker can't be
/// truncated away.
fn ps_snapshot() -> Option<String> {
    let out = Command::new("/bin/ps")
        .args(["-axww", "-o", "pid=,ppid=,args="])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Which roots (session, pane_pid) have a MARKER-carrying descendant.
/// Builds ppid->children once, then walks down from each root; the visited
/// set guards degenerate self-parent lines. Checking the whole line for the
/// marker is safe because the pid/ppid columns are numeric. Pid reuse
/// between the tmux and ps snapshots (<=1s apart) could in principle blame
/// the wrong tree for one tick; not worth guarding.
fn detect(ps: &str, roots: &[(String, u32)]) -> HashSet<String> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut marked: HashSet<u32> = HashSet::new();
    for line in ps.lines() {
        let mut fields = line.split_whitespace();
        let Some(pid) = fields.next().and_then(|f| f.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(ppid) = fields.next().and_then(|f| f.parse::<u32>().ok())
        else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
        if line.contains(MARKER) {
            marked.insert(pid);
        }
    }
    let mut found = HashSet::new();
    for (session, root) in roots {
        let mut queue = vec![*root];
        let mut visited = HashSet::new();
        while let Some(pid) = queue.pop() {
            if !visited.insert(pid) {
                continue;
            }
            for kid in children.get(&pid).into_iter().flatten() {
                if marked.contains(kid) {
                    found.insert(session.clone());
                    queue.clear();
                    break;
                }
                queue.push(*kid);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from a live capture: the real background watch, the identical
    // daemon noise, and the pane/agent processes above them.
    const BG_SHELL: &str = "/bin/zsh -c source /Users/u/.claude/shell-snapshots/snapshot-zsh-1783694291408-gvpjxk.sh 2>/dev/null || true && setopt NO_EXTENDED_GLOB 2>/dev/null || true && eval 'rtk gh run watch 29100470872 --exit-status; echo done' < /dev/null && pwd -P >| /tmp/claude-7ecf-cwd";

    #[test]
    fn bg_shell_under_idle_agent_detected() {
        // Root A's claude parents a marker shell plus the daemon subtree;
        // root B's claude parents only the daemon noise. Only A lights up.
        let ps = format!(
            "  100     1 -zsh\n\
               200   100 claude\n\
               300   200 {BG_SHELL}\n\
               400   200 /Users/u/.local/bin/claude daemon run --origin transient\n\
               500   400 claude bg-pty-host --bg-pty-host /tmp/cc-daemon/spare.pty.sock\n\
               600   500 claude bg-spare --bg-spare /tmp/cc-daemon/spare.claim.sock\n\
               700     1 -zsh\n\
               800   700 claude\n\
               900   800 /Users/u/.local/bin/claude daemon run --origin transient\n"
        );
        let roots = vec![
            ("mux-aaaa1111".to_string(), 100),
            ("mux-bbbb2222".to_string(), 700),
        ];
        let found = detect(&ps, &roots);
        assert_eq!(found.len(), 1);
        assert!(found.contains("mux-aaaa1111"));
    }

    #[test]
    fn marker_outside_roots_ignored() {
        // An orphaned bg shell (claude died, shell reparented to launchd)
        // sits outside every root's subtree.
        let ps = format!("  300     1 {BG_SHELL}\n  100     1 -zsh\n");
        assert!(detect(&ps, &[("mux-aaaa1111".to_string(), 100)]).is_empty());
    }

    #[test]
    fn deep_descendants_found() {
        // pane shell -> wrapper -> claude -> marker shell: depth is not
        // fixed, the walk must reach any level.
        let ps = format!(
            "  100     1 -zsh\n\
               200   100 sh /usr/local/bin/claude-wrapper\n\
               300   200 claude\n\
               400   300 {BG_SHELL}\n"
        );
        let found = detect(&ps, &[("mux-aaaa1111".to_string(), 100)]);
        assert!(found.contains("mux-aaaa1111"));
    }

    #[test]
    fn self_parent_and_malformed_lines_are_harmless() {
        let ps = "    1     1 launchd\n\
                  garbage line without pids\n\
                  \n\
                  9 not-a-pid something\n";
        assert!(detect(ps, &[("mux-aaaa1111".to_string(), 1)]).is_empty());
    }
}
