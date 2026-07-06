//! `mux ask` - the one-shot AI query behind the "?" prompt line.
//!
//! The pane types only `mux ask '<question>' < <ctx-file>`; this module
//! resolves agent + model from config.toml, spawns the agent CLI, and for
//! claude renders its stream-json output live: text deltas as they arrive,
//! tool calls as dim one-liners. (codex exec already streams its own
//! progress and is spawned untouched.)

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

use serde::Deserialize;
use serde_json::Value;

use crate::agent::{self, Agent};
use crate::state;

/// Tells claude that piped stdin is the pane's recent scrollback and that a
/// request to *do* something should be carried out with its tools, not merely
/// described. Only added when stdin is redirected - the normal "?" flow -
/// since without piped context there is nothing to anchor "this pane" to.
const CONTEXT_HINT: &str =
    "The stdin holds this terminal pane's recent scrollback as context. When \
     the request asks you to do something, carry it out with your tools (run \
     commands, edit files) instead of only describing the steps.";

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// The subset of config.toml that `mux ask` needs; unknown keys and tables
/// are ignored, and a broken file falls back to defaults exactly like the
/// GUI's loader does.
#[derive(Deserialize, Default)]
#[serde(default)]
struct AskConfig {
    agent: String,
    agent_model: String,
}

/// Agent + model override from the same config.toml the GUI writes, so a
/// settings change applies to the next "?" submit with no restart.
pub fn configured() -> (&'static Agent, Option<String>) {
    let text = fs::read_to_string(state::config_dir().join("config.toml"))
        .unwrap_or_default();
    parse_config(&text)
}

fn parse_config(text: &str) -> (&'static Agent, Option<String>) {
    let cfg: AskConfig = toml::from_str(text).unwrap_or_default();
    let agent = agent::by_id(&cfg.agent).unwrap_or_else(agent::default_agent);
    let model = (!cfg.agent_model.is_empty()).then(|| cfg.agent_model);
    (agent, model)
}

/// Run the query and return the agent's exit code. Errors are only the
/// "could not even start" kind; agent failures print their own stderr.
pub fn run(
    agent: &Agent,
    model: Option<&str>,
    query: &str,
) -> Result<i32, String> {
    let model = model.filter(|m| !m.is_empty()).or(agent.fast_model);
    let spawn_err =
        |e: io::Error| format!("failed to run {}: {e}", agent.bin);

    if agent.id == "codex" {
        let mut cmd = Command::new(agent.bin);
        cmd.arg("exec");
        // Let the agent act: workspace-write permits running commands and
        // editing files, confined to the pane's cwd (no network, no writes
        // outside the workspace) - `exec`'s default read-only sandbox would
        // block every change.
        cmd.args(["--sandbox", "workspace-write"]);
        if let Some(m) = model {
            cmd.args(["--model", m]);
        }
        cmd.arg(query);
        return Ok(cmd.status().map_err(spawn_err)?.code().unwrap_or(1));
    }

    // The approver must outlive the whole stream: claude may ask for a tool
    // at any point. It removes its socket on drop, when `run` returns.
    let approver = Approver::start();
    let mut cmd = Command::new(agent.bin);
    cmd.args(claude_args(model, !io::stdin().is_terminal()));
    if let (Some(ap), Some(settings)) = (&approver, approval_settings()) {
        cmd.args(["--settings", &settings]);
        cmd.env("MUX_APPROVE_SOCK", &ap.sock_path);
    }
    cmd.arg(query);
    cmd.stdout(Stdio::piped());
    let mut child = cmd.spawn().map_err(spawn_err)?;
    let lines =
        BufReader::new(child.stdout.take().expect("piped stdout")).lines();

    let mut fmt = Formatter::new();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in lines {
        let line =
            line.map_err(|e| format!("reading {} output: {e}", agent.bin))?;
        let _ = fmt.on_line(&line, &mut out);
    }
    let _ = fmt.finish(&mut out);

    let status = child
        .wait()
        .map_err(|e| format!("waiting for {}: {e}", agent.bin))?;
    Ok(status.code().unwrap_or(1))
}

