use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use crate::bridge_listener::{
    BridgeEvent, ConsoleLogListener, ListenerPhase, ListenerStatus, LocalPlayerDeath,
};
use crate::deadlock_path::{self, Detection, DetectionError};
use crate::provider::{
    ConnectedProvider, ProviderCredentials, ProviderError, ProviderKind, ProviderTarget, TargetId,
};
use egui::{Color32, TextEdit, Ui};
use rand::Rng;

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

type ConnectionResult = Result<(ConnectedProvider, Vec<ProviderTarget>), ProviderError>;
type SoundResult = Result<(), ProviderError>;
#[derive(Clone, Debug, Eq, PartialEq)]
enum SoundStatus {
    Sending,
    Sent,
    Failed(String),
}
impl SoundStatus {
    fn label(&self) -> &str {
        match self {
            Self::Sending => "Sending test sound…",
            Self::Sent => "Test sound sent.",
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedShock {
    pub intensity: u8,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeathIdentity {
    session_id: String,
    sequence: u64,
    client_time_ms: u64,
    detection: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ShockRequest {
    provider: ProviderKind,
    target: Option<ProviderTarget>,
    resolved: Option<ResolvedShock>,
    death: DeathIdentity,
}

struct ShockJob {
    client: Arc<ConnectedProvider>,
    request: ShockRequest,
}

struct ShockCompletion {
    request: ShockRequest,
    result: Result<(), ProviderError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ShockStatus {
    Sending(ShockRequest),
    Sent(ShockRequest),
    Failed {
        request: ShockRequest,
        error: String,
    },
    Skipped {
        request: ShockRequest,
        reason: String,
    },
}
impl ShockStatus {
    fn request(&self) -> &ShockRequest {
        match self {
            Self::Sending(request)
            | Self::Sent(request)
            | Self::Failed { request, .. }
            | Self::Skipped { request, .. } => request,
        }
    }
    fn label(&self) -> String {
        let request = self.request();
        let target = request
            .target
            .as_ref()
            .map(|target| target.name())
            .unwrap_or("no target");
        let resolved = request
            .resolved
            .map(|resolved| {
                format!(
                    "{}% for {:.1} s",
                    resolved.intensity,
                    resolved.duration_ms as f32 / 1_000.0
                )
            })
            .unwrap_or_else(|| "settings unavailable".to_owned());
        let death = format!("{}#{}", request.death.session_id, request.death.sequence);
        match self {
            Self::Sending(_) => format!(
                "{}: Sending shock to {target} at {resolved} (death {death})…",
                request.provider.label()
            ),
            Self::Sent(_) => format!(
                "{}: Shock sent to {target} at {resolved} (death {death}).",
                request.provider.label()
            ),
            Self::Failed { error, .. } => format!(
                "{}: Shock failed for {target} at {resolved} (death {death}): {error}",
                request.provider.label()
            ),
            Self::Skipped { reason, .. } => format!(
                "{}: Shock skipped for {target} at {resolved} (death {death}): {reason}",
                request.provider.label()
            ),
        }
    }
    fn color(&self) -> [f32; 4] {
        match self {
            Self::Sending(_) => [0.65, 0.65, 0.65, 1.0],
            Self::Sent(_) => [0.30, 0.78, 0.42, 1.0],
            Self::Failed { .. } => [0.92, 0.32, 0.28, 1.0],
            Self::Skipped { .. } => [0.92, 0.68, 0.22, 1.0],
        }
    }
}

fn spawn_shock_worker() -> (Sender<ShockJob>, Receiver<ShockCompletion>) {
    let (job_sender, job_receiver) = mpsc::channel::<ShockJob>();
    let (completion_sender, completion_receiver) = mpsc::channel::<ShockCompletion>();
    thread::spawn(move || {
        while let Ok(job) = job_receiver.recv() {
            let result = match (job.request.target.as_ref(), job.request.resolved) {
                (Some(target), Some(resolved)) => {
                    job.client
                        .shock(target, resolved.intensity, resolved.duration_ms)
                }
                _ => Err(ProviderError::NotConnected),
            };
            let _ = completion_sender.send(ShockCompletion {
                request: job.request,
                result,
            });
        }
    });
    (job_sender, completion_receiver)
}

pub struct AppState {
    pub provider: ProviderKind,
    pub username: String,
    pub api_key: String,
    pub openshock_token: String,
    pub credential_state: CredentialState,
    pub devices: Vec<ProviderTarget>,
    pub selected_device: Option<TargetId>,
    pub shock_mode: ShockMode,
    pub min_intensity: f32,
    pub max_intensity: f32,
    pub intensity: f32,
    pub min_duration: f32,
    pub max_duration: f32,
    pub duration: f32,
    pub log_path: String,
    client: Option<Arc<ConnectedProvider>>,
    connection_error: Option<String>,
    connection_result: Option<Receiver<ConnectionResult>>,
    sound_result: Option<Receiver<SoundResult>>,
    sound_status: Option<SoundStatus>,
    shock_sender: Sender<ShockJob>,
    shock_result: Receiver<ShockCompletion>,
    shock_in_flight: usize,
    shock_status: Option<ShockStatus>,
    log_detection_status: Option<LogDetectionStatus>,
    bridge_listener: ConsoleLogListener,
    bridge_events: Option<Receiver<BridgeEvent>>,
    last_bridge_event: Option<BridgeEvent>,
    last_death: Option<(String, u64)>,
    listener_action_error: Option<String>,
}

impl Default for AppState {
    fn default() -> Self {
        let (shock_sender, shock_result) = spawn_shock_worker();
        Self {
            provider: ProviderKind::default(),
            username: String::new(),
            api_key: String::new(),
            openshock_token: String::new(),
            credential_state: CredentialState::default(),
            devices: Vec::new(),
            selected_device: None,
            shock_mode: ShockMode::default(),
            min_intensity: 1.0,
            max_intensity: 1.0,
            intensity: 1.0,
            min_duration: 0.3,
            max_duration: 0.3,
            duration: 0.3,
            log_path: String::new(),
            client: None,
            connection_error: None,
            connection_result: None,
            sound_result: None,
            sound_status: None,
            shock_sender,
            shock_result,
            shock_in_flight: 0,
            shock_status: None,
            log_detection_status: None,
            bridge_listener: ConsoleLogListener::default(),
            bridge_events: None,
            last_bridge_event: None,
            last_death: None,
            listener_action_error: None,
        }
    }
}

impl AppState {
    pub fn credentials_present(&self) -> bool {
        match self.provider {
            ProviderKind::PiShock => {
                !self.username.trim().is_empty() && !self.api_key.trim().is_empty()
            }
            ProviderKind::OpenShock => !self.openshock_token.trim().is_empty(),
        }
    }
    fn provider_credentials(&self) -> ProviderCredentials {
        ProviderCredentials {
            pishock_username: self.username.clone(),
            pishock_api_key: self.api_key.clone(),
            openshock_token: self.openshock_token.clone(),
        }
    }
    pub fn selected_device(&self) -> Option<&ProviderTarget> {
        let selected = self.selected_device.as_ref()?;
        self.devices.iter().find(|device| device.id() == selected)
    }
    fn connection_in_progress(&self) -> bool {
        self.connection_result.is_some()
    }
    fn sound_in_progress(&self) -> bool {
        self.sound_result.is_some()
    }
    fn shock_in_progress(&self) -> bool {
        self.shock_in_flight != 0
    }
    fn reset_connection(&mut self) {
        if let Some(client) = self.client.take()
            && let Ok(client) = Arc::try_unwrap(client)
        {
            let _ = client.disconnect();
        }
        self.credential_state = CredentialState::Unknown;
        self.connection_result = None;
        self.devices.clear();
        self.selected_device = None;
        self.connection_error = None;
        self.sound_result = None;
        self.sound_status = None;
    }
    fn set_provider(&mut self, provider: ProviderKind) {
        if self.provider != provider
            && !self.connection_in_progress()
            && !self.sound_in_progress()
            && !self.shock_in_progress()
        {
            self.provider = provider;
            self.reset_connection();
        }
    }
    fn start_connection_test(&mut self, context: egui::Context) {
        let provider = self.provider;
        let credentials = self.provider_credentials();
        self.reset_connection();
        let (sender, receiver) = mpsc::channel();
        self.credential_state = CredentialState::Testing;
        self.connection_error = None;
        self.connection_result = Some(receiver);
        thread::spawn(move || {
            let result = ConnectedProvider::connect(provider, &credentials).and_then(|client| {
                let devices = client.list_targets()?;
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
                self.apply_connection_result(Err(ProviderError::NotConnected));
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
                self.sound_result = None;
                self.sound_status = None;
                self.devices.clear();
                self.selected_device = None;
                self.credential_state = CredentialState::Invalid;
                self.connection_error = Some(error.to_string());
            }
        }
    }
    fn apply_devices(&mut self, devices: Vec<ProviderTarget>) {
        self.selected_device = self
            .selected_device
            .take()
            .filter(|selected| devices.iter().any(|device| device.id() == selected))
            .or_else(|| devices.first().map(|device| device.id().clone()));
        self.devices = devices;
        self.credential_state = CredentialState::Valid;
        self.sound_status = None;
        self.connection_error = None;
    }
    fn start_sound(&mut self, context: egui::Context) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let Some(target) = self.selected_device().cloned() else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        self.sound_status = Some(SoundStatus::Sending);
        self.sound_result = Some(receiver);
        thread::spawn(move || {
            let result = client.test_sound(&target);
            let _ = sender.send(result);
            context.request_repaint();
        });
    }
    fn poll_sound(&mut self) {
        let Some(receiver) = &self.sound_result else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.sound_result = None;
                self.apply_sound_result(result);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.sound_result = None;
                self.apply_sound_result(Err(ProviderError::NotConnected));
            }
        }
    }
    fn apply_sound_result(&mut self, result: SoundResult) {
        self.sound_status = Some(match result {
            Ok(()) => SoundStatus::Sent,
            Err(error) => SoundStatus::Failed(format!("Test sound failed: {error}")),
        });
    }
    pub fn resolve_shock(&self) -> Option<ResolvedShock> {
        let mut rng = rand::rng();
        self.resolve_shock_with(&mut rng)
    }
    fn resolve_shock_with<R: Rng + ?Sized>(&self, rng: &mut R) -> Option<ResolvedShock> {
        let intensity = match self.shock_mode {
            ShockMode::Fixed => portable_intensity(self.intensity)?,
            ShockMode::Interval => {
                let min = portable_intensity(self.min_intensity)?;
                let max = portable_intensity(self.max_intensity)?;
                if min > max {
                    return None;
                }
                rng.random_range(min..=max)
            }
        };
        let duration_ms = match self.shock_mode {
            ShockMode::Fixed => portable_duration(self.duration)?,
            ShockMode::Interval => {
                let min = portable_duration(self.min_duration)?;
                let max = portable_duration(self.max_duration)?;
                if min > max {
                    return None;
                }
                rng.random_range(min..=max)
            }
        };
        Some(ResolvedShock {
            intensity,
            duration_ms,
        })
    }
    fn poll_shock(&mut self) {
        loop {
            match self.shock_result.try_recv() {
                Ok(completion) => {
                    self.shock_in_flight = self.shock_in_flight.saturating_sub(1);
                    self.shock_status = Some(match completion.result {
                        Ok(()) => ShockStatus::Sent(completion.request),
                        Err(error) => ShockStatus::Failed {
                            request: completion.request,
                            error: error.to_string(),
                        },
                    });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.shock_in_flight = 0;
                    break;
                }
            }
        }
    }
    fn death_is_new(&mut self, death: &LocalPlayerDeath) -> bool {
        if let Some((session_id, sequence)) = &self.last_death
            && session_id == &death.session_id
            && death.sequence <= *sequence
        {
            return false;
        }
        self.last_death = Some((death.session_id.clone(), death.sequence));
        true
    }
    fn queue_death_shock(&mut self, death: LocalPlayerDeath) {
        if !self.death_is_new(&death) {
            return;
        }
        let request = ShockRequest {
            provider: self.provider,
            target: self.selected_device().cloned(),
            resolved: self.resolve_shock(),
            death: DeathIdentity {
                session_id: death.session_id,
                sequence: death.sequence,
                client_time_ms: death.client_time_ms,
                detection: death.detection,
            },
        };
        let Some(client) = self.client.clone() else {
            self.shock_status = Some(ShockStatus::Skipped {
                request,
                reason: "provider is not connected".to_owned(),
            });
            return;
        };
        if request.target.is_none() {
            self.shock_status = Some(ShockStatus::Skipped {
                request,
                reason: "no target is selected".to_owned(),
            });
            return;
        }
        if request.resolved.is_none() {
            self.shock_status = Some(ShockStatus::Skipped {
                request,
                reason: "shock settings are invalid".to_owned(),
            });
            return;
        }
        self.shock_status = Some(ShockStatus::Sending(request.clone()));
        self.shock_in_flight = self.shock_in_flight.saturating_add(1);
        if self
            .shock_sender
            .send(ShockJob {
                client,
                request: request.clone(),
            })
            .is_err()
        {
            self.shock_in_flight = self.shock_in_flight.saturating_sub(1);
            self.shock_status = Some(ShockStatus::Failed {
                request,
                error: "shock worker is unavailable".to_owned(),
            });
        }
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
        while let Some(result) = self.bridge_events.as_ref().map(Receiver::try_recv) {
            match result {
                Ok(event) => {
                    self.last_bridge_event = Some(event.clone());
                    if let BridgeEvent::LocalPlayerDeath(death) = event {
                        self.queue_death_shock(death);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
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
        self.listener_action_error = self
            .start_log_listener(PathBuf::from(path))
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
                )))
            }
        }
    }

    pub fn draw(&mut self, ui: &mut Ui) {
        self.poll_sound();
        self.poll_connection_test();
        self.poll_shock();
        self.poll_bridge_events();
        let busy =
            self.connection_in_progress() || self.sound_in_progress() || self.shock_in_progress();
        ui.heading("Provider");
        let mut provider = self.provider;
        ui.add_enabled_ui(!busy, |ui| {
            egui::ComboBox::from_id_salt("provider")
                .selected_text(provider.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut provider, ProviderKind::PiShock, "PiShock");
                    ui.selectable_value(&mut provider, ProviderKind::OpenShock, "OpenShock");
                });
        });
        if provider != self.provider {
            self.set_provider(provider);
        }
        ui.add_space(4.0);
        ui.heading("Credentials");
        let mut credentials_changed = false;
        ui.add_enabled_ui(!busy, |ui| match self.provider {
            ProviderKind::PiShock => {
                credentials_changed |= text_input(ui, "API key", &mut self.api_key, true);
                credentials_changed |= text_input(ui, "Username", &mut self.username, false);
            }
            ProviderKind::OpenShock => {
                credentials_changed |=
                    text_input(ui, "OpenShock token", &mut self.openshock_token, true);
                ui.label("Token must have shockers.use permission and must not be paused.");
                ui.hyperlink_to(
                    "OpenShock token settings",
                    "https://next.openshock.app/settings/api-tokens",
                );
            }
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
        ui.heading("Device group");
        let selection_enabled = !self.devices.is_empty()
            && !self.connection_in_progress()
            && !self.sound_in_progress()
            && !self.shock_in_progress();
        let selected_name = self
            .selected_device()
            .map(|device| device.name().to_owned())
            .unwrap_or_else(|| {
                if self.devices.is_empty() {
                    "No device groups found".to_owned()
                } else {
                    "Select a device group".to_owned()
                }
            });
        let mut selection_changed = false;
        let selected_device = &mut self.selected_device;
        ui.add_enabled_ui(selection_enabled, |ui| {
            egui::ComboBox::from_id_salt("device")
                .selected_text(selected_name.as_str())
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for device in &self.devices {
                        selection_changed |= ui
                            .selectable_value(
                                selected_device,
                                Some(device.id().clone()),
                                device.name(),
                            )
                            .changed();
                    }
                });
        });
        if selection_changed {
            self.sound_status = None;
        }
        let can_sound = self.selected_device.is_some() && self.client.is_some() && !busy;
        if ui
            .add_enabled(
                can_sound,
                egui::Button::new("Send test sound").min_size([ui.available_width(), 0.0].into()),
            )
            .clicked()
        {
            self.start_sound(ui.ctx().clone());
        }
        if let Some(status) = &self.sound_status {
            status_line(ui, status.label(), status.color());
        }
        if let Some(status) = &self.shock_status {
            let label = status.label();
            status_line(ui, &label, status.color());
        }
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        ui.add_enabled_ui(!busy, |ui| {
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
                        1.0..=100.0,
                        1.0,
                        "%",
                    );
                    slider_input(
                        ui,
                        "Maximum intensity",
                        &mut self.max_intensity,
                        1.0..=100.0,
                        1.0,
                        "%",
                    );
                    slider_input(
                        ui,
                        "Minimum duration",
                        &mut self.min_duration,
                        0.3..=3.0,
                        0.1,
                        " s",
                    );
                    slider_input(
                        ui,
                        "Maximum duration",
                        &mut self.max_duration,
                        0.3..=3.0,
                        0.1,
                        " s",
                    );
                }
                ShockMode::Fixed => {
                    slider_input(ui, "Intensity", &mut self.intensity, 1.0..=100.0, 1.0, "%");
                    slider_input(ui, "Duration", &mut self.duration, 0.3..=3.0, 0.1, " s");
                }
            }
        });
        if self.min_intensity > self.max_intensity {
            self.max_intensity = self.min_intensity;
        }
        if self.min_duration > self.max_duration {
            self.max_duration = self.min_duration;
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
        if listener_status.phase != ListenerPhase::Stopped || self.shock_in_progress() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn portable_intensity(value: f32) -> Option<u8> {
    (value.is_finite() && value.fract() == 0.0 && (1.0..=100.0).contains(&value))
        .then_some(value as u8)
}
fn portable_duration(value: f32) -> Option<u64> {
    if !value.is_finite() || !(0.3..=3.0).contains(&value) {
        return None;
    }
    Some((value * 1_000.0).round() as u64)
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
fn slider_input<T: egui::emath::Numeric>(
    ui: &mut Ui,
    label: &str,
    value: &mut T,
    range: RangeInclusive<T>,
    step: f64,
    suffix: &str,
) {
    ui.label(label);
    ui.scope(|ui| {
        ui.spacing_mut().slider_width = ui.available_width() * 0.8;
        let visuals = ui.visuals_mut();
        visuals.widgets.inactive.bg_fill = input_background();
        visuals.widgets.hovered.bg_fill = input_background();
        visuals.widgets.active.bg_fill = input_background();
        ui.add(egui::Slider::new(value, range).step_by(step).suffix(suffix));
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
    fn credentials_require_provider_specific_values() {
        let mut state = AppState::default();
        assert!(!state.credentials_present());
        state.username = "user".into();
        state.api_key = "key".into();
        assert!(state.credentials_present());
        state.set_provider(ProviderKind::OpenShock);
        assert!(!state.credentials_present());
        state.openshock_token = "token".into();
        assert!(state.credentials_present());
    }
    #[test]
    fn switching_provider_resets_active_selection_but_retains_forms() {
        let mut state = AppState {
            username: "user".into(),
            api_key: "key".into(),
            devices: vec![ProviderTarget::new(TargetId::PiShock(1), "hub")],
            selected_device: Some(TargetId::PiShock(1)),
            credential_state: CredentialState::Valid,
            ..AppState::default()
        };
        state.set_provider(ProviderKind::OpenShock);
        assert_eq!(state.provider, ProviderKind::OpenShock);
        assert!(state.devices.is_empty());
        assert!(state.selected_device.is_none());
        assert_eq!(state.username, "user");
        assert_eq!(state.api_key, "key");
    }
    #[test]
    fn device_refresh_retains_valid_selection_and_selects_first_otherwise() {
        let mut state = AppState {
            selected_device: Some(TargetId::PiShock(2)),
            ..AppState::default()
        };
        state.apply_devices(vec![
            ProviderTarget::new(TargetId::PiShock(1), "Alpha"),
            ProviderTarget::new(TargetId::PiShock(2), "Beta"),
        ]);
        assert_eq!(state.selected_device, Some(TargetId::PiShock(2)));
        assert_eq!(
            state.selected_device().map(ProviderTarget::name),
            Some("Beta")
        );
        state.apply_devices(vec![ProviderTarget::new(TargetId::PiShock(3), "Gamma")]);
        assert_eq!(state.selected_device, Some(TargetId::PiShock(3)));
    }
    #[test]
    fn failed_connection_clears_stale_targets() {
        let mut state = AppState {
            devices: vec![ProviderTarget::new(TargetId::PiShock(1), "hub")],
            selected_device: Some(TargetId::PiShock(1)),
            ..AppState::default()
        };
        state.apply_connection_result(Err(ProviderError::NotConnected));
        assert!(state.devices.is_empty());
        assert!(state.selected_device.is_none());
        assert_eq!(state.credential_state, CredentialState::Invalid);
    }
    #[test]
    fn sound_status_reports_success_and_failure() {
        let mut state = AppState::default();
        state.apply_sound_result(Ok(()));
        assert_eq!(state.sound_status, Some(SoundStatus::Sent));
        state.apply_sound_result(Err(ProviderError::NotConnected));
        assert!(matches!(state.sound_status, Some(SoundStatus::Failed(_))));
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
                egui::CentralPanel::default().show(ctx, |ui| state.draw(ui));
            });
            assert!(!output.shapes.is_empty());
        }
    }
    #[test]
    fn openshock_form_renders() {
        let context = egui::Context::default();
        let mut state = AppState {
            provider: ProviderKind::OpenShock,
            ..AppState::default()
        };
        let output = context.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| state.draw(ui));
        });
        assert!(!output.shapes.is_empty());
    }
    #[test]
    fn successful_detection_populates_path_and_status() {
        let path = PathBuf::from("/steam/Deadlock/game/citadel/console.log");
        let mut state = AppState::default();
        state.apply_log_detection(Ok(Detection::Ready { path: path.clone() }));
        assert_eq!(state.log_path, path.display().to_string());
        assert_eq!(state.log_detection_status, Some(LogDetectionStatus::Found));
    }
    #[test]
    fn missing_log_populates_guidance() {
        let path = PathBuf::from("/steam/Deadlock/game/citadel/console.log");
        let mut state = AppState::default();
        state.apply_log_detection(Ok(Detection::NotCreated { path }));
        assert!(
            state
                .log_detection_status
                .expect("status")
                .label()
                .contains("-condebug")
        );
    }
    fn death(session_id: &str, sequence: u64) -> LocalPlayerDeath {
        LocalPlayerDeath {
            schema: 1,
            session_id: session_id.to_owned(),
            client_time_ms: sequence,
            sequence,
            detection: "test".to_owned(),
        }
    }
    #[test]
    fn shock_settings_use_portable_bounds_and_fixed_values() {
        let state = AppState {
            shock_mode: ShockMode::Fixed,
            intensity: 37.0,
            duration: 1.234,
            ..AppState::default()
        };
        assert_eq!(
            state.resolve_shock(),
            Some(ResolvedShock {
                intensity: 37,
                duration_ms: 1_234
            })
        );
        let interval = AppState::default();
        assert_eq!(
            interval.resolve_shock(),
            Some(ResolvedShock {
                intensity: 1,
                duration_ms: 300
            })
        );
        assert_eq!(
            (AppState {
                shock_mode: ShockMode::Fixed,
                intensity: 0.0,
                ..AppState::default()
            })
            .resolve_shock(),
            None
        );
        assert_eq!(
            (AppState {
                min_intensity: 90.0,
                max_intensity: 10.0,
                ..AppState::default()
            })
            .resolve_shock(),
            None
        );
    }
    #[test]
    fn death_deduplication_accepts_new_sessions_without_hook_ready() {
        let mut state = AppState::default();
        state.queue_death_shock(death("first", 4));
        let first = state.shock_status.clone();
        state.queue_death_shock(death("first", 4));
        assert_eq!(state.shock_status, first);
        state.queue_death_shock(death("first", 3));
        assert_eq!(state.shock_status, first);
        state.queue_death_shock(death("second", 1));
        assert_ne!(state.shock_status, first);
    }
    #[test]
    fn hook_ready_is_observed_but_never_triggers_a_shock() {
        let (sender, receiver) = mpsc::channel();
        let mut state = AppState {
            bridge_events: Some(receiver),
            ..AppState::default()
        };
        sender
            .send(BridgeEvent::HookReady(crate::bridge_listener::HookReady {
                schema: 1,
                session_id: "session".to_owned(),
                client_time_ms: 1,
                poll_interval_ms: 100,
            }))
            .unwrap();
        state.poll_bridge_events();
        assert!(state.last_death.is_none());
        assert!(state.shock_status.is_none());
    }
    #[test]
    fn missing_prerequisites_are_recorded_as_skipped() {
        let mut state = AppState::default();
        state.queue_death_shock(death("session", 1));
        assert!(matches!(
            state.shock_status,
            Some(ShockStatus::Skipped { .. })
        ));
        assert_eq!(state.shock_in_flight, 0);
    }
    #[test]
    fn shock_status_contains_provider_target_values_and_death_identity() {
        let request = ShockRequest {
            provider: ProviderKind::OpenShock,
            target: Some(ProviderTarget::new(
                TargetId::OpenShock("group".to_owned()),
                "group",
            )),
            resolved: Some(ResolvedShock {
                intensity: 20,
                duration_ms: 500,
            }),
            death: DeathIdentity {
                session_id: "session".to_owned(),
                sequence: 9,
                client_time_ms: 42,
                detection: "test".to_owned(),
            },
        };
        let status = ShockStatus::Sent(request);
        let label = status.label();
        assert!(label.contains("OpenShock"));
        assert!(label.contains("group"));
        assert!(label.contains("20% for 0.5 s"));
        assert!(label.contains("session#9"));
    }
    #[test]
    fn failed_detection_does_not_replace_manual_path() {
        let mut state = AppState {
            log_path: "/manual/console.log".into(),
            ..AppState::default()
        };
        state.apply_log_detection(Err(DetectionError::DeadlockNotInstalled));
        assert_eq!(state.log_path, "/manual/console.log");
    }
}
