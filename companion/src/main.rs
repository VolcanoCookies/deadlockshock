pub mod action;
pub mod action_ui;
pub mod app;
pub mod bridge_listener;
pub mod deadlock_path;
pub mod persistence;
pub mod provider;
pub mod version_check;
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

fn app_icon() -> egui::IconData {
    let image = image::load_from_memory_with_format(
        include_bytes!("../assets/logo.png"),
        image::ImageFormat::Png,
    )
    .expect("embedded application icon must be a valid PNG image")
    .into_rgba8();

    egui::IconData {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
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
            .with_title("DeadlockShock Companion")
            .with_inner_size([520.0, 720.0])
            .with_min_inner_size([420.0, 600.0])
            .with_icon(app_icon()),
        ..Default::default()
    };

    let result = eframe::run_native(
        "DeadlockShock Companion",
        options,
        Box::new(|creation_context| {
            Ok(Box::new(CompanionApp::load_with_context(
                creation_context.egui_ctx.clone(),
            )))
        }),
    );
    match &result {
        Ok(()) => log::info!(target: "companion", "process_exit status=success"),
        Err(error) => log::error!(target: "companion", "eframe_launch_failed error={:?}", error),
    }
    result
}
