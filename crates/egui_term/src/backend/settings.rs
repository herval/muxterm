use std::path::PathBuf;

const DEFAULT_SHELL: &str = "/bin/bash";

#[derive(Debug, Clone)]
pub struct BackendSettings {
    pub shell: String,
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
    /// muxterm patch P36: lines of local scrollback. Tiny when a
    /// multiplexer that owns scrollback (tmux copy-mode) sits between the
    /// widget and the app; deep when the widget is the scrollback (zmx).
    pub scrollback: usize,
}

impl Default for BackendSettings {
    fn default() -> Self {
        Self {
            shell: DEFAULT_SHELL.to_string(),
            args: vec![],
            working_directory: None,
            scrollback: 200,
        }
    }
}
