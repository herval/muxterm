//! Global agent-hook installation: wires each agent CLI's own lifecycle
//! hooks to `mux agent-event`, which records per-session state files that
//! drive the sidebar status dot (working / attention / idle). Installed
//! globally so hand-launched agents report too; the hook command reads
//! MUXTERM_SESSION from its environment and is inert outside muxterm panes.
//!
//! - claude: command hooks merged into `~/.claude/settings.json`
//! - codex: same hook vocabulary and JSON shape, in `~/.codex/hooks.json`
//! - pi: a generated TypeScript extension in `~/.pi/agent/extensions/`
//!   (auto-discovered), since pi's hooks are TS modules, not commands
//!
//! Everything here is idempotent and re-run at every launch (from a
//! background thread in App::new): merges preserve foreign hooks, replace
//! only muxterm's own entries (refreshing a stale mux path), and rewrite
//! files only when the content actually changed. An agent whose config dir
//! doesn't exist is skipped - no config is conjured for uninstalled CLIs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

/// claude's hook events -> the state each reports. PreToolUse (unmatched =
/// every tool) is deliberate: it flips attention back to working the moment
/// a permission request is approved, not at end of turn.
const CLAUDE_EVENTS: &[(&str, &str)] = &[
    ("UserPromptSubmit", "working"),
    ("PreToolUse", "working"),
    ("Stop", "idle"),
    ("Notification", "attention"),
    ("SessionEnd", "gone"),
];

/// codex shares claude's event vocabulary but has no SessionEnd (the GUI's
/// foreground-process prune covers agent exit) and names the approval event
/// PermissionRequest.
const CODEX_EVENTS: &[(&str, &str)] = &[
    ("UserPromptSubmit", "working"),
    ("PreToolUse", "working"),
    ("Stop", "idle"),
    ("PermissionRequest", "attention"),
];

/// Install/refresh the hooks for every agent whose config dir exists.
/// Called on a background thread: the mux probe shells the login shell.
pub fn ensure_installed() {
    let Some(mux) = resolve_mux() else {
        log::warn!("mux not found in login-shell PATH; agent hooks not installed");
        return;
    };
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let claude_dir = home.join(".claude");
    if claude_dir.exists() {
        merge_hooks_file(&claude_dir.join("settings.json"), CLAUDE_EVENTS, &mux);
    }
    let codex_dir = home.join(".codex");
    if codex_dir.exists() {
        merge_hooks_file(&codex_dir.join("hooks.json"), CODEX_EVENTS, &mux);
    }
    let pi_dir = home.join(".pi");
    if pi_dir.exists() {
        write_pi_extension(
            &pi_dir.join("agent").join("extensions").join("muxterm-status.ts"),
            &mux,
        );
    }
}

/// Absolute path of `mux`, resolved through the interactive login shell for
/// the same reason as `agent::binary_available`: a Finder-launched muxterm
/// has a minimal PATH. Hooks get an absolute path because they run inside
/// the agent's own (unknown) environment. Bare and unquoted in the hook
/// command: claude documents shell execution, codex doesn't - an unquoted
/// absolute path works under both.
fn resolve_mux() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let out = Command::new(shell)
        .args(["-ilc", "command -v mux"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let path = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with('/'))?
        .to_string();
    Path::new(&path).exists().then_some(path)
}

/// Read-merge-write one hooks JSON file (claude settings.json and codex
/// hooks.json share the shape). A file that exists but doesn't parse is
/// left strictly alone - never clobber a user's config to install a dot.
fn merge_hooks_file(path: &PathBuf, events: &[(&str, &str)], mux: &str) {
    let root_text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => "{}".to_string(),
    };
    let mut root: Value = match serde_json::from_str(&root_text) {
        Ok(v @ Value::Object(_)) => v,
        _ => {
            log::warn!(
                "not merging agent hooks into {}: not a JSON object",
                path.display()
            );
            return;
        },
    };
    if !merge_hooks(&mut root, events, mux) {
        return;
    }
    let mut out = serde_json::to_string_pretty(&root).unwrap_or_default();
    out.push('\n');
    if let Err(e) = fs::write(path, out) {
        log::warn!("could not write {}: {e:#}", path.display());
    } else {
        log::info!("agent hooks installed in {}", path.display());
    }
}

/// The muxterm hook group for one state. Kept minimal: no matcher (match
/// every tool/notification), a short timeout so a wedged hook can never
/// stall the agent.
fn hook_group(mux: &str, state: &str) -> Value {
    json!({
        "hooks": [{
            "type": "command",
            "command": format!("{mux} agent-event {state}"),
            "timeout": 5,
        }],
    })
}

