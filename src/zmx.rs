//! Experimental zmx session backend (config `backend = "zmx"`).
//!
//! zmx (https://github.com/neurosnap/zmx) is session persistence and nothing
//! else: one daemon per session, a PTY, and a libghostty-vt mirror of the
//! screen that it replays to a client on reattach. No windows, no copy-mode,
//! no options. That is a close fit for muxterm, which already does its own
//! layout - the trade is that everything tmux's copy-mode did (scrollback,
//! selection, search) moves into the local grid, which under zmx actually
//! owns its history because the app's raw output reaches it untouched.
//!
//! Shared by the GUI and the `mux` CLI, so it lives in the library. The
//! sockets sit in a dedicated `ZMX_DIR` (`~/.muxterm/zmx`) - the zmx
//! equivalent of `tmux -L muxterm` - which keeps muxterm's sessions apart
//! from the user's own and makes the startup GC safe. Keep that path short:
//! a socket path past the unix limit (104 bytes on macOS) makes session
//! creation fail silently.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::state;

/// Which multiplexer keeps a pane alive. Both run side by side: every
/// session belongs to exactly one, and its *name* says which (see
/// `Backend::of_session`), so the GUI, the `mux` CLI, and agents all route a
/// session without consulting any shared state. config `backend` only picks
/// the backend for *new* panes; existing ones stay where they are until
/// closed (a running process can't move between backends).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Tmux,
    Zmx,
}

/// zmx session names carry a `z` right after the prefix: `mux-z1a2b3c4d`.
/// tmux names are the prefix plus eight hex digits (`mux-1a2b3c4d`), and `z`
/// is never a hex digit - so the mark can't collide with an existing name.
pub const ZMX_MARK: &str = "mux-z";

impl Backend {
    /// The backend a session lives on, from its name alone.
    pub fn of_session(name: &str) -> Backend {
        if name.starts_with(ZMX_MARK) {
            Backend::Zmx
        } else {
            Backend::Tmux
        }
    }

    /// The config.toml spelling; None for anything else.
    pub fn parse(value: &str) -> Option<Backend> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tmux" | "" => Some(Backend::Tmux),
            "zmx" => Some(Backend::Zmx),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Tmux => "tmux",
            Backend::Zmx => "zmx",
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BackendConfig {
    backend: String,
}

/// The backend config.toml asks for (`backend = "tmux" | "zmx"`), tmux when
/// unset or unknown.
pub fn configured_backend() -> Backend {
    let text = fs::read_to_string(state::config_dir().join("config.toml"))
        .unwrap_or_default();
    parse_backend(&text)
}

fn parse_backend(text: &str) -> Backend {
    let cfg: BackendConfig = toml::from_str(text).unwrap_or_default();
    Backend::parse(&cfg.backend).unwrap_or(Backend::Tmux)
}

/// The backend new panes get: the configured one, unless it isn't
/// installed and the other one is. Shared by the GUI and `mux split`/
/// `new-tab` (which mint the new pane's name - and so its backend - before
/// the GUI creates it), so both sides always agree.
pub fn default_backend() -> Backend {
    pick_default(
        configured_backend(),
        crate::mesh::find_tmux().is_ok(),
        find_zmx().is_ok(),
    )
}

pub fn pick_default(want: Backend, tmux_ok: bool, zmx_ok: bool) -> Backend {
    match want {
        Backend::Zmx if !zmx_ok && tmux_ok => Backend::Tmux,
        Backend::Tmux if !tmux_ok && zmx_ok => Backend::Zmx,
        b => b,
    }
}

/// One row of `zmx list`.
#[derive(Clone, Debug, PartialEq)]
pub struct ZmxSession {
    pub name: String,
    /// The session's root process: the login shell zmx spawned.
    pub pid: Option<u32>,
    pub clients: u32,
    /// What the shell last reported over OSC 7 (or its start dir). Not live:
    /// a plain `cd` doesn't move it - use `proc_cwd` for that.
    pub cwd: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Zmx {
    bin: PathBuf,
    dir: PathBuf,
}

/// PATH is not guaranteed when launched outside a shell, so probe the usual
/// install locations before falling back to `which` (mirrors find_tmux).
pub fn find_zmx() -> Result<PathBuf> {
    let candidates = ["/opt/homebrew/bin/zmx", "/usr/local/bin/zmx"];
    let home = dirs::home_dir().map(|h| h.join(".local/bin/zmx"));
    candidates
        .iter()
        .map(PathBuf::from)
        .chain(home)
        .find(|p| p.is_file())
        .or_else(|| {
            let out = Command::new("which").arg("zmx").output().ok()?;
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (out.status.success() && !path.is_empty()).then(|| PathBuf::from(path))
        })
        .context("zmx not found - install it with `brew install neurosnap/tap/zmx`")
}

impl Zmx {
    pub fn discover() -> Result<Self> {
        Ok(Self {
            bin: find_zmx()?,
            dir: state::config_dir().join("zmx"),
        })
    }

