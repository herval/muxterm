//! `mux ask` - the one-shot AI query behind the "?" prompt line.
//!
//! The pane types only `mux ask '<question>' < <ctx-file>`; this module
//! resolves agent + model from config.toml, spawns the agent CLI, and for
//! claude renders its stream-json output live: text deltas as they arrive,
//! tool calls as dim one-liners. (codex exec already streams its own
//! progress and is spawned untouched.)

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::Value;

use crate::agent::{self, Agent};
use crate::state;

/// Steers claude away from exploratory tool-use turns - the unbounded part
/// of a one-shot's latency - when the answer is usually already in the
/// piped scrollback. Only added when stdin is redirected: without it there
/// is no terminal output to answer from, and tools may be genuinely needed.
const CONTEXT_HINT: &str =
    "Answer from the terminal output on stdin; avoid tools unless essential.";

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
        if let Some(m) = model {
            cmd.args(["--model", m]);
        }
        cmd.arg(query);
        return Ok(cmd.status().map_err(spawn_err)?.code().unwrap_or(1));
    }

    let mut cmd = Command::new(agent.bin);
    cmd.args(claude_args(model, !io::stdin().is_terminal()));
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
fn claude_args(model: Option<&str>, with_context: bool) -> Vec<String> {
    let mut args = vec!["-p".to_string()];
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
        assert_eq!(args[..3], ["-p", "--model", "haiku"]);
        let args = claude_args(None, false);
        assert!(!args.contains(&"--append-system-prompt".to_string()));
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
    fn summaries_flatten_and_truncate() {
        assert_eq!(one_line("a\n  b\tc", 80), "a b c");
        assert_eq!(one_line("abcdef", 3), "abc…");
        assert_eq!(args_summary(r#"{"command":"cargo test"}"#), "cargo test");
        assert_eq!(args_summary(r#"{"other":1}"#), r#"{"other":1}"#);
        assert_eq!(args_summary("not json"), "");
        assert_eq!(args_summary("{}"), "");
    }
}