/// stream-json is the only print-mode format that exposes tool calls, and
/// it requires --verbose; --include-partial-messages adds the text deltas
/// that make the answer stream instead of landing all at once.
/// --dangerously-skip-permissions turns off the headless auto-deny that
/// would otherwise block every Bash/Edit/Write (there is no prompt to
/// approve in `-p` mode). Approval is reinstated *selectively* by the
/// PreToolUse hook wired in `run` (see [`approval_settings`]): read-only
/// tools run freely, mutating ones ask on /dev/tty first.
fn claude_args(model: Option<&str>, with_context: bool) -> Vec<String> {
    let mut args =
        vec!["-p".to_string(), "--dangerously-skip-permissions".to_string()];
    if let Some(m) = model {
        args.extend(["--model".into(), m.into()]);
    }
    if with_context {
        args.extend(["--append-system-prompt".into(), CONTEXT_HINT.into()]);
    }
    args.extend(
        [
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--verbose",
        ]
        .map(String::from),
    );
    args
}

/// The mutating tools gated behind an interactive y/N: shell commands and
/// file writes. Anchored so `Write` matches only the Write tool, never
/// TodoWrite. Read-only tools (Read, Grep, Glob, ...) stay unmatched and
/// run without a prompt - approving each one would drown the useful gates.
const GATED_TOOLS: &str = "^(Bash|Edit|Write|MultiEdit|NotebookEdit)$";

/// The `--settings` payload that makes claude ask before it acts: a
/// PreToolUse hook on [`GATED_TOOLS`] that shells out to `mux approve`
/// (this same binary), which prompts on /dev/tty and answers allow/deny.
/// None only if the running executable can't be located, in which case the
/// agent falls back to unattended `--dangerously-skip-permissions`.
fn approval_settings() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let hook =
        format!("{} approve", agent::shell_quote(&exe.display().to_string()));
    Some(approval_settings_json(&hook))
}

fn approval_settings_json(hook_command: &str) -> String {
    serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": GATED_TOOLS,
                // Generous: the timer runs while the human decides, and a
                // hook timeout must not silently let the command through.
                "hooks": [{
                    "type": "command",
                    "command": hook_command,
                    "timeout": 600,
                }],
            }],
        },
    })
    .to_string()
}

/// The body behind `mux approve` - claude's PreToolUse hook. Reads the tool
/// call as JSON on stdin and prints the allow/deny decision claude reads
/// back. Claude runs hooks in a fresh session with *no controlling
/// terminal*, so the hook cannot prompt on /dev/tty itself; it relays the
/// request over a unix socket to the `mux ask` parent, which owns the pane
/// and does the asking. Fails closed: no socket, dead parent, or unreadable
/// input all deny, so an unapproved command never runs by default.
pub fn approve() -> io::Result<()> {
    let mut payload = String::new();
    let _ = io::stdin().read_to_string(&mut payload);
    let v: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
    let tool = v["tool_name"].as_str().unwrap_or("tool");
    let summary = args_summary(&v["tool_input"].to_string());

    let approved = relay(tool, &summary).unwrap_or(false);
    println!("{}", decision(approved));
    Ok(())
}

/// Hook side of the relay: hand the tool over to the `mux ask` parent named
/// by `MUX_APPROVE_SOCK` and wait for its verdict.
fn relay(tool: &str, summary: &str) -> io::Result<bool> {
    let path = std::env::var("MUX_APPROVE_SOCK").map_err(|_| {
        io::Error::new(io::ErrorKind::NotFound, "no approval socket")
    })?;
    relay_on(&path, tool, summary)
}

fn relay_on(sock: &str, tool: &str, summary: &str) -> io::Result<bool> {
    let stream = UnixStream::connect(sock)?;
    let req = serde_json::json!({ "tool": tool, "summary": summary });
    writeln!(&stream, "{req}")?;
    let mut resp = String::new();
    BufReader::new(&stream).read_line(&mut resp)?;
    Ok(resp.trim() == "allow")
}

/// Bridges claude's terminal-less hook back to the human. Lives in the
/// `mux ask` process - the pane's foreground job, which *does* own the
/// controlling terminal - and answers relay requests by prompting on
/// /dev/tty. The listener thread dies with the process; the socket file is
/// unlinked on drop.
struct Approver {
    sock_path: PathBuf,
}

impl Approver {
    /// None if the socket can't be bound - the caller then leaves the hook
    /// unwired and the agent runs unattended rather than denying blindly.
    fn start() -> Option<Self> {
        let sock_path = std::env::temp_dir()
            .join(format!("mux-approve-{}.sock", std::process::id()));
        let _ = fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).ok()?;
        thread::spawn(move || serve(&listener));
        Some(Self { sock_path })
    }
}

impl Drop for Approver {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.sock_path);
    }
}

/// One approval per connection. Claude runs tools serially - it blocks on
/// each hook - so requests arrive one at a time and never race the stream
/// for the terminal.
fn serve(listener: &UnixListener) {
    for stream in listener.incoming().flatten() {
        let _ = handle_request(stream);
    }
}

