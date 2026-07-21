#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_hooks;
mod ai_prompt;
mod app;
mod attention;
mod bg_jobs;
mod config;
mod git_status;
mod keys;
mod links;
mod pane;
mod pr_status;
mod scrollback;
mod search;
mod settings;
mod sidebar;
mod tabbar;
mod theme;
mod tmux;
mod workspace;
mod workspace_popup;

use muxterm::state;

fn main() -> eframe::Result {
    env_logger::init();

    // Relocate state from the old ~/Library/Application Support/muxterm to
    // ~/.muxterm before anything reads it (config, state, tmux.conf). No-op
    // once migrated.
    state::migrate_config_dir();

    // The spawned PTYs inherit this process env. When muxterm is launched
    // outside a shell (Finder/Dock) TERM is unset, and the tmux client
    // refuses to start without a usable one.
    std::env::set_var("TERM", "xterm-256color");
    std::env::set_var("COLORTERM", "truecolor");
    // Finder/Dock launches also carry no locale, which leaves the tmux
    // server and every pane program in the POSIX/ASCII locale. The tmux
    // client itself is covered by `-u` (tmux.rs), but shells and CLIs in
    // panes still need a UTF-8 LC_CTYPE; default one like iTerm2 does.
    // Never override a locale the user actually has.
    if std::env::var_os("LC_ALL").is_none()
        && std::env::var_os("LC_CTYPE").is_none()
        && std::env::var_os("LANG").is_none()
    {
        std::env::set_var("LANG", "en_US.UTF-8");
    }
    // A Finder/Dock launch also inherits a minimal PATH
    // (/usr/bin:/bin:/usr/sbin:/sbin) with no Homebrew or user bin dirs, so
    // everything muxterm shells out to breaks in surprising ways: `git
    // worktree add` can't find the `git-lfs` filter (checkout fails), `gh` for
    // PR status is missing, and so on. Adopt the interactive login shell's
    // PATH once - the same source agent-binary probing uses. Panes run their
    // own login shell, so this only affects muxterm's own subprocesses.
    if let Some(path) = login_shell_path() {
        std::env::set_var("PATH", path);
    }

    let tmux = tmux::TmuxCtl::discover(&state::config_dir());

    let options = eframe::NativeOptions {
        // Metal via wgpu; the default glow renderer runs on Apple's
        // deprecated OpenGL stack, with worse present latency.
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([400.0, 300.0])
            .with_title("muxterm")
            // iTerm-style compact chrome: no title bar, content under the
            // traffic lights; the tab bar doubles as the drag handle.
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false),
        ..Default::default()
    };

    eframe::run_native(
        "muxterm",
        options,
        Box::new(move |cc| {
            Ok(match tmux {
                Ok(tmux) => {
                    Box::new(app::App::new(cc, tmux)) as Box<dyn eframe::App>
                },
                Err(e) => Box::new(app::ErrorApp(format!("{e:#}"))),
            })
        }),
    )
}

/// The PATH from the user's interactive login shell (`zsh -ilc`), the same
/// probe `agent::binary_available` uses. A sentinel line isolates the value so
/// any `.zshrc` chatter printed to stdout can't corrupt it. None on any
/// failure - the caller then keeps the inherited (possibly minimal) PATH.
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let out = std::process::Command::new(shell)
        .args(["-ilc", "printf '\\n__MUX_PATH__=%s\\n' \"$PATH\""])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("__MUX_PATH__="))
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}
