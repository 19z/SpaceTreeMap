#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1320.0, 850.0])
            .with_min_inner_size([800.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "SpaceTreeMap",
        options,
        Box::new(|cc| Ok(Box::new(space_tree_map::app::SpaceTreeMapApp::new(cc)))),
    )
}