fn handle_request(stream: UnixStream) -> io::Result<()> {
    let mut req = String::new();
    BufReader::new(&stream).read_line(&mut req)?;
    let v: Value = serde_json::from_str(&req).unwrap_or(Value::Null);
    let tool = v["tool"].as_str().unwrap_or("tool");
    let summary = v["summary"].as_str().unwrap_or("");
    let verdict = if ask_tty(tool, summary).unwrap_or(false) {
        "allow"
    } else {
        "deny"
    };
    writeln!(&stream, "{verdict}")
}

/// Prompt on /dev/tty - not stdin, which carries the piped scrollback - and
/// read a line. Any answer starting with y/Y approves; everything else
/// (including a bare Enter) skips.
fn ask_tty(tool: &str, summary: &str) -> io::Result<bool> {
    let mut tty =
        fs::OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    write!(tty, "{}", approval_prompt(tool, summary))?;
    tty.flush()?;
    let mut line = String::new();
    BufReader::new(tty).read_line(&mut line)?;
    Ok(matches!(line.trim_start().bytes().next(), Some(b'y' | b'Y')))
}

fn approval_prompt(tool: &str, summary: &str) -> String {
    let what = match summary {
        "" => tool.to_string(),
        s => format!("{tool} {s}"),
    };
    format!("\n\x1b[1;33m▸ approve\x1b[0m {what} \x1b[2m[y/N]\x1b[0m ")
}

/// The PreToolUse decision object claude reads from the hook's stdout.
fn decision(approved: bool) -> String {
    let (verdict, reason) = if approved {
        ("allow", "approved in pane")
    } else {
        ("deny", "not approved in pane")
    };
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": verdict,
            "permissionDecisionReason": reason,
        },
    })
    .to_string()
}

/// Renders claude's stream-json lines for a terminal: answer text streams
/// through verbatim, tool calls and thinking markers become dim one-liners,
/// everything else (usage, hooks, signatures) stays invisible.
struct Formatter {
    /// Something was already printed; the final result event is only a
    /// fallback for when nothing streamed.
    wrote: bool,
    /// Output sits at the start of a line (dim markers need their own).
    at_bol: bool,
    /// In-flight tool_use blocks: stream index -> (name, partial input).
    tools: HashMap<u64, (String, String)>,
}

impl Formatter {
    fn new() -> Self {
        Self { wrote: false, at_bol: true, tools: HashMap::new() }
    }

    fn on_line(
        &mut self,
        line: &str,
        out: &mut impl Write,
    ) -> io::Result<()> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            // stream-json stdout should be pure JSON; pass anything else
            // through rather than swallow it.
            self.text(line, out)?;
            return self.text("\n", out);
        };
        match v["type"].as_str() {
            Some("stream_event") => self.on_event(&v["event"], out),
            Some("user") => self.on_tool_results(&v["message"], out),
            Some("result") => {
                let error = v["is_error"].as_bool().unwrap_or(false);
                if error || !self.wrote {
                    let text = match v["result"].as_str() {
                        Some(t) if !t.is_empty() => t.to_string(),
                        _ => v["subtype"].as_str().unwrap_or("").to_string(),
                    };
                    self.break_line(out)?;
                    self.text(&text, out)?;
                    self.break_line(out)?;
                }
                Ok(())
            },
            // "assistant" repeats what the deltas already streamed;
            // "system" (hooks, init) and rate limits are noise here.
            _ => Ok(()),
        }
    }

    fn on_event(
        &mut self,
        ev: &Value,
        out: &mut impl Write,
    ) -> io::Result<()> {
        let index = ev["index"].as_u64().unwrap_or(0);
        match ev["type"].as_str() {
            Some("content_block_start") => {
                let block = &ev["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let name = block["name"].as_str().unwrap_or("tool");
                        self.tools
                            .insert(index, (name.to_string(), String::new()));
                    },
                    // A visible pulse while the model thinks - otherwise
                    // the pane sits silent exactly like before streaming.
                    Some("thinking") => self.dim_line("thinking", out)?,
                    _ => {},
                }
                Ok(())
            },
            Some("content_block_delta") => match ev["delta"]["type"].as_str()
            {
                Some("text_delta") => {
                    self.text(ev["delta"]["text"].as_str().unwrap_or(""), out)
                },
                Some("input_json_delta") => {
                    if let Some((_, input)) = self.tools.get_mut(&index) {
                        input.push_str(
                            ev["delta"]["partial_json"].as_str().unwrap_or(""),
                        );
                    }
                    Ok(())
                },
                // thinking_delta / signature_delta stay invisible.
                _ => Ok(()),
            },
            Some("content_block_stop") => {
                if let Some((name, input)) = self.tools.remove(&index) {
                    let line = match args_summary(&input) {
                        s if s.is_empty() => name,
                        s => format!("{name} {s}"),
                    };
                    self.dim_line(&line, out)?;
                }
                Ok(())
            },
            _ => Ok(()),
        }
    }

    /// Failed tool calls explain why the agent is taking another turn;
    /// successful results would just duplicate what the answer says.
    fn on_tool_results(
        &mut self,
        message: &Value,
        out: &mut impl Write,
    ) -> io::Result<()> {
        let blocks = message["content"].as_array().into_iter().flatten();
        for block in blocks {
            if block["type"] == "tool_result"
                && block["is_error"].as_bool().unwrap_or(false)
            {
                let text = match &block["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => String::new(),
                };
                self.dim_line(
                    &format!("  ⎿ {}", one_line(&text, 100)),
                    out,
                )?;
            }
        }
        Ok(())
    }

    fn text(&mut self, t: &str, out: &mut impl Write) -> io::Result<()> {
        if t.is_empty() {
            return Ok(());
        }
        write!(out, "{t}")?;
        out.flush()?;
        self.wrote = true;
        self.at_bol = t.ends_with('\n');
        Ok(())
    }

    fn dim_line(
        &mut self,
        text: &str,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.break_line(out)?;
        self.text(&format!("{DIM}» {text}{RESET}\n"), out)
    }

    fn break_line(&mut self, out: &mut impl Write) -> io::Result<()> {
        if !self.at_bol {
            self.text("\n", out)?;
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.break_line(out)
    }
}

