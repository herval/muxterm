#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai_prompt;
mod app;
mod attention;
mod config;
mod git_status;
mod keys;
mod links;
mod pane;
mod pr_status;
mod search;
mod settings;
mod tabbar;
mod theme;
mod tmux;

use muxterm::state;

fn main() -> eframe::Result {
    env_logger::init();

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
