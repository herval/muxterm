//! The AI agent CLIs behind the "? " prompt line, shared by the GUI
//! (settings, probing, the typed command) and `mux ask` (the invocation).

use std::path::Path;
use std::process::Command;

/// A one-shot AI agent CLI that the "? " prompt line can drive.
pub struct Agent {
    /// Value stored in config.toml.
    pub id: &'static str,
    /// Human name shown in settings and the compose-line hint.
    pub label: &'static str,
    /// Executable probed before a submit.
    pub bin: &'static str,
    /// Model passed as --model when config.toml doesn't name one. A quick
    /// question about pane output doesn't need the CLI's default model,
    /// which may be a slow flagship; None leaves the choice to the CLI.
    pub fast_model: Option<&'static str>,
}

pub const AGENTS: &[Agent] = &[
    Agent {
        id: "claude",
        label: "Claude Code",
        bin: "claude",
        fast_model: Some("haiku"),
    },
    Agent { id: "codex", label: "Codex", bin: "codex", fast_model: None },
];

pub fn by_id(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.id == id)
}

pub fn default_agent() -> &'static Agent {
    &AGENTS[0]
}

/// The shell command a "? " submit types into the pane. Everything else -
/// agent choice, model, streaming flags, output formatting - lives behind
/// `mux ask` (src/ask.rs), which reads the same config.toml: the visible
/// command stays short, and pane scrollback travels via stdin redirection.
pub fn ask_command(query: &str, ctx_file: Option<&Path>) -> String {
    let mut cmd = format!("mux ask {}", shell_quote(query));
    if let Some(path) = ctx_file {
        cmd.push_str(" < ");
        cmd.push_str(&shell_quote(&path.display().to_string()));
    }
    cmd
}

/// POSIX single-quoting: wrap in '...', embedded ' becomes '\''.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Probe through the user's login shell: muxterm's own env lacks brew/npm
/// PATH entries when launched from Finder (the same reason TmuxCtl::discover
/// probes fixed paths). Fails open on spawn errors - the shell in the pane
/// prints its own "command not found".
pub fn binary_available(bin: &str) -> bool {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    Command::new(shell)
        .args(["-lc", &format!("command -v {bin}")])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_handles_embedded_quotes() {
        assert_eq!(shell_quote("fix it"), "'fix it'");
        assert_eq!(shell_quote("what's this"), "'what'\\''s this'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn ask_commands_compose_with_and_without_context() {
        assert_eq!(ask_command("fix it", None), "mux ask 'fix it'");
        assert_eq!(
            ask_command("what's this", Some(Path::new("/tmp/x.txt"))),
            "mux ask 'what'\\''s this' < '/tmp/x.txt'"
        );
    }

    #[test]
    fn lookup_falls_back_to_claude() {
        assert!(by_id("gpt").is_none());
        assert_eq!(default_agent().id, "claude");
        assert_eq!(default_agent().fast_model, Some("haiku"));
    }
}
