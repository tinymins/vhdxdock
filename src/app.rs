pub struct DockApp;
impl DockApp { pub fn new(_: &eframe::CreationContext<'_>) -> Self { Self } }
impl eframe::App for DockApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default().show(ctx, |ui| { ui.heading("VhdxDock"); ui.label("Development scaffold"); });
    }
}
