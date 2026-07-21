//! Agent-mesh coordination shared by the GUI and the `mux` CLI: the agent
//! registry, per-session inboxes, per-tab context files, and tab-membership
//! resolution. The tab is the isolation boundary: agents only see peers
//! whose sessions belong to the same muxterm tab (per state.json).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::layout::SplitAxis;
use crate::state::StateFile;

pub const SOCKET: &str = "muxterm";
pub const SESSION_PREFIX: &str = "mux-";
pub const TAB_ID_PREFIX: &str = "mux-tab-";

pub fn config_dir() -> PathBuf {
    crate::state::config_dir()
}

pub fn registry_path() -> PathBuf {
    config_dir().join("agents.json")
}

pub fn inbox_dir() -> PathBuf {
    config_dir().join("inbox")
}

pub fn inbox_path(session: &str) -> PathBuf {
    inbox_dir().join(format!("{session}.jsonl"))
}

pub fn flag_path(session: &str) -> PathBuf {
    inbox_dir().join(format!("{session}.flag"))
}

pub fn ctx_dir() -> PathBuf {
    config_dir().join("ctx")
}

pub fn ctx_path(tab_id: &str) -> PathBuf {
    ctx_dir().join(format!("{tab_id}.json"))
}

pub fn requests_dir() -> PathBuf {
    config_dir().join("requests")
}

/// Separate from `requests/`: the split drainer consumes-and-deletes every
/// json file in that directory, so notify requests must not share it.
pub fn notify_dir() -> PathBuf {
    config_dir().join("notify")
}

/// Its own directory for the same reason as `notify_dir`: each drainer eats
/// every json in its directory, so rename requests get their own spool.
pub fn rename_dir() -> PathBuf {
    config_dir().join("rename")
}

/// Its own directory for the same reason as the spools above. Like a split
/// (and unlike notify/rename) it carries an `.err` back-channel keyed by the
/// pre-agreed session name, so the CLI can poll for its specific refusal.
pub fn newtab_dir() -> PathBuf {
    config_dir().join("newtab")
}

/// One file per session (`<session>.json`), written by `mux agent-event`
/// (agent lifecycle hooks) and read by the GUI's poll tick to drive the
/// sidebar status dot. Unlike the spools above these are *state*, not a
/// queue: files are overwritten in place and removed when the agent ends.
pub fn agent_state_dir() -> PathBuf {
    config_dir().join("agent-state")
}

pub fn agent_state_path(session: &str) -> PathBuf {
    agent_state_dir().join(format!("{session}.json"))
}

pub fn ensure_dirs() {
    let _ = fs::create_dir_all(inbox_dir());
    let _ = fs::create_dir_all(ctx_dir());
    let _ = fs::create_dir_all(requests_dir());
    let _ = fs::create_dir_all(notify_dir());
    let _ = fs::create_dir_all(rename_dir());
    let _ = fs::create_dir_all(newtab_dir());
    let _ = fs::create_dir_all(agent_state_dir());
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn new_tab_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{TAB_ID_PREFIX}{}", &id[..8])
}

/// Session names are minted here (not in the GUI's tmux layer) because
/// `mux split` pre-agrees the name with the GUI: the CLI picks it, the GUI
/// creates it, and the CLI learns the outcome by polling tmux for it.
pub fn new_session_name() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{SESSION_PREFIX}{}", &id[..8])
}

/// PATH is not guaranteed when launched outside a shell, so probe the usual
/// install locations before falling back to `which`.
pub fn find_tmux() -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;
    let candidates =
        ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux", "/usr/bin/tmux"];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .or_else(|| {
            let out = Command::new("which").arg("tmux").output().ok()?;
            if !out.status.success() {
                return None;
            }
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!path.is_empty()).then(|| PathBuf::from(path))
        })
        .context(
            "tmux not found - install it with `brew install tmux`",
        )
}

/// Registered agent names: lowercase, digits, `-`/`_`, 1-32 chars.
pub fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {},
        _ => return false,
    }
    s.len() <= 32
        && chars.all(|c| {
            c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || c == '-'
                || c == '_'
        })
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub desc: Option<String>,
    pub joined_at: u64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Registry {
    pub version: u32,
    /// session name -> agent info
    pub agents: BTreeMap<String, AgentInfo>,
}

/// Missing or corrupt registries read as empty - never fatal, in either the
/// GUI or the CLI.
pub fn load_registry() -> Registry {
    match fs::read_to_string(registry_path()) {
        Err(_) => Registry::default(),
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
    }
}

