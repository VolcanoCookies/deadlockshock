pub mod app;
pub mod bridge_listener;
pub mod deadlock_path;
pub mod persistence;
pub mod provider;
use app::CompanionApp;
use eframe::egui;

impl eframe::App for CompanionApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.flush_pending();
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
        Box::new(|_creation_context| Ok(Box::new(CompanionApp::load()))),
    )
}
