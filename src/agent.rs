//! The AI agent CLIs behind the "?" prompt line, shared by the GUI
//! (settings, probing, the typed command) and `mux ask` (the invocation).

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// How `mux ask` (src/ask.rs) drives an agent's CLI.
pub enum AskInvocation {
    /// claude's print mode: stream-json output parsed live, mutating tools
    /// gated through the PreToolUse approval hook. The machinery lives in
    /// src/ask.rs; this variant is the dispatch decision.
    ClaudeStream,
    /// Spawn `bin` with these leading args, then `--model <m>` when a model
    /// is set, then the query; the CLI streams its own progress to the
    /// inherited stdio (codex exec today; the expected shape for new agents).
    Exec { args: &'static [&'static str] },
}

/// A one-shot AI agent CLI that the "?" prompt line can drive.
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
    /// Models offered in the workspace-creation model dropdown. Curated (not
    /// every id the CLI accepts) - a bad pick just makes the CLI error; the
    /// first entry is the dropdown default.
    pub models: &'static [&'static str],
    /// How `mux ask` invokes this CLI (see AskInvocation).
    pub ask: AskInvocation,
    /// Leading args for a quiet captured one-shot (workspace title
    /// generation): `{bin} {oneshot_args...} [--model {fast_model}] '<prompt>'`.
    pub oneshot_args: &'static [&'static str],
}

pub const AGENTS: &[Agent] = &[
    Agent {
        id: "claude",
        label: "Claude Code",
        bin: "claude",
        fast_model: Some("haiku"),
        models: &["opus", "claude-fable-5", "sonnet", "haiku"],
        ask: AskInvocation::ClaudeStream,
        oneshot_args: &["-p"],
    },
    Agent {
        id: "codex",
        label: "Codex",
        bin: "codex",
        fast_model: Some("gpt-5.4-mini"),
        models: &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini"],
        // The write sandbox is deliberate for asks: exec defaults to
        // read-only, but the agent is expected to act on the answer.
        // oneshot_args omits it - read-only is right for titling.
        ask: AskInvocation::Exec {
            args: &["exec", "--sandbox", "workspace-write"],
        },
        oneshot_args: &["exec"],
    },
    Agent {
        id: "pi",
        label: "Pi",
        bin: "pi",
        // Cheap default for `mux ask` questions. Must be a member of
        // `models` (enforced by registry_entries_are_coherent).
        fast_model: Some("haiku"),
        // Curated pi model patterns; first = dropdown default. pi is BYOK
        // multi-provider, so these are Claude-family shorthands (the provider
        // this app is used with) - adjust freely, a bad pick just errors.
        models: &["sonnet", "opus", "haiku"],
        // Print mode acts on its own (runs bash/edit/write) and merges piped
        // stdin into the prompt - exactly the `mux ask` contract. pi has no
        // Claude-style PreToolUse hook, so it runs like codex: autonomous and
        // ungated. Print mode is unrestricted, so no sandbox flag is needed.
        ask: AskInvocation::Exec { args: &["-p"] },
        oneshot_args: &["-p"],
    },
];

pub fn by_id(id: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.id == id)
}

pub fn default_agent() -> &'static Agent {
    &AGENTS[0]
}

/// Registered agent ids, for help text and error messages.
pub fn ids() -> Vec<&'static str> {
    AGENTS.iter().map(|a| a.id).collect()
}

/// The registry filtered by `binary_available` probe results (bin -> ok).
/// Unprobed bins stay visible (the probe is async), and an all-missing
/// result falls back to the full registry - an empty agent picker is never
/// right, and `binary_available` itself fails open on spawn errors.
pub fn installed(ok: &HashMap<&'static str, bool>) -> Vec<&'static Agent> {
    let hits: Vec<&'static Agent> = AGENTS
        .iter()
        .filter(|a| ok.get(a.bin) != Some(&false))
        .collect();
    if hits.is_empty() {
        AGENTS.iter().collect()
    } else {
        hits
    }
}

/// The shell command a "?" submit types into the pane. Everything else -
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

/// The interactive command a new workspace types into its pane to launch the
/// agent seeded with the user's task. Unlike `ask_command` (a one-shot
/// `mux ask`), this starts the agent's own interactive session with the prompt
/// as its first message, so the workspace is an ongoing conversation.
pub fn launch_command(
    agent: &Agent,
    model: Option<&str>,
    prompt: &str,
) -> String {
    let mut cmd = agent.bin.to_string();
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        cmd.push_str(" --model ");
        cmd.push_str(m);
    }
    cmd.push(' ');
    cmd.push_str(&shell_quote(prompt));
    cmd
}

