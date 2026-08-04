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

fn init_logging() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,companion=info"),
    )
    .format_timestamp_millis()
    .format_module_path(true)
    .init();
}

fn main() -> eframe::Result {
    init_logging();
    log::info!(
        target: "companion",
        "process_start version={} os={} arch={}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Companion")
            .with_inner_size([380.0, 620.0])
            .with_min_inner_size([340.0, 540.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        "Companion",
        options,
        Box::new(|_creation_context| Ok(Box::new(CompanionApp::load()))),
    );
    match &result {
        Ok(()) => log::info!(target: "companion", "process_exit status=success"),
        Err(error) => log::error!(target: "companion", "eframe_launch_failed error={:?}", error),
    }
    result
}
