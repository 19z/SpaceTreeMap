pub struct SpaceTreeMapApp;

impl SpaceTreeMapApp {
    pub fn new(_: &eframe::CreationContext<'_>) -> Self {
        Self
    }
}

impl eframe::App for SpaceTreeMapApp {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                ui.heading("SpaceTreeMap Preview");
                ui.label("MFT indexing and native file operations are currently available only on Windows.");
            });
        });
    }
}
