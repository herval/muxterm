//! mux - agent-mesh CLI for muxterm.
//!
//! Agents running inside muxterm panes use this to discover teammates,
//! read each other's terminals, and exchange messages. The muxterm tab is
//! the team boundary: every command resolves peers within the caller's tab
//! only. Isolation is cooperative (anything could drive tmux directly);
//! mux enforces it at the command layer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use muxterm::mesh::{self, AgentInfo};
use muxterm::state;

const EXIT_TMUX: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_NOT_FOUND: i32 = 3;
const EXIT_NO_IDENTITY: i32 = 4;
const EXIT_CONFLICT: i32 = 5;
const EXIT_BUSY: i32 = 6;
const EXIT_NOT_IN_TAB: i32 = 7;

const TELL_MAX: usize = 64 * 1024;
const POST_MAX: usize = 16 * 1024;

const USAGE: &str = "\
mux - agent mesh for muxterm panes (team = the panes of one tab)

usage: mux [--as <session>] [--json] <command> [args]

  whoami                       your session, tab, and registered name
  join <name> [--role <r>] [--desc <t>]
                               register yourself in this tab's team
  leave [--name <n>|--session <s>]
                               deregister (default: yourself)
  peers [--all]                list teammates (--all: unregistered panes too)
  read <peer> [-n <lines>] [--ansi]
                               snapshot of a teammate's terminal
  tell <peer> [msg...] [--no-enter] [--force]
                               type a message into a teammate's terminal
                               (msg from stdin when omitted)
  post <peer> [msg...]         queue a message in their inbox (+1 notify)
  inbox [--consume]            read your queued messages
  ctx set <k> <v...> | get [k] | del <k>
                               shared per-tab key-value scratchpad
  brief                        paste-ready team briefing for a system prompt
  prune                        clean up entries for dead sessions/tabs
";

type Fail = (i32, String);
type CmdResult = Result<(), Fail>;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Err((code, msg)) = run(args) {
        eprintln!("mux: {msg}");
        std::process::exit(code);
    }
}

fn run(mut args: Vec<String>) -> CmdResult {
    let mut as_session = None;
    let mut json = false;
    let mut rest: Vec<String> = Vec::new();
    let mut it = args.drain(..);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--as" => {
                as_session = Some(it.next().ok_or_else(|| {
                    (EXIT_USAGE, "--as needs a session name".to_string())
                })?);
            },
            "--json" => json = true,
            _ => rest.push(a),
        }
    }
    if rest.is_empty() {
        print!("{USAGE}");
        return Ok(());
    }
    let cmd = rest.remove(0);

    mesh::ensure_dirs();
    match cmd.as_str() {
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        },
        "whoami" => cmd_whoami(as_session, json),
        "join" => cmd_join(as_session, rest),
        "leave" => cmd_leave(as_session, rest),
        "peers" => cmd_peers(as_session, rest, json),
        "read" => cmd_read(as_session, rest),
        "tell" => cmd_tell(as_session, rest),
        "post" => cmd_post(as_session, rest),
        "inbox" => cmd_inbox(as_session, rest, json),
        "ctx" => cmd_ctx(as_session, rest, json),
        "brief" => cmd_brief(as_session),
        "prune" => cmd_prune(),
        other => {
            Err((EXIT_USAGE, format!("unknown command {other:?} (mux help)")))
        },
    }
}

// ---------------------------------------------------------------- helpers

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    match args.iter().position(|a| a == flag) {
        Some(i) => {
            args.remove(i);
            true
        },
        None => false,
    }
}

fn take_opt(args: &mut Vec<String>, flag: &str) -> Result<Option<String>, Fail> {
    match args.iter().position(|a| a == flag) {
        Some(i) => {
            args.remove(i);
            if i < args.len() {
                Ok(Some(args.remove(i)))
            } else {
                Err((EXIT_USAGE, format!("{flag} needs a value")))
            }
        },
        None => Ok(None),
    }
}