pub fn save_registry(reg: &Registry) -> anyhow::Result<()> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(reg)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn registry_mtime() -> Option<SystemTime> {
    fs::metadata(registry_path()).and_then(|m| m.modified()).ok()
}

/// Which tab does `session` belong to? Sessions are unique across tabs, so
/// the answer is unambiguous. Returns (tab_id, member sessions in order).
pub fn tab_of_session(
    state: &StateFile,
    session: &str,
) -> Option<(String, Vec<String>)> {
    for window in &state.windows {
        for tab in &window.tabs {
            let mut members = Vec::new();
            tab.tree.session_list(&mut members);
            if members.iter().any(|s| s == session) {
                return Some((tab.id.clone(), members));
            }
        }
    }
    None
}

/// The workspace title of the tab with `tab_id`, if it carries a workspace
/// (bare shell tabs and pre-workspace state files don't). Lets the CLI show an
/// agent its own tab name - and tell whether it's still a `random_title`
/// codename (`state::is_codename`).
pub fn title_of_tab(state: &StateFile, tab_id: &str) -> Option<String> {
    state
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .find(|t| t.id == tab_id)
        .and_then(|t| t.workspace.as_ref())
        .map(|w| w.title.clone())
}

/// A pane asking the GUI to split it (written by `mux split`, drained by
/// the App's poll loop). Splits must go through the GUI - a session created
/// behind its back would never appear in the layout and the startup GC
/// would kill it. One file per request, named after the session-to-be.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SplitRequest {
    pub v: u32,
    /// Session of the pane asking to be split.
    pub from: String,
    /// Pre-agreed name for the new pane's session.
    pub session: String,
    pub axis: SplitAxis,
    #[serde(default)]
    pub cwd: Option<String>,
    pub ts: u64,
}

pub fn request_path(session: &str) -> PathBuf {
    requests_dir().join(format!("{session}.json"))
}

pub fn refusal_path(session: &str) -> PathBuf {
    requests_dir().join(format!("{session}.err"))
}

pub fn write_split_request(req: &SplitRequest) -> anyhow::Result<()> {
    fs::create_dir_all(requests_dir())?;
    let path = request_path(&req.session);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(req)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Drain pending split requests; each file is removed as it is read.
/// (`.json.tmp` staging files have extension "tmp" and are skipped, so a
/// half-written request is never consumed.)
pub fn take_split_requests() -> Vec<SplitRequest> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(requests_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let _ = fs::remove_file(&path);
        if let Ok(req) = serde_json::from_str::<SplitRequest>(&text) {
            out.push(req);
        }
    }
    out
}

/// The GUI's "no": the requester polls for this alongside the session.
pub fn write_split_refusal(session: &str, reason: &str) {
    let _ = fs::create_dir_all(requests_dir());
    let _ = fs::write(refusal_path(session), reason);
}

pub fn take_split_refusal(session: &str) -> Option<String> {
    let path = refusal_path(session);
    let reason = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    Some(reason)
}

