//! The AI agent CLIs behind the "?" prompt line, shared by the GUI
//! (settings, probing, the typed command) and `mux ask` (the invocation).

use std::collections::HashMap;

use std::process::Command;

/// How `mux ask` (src/ask.rs) drives an agent's CLI.
pub enum AskInvocation {
    /// claude's print mode: stream-json output parsed live, mutating tools
    /// gated through the PreToolUse approval hook. The machinery lives in
    /// src/ask.rs; this variant is the dispatch decision.
    ClaudeStream,
    /// Spawn `bin` with these leading args, then `--model <m>` when a model
    /// is set, then the query; the CLI streams its own progress to the
    /// inherited stdio (Codex, Pi, and OpenCode today).
    Exec { args: &'static [&'static str] },
}

/// A one-shot AI agent CLI that the "?" prompt line can drive.
pub struct Agent {
    /// Value stored in config.toml.
    pub id: &'static str,
    /// Human name shown in settings and the workspace popup.
    pub label: &'static str,
    /// Executable probed before a submit.
    pub bin: &'static str,
    /// Model passed as --model when config.toml doesn't name one. A quick
    /// question about pane output doesn't need the CLI's default model,
    /// which may be a slow flagship; None leaves the choice to the CLI. A
    /// preference, not a promise: `models::fast_model` resolves it against
    /// the live list and falls back to that list's default.
    pub fast_model: Option<&'static str>,
    /// The compiled-in model list: what the pickers offer until
    /// `models::discover` replaces it with the CLI's own catalog, and the
    /// fallback whenever that fails. Readers go through
    /// `models::for_agent`, never this field. Curated (not every id the CLI
    /// accepts) - a bad pick just makes the CLI error; the first entry is
    /// the picker default. An empty entry means "use the CLI's configured
    /// default" (needed by provider-agnostic agents).
    pub seed_models: &'static [&'static str],
    /// Some interactive CLIs take the initial task as a positional argument;
    /// others (OpenCode) reserve that position for a project path and need a
    /// named prompt flag instead.
    pub prompt_flag: Option<&'static str>,
    /// How `mux ask` invokes this CLI (see AskInvocation).
    pub ask: AskInvocation,
    /// Leading args for a quiet captured one-shot (workspace title
    /// generation): `{bin} {oneshot_args...} [--model {fast_model}] '<prompt>'`.
    pub oneshot_args: &'static [&'static str],
    /// Args that reopen one specific conversation, followed by its session
    /// id (`resume_session_command`): what brings a pane's agent back after
    /// its tmux session died. Empty when the CLI can't be pointed at a
    /// conversation by id - or its hooks never report one (pi, opencode).
    pub resume_args: &'static [&'static str],
}

