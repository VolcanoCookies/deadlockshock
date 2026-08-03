use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use crate::bridge_listener::{BridgeEvent, ConsoleLogListener, ListenerPhase, ListenerStatus};
use crate::deadlock_path::{self, Detection, DetectionError};
use egui::{Color32, TextEdit, Ui};
use pishock::{Credentials, Device, Error as PiShockError, WebSocketClient};

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

type ConnectionResult = Result<(WebSocketClient, Vec<Device>), PiShockError>;
type BeepResult = Result<(), PiShockError>;

const SENDER_NAME: &str = "deadlockshock-companion";
const TEST_BEEP_DURATION_SECONDS: u8 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
enum BeepStatus {
    Sending,
    Sent,
    Failed(String),
}

impl BeepStatus {
    fn label(&self) -> &str {
        match self {
            Self::Sending => "Sending beep…",
            Self::Sent => "Beep sent.",
            Self::Failed(message) => message,
        }
    }

    fn color(&self) -> [f32; 4] {
        match self {
            Self::Sending => [0.65, 0.65, 0.65, 1.0],
            Self::Sent => [0.30, 0.78, 0.42, 1.0],
            Self::Failed(_) => [0.92, 0.32, 0.28, 1.0],
        }
    }
}

#[derive(Default)]
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
    client: Option<Arc<WebSocketClient>>,
    connection_error: Option<String>,
    connection_result: Option<Receiver<ConnectionResult>>,
    beep_result: Option<Receiver<BeepResult>>,
    beep_status: Option<BeepStatus>,
    log_detection_status: Option<LogDetectionStatus>,
    bridge_listener: ConsoleLogListener,
    bridge_events: Option<Receiver<BridgeEvent>>,
    last_bridge_event: Option<BridgeEvent>,
    listener_action_error: Option<String>,
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
        self.client = None;
        self.devices.clear();
        self.selected_device = None;
        self.connection_error = None;
        self.beep_result = None;
        self.beep_status = None;
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
            let result = WebSocketClient::connect(credentials, SENDER_NAME).and_then(|client| {
                let devices = client.list_devices()?;
                Ok((client, devices))
            });
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
            Ok((client, devices)) => {
                self.client = Some(Arc::new(client));
                self.apply_devices(devices);
            }
            Err(error) => {
                self.client = None;
                self.beep_result = None;
                self.beep_status = None;
                self.devices.clear();
                self.selected_device = None;
                self.credential_state = CredentialState::Invalid;
                self.connection_error = Some(error.to_string());
            }
        }
    }

    fn apply_devices(&mut self, devices: Vec<Device>) {
        self.selected_device = self
            .selected_device
            .filter(|selected| devices.iter().any(|device| device.client_id == *selected))
            .or_else(|| devices.first().map(|device| device.client_id));
        self.devices = devices;
        self.credential_state = CredentialState::Valid;
        self.beep_status = None;
        self.connection_error = None;
    }

    fn beep_in_progress(&self) -> bool {
        self.beep_result.is_some()
    }

    fn start_beep(&mut self, context: egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(device) = self.selected_device().cloned() else {
            return;
        };
        let (sender, receiver) = mpsc::channel();

        self.beep_status = Some(BeepStatus::Sending);
        self.beep_result = Some(receiver);
        thread::spawn(move || {
            let result = client.beep_device(&device, TEST_BEEP_DURATION_SECONDS);
            let _ = sender.send(result);
            context.request_repaint();
        });
    }

    fn poll_beep(&mut self) {
        let Some(receiver) = &self.beep_result else {
            return;
        };

        match receiver.try_recv() {
            Ok(result) => {
                self.beep_result = None;
                self.apply_beep_result(result);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.beep_result = None;
                self.apply_beep_result(Err(PiShockError::Transport));
            }
        }
    }

    fn apply_beep_result(&mut self, result: BeepResult) {
        self.beep_status = Some(match result {
            Ok(()) => BeepStatus::Sent,
            Err(error) => BeepStatus::Failed(format!("Beep failed: {error}")),
        });
    }

    fn ensure_bridge_subscription(&mut self) {
        if self.bridge_events.is_none() {
            self.bridge_events = Some(self.bridge_listener.subscribe());
        }
    }

    fn start_log_listener(&mut self, path: PathBuf) -> std::io::Result<()> {
        self.ensure_bridge_subscription();
        self.bridge_listener.start(path)
    }

    fn poll_bridge_events(&mut self) {
        loop {
            let result = self.bridge_events.as_ref().map(Receiver::try_recv);
            match result {
                Some(Ok(event)) => self.last_bridge_event = Some(event),
                Some(Err(TryRecvError::Empty)) | None => break,
                Some(Err(TryRecvError::Disconnected)) => {
                    self.bridge_events = None;
                    break;
                }
            }
        }
    }

    fn start_listener_from_input(&mut self) {
        let path = self.log_path.trim();
        if path.is_empty() {
            self.listener_action_error =
                Some("Enter a console.log path before starting the listener.".to_owned());
            return;
        }
        let path = PathBuf::from(path);
        self.listener_action_error = self
            .start_log_listener(path)
            .err()
            .map(|error| format!("Could not start listener: {error}"));
    }

    fn auto_detect_log_path(&mut self) {
        self.listener_action_error = None;
        self.apply_log_detection(deadlock_path::detect());
    }

    fn apply_log_detection(&mut self, result: Result<Detection, DetectionError>) {
        match result {
            Ok(Detection::Ready { path }) => {
                self.log_path = path.display().to_string();
                self.log_detection_status = match self.start_log_listener(path) {
                    Ok(()) => Some(LogDetectionStatus::Found),
                    Err(error) => Some(LogDetectionStatus::Failed(format!(
                        "Deadlock console.log was found, but the listener could not start: {error}"
                    ))),
                };
            }
            Ok(Detection::NotCreated { path }) => {
                self.log_path = path.display().to_string();
                self.log_detection_status = match self.start_log_listener(path) {
                    Ok(()) => Some(LogDetectionStatus::NotCreated),
                    Err(error) => Some(LogDetectionStatus::Failed(format!(
                        "Deadlock was found, but the listener could not start: {error}"
                    ))),
                };
            }
            Err(error) => {
                self.log_detection_status = Some(LogDetectionStatus::Failed(format!(
                    "Auto-detect failed: {error}"
                )));
            }
        }
    }

    pub fn draw(&mut self, ui: &mut Ui) {
        self.poll_beep();
        self.poll_connection_test();
        self.poll_bridge_events();

        ui.heading("Credentials");
        let busy = self.connection_in_progress() || self.beep_in_progress();
        let mut credentials_changed = false;
        ui.add_enabled_ui(!busy, |ui| {
            credentials_changed |= text_input(ui, "API key", &mut self.api_key, true);
            credentials_changed |= text_input(ui, "Username", &mut self.username, false);
        });
        if credentials_changed {
            self.reset_connection();
        }

        let can_test = self.credentials_present() && !busy;
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
        let device_selection_enabled =
            !self.devices.is_empty() && !self.connection_in_progress() && !self.beep_in_progress();
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
        let mut selection_changed = false;
        let selected_device = &mut self.selected_device;
        ui.add_enabled_ui(device_selection_enabled, |ui| {
            egui::ComboBox::from_id_salt("device")
                .selected_text(selected_name)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for device in devices {
                        selection_changed |= ui
                            .selectable_value(
                                selected_device,
                                Some(device.client_id),
                                device.name.as_str(),
                            )
                            .changed();
                    }
                });
        });
        if selection_changed {
            self.beep_status = None;
        }
        let can_beep = self.selected_device.is_some()
            && self.client.is_some()
            && !self.connection_in_progress()
            && !self.beep_in_progress();
        if ui
            .add_enabled(
                can_beep,
                egui::Button::new("Send beep").min_size([ui.available_width(), 0.0].into()),
            )
            .clicked()
        {
            self.start_beep(ui.ctx().clone());
        }
        if let Some(status) = &self.beep_status {
            status_line(ui, status.label(), status.color());
        }

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
        ui.horizontal(|ui| {
            if ui
                .add_sized(
                    [ui.available_width() * 0.5, 0.0],
                    egui::Button::new("Auto-detect"),
                )
                .clicked()
            {
                self.auto_detect_log_path();
            }
            if ui
                .add_sized(
                    [ui.available_width(), 0.0],
                    egui::Button::new("Start/Restart listener"),
                )
                .clicked()
            {
                self.start_listener_from_input();
            }
        });
        if let Some(status) = &self.log_detection_status {
            status_line(ui, status.label(), status.color());
        }
        if let Some(error) = &self.listener_action_error {
            status_line(ui, error, [0.92, 0.32, 0.28, 1.0]);
        }
        let listener_status = self.bridge_listener.status();
        draw_listener_status(ui, &listener_status, self.last_bridge_event.as_ref());
        if listener_status.phase != ListenerPhase::Stopped {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn draw_listener_status(ui: &mut Ui, status: &ListenerStatus, last_event: Option<&BridgeEvent>) {
    let (phase_label, phase_color) = match status.phase {
        ListenerPhase::Stopped => ("Listener stopped.".to_owned(), [0.65, 0.65, 0.65, 1.0]),
        ListenerPhase::WaitingForFile => (
            "Listener waiting for console.log to be created.".to_owned(),
            [0.92, 0.68, 0.22, 1.0],
        ),
        ListenerPhase::Listening => (
            "Listener is monitoring console.log.".to_owned(),
            [0.30, 0.78, 0.42, 1.0],
        ),
        ListenerPhase::Failed => (
            format!(
                "Listener failed: {}",
                status.current_error.as_deref().unwrap_or("unknown error")
            ),
            [0.92, 0.32, 0.28, 1.0],
        ),
    };

    if let Some(path) = &status.configured_path {
        ui.label(format!("Configured listener path: {}", path.display()));
    }
    status_line(ui, &phase_label, phase_color);

    let activity = status
        .last_activity_at
        .map(|at| format!("Last log activity: {} ago.", format_duration(at.elapsed())))
        .unwrap_or_else(|| "Last log activity: none since listener start.".to_owned());
    ui.label(activity);

    let event = match (last_event, status.last_event_at) {
        (Some(event), Some(at)) => format!(
            "Last bridge event: {} ({} ago).",
            event.event_name(),
            format_duration(at.elapsed())
        ),
        _ => "Last bridge event: none since listener start.".to_owned(),
    };
    ui.label(event);
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() >= 60 {
        format!("{}m {}s", duration.as_secs() / 60, duration.as_secs() % 60)
    } else {
        format!("{:.1}s", duration.as_secs_f32())
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

        state.apply_devices(vec![device(1, "Alpha"), device(2, "Beta")]);

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

        state.apply_devices(vec![device(1, "Alpha"), device(2, "Beta")]);

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
    fn beep_result_reports_success_and_failure() {
        let mut state = AppState::default();

        state.apply_beep_result(Ok(()));
        assert_eq!(state.beep_status, Some(BeepStatus::Sent));

        state.apply_beep_result(Err(PiShockError::Transport));
        assert_eq!(
            state.beep_status,
            Some(BeepStatus::Failed(
                "Beep failed: PiShock transport error".to_owned()
            ))
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