/// Requests are ephemeral - the writer polls for a few seconds and gives
/// up. Anything still spooled when the GUI starts is from a dead writer.
pub fn clear_split_requests() {
    if let Ok(entries) = fs::read_dir(requests_dir()) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `mux notify`: a pane raising its hand for the muxterm UI (tab badge,
/// and a banner while the window is unfocused). Fire-and-forget: the GUI
/// drains the spool on its poll tick; nothing travels back.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NotifyRequest {
    pub v: u32,
    /// Session of the pane raising its hand.
    pub from: String,
    #[serde(default)]
    pub message: Option<String>,
    pub ts: u64,
}

pub fn write_notify_request(req: &NotifyRequest) -> anyhow::Result<()> {
    fs::create_dir_all(notify_dir())?;
    // ts is seconds; the pid keeps rapid-fire notifies from distinct
    // invocations from overwriting each other.
    let path = notify_dir().join(format!(
        "{}-{}-{}.json",
        req.from,
        req.ts,
        std::process::id()
    ));
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(req)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Drain pending notify requests, oldest first; each file is removed as
/// it is read (`.json.tmp` staging files are skipped, as with splits).
pub fn take_notify_requests() -> Vec<NotifyRequest> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(notify_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let _ = fs::remove_file(&path);
        if let Ok(req) = serde_json::from_str::<NotifyRequest>(&text) {
            out.push(req);
        }
    }
    out.sort_by_key(|req| req.ts);
    out
}

/// A raise spooled while the GUI was closed is stale by the next launch.
pub fn clear_notify_requests() {
    if let Ok(entries) = fs::read_dir(notify_dir()) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `mux rename`: a pane relabelling the workspace (tab) it lives in when the
/// objective drifts from what the workspace was created for. Display-only -
/// touches the workspace title/description, never the git branch or worktree.
/// Fire-and-forget like a notify: the GUI drains the spool on its poll tick,
/// resolves the caller's session to its tab, and updates that workspace.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RenameRequest {
    pub v: u32,
    /// Session of the pane asking to relabel its workspace.
    pub from: String,
    /// New display name; None leaves the title unchanged.
    #[serde(default)]
    pub title: Option<String>,
    /// New one-line description; None leaves it unchanged.
    #[serde(default)]
    pub description: Option<String>,
    pub ts: u64,
}

pub fn write_rename_request(req: &RenameRequest) -> anyhow::Result<()> {
    fs::create_dir_all(rename_dir())?;
    // Same collision-proof naming as notify: session + ts + pid.
    let path = rename_dir().join(format!(
        "{}-{}-{}.json",
        req.from,
        req.ts,
        std::process::id()
    ));
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(req)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Drain pending rename requests, oldest first; each file is removed as it is
/// read (`.json.tmp` staging files are skipped, as with splits/notifies).
pub fn take_rename_requests() -> Vec<RenameRequest> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(rename_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let _ = fs::remove_file(&path);
        if let Ok(req) = serde_json::from_str::<RenameRequest>(&text) {
            out.push(req);
        }
    }
    out.sort_by_key(|req| req.ts);
    out
}

/// A rename spooled while the GUI was closed is stale by the next launch.
pub fn clear_rename_requests() {
    if let Ok(entries) = fs::read_dir(rename_dir()) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `mux new-tab`: a pane asking the GUI to open a brand-new tab (not a split).
/// Same handshake as a split - the CLI pre-agrees the session name, the GUI
/// creates it, and the CLI learns the outcome by polling tmux for the session
/// (or this spool for a sibling `.err` refusal). One file per request, named
/// after the session-to-be, so the requester can poll for its specific `.err`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NewTabRequest {
    pub v: u32,
    /// Session of the pane asking to open a tab (authorizes the request).
    pub from: String,
    /// Pre-agreed name for the new tab's first (only) session.
    pub session: String,
    /// Where the new shell starts (tmux `new-session -c`); None uses the default.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Optional workspace title; None gives the tab a random codename.
    #[serde(default)]
    pub title: Option<String>,
    pub ts: u64,
}

pub fn newtab_request_path(session: &str) -> PathBuf {
    newtab_dir().join(format!("{session}.json"))
}

pub fn newtab_refusal_path(session: &str) -> PathBuf {
    newtab_dir().join(format!("{session}.err"))
}

pub fn write_newtab_request(req: &NewTabRequest) -> anyhow::Result<()> {
    fs::create_dir_all(newtab_dir())?;
    let path = newtab_request_path(&req.session);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(req)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Drain pending new-tab requests; each file is removed as it is read
/// (`.json.tmp` staging files are skipped, as with splits).
pub fn take_newtab_requests() -> Vec<NewTabRequest> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(newtab_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let _ = fs::remove_file(&path);
        if let Ok(req) = serde_json::from_str::<NewTabRequest>(&text) {
            out.push(req);
        }
    }
    out
}

/// The GUI's "no": the requester polls for this alongside the session.
pub fn write_newtab_refusal(session: &str, reason: &str) {
    let _ = fs::create_dir_all(newtab_dir());
    let _ = fs::write(newtab_refusal_path(session), reason);
}

pub fn take_newtab_refusal(session: &str) -> Option<String> {
    let path = newtab_refusal_path(session);
    let reason = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    Some(reason)
}

/// A new-tab request spooled while the GUI was closed is stale by next launch.
pub fn clear_newtab_requests() {
    if let Ok(entries) = fs::read_dir(newtab_dir()) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Deregister a session and drop its inbox artifacts (pane closed).
pub fn remove_session(session: &str) {
    let mut reg = load_registry();
    if reg.agents.remove(session).is_some() {
        let _ = save_registry(&reg);
    }
    let _ = fs::remove_file(inbox_path(session));
    let _ = fs::remove_file(flag_path(session));
    remove_agent_state(session);
}

/// A pane agent's lifecycle state, as reported by its own hooks through
/// `mux agent-event`: "working" (turn running), "idle" (waiting for the
/// user), "attention" (blocked on a permission/notification). The string is
/// deliberately open - an unknown state renders as idle rather than erroring.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentState {
    pub state: String,
    pub ts: u64,
}

/// Overwrite the session's state. Concurrent hook invocations (parallel
/// tool calls each firing PreToolUse) may race; a pid-unique temp name plus
/// rename keeps every observed file whole, and the racers all write the
/// same verdict anyway.
pub fn write_agent_state(session: &str, state: &str) -> anyhow::Result<()> {
    let _ = fs::create_dir_all(agent_state_dir());
    let path = agent_state_path(session);
    let tmp = agent_state_dir().join(format!(
        "{session}.json.{}",
        std::process::id()
    ));
    let record = AgentState { state: state.to_string(), ts: now() };
    fs::write(&tmp, serde_json::to_string(&record)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn remove_agent_state(session: &str) {
    let _ = fs::remove_file(agent_state_path(session));
}

/// Every session's reported state (file stem -> parsed record). Unreadable
/// entries are skipped - a torn or foreign file must not break the sweep.
pub fn read_agent_states() -> BTreeMap<String, AgentState> {
    let mut out = BTreeMap::new();
    let Ok(entries) = fs::read_dir(agent_state_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(session) =
            path.file_stem().and_then(|s| s.to_str()).map(str::to_string)
        else {
            continue;
        };
        if let Some(state) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<AgentState>(&text).ok())
        {
            out.insert(session, state);
        }
    }
    out
}

/// Drop registry entries / inboxes for dead sessions and context files for
/// dead tabs. Keyed strictly off live tmux sessions and live tab ids, so a
/// live agent can never be pruned. Returns human-readable removal notes.
pub fn prune(
    live_sessions: &HashSet<String>,
    live_tabs: &HashSet<String>,
) -> Vec<String> {
    let mut removed = Vec::new();

    let mut reg = load_registry();
    let dead: Vec<String> = reg
        .agents
        .keys()
        .filter(|s| !live_sessions.contains(*s))
        .cloned()
        .collect();
    for session in &dead {
        if let Some(info) = reg.agents.remove(session) {
            removed.push(format!("agent {} ({session})", info.name));
        }
    }
    if !dead.is_empty() {
        let _ = save_registry(&reg);
    }

    if let Ok(entries) = fs::read_dir(inbox_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let session = name
                .trim_end_matches(".jsonl")
                .trim_end_matches(".flag")
                .to_string();
            if !live_sessions.contains(&session) {
                let _ = fs::remove_file(entry.path());
                removed.push(format!("inbox file {name}"));
            }
        }
    }

    if let Ok(entries) = fs::read_dir(ctx_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let tab_id = name.trim_end_matches(".json").to_string();
            if !live_tabs.contains(&tab_id) {
                let _ = fs::remove_file(entry.path());
                removed.push(format!("context of dead tab {tab_id}"));
            }
        }
    }

    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SplitAxis;
    use crate::state::{NodeState, TabState, WindowState};

    #[test]
    fn name_validation() {
        assert!(valid_name("alice"));
        assert!(valid_name("bob-2"));
        assert!(valid_name("a_b"));
        assert!(!valid_name(""));
        assert!(!valid_name("Alice"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("has space"));
        assert!(!valid_name(&"x".repeat(33)));
    }

    #[test]
    fn registry_round_trip() {
        let mut reg = Registry::default();
        reg.agents.insert(
            "mux-aaaa".into(),
            AgentInfo {
                name: "alice".into(),
                role: Some("writer".into()),
                desc: None,
                joined_at: 123,
            },
        );
        let json = serde_json::to_string(&reg).unwrap();
        let back: Registry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agents["mux-aaaa"].name, "alice");
        assert_eq!(back.agents["mux-aaaa"].role.as_deref(), Some("writer"));
    }

    #[test]
    fn tab_membership_resolution() {
        let state = StateFile {
            version: 1,
            last_workspace_dir: None,
            sidebar_open: true,
            archived_collapsed: false,
            projects: Vec::new(),
            templates: Vec::new(),
            windows: vec![WindowState {
                active_tab: 0,
                tabs: vec![
                    TabState {
                        id: "mux-tab-1111".into(),
                        focused_session: "mux-a".into(),
                        workspace: None,
                        tree: NodeState::Split {
                            axis: SplitAxis::SideBySide,
                            ratio: 0.5,
                            first: Box::new(NodeState::Leaf {
                                session: "mux-a".into(),
                            }),
                            second: Box::new(NodeState::Leaf {
                                session: "mux-b".into(),
                            }),
                        },
                    },
                    TabState {
                        id: "mux-tab-2222".into(),
                        focused_session: "mux-c".into(),
                        workspace: None,
                        tree: NodeState::Leaf {
                            session: "mux-c".into(),
                        },
                    },
                ],
            }],
        };

        let (tab, members) = tab_of_session(&state, "mux-b").unwrap();
        assert_eq!(tab, "mux-tab-1111");
        assert_eq!(members, vec!["mux-a".to_string(), "mux-b".to_string()]);

        let (tab, members) = tab_of_session(&state, "mux-c").unwrap();
        assert_eq!(tab, "mux-tab-2222");
        assert_eq!(members, vec!["mux-c".to_string()]);

        assert!(tab_of_session(&state, "mux-nope").is_none());
    }

    #[test]
    fn tab_ids_have_expected_shape() {
        let id = new_tab_id();
        assert!(id.starts_with(TAB_ID_PREFIX));
        assert_eq!(id.len(), TAB_ID_PREFIX.len() + 8);
    }

    #[test]
    fn agent_state_serde_round_trips_and_tolerates_unknowns() {
        let s = AgentState { state: "working".into(), ts: 42 };
        let back: AgentState =
            serde_json::from_str(&serde_json::to_string(&s).unwrap())
                .unwrap();
        assert_eq!(back.state, "working");
        assert_eq!(back.ts, 42);
        // A newer mux may report states this build doesn't know; they must
        // parse (the GUI renders unknowns as idle).
        let odd: AgentState =
            serde_json::from_str(r#"{"state":"pondering","ts":1}"#).unwrap();
        assert_eq!(odd.state, "pondering");
    }

    #[test]
    fn session_names_have_expected_shape() {
        let name = new_session_name();
        assert!(name.starts_with(SESSION_PREFIX));
        assert_eq!(name.len(), SESSION_PREFIX.len() + 8);
        assert!(!name.starts_with(TAB_ID_PREFIX));
    }

    #[test]
    fn split_request_round_trip() {
        let req = SplitRequest {
            v: 1,
            from: "mux-aaaa".into(),
            session: "mux-bbbb".into(),
            axis: SplitAxis::Stacked,
            cwd: Some("/tmp".into()),
            ts: 42,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: SplitRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.from, "mux-aaaa");
        assert_eq!(back.session, "mux-bbbb");
        assert_eq!(back.axis, SplitAxis::Stacked);
        assert_eq!(back.cwd.as_deref(), Some("/tmp"));
        assert_eq!(back.ts, 42);
        // cwd is optional on the wire.
        let bare: SplitRequest = serde_json::from_str(
            r#"{"v":1,"from":"mux-a","session":"mux-b","axis":"SideBySide","ts":1}"#,
        )
        .unwrap();
        assert!(bare.cwd.is_none());
    }

    #[test]
    fn newtab_request_round_trip() {
        let req = NewTabRequest {
            v: 1,
            from: "mux-aaaa".into(),
            session: "mux-bbbb".into(),
            cwd: Some("/tmp".into()),
            title: Some("probe".into()),
            ts: 42,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: NewTabRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.from, "mux-aaaa");
        assert_eq!(back.session, "mux-bbbb");
        assert_eq!(back.cwd.as_deref(), Some("/tmp"));
        assert_eq!(back.title.as_deref(), Some("probe"));
        assert_eq!(back.ts, 42);
        // cwd and title are optional on the wire.
        let bare: NewTabRequest = serde_json::from_str(
            r#"{"v":1,"from":"mux-a","session":"mux-b","ts":1}"#,
        )
        .unwrap();
        assert!(bare.cwd.is_none());
        assert!(bare.title.is_none());
    }

    #[test]
    fn notify_request_round_trip() {
        let req = NotifyRequest {
            v: 1,
            from: "mux-aaaa".into(),
            message: Some("tests green".into()),
            ts: 42,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: NotifyRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.from, "mux-aaaa");
        assert_eq!(back.message.as_deref(), Some("tests green"));
        assert_eq!(back.ts, 42);
        // message is optional on the wire.
        let bare: NotifyRequest =
            serde_json::from_str(r#"{"v":1,"from":"mux-a","ts":1}"#).unwrap();
        assert!(bare.message.is_none());
    }

    #[test]
    fn rename_request_round_trip() {
        let req = RenameRequest {
            v: 1,
            from: "mux-aaaa".into(),
            title: Some("Fix auth".into()),
            description: Some("reworking the login flow".into()),
            ts: 42,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: RenameRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.from, "mux-aaaa");
        assert_eq!(back.title.as_deref(), Some("Fix auth"));
        assert_eq!(back.description.as_deref(), Some("reworking the login flow"));
        assert_eq!(back.ts, 42);
        // title and description are both optional on the wire.
        let bare: RenameRequest =
            serde_json::from_str(r#"{"v":1,"from":"mux-a","ts":1}"#).unwrap();
        assert!(bare.title.is_none());
        assert!(bare.description.is_none());
    }
}