/// The one argument a human would want to see: `Read <file>`, `Bash <cmd>`.
fn args_summary(input_json: &str) -> String {
    const KEYS: [&str; 8] = [
        "command", "file_path", "path", "pattern", "query", "url", "prompt",
        "description",
    ];
    match serde_json::from_str::<Value>(input_json) {
        Ok(Value::Object(map)) if !map.is_empty() => {
            for key in KEYS {
                if let Some(Value::String(s)) = map.get(key) {
                    return one_line(s, 80);
                }
            }
            one_line(&Value::Object(map).to_string(), 80)
        },
        _ => String::new(),
    }
}

/// Collapse whitespace runs (including newlines) and cap the length.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(max).collect();
    if flat.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(lines: &[&str]) -> String {
        let mut fmt = Formatter::new();
        let mut out: Vec<u8> = Vec::new();
        for line in lines {
            fmt.on_line(line, &mut out).unwrap();
        }
        fmt.finish(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn config_parses_agent_and_model() {
        let (agent, model) =
            parse_config("agent = \"codex\"\nagent_model = \"gpt-5\"\n");
        assert_eq!(agent.id, "codex");
        assert_eq!(model.as_deref(), Some("gpt-5"));

        let (agent, model) = parse_config("theme = \"dracula\"\n[font]\n");
        assert_eq!(agent.id, "claude");
        assert_eq!(model, None);

        let (agent, _) = parse_config("not [ valid toml");
        assert_eq!(agent.id, "claude");
    }

    #[test]
    fn claude_args_gate_the_hint_on_piped_context() {
        let args = claude_args(Some("haiku"), true);
        assert!(args.contains(&"--append-system-prompt".to_string()));
        // Full tool access is always on so the agent can act, not just answer.
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert_eq!(
            args[..4],
            ["-p", "--dangerously-skip-permissions", "--model", "haiku"]
        );
        let args = claude_args(None, false);
        assert!(!args.contains(&"--append-system-prompt".to_string()));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(args.contains(&"--include-partial-messages".to_string()));
    }

    #[test]
    fn text_deltas_stream_and_finish_adds_newline() {
        let out = render(&[
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}}"#,
        ]);
        assert_eq!(out, "Hello world\n");
    }

    #[test]
    fn tool_calls_render_as_dim_lines() {
        let out = render(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"/tmp/ctx.txt\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Done."}}}"#,
        ]);
        assert_eq!(out, format!("{DIM}» Read /tmp/ctx.txt{RESET}\nDone.\n"));
    }

    #[test]
    fn thinking_pulses_and_deltas_stay_invisible() {
        let out = render(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Answer."}}}"#,
        ]);
        assert_eq!(out, format!("{DIM}» thinking{RESET}\nAnswer.\n"));
    }

    #[test]
    fn result_is_fallback_only_but_errors_always_print() {
        // Nothing streamed: the result text is the output.
        let out =
            render(&[r#"{"type":"result","is_error":false,"result":"Hi."}"#]);
        assert_eq!(out, "Hi.\n");

        // Text streamed: the (duplicate) result stays silent.
        let out = render(&[
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi."}}}"#,
            r#"{"type":"result","is_error":false,"result":"Hi."}"#,
        ]);
        assert_eq!(out, "Hi.\n");

        // Errors print even after streamed text, on their own line.
        let out = render(&[
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}}"#,
            r#"{"type":"result","is_error":true,"result":"","subtype":"error_max_turns"}"#,
        ]);
        assert_eq!(out, "partial\nerror_max_turns\n");
    }

    #[test]
    fn failed_tool_results_show_dim_and_unparseable_passes_through() {
        let out = render(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","is_error":true,"content":"cat: no such file\nor directory"}]}}"#,
            "plain warning",
        ]);
        assert_eq!(
            out,
            format!(
                "{DIM}»   ⎿ cat: no such file or directory{RESET}\nplain warning\n"
            )
        );
    }

    #[test]
    fn approval_settings_wire_the_hook_on_mutating_tools() {
        let json = approval_settings_json("'/opt/mux' approve");
        let v: Value = serde_json::from_str(&json).unwrap();
        let hook = &v["hooks"]["PreToolUse"][0];
        assert_eq!(hook["matcher"], GATED_TOOLS);
        assert_eq!(hook["hooks"][0]["command"], "'/opt/mux' approve");
        assert_eq!(hook["hooks"][0]["type"], "command");
        // The matcher gates the Write tool but not TodoWrite.
        let re = regex_lite(GATED_TOOLS);
        assert!(re("Bash") && re("Write") && re("NotebookEdit"));
        assert!(!re("TodoWrite") && !re("Read") && !re("WebFetch"));
    }

    /// Just enough of the anchored-alternation matcher to assert intent
    /// without a regex dep: `^(a|b|c)$` means exact membership.
    fn regex_lite(pattern: &str) -> impl Fn(&str) -> bool {
        let inner = pattern
            .trim_start_matches("^(")
            .trim_end_matches(")$")
            .split('|')
            .map(str::to_string)
            .collect::<Vec<_>>();
        move |s: &str| inner.iter().any(|p| p == s)
    }

    #[test]
    fn decision_maps_to_allow_or_deny() {
        let allow: Value = serde_json::from_str(&decision(true)).unwrap();
        assert_eq!(
            allow["hookSpecificOutput"]["permissionDecision"],
            "allow"
        );
        let deny: Value = serde_json::from_str(&decision(false)).unwrap();
        assert_eq!(deny["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(deny["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    }

    #[test]
    fn relay_round_trips_request_and_verdict() {
        let sock = std::env::temp_dir()
            .join(format!("mux-approve-test-{}.sock", std::process::id()));
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let server = thread::spawn(move || {
            let mut verdicts = ["allow\n", "deny\n"].into_iter();
            for stream in listener.incoming().flatten().take(2) {
                let mut req = String::new();
                BufReader::new(&stream).read_line(&mut req).unwrap();
                let v: Value = serde_json::from_str(&req).unwrap();
                assert_eq!(v["tool"], "Bash");
                write!(&stream, "{}", verdicts.next().unwrap()).unwrap();
            }
        });
        let path = sock.display().to_string();
        assert!(relay_on(&path, "Bash", "echo hi").unwrap());
        assert!(!relay_on(&path, "Bash", "rm -rf /").unwrap());
        server.join().unwrap();
        let _ = fs::remove_file(&sock);
    }

    #[test]
    fn approval_prompt_names_the_tool_and_summary() {
        let p = approval_prompt("Bash", "git push --force");
        assert!(p.contains("Bash git push --force"));
        assert!(p.contains("[y/N]"));
        // No summary: just the tool name, no trailing space before the tag.
        assert!(approval_prompt("Bash", "").contains("approve\x1b[0m Bash "));
    }

    #[test]
    fn summaries_flatten_and_truncate() {
        assert_eq!(one_line("a\n  b\tc", 80), "a b c");
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(args_summary(r#"{"command":"cargo test"}"#), "cargo test");
        assert_eq!(args_summary(r#"{"other":1}"#), r#"{"other":1}"#);
        assert_eq!(args_summary("not json"), "");
        assert_eq!(args_summary("{}"), "");
    }
}