    pub fn bin(&self) -> &PathBuf {
        &self.bin
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// Every invocation targets muxterm's own socket dir, and never inherits
    /// a session context: inside a zmx pane ZMX_SESSION is set, which turns
    /// `attach` into a switch and would make commands act on the caller.
    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.env("ZMX_DIR", &self.dir)
            .env_remove("ZMX_SESSION")
            .env_remove("ZMX_SESSION_PREFIX");
        c
    }

    fn run(&self, args: &[&str]) -> Option<Output> {
        self.cmd().args(args).output().ok()
    }

    /// The pane PTY's argv: `env K=V… zmx attach <session>`. zmx has no `-e`,
    /// so the pane environment rides on the client - a new session's daemon
    /// forks from it and its shell inherits everything, while a reattach
    /// ignores it (same as tmux's `-e`). The detach key is turned off: in
    /// muxterm, quitting the app *is* detaching, and ctrl+\ belongs to the
    /// program in the pane.
    pub fn attach_argv(&self, session: &str, env: &[(String, String)]) -> (String, Vec<String>) {
        let mut args: Vec<String> = vec![
            format!("ZMX_DIR={}", self.dir.display()),
            "ZMX_NO_DETACH_KEY=1".into(),
        ];
        args.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
        args.push(self.bin.display().to_string());
        args.push("attach".into());
        args.push(session.into());
        ("/usr/bin/env".into(), args)
    }

    pub fn list(&self) -> Vec<ZmxSession> {
        match self.run(&["list"]) {
            Some(out) if out.status.success() => parse_list(&String::from_utf8_lossy(&out.stdout)),
            _ => Vec::new(),
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.list().into_iter().map(|s| s.name).collect()
    }

    pub fn has(&self, session: &str) -> bool {
        self.list().iter().any(|s| s.name == session)
    }

    pub fn kill(&self, session: &str) {
        let _ = self.run(&["kill", session, "--force"]);
    }

    /// The last `lines` lines of the session's scrollback + screen, plain
    /// text (or with SGR colors when `ansi`), trailing blank rows dropped.
    pub fn history(&self, session: &str, lines: usize, ansi: bool) -> Option<String> {
        let mut args = vec!["history", session];
        if ansi {
            args.push("--vt");
        }
        let out = self.run(&args)?;
        if !out.status.success() {
            return None;
        }
        Some(tail_lines(&String::from_utf8_lossy(&out.stdout), lines))
    }

    /// Raw bytes into the session's PTY, exactly as if typed.
    pub fn send(&self, session: &str, text: &str) -> bool {
        self.run(&["send", session, text]).is_some_and(|o| o.status.success())
    }

    /// Text as one bracketed paste (so a multi-line message lands as a
    /// single input in shells and agent CLIs, like tmux's `paste-buffer -p`),
    /// optionally followed by Enter.
    pub fn paste(&self, session: &str, text: &str, enter: bool) -> bool {
        let mut data = format!("\x1b[200~{text}\x1b[201~");
        if enter {
            data.push('\r');
        }
        self.send(session, &data)
    }
}

/// `zmx list` rows: `  name=<n>\tpid=<p>\tclients=<c>\tcreated=<t>\tcwd=<url>\tcmd=<…>`.
fn parse_list(text: &str) -> Vec<ZmxSession> {
    text.lines()
        .filter_map(|line| {
            let mut s = ZmxSession { name: String::new(), pid: None, clients: 0, cwd: None };
            for field in line.trim().split('\t') {
                let Some((k, v)) = field.split_once('=') else { continue };
                match k {
                    "name" => s.name = v.to_string(),
                    "pid" => s.pid = v.parse().ok(),
                    "clients" => s.clients = v.parse().unwrap_or(0),
                    "cwd" => s.cwd = file_url_path(v),
                    _ => {},
                }
            }
            (!s.name.is_empty()).then_some(s)
        })
        .collect()
}

/// `file://host/some/path` -> `/some/path` (percent escapes decoded).
fn file_url_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let mut out = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&path[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok().filter(|p| !p.is_empty())
}

fn tail_lines(text: &str, lines: usize) -> String {
    let trimmed = text.trim_end();
    let all: Vec<&str> = trimmed.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

// ───────────── process inspection ─────────────
//
// tmux answers "what's running in the pane, and where" (#{pane_current_command},
// #{pane_current_path}) from the PTY's foreground process group. zmx doesn't
// expose that, but it does report the shell's pid, and the shell's controlling
// terminal knows its foreground group - so the same answer comes straight
// from the kernel.

/// The foreground job of the terminal `shell_pid` controls: (command name,
/// pid). Falls back to the shell itself when it's at its prompt.
pub fn proc_foreground(shell_pid: u32) -> Option<(String, u32)> {
    let info = bsd_info(shell_pid)?;
    let fg = if info.e_tpgid > 0 { info.e_tpgid } else { shell_pid };
    let name = bsd_info(fg).map(|i| comm(&i)).unwrap_or_else(|| comm(&info));
    Some((name, fg))
}

/// The current working directory of `pid`.
pub fn proc_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut vpi: libc::proc_vnodepathinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut vpi as *mut _ as *mut libc::c_void,
            size,
        );
        if n != size {
            return None;
        }
        let raw = std::slice::from_raw_parts(
            vpi.pvi_cdir.vip_path.as_ptr() as *const u8,
            std::mem::size_of_val(&vpi.pvi_cdir.vip_path),
        );
        let len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let path = String::from_utf8_lossy(&raw[..len]).into_owned();
        (!path.is_empty()).then(|| PathBuf::from(path))
    }
    #[cfg(not(target_os = "macos"))]
    {
        fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
}

#[cfg(target_os = "macos")]
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        (n == size).then_some(info)
    }
}