/// The captured one-shot behind AI workspace-title generation (workspace.rs):
/// non-interactive, fast model, plain-text stdout. Unlike `launch_command`
/// (interactive, user-picked model), this always uses the registry's
/// fast_model - a summary line doesn't need a flagship.
pub fn oneshot_command(agent: &Agent, prompt: &str) -> String {
    let mut cmd = agent.bin.to_string();
    for arg in agent.oneshot_args {
        cmd.push(' ');
        cmd.push_str(arg);
    }
    if let Some(m) = agent.fast_model {
        cmd.push_str(" --model ");
        cmd.push_str(m);
    }
    cmd.push(' ');
    cmd.push_str(&shell_quote(prompt));
    cmd
}

/// POSIX single-quoting: wrap in '...', embedded ' becomes '\''.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Probe through the user's interactive login shell: muxterm's own env lacks
/// brew/npm PATH entries when launched from Finder (the same reason
/// TmuxCtl::discover probes fixed paths). The `-i` is load-bearing: zsh only
/// sources `.zshrc` for *interactive* shells, and most users put their PATH
/// there (not `.zprofile`), so a plain `-lc` probe misses `~/.local/bin` and
/// the like - which is exactly where `claude` tends to live. This matches the
/// pane's own interactive shell, where `mux ask` actually runs. Fails open on
/// spawn errors - the shell in the pane prints its own "command not found".
pub fn binary_available(bin: &str) -> bool {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    Command::new(shell)
        .args(["-ilc", &format!("command -v {bin}")])
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
        assert_eq!(default_agent().models.first(), Some(&"opus"));
    }

    #[test]
    fn registry_entries_are_coherent() {
        for a in AGENTS {
            assert!(!a.models.is_empty(), "{} has no models", a.id);
            if let Some(fast) = a.fast_model {
                assert!(
                    a.models.contains(&fast),
                    "{}'s fast_model {fast:?} is not in its models list",
                    a.id
                );
            }
        }
    }

    #[test]
    fn installed_filters_missing_bins_and_fails_open() {
        // Unprobed bins stay visible.
        let ok = HashMap::new();
        assert_eq!(installed(&ok).len(), AGENTS.len());
        // A bin probed as missing disappears.
        let ok: HashMap<&'static str, bool> = [("codex", false)].into();
        let hits = installed(&ok);
        assert!(hits.iter().all(|a| a.id != "codex"));
        assert!(!hits.is_empty());
        // Everything missing falls back to the full registry.
        let ok: HashMap<&'static str, bool> =
            AGENTS.iter().map(|a| (a.bin, false)).collect();
        assert_eq!(installed(&ok).len(), AGENTS.len());
    }

    #[test]
    fn oneshot_command_composes() {
        let claude = by_id("claude").unwrap();
        assert_eq!(
            oneshot_command(claude, "name this"),
            "claude -p --model haiku 'name this'"
        );
        let codex = by_id("codex").unwrap();
        assert_eq!(
            oneshot_command(codex, "name this"),
            "codex exec --model gpt-5.4-mini 'name this'"
        );
        let pi = by_id("pi").unwrap();
        assert_eq!(
            oneshot_command(pi, "name this"),
            "pi -p --model haiku 'name this'"
        );
    }

    #[test]
    fn launch_command_composes() {
        let claude = by_id("claude").unwrap();
        assert_eq!(
            launch_command(claude, Some("sonnet"), "fix the build"),
            "claude --model sonnet 'fix the build'"
        );
        // No model and an empty model both omit the flag.
        assert_eq!(
            launch_command(claude, None, "what's up"),
            "claude 'what'\\''s up'"
        );
        assert_eq!(
            launch_command(claude, Some(""), "hi"),
            "claude 'hi'"
        );
        // pi launches interactively with a bare prompt, same as the others.
        let pi = by_id("pi").unwrap();
        assert_eq!(
            launch_command(pi, Some("sonnet"), "fix it"),
            "pi --model sonnet 'fix it'"
        );
    }
}
