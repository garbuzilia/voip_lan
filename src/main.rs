mod audio;
mod gui;
mod network;
mod settings;
mod state;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([420.0, 780.0]),
        ..Default::default()
    };

    eframe::run_native(
        "voip_lan",
        options,
        Box::new(|_cc| Ok(Box::new(gui::VoipApp::new()))),
    )
}
