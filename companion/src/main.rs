pub mod app;

use app::AppState;
use eframe::egui;

impl eframe::App for AppState {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Frame::NONE.inner_margin(8.0).show(ui, |ui| {
            if self.draw(ui) {
                println!("Connection requested for user {}", self.user_id.trim());
            }
        });
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Companion")
            .with_inner_size([380.0, 620.0])
            .with_min_inner_size([340.0, 540.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Companion",
        options,
        Box::new(|_creation_context| Ok(Box::new(AppState::default()))),
    )
}
