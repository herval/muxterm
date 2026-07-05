#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai_prompt;
mod app;
mod config;
mod keys;
mod pane;
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