#[cfg(target_os = "macos")]
fn comm(info: &libc::proc_bsdinfo) -> String {
    // pbi_name holds the full (up to 32 byte) name; pbi_comm is truncated
    // to 16. Login shells show up as "-zsh", which is_shell already strips.
    let pick = if info.pbi_name[0] != 0 { &info.pbi_name[..] } else { &info.pbi_comm[..] };
    let bytes: Vec<u8> = pick.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(not(target_os = "macos"))]
pub fn proc_foreground(_shell_pid: u32) -> Option<(String, u32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_from_config() {
        assert_eq!(parse_backend(""), Backend::Tmux);
        assert_eq!(parse_backend("backend = \"zmx\""), Backend::Zmx);
        assert_eq!(parse_backend("backend = \"ZMX\"\ntheme = \"bbs\""), Backend::Zmx);
        assert_eq!(parse_backend("backend = \"screen\""), Backend::Tmux);
        assert_eq!(parse_backend("not toml ["), Backend::Tmux);
    }

    #[test]
    fn session_names_carry_their_backend() {
        assert_eq!(Backend::of_session("mux-1a2b3c4d"), Backend::Tmux);
        assert_eq!(Backend::of_session("mux-z1a2b3c4d"), Backend::Zmx);
        // Every legacy tmux name is <prefix><hex>, which never starts with z.
        for c in "0123456789abcdef".chars() {
            assert_eq!(Backend::of_session(&format!("mux-{c}0000000")), Backend::Tmux);
        }
    }

    #[test]
    fn default_falls_back_to_what_is_installed() {
        use Backend::*;
        assert_eq!(pick_default(Zmx, true, true), Zmx);
        assert_eq!(pick_default(Zmx, true, false), Tmux);
        assert_eq!(pick_default(Tmux, false, true), Zmx);
        assert_eq!(pick_default(Tmux, true, false), Tmux);
        // Neither installed: keep the ask; startup reports the real error.
        assert_eq!(pick_default(Zmx, false, false), Zmx);
    }

    #[test]
    fn parses_list_rows() {
        let text = "  name=mux-1a2b3c4d\tpid=78890\tclients=1\tcreated=1792215839\tcwd=file://brick-3.local/Users/me/My%20Dir\tcmd=/bin/zsh -l\n  name=other\tpid=x\tclients=0\n";
        let rows = parse_list(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "mux-1a2b3c4d");
        assert_eq!(rows[0].pid, Some(78890));
        assert_eq!(rows[0].clients, 1);
        assert_eq!(rows[0].cwd.as_deref(), Some("/Users/me/My Dir"));
        assert_eq!(rows[1].pid, None);
        assert_eq!(rows[1].cwd, None);
    }

    #[test]
    fn tails_history() {
        assert_eq!(tail_lines("a\nb\nc\n\n\n", 2), "b\nc");
        assert_eq!(tail_lines("a\nb", 10), "a\nb");
        assert_eq!(tail_lines("", 3), "");
    }

    #[test]
    fn attach_argv_carries_env() {
        let z = Zmx { bin: "/opt/homebrew/bin/zmx".into(), dir: "/Users/me/.muxterm/zmx".into() };
        let (shell, args) = z.attach_argv("mux-1", &[("MUXTERM".into(), "1".into())]);
        assert_eq!(shell, "/usr/bin/env");
        assert_eq!(
            args,
            [
                "ZMX_DIR=/Users/me/.muxterm/zmx",
                "ZMX_NO_DETACH_KEY=1",
                "MUXTERM=1",
                "/opt/homebrew/bin/zmx",
                "attach",
                "mux-1"
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inspects_own_process() {
        let me = std::process::id();
        assert_eq!(proc_cwd(me), std::env::current_dir().ok());
        assert!(proc_foreground(me).is_some());
    }
}