pub const AGENTS: &[Agent] = &[
    Agent {
        id: "claude",
        label: "Claude Code",
        bin: "claude",
        fast_model: Some("haiku"),
        // Floating family aliases (each resolves to the family's latest
        // release inside the CLI), so the seed can't go stale by itself;
        // models::CLAUDE_ALIASES mirrors it for the discovered list.
        seed_models: &["opus", "fable", "sonnet", "haiku"],
        prompt_flag: None,
        ask: AskInvocation::ClaudeStream,
        // --max-turns 1: a title/summary needs exactly one model turn. In
        // print mode a tool attempt has no TTY to approve through and can
        // stall the whole one-shot; capping turns makes it return (possibly
        // empty - the caller treats that as a clean failure) instead.
        // --strict-mcp-config: ignore user/project MCP servers - a title
        // needs no tools and must not pay their startup.
        oneshot_args: &["-p", "--max-turns", "1", "--strict-mcp-config"],
        resume_args: &["--resume"],
    },
    Agent {
        id: "codex",
        label: "Codex",
        bin: "codex",
        fast_model: Some("gpt-5.6-terra"),
        seed_models: &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"],
        prompt_flag: None,
        // The write sandbox is deliberate for asks: exec defaults to
        // read-only, but the agent is expected to act on the answer.
        // oneshot_args omits it - read-only is right for titling.
        ask: AskInvocation::Exec {
            args: &["exec", "--sandbox", "workspace-write"],
        },
        oneshot_args: &["exec"],
        resume_args: &["resume"],
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
        seed_models: &["sonnet", "opus", "haiku"],
        prompt_flag: None,
        // Print mode acts on its own (runs bash/edit/write) and merges piped
        // stdin into the prompt - exactly the `mux ask` contract. pi has no
        // Claude-style PreToolUse hook, so it runs like codex: autonomous and
        // ungated. Print mode is unrestricted, so no sandbox flag is needed.
        ask: AskInvocation::Exec { args: &["-p"] },
        oneshot_args: &["-p"],
        resume_args: &[],
    },
    Agent {
        id: "opencode",
        label: "OpenCode",
        bin: "opencode",
        // OpenCode is multi-provider and its configured default is the only
        // portable choice. The optional explicit picks use OpenCode Zen's
        // provider-qualified ids; the empty first entry keeps BYOK and local
        // provider setups first-class too.
        fast_model: None,
        seed_models: &[
            "",
            "opencode/gpt-5.6-sol",
            "opencode/gpt-5.6-terra",
            "opencode/gpt-5.6-luna",
        ],
        prompt_flag: Some("--prompt"),
        // `run` consumes redirected stdin as extra prompt context. `--auto`
        // lets asks act through tools while preserving explicit config denies.
        ask: AskInvocation::Exec { args: &["run", "--auto"] },
        oneshot_args: &["run"],
        resume_args: &[],
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

/// The command a "?" types into the pane: the interactive AI prompt, which
/// owns the pane until the user leaves it. Everything else - agent choice,
/// model, the question loop, scrollback context, streaming flags, output
/// formatting - lives behind `mux ask -i` (src/ask.rs), which reads the
/// same config.toml, so the typed command stays a fixed short string.
pub fn ask_command(inline: Option<(u16, u16)>) -> String {
    let mut cmd = "mux ask -i".to_string();
    // Internal flag: `<rows-up>,<start-column>` tells `mux ask` where the
    // shell's echo of this very command sits on screen, so it can wipe it
    // before printing its own prompt there (src/ask.rs `erase_echo`). The
    // caller works the geometry out; omitting it just leaves the command
    // visible above the prompt.
    if let Some((up, col)) = inline {
        cmd.push_str(&format!(" --inline {up},{col}"));
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
    if let Some(flag) = agent.prompt_flag {
        cmd.push_str(flag);
        cmd.push(' ');
    }
    cmd.push_str(&shell_quote(prompt));
    cmd
}

/// Relaunch the agent's interactive session with no task attached, for
/// reboot recovery (`app::relaunch_agent_for_recovery`): the same CLI and
/// user-picked model as `launch_command`, but *without* a first message.
/// The reboot killed the agent mid-task; re-sending the original prompt
/// could redo or corrupt work, so we bring the CLI back up ready and let the
/// user resume.
pub fn resume_command(agent: &Agent, model: Option<&str>) -> String {
    let mut cmd = agent.bin.to_string();
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        cmd.push_str(" --model ");
        cmd.push_str(m);
    }
    cmd
}

/// Reopen one specific agent conversation by the CLI's own session id -
/// `claude --resume <id>`, `codex resume <id>` - for a pane whose tmux
/// session died under it (server crash, reboot). Unlike `resume_command`
/// this carries no model: the conversation is what's being continued, and
/// the CLI restores it as it was. None for a CLI with no resume-by-id.
pub fn resume_session_command(agent: &Agent, session: &str) -> Option<String> {
    if agent.resume_args.is_empty() || session.is_empty() {
        return None;
    }
    let mut cmd = agent.bin.to_string();
    for arg in agent.resume_args {
        cmd.push(' ');
        cmd.push_str(arg);
    }
    cmd.push(' ');
    cmd.push_str(&shell_quote(session));
    Some(cmd)
}

/// The captured one-shot behind AI workspace-title generation (workspace.rs,
/// `mux retitle`): non-interactive, fast model, plain-text stdout. Unlike
/// `launch_command` (interactive, user-picked model), this always uses the
/// registry's fast_model (as `models::fast_model` resolves it) - a summary
/// line doesn't need a flagship.
///
/// Argv form (bin first), for callers that spawn the process directly: no
/// shell means no quoting surface and - load-bearing for `mux retitle`'s
/// timeout - a `kill()` that reaches the agent instead of a wrapper sh.
/// Longest title/description `mux rename` accepts and a one-shot reply is
/// clipped to.
pub const TITLE_MAX: usize = 256;

/// Pull `title | description` out of a one-shot agent reply. Exec-style
/// CLIs stream progress lines before the answer, so the *last* non-empty
/// line wins; quotes/backticks a model might add are stripped; both halves
/// are capped at TITLE_MAX (what `mux rename` would accept). None when no
/// usable title remains.
pub fn parse_title_reply(stdout: &str) -> Option<(String, Option<String>)> {
    let line = stdout.lines().rev().find(|l| !l.trim().is_empty())?;
    let (title, desc) = match line.split_once('|') {
        Some((t, d)) => (t, Some(d)),
        None => (line, None),
    };
    let clean = |s: &str| -> String {
        let mut s = s
            .trim()
            .trim_matches(|c| c == '"' || c == '\'' || c == '`')
            .trim()
            .to_string();
        while s.len() > TITLE_MAX {
            s.pop();
        }
        s
    };
    let title = clean(title);
    if title.is_empty() || title.eq_ignore_ascii_case("keep") {
        return None;
    }
    let description = desc.map(|d| clean(d)).filter(|d| !d.is_empty());
    Some((title, description))
}

pub fn oneshot_argv(agent: &Agent, prompt: &str) -> Vec<String> {
    let mut argv = vec![agent.bin.to_string()];
    argv.extend(agent.oneshot_args.iter().map(|s| s.to_string()));
    if let Some(m) = crate::models::fast_model(agent) {
        argv.push("--model".to_string());
        argv.push(m);
    }
    argv.push(prompt.to_string());
    argv
}

/// Shell-command form of `oneshot_argv`, for callers that must go through a
/// shell (the GUI's title generation runs `$SHELL -ilc` to get the user's
/// PATH). Only the prompt needs quoting - every other token is a fixed
/// registry string.
pub fn oneshot_command(agent: &Agent, prompt: &str) -> String {
    let argv = oneshot_argv(agent, prompt);
    let (prompt, fixed) = argv.split_last().expect("argv has bin + prompt");
    format!("{} {}", fixed.join(" "), shell_quote(prompt))
}

/// The argv behind a scheduled automation run (`mux automations exec`):
/// headless, self-terminating, and - since nobody is at the keyboard at 3am -
/// carrying the registry's existing *unattended* posture rather than a new
/// one. That posture is exactly what `mux ask` already uses for its
/// non-interactive path: claude's print mode with `--dangerously-skip-
/// permissions` (headless `-p` auto-denies every mutating tool otherwise),
/// and each Exec agent's own leading args (codex's `--sandbox
/// workspace-write`, pi's `-p`, opencode's `run --auto`).
///
/// Unlike `oneshot_argv` there is no `--max-turns 1` and no `fast_model`: an
/// automation is a real task on the user's chosen model. Unlike
/// `launch_command` it never starts an interactive session - a scheduled run
/// has to end on its own. Output stays plain text (not claude's stream-json),
/// because it lands in the automation's pane and log for a human to read.
pub fn scheduled_argv(
    agent: &Agent,
    model: Option<&str>,
    prompt: &str,
) -> Vec<String> {
    let mut argv = vec![agent.bin.to_string()];
    match agent.ask {
        AskInvocation::ClaudeStream => argv.extend([
            "-p".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ]),
        AskInvocation::Exec { args } => {
            argv.extend(args.iter().map(|s| s.to_string()))
        },
    }
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        argv.extend(["--model".to_string(), m.to_string()]);
    }
    argv.push(prompt.to_string());
    argv
}

/// Compact label for the workspace model picker. The stored/passed value
/// remains provider-qualified; only OpenCode Zen's redundant provider prefix
/// is elided in the already-labelled OpenCode row.
pub fn model_label(model: &str) -> &str {
    if model.is_empty() {
        "default"
    } else {
        model.strip_prefix("opencode/").unwrap_or(model)
    }
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

/// Run a command to completion under a deadline, killing it on expiry
/// (None). Polls `try_wait` rather than blocking, which is what makes the
/// kill possible; the expected output is one short line, so the pipes
/// cannot fill up and stall the child before the deadline reaps it.
pub fn output_with_timeout(
    cmd: &mut Command,
    timeout: std::time::Duration,
) -> std::io::Result<Option<std::process::Output>> {
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map(Some);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait_with_output();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_session_command_per_cli() {
        let id = "7dbe68d3-22d9-4bc8-9e30-86e3dac4ed1d";
        assert_eq!(
            resume_session_command(by_id("claude").unwrap(), id).as_deref(),
            Some("claude --resume '7dbe68d3-22d9-4bc8-9e30-86e3dac4ed1d'")
        );
        assert_eq!(
            resume_session_command(by_id("codex").unwrap(), id).as_deref(),
            Some("codex resume '7dbe68d3-22d9-4bc8-9e30-86e3dac4ed1d'")
        );
        assert_eq!(resume_session_command(by_id("pi").unwrap(), id), None);
        assert_eq!(resume_session_command(by_id("claude").unwrap(), ""), None);
    }

    #[test]
    fn output_with_timeout_kills_on_deadline() {
        use std::time::{Duration, Instant};
        // A child outliving its budget is killed and reads as None, well
        // before its own runtime would have elapsed.
        let started = Instant::now();
        let out = output_with_timeout(
            Command::new("sleep").arg("5"),
            Duration::from_millis(100),
        )
        .unwrap();
        assert!(out.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
        // A child finishing in time comes back whole.
        let out = output_with_timeout(
            Command::new("echo").arg("hi"),
            Duration::from_secs(5),
        )
        .unwrap()
        .expect("echo finishes");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[test]
    fn quoting_handles_embedded_quotes() {
        assert_eq!(shell_quote("fix it"), "'fix it'");
        assert_eq!(shell_quote("what's this"), "'what'\\''s this'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn ask_command_opens_the_interactive_prompt() {
        assert_eq!(ask_command(None), "mux ask -i");
        // The erase geometry rides along when the caller worked it out.
        assert_eq!(ask_command(Some((2, 41))), "mux ask -i --inline 2,41");
    }

    #[test]
    fn lookup_falls_back_to_claude() {
        assert!(by_id("gpt").is_none());
        assert_eq!(default_agent().id, "claude");
        assert_eq!(default_agent().fast_model, Some("haiku"));
        assert_eq!(default_agent().seed_models.first(), Some(&"opus"));
    }

    #[test]
    fn registry_entries_are_coherent() {
        for a in AGENTS {
            assert!(!a.seed_models.is_empty(), "{} has no models", a.id);
            if let Some(fast) = a.fast_model {
                assert!(
                    a.seed_models.contains(&fast),
                    "{}'s fast_model {fast:?} is not in its seed list",
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
            "claude -p --max-turns 1 --strict-mcp-config --model haiku 'name this'"
        );
        let codex = by_id("codex").unwrap();
        assert_eq!(
            oneshot_command(codex, "name this"),
            "codex exec --model gpt-5.6-terra 'name this'"
        );
        let pi = by_id("pi").unwrap();
        assert_eq!(
            oneshot_command(pi, "name this"),
            "pi -p --model haiku 'name this'"
        );
        let opencode = by_id("opencode").unwrap();
        assert_eq!(
            oneshot_command(opencode, "name this"),
            "opencode run 'name this'"
        );
    }

    /// A scheduled run is unattended, so every agent must carry the posture
    /// that lets it act without a human: claude's skip-permissions, and each
    /// Exec agent's own leading args. It must also *not* carry `oneshot`'s
    /// `--max-turns 1` (an automation is a real task) or its fast_model.
    #[test]
    fn scheduled_argv_is_unattended_and_unlimited() {
        let claude = by_id("claude").unwrap();
        assert_eq!(
            scheduled_argv(claude, None, "check the build"),
            [
                "claude",
                "-p",
                "--dangerously-skip-permissions",
                "check the build"
            ]
        );
        // A user-picked model rides along; the fast model never does.
        assert_eq!(
            scheduled_argv(claude, Some("opus"), "check the build"),
            [
                "claude",
                "-p",
                "--dangerously-skip-permissions",
                "--model",
                "opus",
                "check the build"
            ]
        );
        assert_eq!(
            scheduled_argv(by_id("codex").unwrap(), None, "go"),
            ["codex", "exec", "--sandbox", "workspace-write", "go"]
        );
        assert_eq!(scheduled_argv(by_id("pi").unwrap(), None, "go"), [
            "pi", "-p", "go"
        ]);
        assert_eq!(scheduled_argv(by_id("opencode").unwrap(), None, "go"), [
            "opencode", "run", "--auto", "go"
        ]);
        // An empty model means "the CLI's own default", not `--model ""`.
        assert_eq!(
            scheduled_argv(by_id("opencode").unwrap(), Some(""), "go"),
            ["opencode", "run", "--auto", "go"]
        );
        for a in AGENTS {
            let argv = scheduled_argv(a, None, "go");
            assert!(
                !argv.iter().any(|x| x == "--max-turns"),
                "{} caps its turns on a scheduled run",
                a.id
            );
            assert_eq!(argv.last().unwrap(), "go");
        }
    }

    #[test]
    fn oneshot_argv_matches_command_form() {
        let claude = by_id("claude").unwrap();
        // Argv carries the prompt verbatim (no shell, no quoting), one
        // token per arg.
        assert_eq!(
            oneshot_argv(claude, "it's a name"),
            vec![
                "claude",
                "-p",
                "--max-turns",
                "1",
                "--strict-mcp-config",
                "--model",
                "haiku",
                "it's a name"
            ]
        );
        // The command form is the same tokens with the prompt quoted.
        assert_eq!(
            oneshot_command(claude, "it's a name"),
            "claude -p --max-turns 1 --strict-mcp-config --model haiku 'it'\\''s a name'"
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
        // OpenCode's positional is a project path, so the task uses --prompt.
        let opencode = by_id("opencode").unwrap();
        assert_eq!(
            launch_command(opencode, None, "fix it"),
            "opencode --prompt 'fix it'"
        );
        assert_eq!(
            launch_command(
                opencode,
                Some("opencode/gpt-5.6-terra"),
                "fix it"
            ),
            "opencode --model opencode/gpt-5.6-terra --prompt 'fix it'"
        );
    }

    #[test]
    fn resume_command_omits_the_prompt() {
        let claude = by_id("claude").unwrap();
        // Same CLI + model as launch_command, but no task message appended.
        assert_eq!(resume_command(claude, Some("sonnet")), "claude --model sonnet");
        assert_eq!(resume_command(claude, None), "claude");
        assert_eq!(resume_command(claude, Some("")), "claude");
        let opencode = by_id("opencode").unwrap();
        assert_eq!(resume_command(opencode, None), "opencode");
    }

    #[test]
    fn model_labels_keep_default_and_opencode_compact() {
        assert_eq!(model_label(""), "default");
        assert_eq!(model_label("opencode/gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(
            model_label("anthropic/claude-sonnet-5"),
            "anthropic/claude-sonnet-5"
        );
    }

    #[test]
    fn parse_title_reply_title_and_description() {
        assert_eq!(
            parse_title_reply("fix oauth flow | wiring the token refresh path\n"),
            Some((
                "fix oauth flow".to_string(),
                Some("wiring the token refresh path".to_string())
            ))
        );
    }

    #[test]
    fn parse_title_reply_last_line_wins_over_progress_noise() {
        // Exec-style CLIs stream progress before the answer.
        let out = "thinking...\nrunning tools\n\nship v2 api | rolling the gateway out\n\n";
        assert_eq!(
            parse_title_reply(out),
            Some((
                "ship v2 api".to_string(),
                Some("rolling the gateway out".to_string())
            ))
        );
    }

    #[test]
    fn parse_title_reply_strips_quotes_and_handles_bare_title() {
        assert_eq!(
            parse_title_reply("\"debug flaky tests\"\n"),
            Some(("debug flaky tests".to_string(), None))
        );
        // An empty description half falls back to title-only.
        assert_eq!(
            parse_title_reply("just a title |  \n"),
            Some(("just a title".to_string(), None))
        );
    }

    #[test]
    fn parse_title_reply_rejects_empty_output() {
        assert_eq!(parse_title_reply(""), None);
        assert_eq!(parse_title_reply("\n  \n"), None);
        assert_eq!(parse_title_reply(" | only a description\n"), None);
    }

    #[test]
    fn parse_title_reply_keep_means_no_rename() {
        assert_eq!(parse_title_reply("KEEP\n"), None);
        assert_eq!(parse_title_reply("progress...\n\"keep\"\n"), None);
        // A title that merely starts with the word is still a title.
        assert_eq!(
            parse_title_reply("keep alive fixes | pinging idle sockets"),
            Some((
                "keep alive fixes".to_string(),
                Some("pinging idle sockets".to_string())
            ))
        );
    }

    #[test]
    fn parse_title_reply_caps_at_title_max() {
        let long = "x".repeat(TITLE_MAX + 50);
        let (title, _) = parse_title_reply(&long).unwrap();
        assert_eq!(title.len(), TITLE_MAX);
    }
}
