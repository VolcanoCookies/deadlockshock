use std::ops::RangeInclusive;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use crate::deadlock_path::{self, Detection, DetectionError};
use egui::{Color32, TextEdit, Ui};
use pishock::{Credentials, Device, Error as PiShockError, PiShockClient};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CredentialState {
    Valid,
    Invalid,
    Testing,
    #[default]
    Unknown,
}

impl CredentialState {
    fn label(self) -> &'static str {
        match self {
            Self::Valid => "Connection valid",
            Self::Invalid => "Connection failed",
            Self::Testing => "Testing connection…",
            Self::Unknown => "Connection not tested",
        }
    }

    fn color(self) -> [f32; 4] {
        match self {
            Self::Valid => [0.30, 0.78, 0.42, 1.0],
            Self::Invalid => [0.92, 0.32, 0.28, 1.0],
            Self::Testing | Self::Unknown => [0.65, 0.65, 0.65, 1.0],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ShockMode {
    #[default]
    Interval,
    Fixed,
}

impl ShockMode {
    fn label(self) -> &'static str {
        match self {
            Self::Interval => "Interval",
            Self::Fixed => "Fixed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ListeningState {
    Listening,
    #[default]
    NotListening,
}

impl ListeningState {
    fn label(self) -> &'static str {
        match self {
            Self::Listening => "Server listening",
            Self::NotListening => "Server not listening",
        }
    }

    fn color(self) -> [f32; 4] {
        match self {
            Self::Listening => [0.30, 0.78, 0.42, 1.0],
            Self::NotListening => [0.65, 0.65, 0.65, 1.0],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LogDetectionStatus {
    Found,
    NotCreated,
    Failed(String),
}

impl LogDetectionStatus {
    fn label(&self) -> &str {
        match self {
            Self::Found => "Found Deadlock console.log.",
            Self::NotCreated => {
                "Deadlock is installed, but console.log has not been created. Add -condebug to Deadlock's Steam launch options, then launch the game."
            }
            Self::Failed(message) => message,
        }
    }

    fn color(&self) -> [f32; 4] {
        match self {
            Self::Found => [0.30, 0.78, 0.42, 1.0],
            Self::NotCreated => [0.92, 0.68, 0.22, 1.0],
            Self::Failed(_) => [0.92, 0.32, 0.28, 1.0],
        }
    }
}

type ConnectionResult = Result<Vec<Device>, PiShockError>;

const SENDER_NAME: &str = "deadlockshock-companion";

#[derive(Debug, Default)]
pub struct AppState {
    pub api_key: String,
    pub username: String,
    pub credential_state: CredentialState,
    pub devices: Vec<Device>,
    pub selected_device: Option<u64>,
    pub shock_mode: ShockMode,
    pub min_intensity: f32,
    pub max_intensity: f32,
    pub intensity: f32,
    pub min_duration: f32,
    pub max_duration: f32,
    pub duration: f32,
    pub log_path: String,
    pub listening_state: ListeningState,
    connection_error: Option<String>,
    connection_result: Option<Receiver<ConnectionResult>>,
    log_detection_status: Option<LogDetectionStatus>,
}

impl AppState {
    pub fn credentials_present(&self) -> bool {
        !self.api_key.trim().is_empty() && !self.username.trim().is_empty()
    }

    pub fn selected_device(&self) -> Option<&Device> {
        let selected_device = self.selected_device?;
        self.devices
            .iter()
            .find(|device| device.client_id == selected_device)
    }

    fn connection_in_progress(&self) -> bool {
        self.connection_result.is_some()
    }

    fn reset_connection(&mut self) {
        self.credential_state = CredentialState::Unknown;
        self.devices.clear();
        self.selected_device = None;
        self.connection_error = None;
    }

    fn start_connection_test(&mut self, context: egui::Context) {
        let credentials = Credentials::new(
            self.username.trim().to_owned(),
            self.api_key.trim().to_owned(),
        );
        let (sender, receiver) = mpsc::channel();

        self.credential_state = CredentialState::Testing;
        self.connection_error = None;
        self.connection_result = Some(receiver);

        thread::spawn(move || {
            let result = PiShockClient::connect(credentials, SENDER_NAME)
                .and_then(|client| client.list_devices());
            let _ = sender.send(result);
            context.request_repaint();
        });
    }

    fn poll_connection_test(&mut self) {
        let Some(receiver) = &self.connection_result else {
            return;
        };

        match receiver.try_recv() {
            Ok(result) => {
                self.connection_result = None;
                self.apply_connection_result(result);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.connection_result = None;
                self.apply_connection_result(Err(PiShockError::Transport));
            }
        }
    }

    fn apply_connection_result(&mut self, result: ConnectionResult) {
        match result {
            Ok(devices) => {
                self.selected_device = self
                    .selected_device
                    .filter(|selected| devices.iter().any(|device| device.client_id == *selected))
                    .or_else(|| devices.first().map(|device| device.client_id));
                self.devices = devices;
                self.credential_state = CredentialState::Valid;
                self.connection_error = None;
            }
            Err(error) => {
                self.devices.clear();
                self.selected_device = None;
                self.credential_state = CredentialState::Invalid;
                self.connection_error = Some(error.to_string());
            }
        }
    }

    fn auto_detect_log_path(&mut self) {
        self.apply_log_detection(deadlock_path::detect());
    }

    fn apply_log_detection(&mut self, result: Result<Detection, DetectionError>) {
        match result {
            Ok(Detection::Ready { path }) => {
                self.log_path = path.display().to_string();
                self.log_detection_status = Some(LogDetectionStatus::Found);
            }
            Ok(Detection::NotCreated { path }) => {
                self.log_path = path.display().to_string();
                self.log_detection_status = Some(LogDetectionStatus::NotCreated);
            }
            Err(error) => {
                self.log_detection_status = Some(LogDetectionStatus::Failed(format!(
                    "Auto-detect failed: {error}"
                )));
            }
        }
    }

    pub fn draw(&mut self, ui: &mut Ui) {
        self.poll_connection_test();

        ui.heading("Credentials");
        let mut credentials_changed = false;
        ui.add_enabled_ui(!self.connection_in_progress(), |ui| {
            credentials_changed |= text_input(ui, "API key", &mut self.api_key, true);
            credentials_changed |= text_input(ui, "Username", &mut self.username, false);
        });
        if credentials_changed {
            self.reset_connection();
        }

        let can_test = self.credentials_present() && !self.connection_in_progress();
        if ui
            .add_enabled(
                can_test,
                egui::Button::new("Test connection").min_size([ui.available_width(), 0.0].into()),
            )
            .clicked()
        {
            self.start_connection_test(ui.ctx().clone());
        }
        status_line(
            ui,
            self.credential_state.label(),
            self.credential_state.color(),
        );
        if let Some(error) = &self.connection_error {
            status_line(ui, error, CredentialState::Invalid.color());
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        ui.heading("Device");
        let device_selection_enabled = !self.devices.is_empty() && !self.connection_in_progress();
        let devices = &self.devices;
        let selected_name = self
            .selected_device
            .and_then(|selected| devices.iter().find(|device| device.client_id == selected))
            .map(|device| device.name.as_str())
            .unwrap_or_else(|| {
                if devices.is_empty() {
                    "No devices found"
                } else {
                    "Select a device"
                }
            });
        let selected_device = &mut self.selected_device;
        ui.add_enabled_ui(device_selection_enabled, |ui| {
            egui::ComboBox::from_id_salt("device")
                .selected_text(selected_name)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for device in devices {
                        ui.selectable_value(
                            selected_device,
                            Some(device.client_id),
                            device.name.as_str(),
                        );
                    }
                });
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        ui.heading("Shock mode");
        ui.horizontal(|ui| {
            ui.label("Mode");
            egui::ComboBox::from_id_salt("shock-mode")
                .selected_text(self.shock_mode.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.shock_mode, ShockMode::Interval, "Interval");
                    ui.selectable_value(&mut self.shock_mode, ShockMode::Fixed, "Fixed");
                });
        });
        ui.add_space(4.0);

        match self.shock_mode {
            ShockMode::Interval => {
                slider_input(
                    ui,
                    "Minimum intensity",
                    &mut self.min_intensity,
                    0.0..=100.0,
                    "",
                );
                slider_input(
                    ui,
                    "Maximum intensity",
                    &mut self.max_intensity,
                    0.0..=100.0,
                    "",
                );
                slider_input(
                    ui,
                    "Minimum duration",
                    &mut self.min_duration,
                    0.0..=3.0,
                    " s",
                );
                slider_input(
                    ui,
                    "Maximum duration",
                    &mut self.max_duration,
                    0.0..=3.0,
                    " s",
                );
            }
            ShockMode::Fixed => {
                slider_input(ui, "Intensity", &mut self.intensity, 0.0..=100.0, "");
                slider_input(ui, "Duration", &mut self.duration, 0.0..=3.0, " s");
            }
        }

        text_input(ui, "Log path", &mut self.log_path, false);
        if ui
            .add_sized(
                [ui.available_width(), 0.0],
                egui::Button::new("Auto-detect"),
            )
            .clicked()
        {
            self.auto_detect_log_path();
        }
        if let Some(status) = &self.log_detection_status {
            status_line(ui, status.label(), status.color());
        }
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        status_line(
            ui,
            self.listening_state.label(),
            self.listening_state.color(),
        );
    }
}

fn input_background() -> Color32 {
    Color32::from_rgb(38, 38, 42)
}
fn text_input(ui: &mut Ui, label: &str, value: &mut String, password: bool) -> bool {
    ui.label(label);
    ui.add(
        TextEdit::singleline(value)
            .password(password)
            .desired_width(f32::INFINITY)
            .background_color(input_background()),
    )
    .changed()
}
fn slider_input(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    suffix: &str,
) {
    ui.label(label);
    ui.scope(|ui| {
        let available_width = ui.available_width();
        ui.spacing_mut().slider_width = available_width * 0.8;

        let visuals = ui.visuals_mut();
        visuals.widgets.inactive.bg_fill = input_background();
        visuals.widgets.hovered.bg_fill = input_background();
        visuals.widgets.active.bg_fill = input_background();

        ui.add(egui::Slider::new(value, range).suffix(suffix));
    });
}

fn status_line(ui: &mut Ui, value: &str, color: [f32; 4]) {
    ui.colored_label(to_color(color), value);
}

fn to_color(color: [f32; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(
        (color[0].clamp(0.0, 1.0) * 255.0) as u8,
        (color[1].clamp(0.0, 1.0) * 255.0) as u8,
        (color[2].clamp(0.0, 1.0) * 255.0) as u8,
        (color[3].clamp(0.0, 1.0) * 255.0) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_require_two_non_whitespace_values() {
        let mut state = AppState::default();
        assert!(!state.credentials_present());

        state.api_key = "key".into();
        assert!(!state.credentials_present());

        state.username = "  ".into();
        assert!(!state.credentials_present());

        state.username = "user".into();
        assert!(state.credentials_present());
    }

    #[test]
    fn successful_detection_populates_path_and_status() {
        let path = std::path::PathBuf::from("/steam/Deadlock/game/citadel/console.log");
        let mut state = AppState::default();

        state.apply_log_detection(Ok(Detection::Ready { path: path.clone() }));

        assert_eq!(state.log_path, path.display().to_string());
        assert_eq!(state.log_detection_status, Some(LogDetectionStatus::Found));
    }

    #[test]
    fn missing_log_populates_expected_path_with_guidance() {
        let path = std::path::PathBuf::from("/steam/Deadlock/game/citadel/console.log");
        let mut state = AppState::default();

        state.apply_log_detection(Ok(Detection::NotCreated { path: path.clone() }));

        assert_eq!(state.log_path, path.display().to_string());
        let status = state.log_detection_status.expect("detection status");
        assert!(status.label().contains("-condebug"));
    }

    #[test]
    fn failed_detection_does_not_replace_manual_path() {
        let mut state = AppState {
            log_path: "/manual/console.log".to_owned(),
            ..AppState::default()
        };

        state.apply_log_detection(Err(DetectionError::DeadlockNotInstalled));

        assert_eq!(state.log_path, "/manual/console.log");
        assert!(matches!(
            state.log_detection_status,
            Some(LogDetectionStatus::Failed(_))
        ));
    }

    #[test]
    fn connection_result_populates_devices_and_keeps_the_selection() {
        let mut state = AppState {
            selected_device: Some(2),
            ..AppState::default()
        };

        state.apply_connection_result(Ok(vec![device(1, "Alpha"), device(2, "Beta")]));

        assert_eq!(state.credential_state, CredentialState::Valid);
        assert_eq!(state.devices.len(), 2);
        assert_eq!(state.selected_device, Some(2));
        assert_eq!(
            state.selected_device().map(|device| device.name.as_str()),
            Some("Beta")
        );
    }

    #[test]
    fn connection_result_selects_first_device_when_selection_is_unavailable() {
        let mut state = AppState {
            selected_device: Some(99),
            ..AppState::default()
        };

        state.apply_connection_result(Ok(vec![device(1, "Alpha"), device(2, "Beta")]));

        assert_eq!(state.selected_device, Some(1));
    }

    #[test]
    fn failed_connection_clears_stale_devices_and_selection() {
        let mut state = AppState {
            devices: vec![device(1, "Alpha")],
            selected_device: Some(1),
            ..AppState::default()
        };

        state.apply_connection_result(Err(PiShockError::AuthenticationRejected));

        assert_eq!(state.credential_state, CredentialState::Invalid);
        assert!(state.devices.is_empty());
        assert_eq!(state.selected_device, None);
        assert_eq!(
            state.connection_error.as_deref(),
            Some("PiShock authentication was rejected")
        );
    }

    #[test]
    fn both_shock_modes_render_draw_data() {
        for mode in [ShockMode::Interval, ShockMode::Fixed] {
            let context = egui::Context::default();
            let mut state = AppState {
                shock_mode: mode,
                ..AppState::default()
            };
            let output = context.run_ui(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    state.draw(ui);
                });
            });

            assert!(!output.shapes.is_empty());
        }
    }

    fn device(client_id: u64, name: &str) -> Device {
        Device {
            client_id,
            name: name.to_owned(),
            user_id: 42,
            username: "user".to_owned(),
            shockers: Vec::new(),
        }
    }
}
