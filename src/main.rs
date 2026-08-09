// Прячем консольное окно в release-сборке (в debug оно остаётся —
// пригодится, если что-то падает ещё до открытия окна).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod gui;
mod network;
mod settings;
mod state;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([380.0, gui::INITIAL_HEIGHT])
            .with_resizable(false),
        ..Default::default()
    };

    eframe::run_native(
        "voip_lan",
        options,
        Box::new(|_cc| Ok(Box::new(gui::VoipApp::new()))),
    )
}