/// Merge muxterm's hook groups into `root.hooks.<event>`, preserving every
/// foreign entry. Muxterm's own group (recognized by the `agent-event`
/// command) is replaced in place when it drifted (e.g. mux moved), appended
/// when missing, and left untouched when identical. Returns whether the
/// document changed.
fn merge_hooks(root: &mut Value, events: &[(&str, &str)], mux: &str) -> bool {
    let Some(obj) = root.as_object_mut() else {
        return false;
    };
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for (event, state) in events {
        let desired = hook_group(mux, state);
        let entry = hooks.entry(*event).or_insert_with(|| json!([]));
        let Some(list) = entry.as_array_mut() else {
            continue;
        };
        match list.iter().position(is_muxterm_group) {
            Some(i) if list[i] == desired => {},
            Some(i) => {
                list[i] = desired;
                changed = true;
            },
            None => {
                list.push(desired);
                changed = true;
            },
        }
    }
    changed
}

fn is_muxterm_group(group: &Value) -> bool {
    group["hooks"].as_array().is_some_and(|hooks| {
        hooks.iter().any(|h| {
            h["command"]
                .as_str()
                .is_some_and(|c| c.contains(" agent-event "))
        })
    })
}

/// pi hooks are TypeScript extension modules, not commands: generate one
/// that shells out to `mux agent-event` on the agent-loop events. Written
/// only when the content differs (mirrors tmux.conf regeneration).
fn write_pi_extension(path: &PathBuf, mux: &str) {
    let content = pi_extension(mux);
    if fs::read_to_string(path).ok().as_deref() == Some(content.as_str()) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(path, content) {
        log::warn!("could not write {}: {e:#}", path.display());
    } else {
        log::info!("pi status extension written to {}", path.display());
    }
}

fn pi_extension(mux: &str) -> String {
    // JSON-encode the path so it lands as a valid TS string literal.
    let mux_literal =
        serde_json::to_string(mux).unwrap_or_else(|_| "\"mux\"".into());
    format!(
        r#"// managed by muxterm - regenerated at every launch; edits are overwritten.
// Reports pi's agent lifecycle to muxterm's sidebar status dot. Inert
// outside muxterm panes: without MUXTERM_SESSION, agent-event is a no-op.
import {{ spawn }} from "node:child_process";

const MUX = {mux_literal};

function send(state: string) {{
    if (!process.env.MUXTERM_SESSION) return;
    try {{
        const child = spawn(MUX, ["agent-event", state], {{
            stdio: "ignore",
            detached: true,
        }});
        child.unref();
    }} catch {{}}
}}

export default function (pi: any) {{
    pi.on("agent_start", () => send("working"));
    pi.on("agent_end", () => send("idle"));
    pi.on("session_shutdown", () => send("gone"));
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_into_empty_adds_all_events() {
        let mut root = json!({});
        assert!(merge_hooks(&mut root, CLAUDE_EVENTS, "/usr/local/bin/mux"));
        for (event, state) in CLAUDE_EVENTS {
            let cmd = root["hooks"][*event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert_eq!(
                cmd,
                format!("/usr/local/bin/mux agent-event {state}")
            );
        }
        // Idempotent: a second merge with the same path changes nothing.
        assert!(!merge_hooks(&mut root, CLAUDE_EVENTS, "/usr/local/bin/mux"));
    }

    #[test]
    fn merge_preserves_foreign_hooks_and_refreshes_stale_path() {
        let mut root = json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [{
                    "matcher": "^Bash$",
                    "hooks": [{"type": "command", "command": "my-linter"}],
                }],
                "Stop": [{
                    "hooks": [{"type": "command", "command": "/old/mux agent-event idle", "timeout": 5}],
                }],
            },
        });
        assert!(merge_hooks(&mut root, CLAUDE_EVENTS, "/new/mux"));
        // Foreign top-level keys and hook groups survive.
        assert_eq!(root["model"], "opus");
        assert_eq!(
            root["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "my-linter"
        );
        // Ours was appended after the foreign PreToolUse group...
        assert_eq!(
            root["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
            "/new/mux agent-event working"
        );
        // ...and the stale Stop entry was replaced in place, not duplicated.
        let stops = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(
            stops[0]["hooks"][0]["command"],
            "/new/mux agent-event idle"
        );
    }

    #[test]
    fn pi_extension_wires_the_agent_events() {
        let ts = pi_extension("/Users/x/.cargo/bin/mux");
        for needle in [
            "agent_start",
            "agent_end",
            "session_shutdown",
            "MUXTERM_SESSION",
            "\"/Users/x/.cargo/bin/mux\"",
        ] {
            assert!(ts.contains(needle), "missing {needle}");
        }
    }
}
