#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod app;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1100.0, 760.0]).with_min_inner_size([860.0, 600.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native("VhdxDock", options, Box::new(|cc| Ok(Box::new(app::DockApp::new(cc)))))
}
