use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::layout::SplitAxis;

pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug)]
pub struct StateFile {
    pub version: u32,
    pub windows: Vec<WindowState>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WindowState {
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TabState {
    /// Stable tab identity (`mux-tab-<8hex>`), used to scope the agent
    /// mesh's per-tab context. Empty in pre-mesh state files; backfilled
    /// on load.
    #[serde(default)]
    pub id: String,
    pub tree: NodeState,
    pub focused_session: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum NodeState {
    Leaf {
        session: String,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<NodeState>,
        second: Box<NodeState>,
    },
}

impl NodeState {
    pub fn sessions(&self, out: &mut HashSet<String>) {
        match self {
            NodeState::Leaf { session } => {
                out.insert(session.clone());
            },
            NodeState::Split { first, second, .. } => {
                first.sessions(out);
                second.sessions(out);
            },
        }
    }

    /// In-order session names (pane order within the tab).
    pub fn session_list(&self, out: &mut Vec<String>) {
        match self {
            NodeState::Leaf { session } => out.push(session.clone()),
            NodeState::Split { first, second, .. } => {
                first.session_list(out);
                second.session_list(out);
            },
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .expect("no config directory on this platform")
        .join("muxterm")
}

pub fn state_path() -> PathBuf {
    config_dir().join("state.json")
}

pub enum LoadResult {
    Loaded(StateFile),
    FirstRun,
    /// Present but unreadable. The caller must skip session GC in this case.
    Corrupt,
}

/// Read-only load for external tools (the `mux` CLI): no `.bak` renaming,
/// no side effects. Returns None when missing or unreadable.
pub fn peek() -> Option<StateFile> {
    let text = fs::read_to_string(state_path()).ok()?;
    serde_json::from_str::<StateFile>(&text)
        .ok()
        .filter(|s| s.version == VERSION)
}

pub fn load() -> LoadResult {
    let path = state_path();
    match fs::read_to_string(&path) {
        Err(_) => LoadResult::FirstRun,
        Ok(text) => match serde_json::from_str::<StateFile>(&text) {
            Ok(s) if s.version == VERSION => LoadResult::Loaded(s),
            _ => {
                log::warn!("unreadable state file, moving it to state.json.bak");
                let _ = fs::rename(&path, path.with_extension("json.bak"));
                LoadResult::Corrupt
            },
        },
    }
}

pub fn save(state: &StateFile) -> anyhow::Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip() {
        let state = StateFile {
            version: VERSION,
            windows: vec![WindowState {
                active_tab: 1,
                tabs: vec![
                    TabState {
                        id: "mux-tab-1111".into(),
                        tree: NodeState::Leaf {
                            session: "mux-aaaa".into(),
                        },
                        focused_session: "mux-aaaa".into(),
                    },
                    TabState {
                        id: "mux-tab-2222".into(),
                        tree: NodeState::Split {
                            axis: SplitAxis::SideBySide,
                            ratio: 0.3,
                            first: Box::new(NodeState::Leaf {
                                session: "mux-bbbb".into(),
                            }),
                            second: Box::new(NodeState::Leaf {
                                session: "mux-cccc".into(),
                            }),
                        },
                        focused_session: "mux-cccc".into(),
                    },
                ],
            }],
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let back: StateFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, VERSION);
        assert_eq!(back.windows[0].active_tab, 1);
        assert_eq!(back.windows[0].tabs.len(), 2);

        let mut sessions = HashSet::new();
        for tab in &back.windows[0].tabs {
            tab.tree.sessions(&mut sessions);
        }
        assert_eq!(sessions.len(), 3);
        assert!(sessions.contains("mux-cccc"));
    }
}
