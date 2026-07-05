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
}

pub const AGENTS: &[Agent] = &[
    Agent { id: "claude", label: "Claude Code", bin: "claude" },
    Agent { id: "codex", label: "Codex", bin: "codex" },
];

pub fn by_id(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.id == id)
}

pub fn default_agent() -> &'static Agent {
    &AGENTS[0]
}

impl Agent {
    /// The shell command a submit types into the pane. Pane scrollback
    /// travels via stdin redirection: both CLIs combine a prompt argument
    /// with piped stdin (codex appends it as a `<stdin>` block), and the
    /// shell doing the redirect keeps the visible command short.
    pub fn command(&self, query: &str, ctx_file: Option<&Path>) -> String {
        let quoted = shell_quote(query);
        let mut cmd = match self.id {
            "codex" => format!("codex exec {quoted}"),
            _ => format!("claude -p {quoted}"),
        };
        if let Some(path) = ctx_file {
            cmd.push_str(" < ");
            cmd.push_str(&shell_quote(&path.display().to_string()));
        }
        cmd
    }
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
    fn commands_compose_with_and_without_context() {
        let claude = by_id("claude").unwrap();
        assert_eq!(claude.command("fix it", None), "claude -p 'fix it'");
        assert_eq!(
            claude.command("fix it", Some(Path::new("/tmp/x.txt"))),
            "claude -p 'fix it' < '/tmp/x.txt'"
        );
        let codex = by_id("codex").unwrap();
        assert_eq!(
            codex.command("what's this", None),
            "codex exec 'what'\\''s this'"
        );
    }

    #[test]
    fn lookup_falls_back_to_claude() {
        assert!(by_id("gpt").is_none());
        assert_eq!(default_agent().id, "claude");
    }
}
