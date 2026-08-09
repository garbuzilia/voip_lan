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
        // glow (обычный OpenGL) — значительно легче по памяти, чем wgpu
        // (дефолтный бэкенд eframe, который тянет за собой поддержку сразу
        // DirectX12/Vulkan/Metal). Для такого простого интерфейса разница
        // в потреблении ресурсов ощутимая, а возможностей glow хватает с запасом.
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "voip_lan",
        options,
        Box::new(|_cc| Ok(Box::new(gui::VoipApp::new()))),
    )
}