/// Message from remaining args, or stdin when none were given.
fn message_from(args: &[String]) -> Result<String, Fail> {
    let text = if args.is_empty() {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| (EXIT_USAGE, format!("reading stdin: {e}")))?;
        buf
    } else {
        args.join(" ")
    };
    let text = text.trim_end_matches('\n').to_string();
    if text.trim().is_empty() {
        return Err((EXIT_USAGE, "empty message".to_string()));
    }
    Ok(text)
}

fn fmt_ts(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// ------------------------------------------------------------------ tmux

struct Tmux {
    bin: PathBuf,
}

impl Tmux {
    fn new() -> Result<Self, Fail> {
        mesh::find_tmux()
            .map(|bin| Self { bin })
            .map_err(|e| (EXIT_TMUX, format!("{e:#}")))
    }

    fn output(&self, args: &[&str]) -> Result<String, Fail> {
        let out = Command::new(&self.bin)
            .args(["-L", mesh::SOCKET])
            .args(args)
            .output()
            .map_err(|e| (EXIT_TMUX, format!("failed to run tmux: {e}")))?;
        if !out.status.success() {
            return Err((
                EXIT_TMUX,
                format!(
                    "tmux {}: {}",
                    args.first().copied().unwrap_or(""),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn has_session(&self, session: &str) -> bool {
        Command::new(&self.bin)
            .args(["-L", mesh::SOCKET, "has-session", "-t"])
            .arg(format!("={session}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn live_sessions(&self) -> HashSet<String> {
        match Command::new(&self.bin)
            .args(["-L", mesh::SOCKET, "list-sessions", "-F", "#{session_name}"])
            .output()
        {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::to_owned)
                    .collect()
            },
            _ => HashSet::new(),
        }
    }

    /// session -> (current command, cwd, last activity) for every pane.
    fn pane_info(&self) -> HashMap<String, (String, String, String)> {
        let out = self
            .output(&[
                "list-panes",
                "-a",
                "-F",
                "#{session_name}\t#{pane_current_command}\t#{pane_current_path}\t#{t:session_activity}",
            ])
            .unwrap_or_default();
        let mut map = HashMap::new();
        for line in out.lines() {
            let mut parts = line.splitn(4, '\t');
            if let (Some(s), Some(cmd), Some(cwd), Some(act)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            {
                map.entry(s.to_string()).or_insert((
                    cmd.to_string(),
                    cwd.to_string(),
                    act.to_string(),
                ));
            }
        }
        map
    }

    // NOTE: pane-targeting commands (capture-pane, paste-buffer, send-keys,
    // display-message) reject the `=` exact-match prefix that target-session
    // commands accept; plain names resolve exact-first, and our session
    // names are fixed-length uuids, so prefix ambiguity can't bite.

    fn capture(
        &self,
        session: &str,
        lines: usize,
        ansi: bool,
    ) -> Result<String, Fail> {
        let target = session.to_string();
        let start = format!("-{lines}");
        let mut args =
            vec!["capture-pane", "-p", "-J", "-S", &start, "-t", &target];
        if ansi {
            args.insert(1, "-e");
        }
        self.output(&args)
    }

    fn in_copy_mode(&self, session: &str) -> Result<bool, Fail> {
        let target = session.to_string();
        let out = self.output(&[
            "display-message",
            "-p",
            "-t",
            &target,
            "#{pane_in_mode}",
        ])?;
        Ok(out.trim() == "1")
    }

    /// Deliver text through a tmux buffer: bytes never travel through argv
    /// (no `;` separator or ARG_MAX hazards), and `paste-buffer -p` uses
    /// bracketed paste iff the receiving program requested it.
    fn paste_text(
        &self,
        session: &str,
        text: &str,
        enter: bool,
    ) -> CmdResult {
        let buffer = format!("mux-msg-{}", std::process::id());
        let mut child = Command::new(&self.bin)
            .args(["-L", mesh::SOCKET, "load-buffer", "-b", &buffer, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| (EXIT_TMUX, format!("failed to run tmux: {e}")))?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(text.as_bytes())
            .map_err(|e| (EXIT_TMUX, format!("load-buffer: {e}")))?;
        let status = child
            .wait()
            .map_err(|e| (EXIT_TMUX, format!("load-buffer: {e}")))?;
        if !status.success() {
            return Err((EXIT_TMUX, "tmux load-buffer failed".to_string()));
        }

        self.output(&["paste-buffer", "-d", "-p", "-b", &buffer, "-t", session])?;
        if enter {
            self.output(&["send-keys", "-t", session, "Enter"])?;
        }
        Ok(())
    }
}

// -------------------------------------------------------------- identity

fn resolve_identity(
    tmux: &Tmux,
    as_session: Option<String>,
) -> Result<String, Fail> {
    if let Some(session) = as_session {
        if tmux.has_session(&session) {
            return Ok(session);
        }
        return Err((
            EXIT_NOT_FOUND,
            format!("session {session:?} not found on the muxterm socket"),
        ));
    }

    let tmux_env = env::var("TMUX").map_err(|_| {
        (
            EXIT_NO_IDENTITY,
            "not inside a muxterm pane (use --as <session>)".to_string(),
        )
    })?;
    let socket_path = tmux_env.split(',').next().unwrap_or("").to_string();
    let socket_name = std::path::Path::new(&socket_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if socket_name != mesh::SOCKET {
        return Err((
            EXIT_NO_IDENTITY,
            format!(
                "this pane belongs to tmux server {socket_name:?}, not \
                 muxterm (nested tmux?); use --as <session>"
            ),
        ));
    }
    let pane = env::var("TMUX_PANE").map_err(|_| {
        (EXIT_NO_IDENTITY, "TMUX_PANE unset; use --as <session>".to_string())
    })?;
    let out = Command::new(&tmux.bin)
        .args(["-S", &socket_path, "display-message", "-p", "-t", &pane])
        .arg("#{session_name}")
        .output()
        .map_err(|e| (EXIT_TMUX, format!("failed to run tmux: {e}")))?;
    if !out.status.success() {
        return Err((
            EXIT_TMUX,
            format!(
                "resolving own session: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Identity + tab membership: the scope every peer operation runs in.
struct Scope {
    session: String,
    tab_id: String,
    members: Vec<String>,
    all_sessions: HashSet<String>,
    registry: mesh::Registry,
}

fn scope(tmux: &Tmux, as_session: Option<String>) -> Result<Scope, Fail> {
    let session = resolve_identity(tmux, as_session)?;
    let st = state::peek().ok_or_else(|| {
        (
            EXIT_NO_IDENTITY,
            "no readable muxterm state.json (has muxterm run yet?)"
                .to_string(),
        )
    })?;
    let (tab_id, members) =
        mesh::tab_of_session(&st, &session).ok_or_else(|| {
            (
                EXIT_NO_IDENTITY,
                format!("session {session} is not part of any muxterm tab"),
            )
        })?;
    let mut all_sessions = HashSet::new();
    for window in &st.windows {
        for tab in &window.tabs {
            tab.tree.sessions(&mut all_sessions);
        }
    }
    Ok(Scope {
        session,
        tab_id,
        members,
        all_sessions,
        registry: mesh::load_registry(),
    })
}

impl Scope {
    fn my_display(&self) -> String {
        self.registry
            .agents
            .get(&self.session)
            .map(|i| i.name.clone())
            .unwrap_or_else(|| self.session.clone())
    }

    /// Resolve a peer argument (agent name or raw session) within this tab.
    fn resolve_peer(&self, arg: &str) -> Result<(String, String), Fail> {
        for (session, info) in &self.registry.agents {
            if info.name == arg {
                if self.members.contains(session) {
                    return Ok((session.clone(), info.name.clone()));
                }
                return Err((
                    EXIT_NOT_IN_TAB,
                    format!("{arg:?} is not in your tab"),
                ));
            }
        }
        if self.members.iter().any(|m| m == arg) {
            return Ok((arg.to_string(), arg.to_string()));
        }
        if arg.starts_with(mesh::SESSION_PREFIX)
            && (self.all_sessions.contains(arg)
                || self.registry.agents.contains_key(arg))
        {
            return Err((
                EXIT_NOT_IN_TAB,
                format!("session {arg} is not in your tab"),
            ));
        }
        Err((
            EXIT_NOT_FOUND,
            format!("no agent named {arg:?} in your tab (try: mux peers)"),
        ))
    }
}

// ---------------------------------------------------------- subcommands

fn cmd_whoami(as_session: Option<String>, json: bool) -> CmdResult {
    let tmux = Tmux::new()?;
    let session = resolve_identity(&tmux, as_session)?;
    let tab = state::peek().and_then(|s| mesh::tab_of_session(&s, &session));
    let registry = mesh::load_registry();
    let info = registry.agents.get(&session);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "session": session,
                "tab": tab.as_ref().map(|(id, _)| id),
                "name": info.map(|i| i.name.clone()),
                "role": info.and_then(|i| i.role.clone()),
            })
        );
    } else {
        println!("session={session}");
        if let Some((tab_id, _)) = &tab {
            println!("tab={tab_id}");
        }
        if let Some(info) = info {
            println!("name={}", info.name);
            if let Some(role) = &info.role {
                println!("role={role}");
            }
        }
    }
    Ok(())
}

fn cmd_join(as_session: Option<String>, mut args: Vec<String>) -> CmdResult {
    let role = take_opt(&mut args, "--role")?;
    let desc = take_opt(&mut args, "--desc")?;
    let name = args.first().cloned().ok_or_else(|| {
        (EXIT_USAGE, "usage: mux join <name> [--role <r>] [--desc <t>]".to_string())
    })?;
    if !mesh::valid_name(&name) {
        return Err((
            EXIT_USAGE,
            "invalid name: lowercase letters/digits then letters, digits, \
             '-' or '_' (max 32 chars)"
                .to_string(),
        ));
    }

    let tmux = Tmux::new()?;
    let sc = scope(&tmux, as_session)?;
    let live = tmux.live_sessions();
    let mut registry = mesh::load_registry();

    // Names are unique within the tab; a dead holder is silently replaced.
    let holder = registry
        .agents
        .iter()
        .find(|(session, info)| {
            info.name == name
                && **session != sc.session
                && sc.members.contains(*session)
        })
        .map(|(session, _)| session.clone());
    if let Some(holder) = holder {
        if live.contains(&holder) {
            return Err((
                EXIT_CONFLICT,
                format!("name {name:?} is taken by {holder} in this tab"),
            ));
        }
        registry.agents.remove(&holder);
    }

    registry.agents.insert(
        sc.session.clone(),
        AgentInfo {
            name: name.clone(),
            role,
            desc,
            joined_at: mesh::now(),
        },
    );
    mesh::save_registry(&registry)
        .map_err(|e| (EXIT_TMUX, format!("saving registry: {e:#}")))?;
    println!("joined as {name} ({})", sc.session);
    Ok(())
}

fn cmd_leave(as_session: Option<String>, mut args: Vec<String>) -> CmdResult {
    let by_name = take_opt(&mut args, "--name")?;
    let by_session = take_opt(&mut args, "--session")?;
    let tmux = Tmux::new()?;

    let (session, explicit) = if let Some(session) = by_session {
        (session, true)
    } else if let Some(name) = by_name {
        let registry = mesh::load_registry();
        let session = registry
            .agents
            .iter()
            .find(|(_, i)| i.name == name)
            .map(|(s, _)| s.clone())
            .ok_or_else(|| {
                (EXIT_NOT_FOUND, format!("no agent named {name:?}"))
            })?;
        (session, true)
    } else {
        (resolve_identity(&tmux, as_session)?, false)
    };

    let registered = mesh::load_registry().agents.contains_key(&session);
    if !registered && explicit {
        return Err((EXIT_NOT_FOUND, format!("{session} is not registered")));
    }
    mesh::remove_session(&session);
    if registered {
        println!("left ({session})");
    } else {
        println!("was not registered ({session})");
    }
    Ok(())
}

fn cmd_peers(
    as_session: Option<String>,
    mut args: Vec<String>,
    json: bool,
) -> CmdResult {
    let all = take_flag(&mut args, "--all");
    let tmux = Tmux::new()?;
    let sc = scope(&tmux, as_session)?;
    let live = tmux.live_sessions();
    let panes = tmux.pane_info();

    struct Row {
        name: String,
        role: String,
        session: String,
        cmd: String,
        cwd: String,
        active: String,
    }
    let mut rows = Vec::new();
    for member in &sc.members {
        if !live.contains(member) {
            continue;
        }
        let info = sc.registry.agents.get(member);
        if info.is_none() && !all {
            continue;
        }
        let (cmd, cwd, active) = panes
            .get(member)
            .cloned()
            .unwrap_or_else(|| ("-".into(), "-".into(), "-".into()));
        let star = if *member == sc.session { "*" } else { "" };
        rows.push(Row {
            name: info
                .map(|i| format!("{}{star}", i.name))
                .unwrap_or_else(|| format!("-{star}")),
            role: info
                .and_then(|i| i.role.clone())
                .unwrap_or_else(|| "-".into()),
            session: member.clone(),
            cmd,
            cwd,
            active,
        });
    }

    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name.trim_end_matches('*'),
                    "self": r.name.ends_with('*'),
                    "role": r.role, "session": r.session,
                    "command": r.cmd, "cwd": r.cwd, "active": r.active,
                })
            })
            .collect();
        println!("{}", serde_json::json!(arr));
        return Ok(());
    }

    if rows.is_empty() {
        eprintln!("(no registered agents in this tab; join with: mux join <name>)");
        return Ok(());
    }
    let w_name = rows.iter().map(|r| r.name.len()).max().unwrap().max(4);
    let w_role = rows.iter().map(|r| r.role.len()).max().unwrap().max(4);
    let w_sess = rows.iter().map(|r| r.session.len()).max().unwrap().max(7);
    let w_cmd = rows.iter().map(|r| r.cmd.len()).max().unwrap().max(3);
    println!(
        "{:<w_name$}  {:<w_role$}  {:<w_sess$}  {:<w_cmd$}  {}",
        "NAME", "ROLE", "SESSION", "CMD", "CWD",
    );
    for r in rows {
        println!(
            "{:<w_name$}  {:<w_role$}  {:<w_sess$}  {:<w_cmd$}  {}",
            r.name, r.role, r.session, r.cmd, r.cwd,
        );
    }
    Ok(())
}

fn cmd_read(as_session: Option<String>, mut args: Vec<String>) -> CmdResult {
    let ansi = take_flag(&mut args, "--ansi");
    let lines: usize = match take_opt(&mut args, "-n")? {
        Some(n) => n.parse().map_err(|_| {
            (EXIT_USAGE, format!("-n expects a number, got {n:?}"))
        })?,
        None => 200,
    };
    let target = args.first().cloned().ok_or_else(|| {
        (EXIT_USAGE, "usage: mux read <peer> [-n <lines>] [--ansi]".to_string())
    })?;

    let tmux = Tmux::new()?;
    let sc = scope(&tmux, as_session)?;
    let (session, display) = sc.resolve_peer(&target)?;
    if !tmux.has_session(&session) {
        return Err((
            EXIT_NOT_FOUND,
            format!("{display} is registered but its session {session} is gone (mux prune to clean up)"),
        ));
    }

    let raw = tmux.capture(&session, lines, ansi)?;
    let trimmed: Vec<&str> = {
        let mut v: Vec<&str> = raw.lines().collect();
        while v.last().is_some_and(|l| l.trim().is_empty()) {
            v.pop();
        }
        let skip = v.len().saturating_sub(lines);
        v.split_off(skip)
    };
    for line in trimmed {
        println!("{line}");
    }
    Ok(())
}

fn cmd_tell(as_session: Option<String>, mut args: Vec<String>) -> CmdResult {
    let no_enter = take_flag(&mut args, "--no-enter");
    let force = take_flag(&mut args, "--force");
    if args.is_empty() {
        return Err((
            EXIT_USAGE,
            "usage: mux tell <peer> [message...] [--no-enter] [--force]"
                .to_string(),
        ));
    }
    let target = args.remove(0);
    let message = message_from(&args)?;
    if message.len() > TELL_MAX {
        return Err((
            EXIT_USAGE,
            "message too large (64 KiB max); write it to a file and tell the path".to_string(),
        ));
    }

    let tmux = Tmux::new()?;
    let sc = scope(&tmux, as_session)?;
    let (session, display) = sc.resolve_peer(&target)?;
    if session == sc.session && !force {
        return Err((
            EXIT_USAGE,
            "refusing to type into your own pane (--force to override)"
                .to_string(),
        ));
    }
    if !tmux.has_session(&session) {
        return Err((
            EXIT_NOT_FOUND,
            format!("{display}'s session {session} is gone (mux prune to clean up)"),
        ));
    }
    if tmux.in_copy_mode(&session)? {
        if force {
            tmux.output(&["send-keys", "-t", &session, "-X", "cancel"])?;
        } else {
            return Err((
                EXIT_BUSY,
                format!("{display}'s pane is in copy-mode; retry, or pass --force to cancel it"),
            ));
        }
    }
    tmux.paste_text(&session, &message, !no_enter)
}

#[derive(Serialize, Deserialize)]
struct InboxMsg {
    v: u32,
    from: String,
    from_session: String,
    ts: u64,
    text: String,
}

fn cmd_post(as_session: Option<String>, mut args: Vec<String>) -> CmdResult {
    if args.is_empty() {
        return Err((
            EXIT_USAGE,
            "usage: mux post <peer> [message...]".to_string(),
        ));
    }
    let target = args.remove(0);
    let message = message_from(&args)?;
    if message.len() > POST_MAX {
        return Err((
            EXIT_USAGE,
            "message too large (16 KiB max); write it to a file and post the path".to_string(),
        ));
    }

    let tmux = Tmux::new()?;
    let sc = scope(&tmux, as_session)?;
    let (session, display) = sc.resolve_peer(&target)?;
    if !tmux.has_session(&session) {
        return Err((
            EXIT_NOT_FOUND,
            format!("{display}'s session {session} is gone (mux prune to clean up)"),
        ));
    }

    let msg = InboxMsg {
        v: 1,
        from: sc.my_display(),
        from_session: sc.session.clone(),
        ts: mesh::now(),
        text: message,
    };
    let line = serde_json::to_string(&msg)
        .map_err(|e| (EXIT_TMUX, format!("encoding message: {e}")))?
        + "\n";
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(mesh::inbox_path(&session))
        .map_err(|e| (EXIT_TMUX, format!("opening inbox: {e}")))?;
    file.write_all(line.as_bytes())
        .map_err(|e| (EXIT_TMUX, format!("writing inbox: {e}")))?;

    // One notification per inbox-check cycle, no matter how many posts:
    // whoever wins creating the flag file injects the (single) nudge.
    if session != sc.session {
        let fresh = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(mesh::flag_path(&session))
            .is_ok();
        if fresh {
            let note = format!(
                "[mux] new message from {} - run: mux inbox",
                sc.my_display()
            );
            // Best effort: the message is already durable in the inbox.
            let _ = tmux.paste_text(&session, &note, true);
        }
    }
    println!("posted to {display}");
    Ok(())
}

fn cmd_inbox(
    as_session: Option<String>,
    mut args: Vec<String>,
    json: bool,
) -> CmdResult {
    let consume = take_flag(&mut args, "--consume");
    let tmux = Tmux::new()?;
    let session = resolve_identity(&tmux, as_session)?;
    let path = mesh::inbox_path(&session);

    let text = if consume {
        let tmp = path.with_extension(format!("consuming.{}", std::process::id()));
        match fs::rename(&path, &tmp) {
            Ok(()) => {
                let t = fs::read_to_string(&tmp).unwrap_or_default();
                let _ = fs::remove_file(&tmp);
                t
            },
            Err(_) => String::new(),
        }
    } else {
        fs::read_to_string(&path).unwrap_or_default()
    };
    // The notification's job is done once the inbox is read; re-arm it.
    let _ = fs::remove_file(mesh::flag_path(&session));

    if text.trim().is_empty() {
        eprintln!("(inbox empty)");
        return Ok(());
    }
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if json {
            println!("{line}");
            continue;
        }
        match serde_json::from_str::<InboxMsg>(line) {
            Ok(msg) => {
                let mut lines = msg.text.lines();
                println!(
                    "[{}] {}: {}",
                    fmt_ts(msg.ts),
                    msg.from,
                    lines.next().unwrap_or("")
                );
                for cont in lines {
                    println!("    {cont}");
                }
            },
            Err(_) => println!("{line}"),
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Default)]
struct CtxFile {
    version: u32,
    entries: BTreeMap<String, CtxEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct CtxEntry {
    value: String,
    by: String,
    ts: u64,
}

fn valid_ctx_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().all(|c| {
            c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(c, '-' | '_' | '.' | '/')
        })
}

/// Serialize read-modify-write cycles on a ctx file; a stale lock (killed
/// process) is broken after ~1s rather than deadlocking.
fn with_ctx_lock<T>(path: &std::path::Path, f: impl FnOnce() -> T) -> T {
    let lock = path.with_extension("lock");
    for _ in 0..100 {
        if OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .is_ok()
        {
            let result = f();
            let _ = fs::remove_file(&lock);
            return result;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = fs::remove_file(&lock);
    f()
}

fn cmd_ctx(
    as_session: Option<String>,
    args: Vec<String>,
    json: bool,
) -> CmdResult {
    let usage = || {
        (
            EXIT_USAGE,
            "usage: mux ctx set <key> <value...> | get [<key>] | del <key>"
                .to_string(),
        )
    };
    let sub = args.first().cloned().ok_or_else(usage)?;
    let tmux = Tmux::new()?;
    let sc = scope(&tmux, as_session)?;
    let path = mesh::ctx_path(&sc.tab_id);

    let load = || -> CtxFile {
        fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    };

    match sub.as_str() {
        "set" => {
            let key = args.get(1).cloned().ok_or_else(usage)?;
            if !valid_ctx_key(&key) {
                return Err((
                    EXIT_USAGE,
                    "invalid key: lowercase letters, digits, - _ . / (max 128)".to_string(),
                ));
            }
            if args.len() < 3 {
                return Err(usage());
            }
            let value = args[2..].join(" ");
            with_ctx_lock(&path, || -> CmdResult {
                let mut ctx = load();
                ctx.version = 1;
                ctx.entries.insert(
                    key.clone(),
                    CtxEntry {
                        value,
                        by: sc.my_display(),
                        ts: mesh::now(),
                    },
                );
                let tmp = path.with_extension("json.tmp");
                fs::write(&tmp, serde_json::to_string_pretty(&ctx).unwrap())
                    .and_then(|_| fs::rename(&tmp, &path))
                    .map_err(|e| (EXIT_TMUX, format!("writing ctx: {e}")))
            })?;
            Ok(())
        },
        "get" => {
            let ctx = load();
            match args.get(1) {
                Some(key) => match ctx.entries.get(key) {
                    Some(entry) => {
                        println!("{}", entry.value);
                        Ok(())
                    },
                    None => Err((
                        EXIT_NOT_FOUND,
                        format!("no ctx key {key:?} in this tab"),
                    )),
                },
                None => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&ctx.entries)
                                .unwrap_or_else(|_| "{}".into())
                        );
                    } else {
                        for (key, entry) in &ctx.entries {
                            println!("{key}\t{}", entry.value);
                        }
                    }
                    Ok(())
                },
            }
        },
        "del" => {
            let key = args.get(1).cloned().ok_or_else(usage)?;
            with_ctx_lock(&path, || -> CmdResult {
                let mut ctx = load();
                if ctx.entries.remove(&key).is_none() {
                    return Err((
                        EXIT_NOT_FOUND,
                        format!("no ctx key {key:?} in this tab"),
                    ));
                }
                let tmp = path.with_extension("json.tmp");
                fs::write(&tmp, serde_json::to_string_pretty(&ctx).unwrap())
                    .and_then(|_| fs::rename(&tmp, &path))
                    .map_err(|e| (EXIT_TMUX, format!("writing ctx: {e}")))
            })
        },
        _ => Err(usage()),
    }
}

fn cmd_brief(as_session: Option<String>) -> CmdResult {
    let tmux = Tmux::new()?;
    let sc = scope(&tmux, as_session)?;
    let live = tmux.live_sessions();
    let me = sc.my_display();

    println!("## muxterm agent mesh");
    println!();
    if let Some(info) = sc.registry.agents.get(&sc.session) {
        match &info.role {
            Some(role) => println!("You are **{}** (role: {role}) in a shared muxterm tab.", info.name),
            None => println!("You are **{}** in a shared muxterm tab.", info.name),
        }
    } else {
        println!(
            "You are an unregistered pane ({}) in a shared muxterm tab. \
             Register first: `mux join <name> --role <role>`",
            sc.session
        );
    }
    println!();
    println!("Teammates in this tab:");
    let mut any = false;
    for member in &sc.members {
        if member == &sc.session || !live.contains(member) {
            continue;
        }
        if let Some(info) = sc.registry.agents.get(member) {
            any = true;
            match (&info.role, &info.desc) {
                (Some(r), Some(d)) => println!("- **{}** ({r}): {d}", info.name),
                (Some(r), None) => println!("- **{}** ({r})", info.name),
                (None, Some(d)) => println!("- **{}**: {d}", info.name),
                (None, None) => println!("- **{}**", info.name),
            }
        }
    }
    if !any {
        println!("- (none registered yet)");
    }
    println!();
    println!("Coordinate through the `mux` CLI (run via your shell):");
    println!("- `mux peers` - who is on the team");
    println!("- `mux read <name> -n 200` - snapshot of a teammate's terminal (works for any program they run)");
    println!("- `mux post <name> <text>` - queue a message in their inbox; they get one `[mux]` nudge");
    println!("- `mux inbox --consume` - read messages sent to you (do this when you see a `[mux]` line)");
    println!("- `mux tell <name> <text>` - type directly into their terminal (immediate but can interleave)");
    println!("- `mux ctx set/get <key> [value]` - shared scratchpad for this tab");
    println!();
    println!(
        "Etiquette: prefer `post` for anything a teammate should act on; \
         `tell` only when they expect immediate input. You cannot reach \
         panes in other tabs. Sign your work as **{me}**."
    );
    Ok(())
}

fn cmd_prune() -> CmdResult {
    let tmux = Tmux::new()?;
    let live = tmux.live_sessions();
    let live_tabs: HashSet<String> = state::peek()
        .map(|st| {
            st.windows
                .iter()
                .flat_map(|w| w.tabs.iter().map(|t| t.id.clone()))
                .collect()
        })
        .unwrap_or_default();
    let removed = mesh::prune(&live, &live_tabs);
    if removed.is_empty() {
        println!("nothing to prune");
    } else {
        for item in removed {
            println!("removed {item}");
        }
    }
    Ok(())
}
