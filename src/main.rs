#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod keys;
mod layout;
mod pane;
mod state;
mod tabbar;
mod theme;
mod tmux;

fn main() -> eframe::Result {
    env_logger::init();

    // The spawned PTYs inherit this process env. When muxterm is launched
    // outside a shell (Finder/Dock) TERM is unset, and the tmux client
    // refuses to start without a usable one.
    std::env::set_var("TERM", "xterm-256color");
    std::env::set_var("COLORTERM", "truecolor");

    let tmux = tmux::TmuxCtl::discover(&state::config_dir());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([400.0, 300.0])
            .with_title("muxterm"),
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
