use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use crate::bridge_listener::{
    AbilityTrigger, BridgeEvent, ConsoleLogListener, ListenerPhase, ListenerStatus,
    LocalPlayerDeath, ModVersionObservation,
};
use crate::deadlock_path::{self, Detection, DetectionError};
use crate::persistence::{PersistedState, Persistence, default_state_path};
use crate::provider::{
    ConnectedProvider, ProviderCredentials, ProviderError, ProviderKind, ProviderTarget, TargetId,
};
use crate::version_check::{
    COMPANION_RELEASE_URL, LATEST_RELEASE_URL, MOD_RELEASE_URL, VersionCheckOwner,
    VersionCheckState, WarningSelection, app_version, select_warnings,
};
use egui::{Color32, TextEdit, Ui};
use rand::Rng;

pub const MIN_SHOCK_INTENSITY: f32 = 1.0;
pub const MAX_SHOCK_INTENSITY: f32 = 100.0;
pub const MIN_SHOCK_DURATION: f32 = 0.3;
pub const MAX_SHOCK_DURATION: f32 = 3.0;
pub(crate) const SHOCK_QUEUE_CAPACITY: usize = 10;
pub(crate) const MAX_SHOCK_QUEUE_AGE: Duration = Duration::from_secs(30);

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

#[derive(Clone, Debug, PartialEq)]
pub struct ShockIntervalSettings {
    pub minimum_intensity: f32,
    pub maximum_intensity: f32,
    pub minimum_duration_seconds: f32,
    pub maximum_duration_seconds: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShockFixedSettings {
    pub intensity: f32,
    pub duration_seconds: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShockSettings {
    pub mode: ShockMode,
    pub interval: ShockIntervalSettings,
    pub fixed: ShockFixedSettings,
}

impl Default for ShockSettings {
    fn default() -> Self {
        Self {
            mode: ShockMode::default(),
            interval: ShockIntervalSettings {
                minimum_intensity: MIN_SHOCK_INTENSITY,
                maximum_intensity: MIN_SHOCK_INTENSITY,
                minimum_duration_seconds: MIN_SHOCK_DURATION,
                maximum_duration_seconds: MIN_SHOCK_DURATION,
            },
            fixed: ShockFixedSettings {
                intensity: MIN_SHOCK_INTENSITY,
                duration_seconds: MIN_SHOCK_DURATION,
            },
        }
    }
}

impl ShockSettings {
    pub fn resolve(&self) -> Option<ResolvedShock> {
        let mut rng = rand::rng();
        self.resolve_with(&mut rng)
    }

    fn resolve_with<R: Rng + ?Sized>(&self, rng: &mut R) -> Option<ResolvedShock> {
        let intensity = match self.mode {
            ShockMode::Fixed => portable_intensity(self.fixed.intensity)?,
            ShockMode::Interval => {
                let minimum = portable_intensity(self.interval.minimum_intensity)?;
                let maximum = portable_intensity(self.interval.maximum_intensity)?;
                if minimum > maximum {
                    return None;
                }
                rng.random_range(minimum..=maximum)
            }
        };
        let duration_ms = match self.mode {
            ShockMode::Fixed => portable_duration(self.fixed.duration_seconds)?,
            ShockMode::Interval => {
                let minimum = portable_duration(self.interval.minimum_duration_seconds)?;
                let maximum = portable_duration(self.interval.maximum_duration_seconds)?;
                if minimum > maximum {
                    return None;
                }
                rng.random_range(minimum..=maximum)
            }
        };
        Some(ResolvedShock {
            intensity,
            duration_ms,
        })
    }

    fn summary(&self) -> String {
        match self.mode {
            ShockMode::Fixed => format!(
                "{:.0}% for {:.1} s",
                self.fixed.intensity, self.fixed.duration_seconds
            ),
            ShockMode::Interval => format!(
                "{:.0}–{:.0}% for {:.1}–{:.1} s",
                self.interval.minimum_intensity,
                self.interval.maximum_intensity,
                self.interval.minimum_duration_seconds,
                self.interval.maximum_duration_seconds
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TriggerSettings {
    pub enabled: bool,
    pub shock: ShockSettings,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AbilityFilter {
    All,
    Selected(BTreeSet<u32>),
}

impl Default for AbilityFilter {
    fn default() -> Self {
        Self::All
    }
}

impl AbilityFilter {
    pub fn accepts(&self, ability_slot: u32) -> bool {
        match self {
            Self::All => true,
            Self::Selected(slots) => slots.contains(&ability_slot),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AbilityTriggerSettings {
    pub trigger: TriggerSettings,
    pub ability_filter: AbilityFilter,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TriggerSettingsSet {
    pub death: TriggerSettings,
    pub ability_use: AbilityTriggerSettings,
    pub ability_cooldown_ready: AbilityTriggerSettings,
}

impl Default for TriggerSettingsSet {
    fn default() -> Self {
        let shock = ShockSettings::default();
        Self {
            death: TriggerSettings {
                enabled: true,
                shock: shock.clone(),
            },
            ability_use: AbilityTriggerSettings {
                trigger: TriggerSettings {
                    enabled: false,
                    shock: shock.clone(),
                },
                ability_filter: AbilityFilter::All,
            },
            ability_cooldown_ready: AbilityTriggerSettings {
                trigger: TriggerSettings {
                    enabled: false,
                    shock,
                },
                ability_filter: AbilityFilter::All,
            },
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
fn provider_error_kind(error: &ProviderError) -> &'static str {
    match error {
        ProviderError::PiShock(_) => "pishock",
        ProviderError::OpenShock(_) => "openshock",
        ProviderError::TargetProviderMismatch => "target_provider_mismatch",
        ProviderError::NotConnected => "not_connected",
    }
}
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TriggerKind {
    Death,
    AbilityUse,
    AbilityCooldownReady,
}

impl TriggerKind {
    fn label(self) -> &'static str {
        match self {
            Self::Death => "death",
            Self::AbilityUse => "ability use",
            Self::AbilityCooldownReady => "ability cooldown ready",
        }
    }
}

impl TriggerSettingsSet {
    fn get(&self, kind: TriggerKind) -> &TriggerSettings {
        match kind {
            TriggerKind::Death => &self.death,
            TriggerKind::AbilityUse => &self.ability_use.trigger,
            TriggerKind::AbilityCooldownReady => &self.ability_cooldown_ready.trigger,
        }
    }

    fn get_mut(&mut self, kind: TriggerKind) -> &mut TriggerSettings {
        match kind {
            TriggerKind::Death => &mut self.death,
            TriggerKind::AbilityUse => &mut self.ability_use.trigger,
            TriggerKind::AbilityCooldownReady => &mut self.ability_cooldown_ready.trigger,
        }
    }

    fn ability_filter(&self, kind: TriggerKind) -> Option<&AbilityFilter> {
        match kind {
            TriggerKind::Death => None,
            TriggerKind::AbilityUse => Some(&self.ability_use.ability_filter),
            TriggerKind::AbilityCooldownReady => Some(&self.ability_cooldown_ready.ability_filter),
        }
    }

    fn ability_filter_mut(&mut self, kind: TriggerKind) -> Option<&mut AbilityFilter> {
        match kind {
            TriggerKind::Death => None,
            TriggerKind::AbilityUse => Some(&mut self.ability_use.ability_filter),
            TriggerKind::AbilityCooldownReady => {
                Some(&mut self.ability_cooldown_ready.ability_filter)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AppSection {
    #[default]
    Setup,
    Effects,
    GameConnection,
}

impl AppSection {
    fn label(self) -> &'static str {
        match self {
            Self::Setup => "Setup",
            Self::Effects => "Effects",
            Self::GameConnection => "Game connection",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TriggerIdentity {
    kind: TriggerKind,
    session_id: String,
    sequence: u64,
    client_time_ms: u64,
    detection: String,
    ability_slot: Option<u32>,
    ability_name: Option<String>,
    charges_before: Option<u64>,
    charges_after: Option<u64>,
}

impl TriggerIdentity {
    fn from_death(death: LocalPlayerDeath) -> Self {
        Self {
            kind: TriggerKind::Death,
            session_id: death.session_id,
            sequence: death.sequence,
            client_time_ms: death.client_time_ms,
            detection: death.detection,
            ability_slot: None,
            ability_name: None,
            charges_before: None,
            charges_after: None,
        }
    }

    fn from_ability(kind: TriggerKind, ability: AbilityTrigger) -> Self {
        Self {
            kind,
            session_id: ability.session_id,
            sequence: ability.sequence,
            client_time_ms: ability.client_time_ms,
            detection: ability.detection,
            ability_slot: Some(ability.ability_slot),
            ability_name: ability.ability_name,
            charges_before: ability.charges_before,
            charges_after: ability.charges_after,
        }
    }

    fn status_description(&self) -> String {
        if self.kind == TriggerKind::Death {
            return format!("death {}#{}", self.session_id, self.sequence);
        }
        let name = self
            .ability_name
            .as_deref()
            .map(|name| format!(" ({name})"))
            .unwrap_or_default();
        let charges = match (self.charges_before, self.charges_after) {
            (Some(before), Some(after)) => format!(", charges {before}→{after}"),
            _ => String::new(),
        };
        format!(
            "{} slot {}{name}, detection {}{charges}, {}#{}",
            self.kind.label(),
            self.ability_slot.unwrap_or_default(),
            self.detection,
            self.session_id,
            self.sequence
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ShockRequest {
    provider: ProviderKind,
    target: Option<ProviderTarget>,
    resolved: Option<ResolvedShock>,
    trigger: TriggerIdentity,
}

struct ShockJob {
    client: Arc<ConnectedProvider>,
    request: ShockRequest,
    queued_at: Instant,
}

#[derive(Debug)]
enum ShockCompletionResult {
    Completed(Result<(), ProviderError>),
    Skipped { reason: &'static str },
}

struct ShockCompletion {
    request: ShockRequest,
    result: ShockCompletionResult,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShockEnqueueResult {
    Accepted,
    Full,
    Disconnected,
}

fn shock_job_expired_at(queued_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(queued_at) >= MAX_SHOCK_QUEUE_AGE
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
        let trigger = request.trigger.status_description();
        match self {
            Self::Sending(_) => format!(
                "{}: Sending shock to {target} at {resolved} ({trigger})…",
                request.provider.label()
            ),
            Self::Sent(_) => format!(
                "{}: Shock sent to {target} at {resolved} ({trigger}).",
                request.provider.label()
            ),
            Self::Failed { error, .. } => format!(
                "{}: Shock failed for {target} at {resolved} ({trigger}): {error}",
                request.provider.label()
            ),
            Self::Skipped { reason, .. } => format!(
                "{}: Shock skipped for {target} at {resolved} ({trigger}): {reason}",
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

fn spawn_shock_worker() -> (SyncSender<ShockJob>, Receiver<ShockCompletion>) {
    let (job_sender, job_receiver) = mpsc::sync_channel::<ShockJob>(SHOCK_QUEUE_CAPACITY);
    let (completion_sender, completion_receiver) = mpsc::channel::<ShockCompletion>();
    thread::spawn(move || {
        while let Ok(job) = job_receiver.recv() {
            let result = if shock_job_expired_at(job.queued_at, Instant::now()) {
                ShockCompletionResult::Skipped { reason: "expired" }
            } else {
                ShockCompletionResult::Completed(
                    match (job.request.target.as_ref(), job.request.resolved) {
                        (Some(target), Some(resolved)) => {
                            job.client
                                .shock(target, resolved.intensity, resolved.duration_ms)
                        }
                        _ => Err(ProviderError::NotConnected),
                    },
                )
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
    pub preferred_target: Option<TargetId>,
    pub triggers: TriggerSettingsSet,
    pub log_path: String,
    client: Option<Arc<ConnectedProvider>>,
    connection_error: Option<String>,
    connection_result: Option<Receiver<ConnectionResult>>,
    sound_result: Option<Receiver<SoundResult>>,
    sound_status: Option<SoundStatus>,
    shock_sender: SyncSender<ShockJob>,
    shock_result: Receiver<ShockCompletion>,
    shock_in_flight: usize,
    shock_status: Option<ShockStatus>,
    log_detection_status: Option<LogDetectionStatus>,
    bridge_listener: ConsoleLogListener,
    bridge_events: Option<Receiver<BridgeEvent>>,
    last_bridge_event: Option<BridgeEvent>,
    last_sequence: Option<(String, u64)>,
    ability_catalog: BTreeMap<u32, Option<String>>,
    listener_action_error: Option<String>,
    selected_section: AppSection,
    selected_effect: TriggerKind,
    copy_source: TriggerKind,
    copy_feedback: Option<String>,
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
            preferred_target: None,
            triggers: TriggerSettingsSet::default(),
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
            last_sequence: None,
            ability_catalog: BTreeMap::new(),
            listener_action_error: None,
            selected_section: AppSection::default(),
            selected_effect: TriggerKind::Death,
            copy_source: TriggerKind::AbilityUse,
            copy_feedback: None,
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
    pub(crate) fn is_busy(&self) -> bool {
        self.connection_in_progress() || self.sound_in_progress() || self.shock_in_progress()
    }
    pub(crate) fn reset_saved_state(&mut self) -> bool {
        if self.is_busy() {
            log::warn!(target: "companion::app", "settings_reset_skipped reason=busy");
            return false;
        }

        self.bridge_listener.stop();
        self.reset_connection();
        self.provider = ProviderKind::default();
        self.username.clear();
        self.api_key.clear();
        self.openshock_token.clear();
        self.triggers = TriggerSettingsSet::default();
        self.log_path.clear();
        self.log_detection_status = None;
        self.bridge_listener = ConsoleLogListener::default();
        self.bridge_events = None;
        self.last_bridge_event = None;
        self.last_sequence = None;
        self.ability_catalog.clear();
        self.listener_action_error = None;
        self.copy_feedback = None;
        self.shock_status = None;
        self.shock_in_flight = 0;
        let (shock_sender, shock_result) = spawn_shock_worker();
        self.shock_sender = shock_sender;
        self.shock_result = shock_result;
        log::info!(target: "companion::app", "settings_reset_applied provider={}", self.provider.label());
        true
    }
    #[cfg(test)]
    pub(crate) fn listener_is_running(&self) -> bool {
        self.bridge_listener.status().phase != ListenerPhase::Stopped
    }
    #[cfg(test)]
    pub(crate) fn runtime_trigger_and_shock_state_is_clear(&self) -> bool {
        self.bridge_events.is_none()
            && self.last_bridge_event.is_none()
            && self.last_sequence.is_none()
            && self.ability_catalog.is_empty()
            && self.shock_status.is_none()
            && self.shock_in_flight == 0
    }

    fn reset_connection(&mut self) {
        if let Some(client) = self.client.take() {
            let provider = client.kind().label();
            match Arc::try_unwrap(client) {
                Ok(client) => match client.disconnect() {
                    Ok(()) => log::info!(
                        target: "companion::app",
                        "provider_disconnected provider={provider} outcome=success"
                    ),
                    Err(error) => log::warn!(
                        target: "companion::app",
                        "provider_disconnected provider={provider} outcome=failed error_kind={}",
                        provider_error_kind(&error)
                    ),
                },
                Err(_) => log::debug!(
                    target: "companion::app",
                    "provider_disconnect_skipped provider={provider} reason=shared_client"
                ),
            }
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
        if self.provider == provider {
            return;
        }
        if self.connection_in_progress() || self.sound_in_progress() || self.shock_in_progress() {
            log::debug!(
                target: "companion::app",
                "provider_change_skipped from={} to={} reason=busy",
                self.provider.label(),
                provider.label()
            );
            return;
        }
        let previous = self.provider;
        self.provider = provider;
        self.preferred_target = None;
        self.reset_connection();
        log::info!(
            target: "companion::app",
            "provider_changed from={} to={}",
            previous.label(),
            provider.label()
        );
    }
    fn start_connection_test(&mut self, context: egui::Context) {
        let provider = self.provider;
        let credentials = self.provider_credentials();
        log::info!(
            target: "companion::app",
            "connection_test_started provider={}",
            provider.label()
        );
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
                log::error!(
                    target: "companion::app",
                    "connection_worker_failed provider={} reason=channel_closed",
                    self.provider.label()
                );
                self.connection_result = None;
                self.apply_connection_error(ProviderError::NotConnected);
            }
        }
    }

    fn apply_connection_result(&mut self, result: ConnectionResult) {
        match result {
            Ok((client, devices)) => {
                let provider = client.kind();
                log::info!(
                    target: "companion::app",
                    "connection_test_succeeded provider={} targets={}",
                    provider.label(),
                    devices.len()
                );
                self.client = Some(Arc::new(client));
                self.apply_devices(devices);
            }
            Err(error) => {
                log::warn!(
                    target: "companion::app",
                    "connection_test_failed provider={} error_kind={}",
                    self.provider.label(),
                    provider_error_kind(&error)
                );
                self.apply_connection_error(error);
            }
        }
    }
    fn apply_connection_error(&mut self, error: ProviderError) {
        self.client = None;
        self.sound_result = None;
        self.sound_status = None;
        self.devices.clear();
        self.selected_device = None;
        self.credential_state = CredentialState::Invalid;
        self.connection_error = Some(error.to_string());
    }
    fn apply_devices(&mut self, devices: Vec<ProviderTarget>) {
        let selected = self
            .preferred_target
            .as_ref()
            .filter(|preferred| devices.iter().any(|device| device.id() == *preferred))
            .cloned()
            .or_else(|| devices.first().map(|device| device.id().clone()));
        self.preferred_target = selected.clone();
        self.selected_device = selected;
        self.devices = devices;
        self.credential_state = CredentialState::Valid;
        self.sound_status = None;
        self.connection_error = None;
    }
    fn select_device(&mut self, target: TargetId) -> bool {
        if !self.devices.iter().any(|device| device.id() == &target) {
            return false;
        }
        self.selected_device = Some(target.clone());
        self.preferred_target = Some(target);
        self.sound_status = None;
        true
    }

    fn start_sound(&mut self, context: egui::Context) {
        let Some(client) = self.client.clone() else {
            log::warn!(target: "companion::app", "test_sound_skipped reason=not_connected");
            return;
        };
        let Some(target) = self.selected_device().cloned() else {
            log::warn!(target: "companion::app", "test_sound_skipped reason=no_target");
            return;
        };
        log::info!(
            target: "companion::app",
            "test_sound_started provider={} target={:?}",
            client.kind().label(),
            target.id()
        );
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
                log::error!(
                    target: "companion::app",
                    "test_sound_worker_failed reason=channel_closed"
                );
                self.sound_result = None;
                self.apply_sound_error(ProviderError::NotConnected);
            }
        }
    }

    fn apply_sound_result(&mut self, result: SoundResult) {
        match result {
            Ok(()) => {
                log::info!(target: "companion::app", "test_sound_succeeded");
                self.sound_status = Some(SoundStatus::Sent);
            }
            Err(error) => {
                log::warn!(
                    target: "companion::app",
                    "test_sound_failed error_kind={}",
                    provider_error_kind(&error)
                );
                self.apply_sound_error(error);
            }
        }
    }
    fn apply_sound_error(&mut self, error: ProviderError) {
        self.sound_status = Some(SoundStatus::Failed(format!("Test sound failed: {error}")));
    }
    fn copy_shock_settings(&mut self, source: TriggerKind, destination: TriggerKind) -> bool {
        if source == destination {
            return false;
        }
        let shock = self.triggers.get(source).shock.clone();
        self.triggers.get_mut(destination).shock = shock;
        self.copy_feedback = Some(format!(
            "Copied {} shock settings to {}.",
            source.label(),
            destination.label()
        ));
        true
    }
    fn select_effect(&mut self, kind: TriggerKind) {
        self.selected_effect = kind;
        if self.copy_source == kind {
            self.copy_source = first_copy_source(kind);
        }
        self.copy_feedback = None;
    }

    fn replace_ability_catalog(&mut self, catalog: crate::bridge_listener::AbilityCatalog) {
        self.ability_catalog = catalog
            .abilities
            .into_iter()
            .filter(|ability| ability.ability_slot > 0)
            .map(|ability| (ability.ability_slot, ability.ability_name))
            .collect();
    }
    fn poll_shock(&mut self) {
        loop {
            match self.shock_result.try_recv() {
                Ok(completion) => {
                    self.shock_in_flight = self.shock_in_flight.saturating_sub(1);
                    let trigger = &completion.request.trigger;
                    match completion.result {
                        ShockCompletionResult::Skipped { reason } => {
                            log::warn!(
                                target: "companion::app",
                                "shock_skipped trigger={} provider={} target={:?} intensity={:?} duration_ms={:?} session_id={:?} sequence={} reason={}",
                                trigger.kind.label(),
                                completion.request.provider.label(),
                                completion.request.target.as_ref().map(ProviderTarget::id),
                                completion.request.resolved.map(|resolved| resolved.intensity),
                                completion.request.resolved.map(|resolved| resolved.duration_ms),
                                trigger.session_id,
                                trigger.sequence,
                                reason
                            );
                            self.shock_status = Some(ShockStatus::Skipped {
                                request: completion.request,
                                reason: reason.to_owned(),
                            });
                        }
                        ShockCompletionResult::Completed(Ok(())) => {
                            log::info!(
                                target: "companion::app",
                                "shock_sent trigger={} provider={} target={:?} intensity={:?} duration_ms={:?} session_id={:?} sequence={}",
                                trigger.kind.label(),
                                completion.request.provider.label(),
                                completion.request.target.as_ref().map(ProviderTarget::id),
                                completion.request.resolved.map(|resolved| resolved.intensity),
                                completion.request.resolved.map(|resolved| resolved.duration_ms),
                                trigger.session_id,
                                trigger.sequence
                            );
                            self.shock_status = Some(ShockStatus::Sent(completion.request));
                        }
                        ShockCompletionResult::Completed(Err(error)) => {
                            log::warn!(
                                target: "companion::app",
                                "shock_failed trigger={} provider={} target={:?} intensity={:?} duration_ms={:?} session_id={:?} sequence={} error_kind={}",
                                trigger.kind.label(),
                                completion.request.provider.label(),
                                completion.request.target.as_ref().map(ProviderTarget::id),
                                completion.request.resolved.map(|resolved| resolved.intensity),
                                completion.request.resolved.map(|resolved| resolved.duration_ms),
                                trigger.session_id,
                                trigger.sequence,
                                provider_error_kind(&error)
                            );
                            self.shock_status = Some(ShockStatus::Failed {
                                request: completion.request,
                                error: error.to_string(),
                            });
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::error!(
                        target: "companion::app",
                        "shock_worker_channel_failed reason=disconnected in_flight={}",
                        self.shock_in_flight
                    );
                    self.shock_in_flight = 0;
                    break;
                }
            }
        }
    }

    fn trigger_is_new(&mut self, trigger: &TriggerIdentity) -> bool {
        if let Some((session_id, sequence)) = &self.last_sequence
            && session_id == &trigger.session_id
            && trigger.sequence <= *sequence
        {
            log::debug!(
                target: "companion::app",
                "shock_skipped reason=duplicate_or_out_of_order trigger={} session_id={:?} sequence={}",
                trigger.kind.label(),
                trigger.session_id,
                trigger.sequence
            );
            return false;
        }
        self.last_sequence = Some((trigger.session_id.clone(), trigger.sequence));
        true
    }

    fn apply_shock_enqueue_result(&mut self, request: ShockRequest, result: ShockEnqueueResult) {
        let trigger = &request.trigger;
        match result {
            ShockEnqueueResult::Accepted => {
                self.shock_status = Some(ShockStatus::Sending(request.clone()));
                self.shock_in_flight = self.shock_in_flight.saturating_add(1);
                log::info!(
                    target: "companion::app",
                    "shock_queued trigger={} provider={} target={:?} intensity={:?} duration_ms={:?} session_id={:?} sequence={}",
                    trigger.kind.label(),
                    request.provider.label(),
                    request.target.as_ref().map(ProviderTarget::id),
                    request.resolved.map(|resolved| resolved.intensity),
                    request.resolved.map(|resolved| resolved.duration_ms),
                    trigger.session_id,
                    trigger.sequence
                );
            }
            ShockEnqueueResult::Full => {
                log::warn!(
                    target: "companion::app",
                    "shock_skipped trigger={} provider={} target={:?} intensity={:?} duration_ms={:?} session_id={:?} sequence={} reason=queue_capacity",
                    trigger.kind.label(),
                    request.provider.label(),
                    request.target.as_ref().map(ProviderTarget::id),
                    request.resolved.map(|resolved| resolved.intensity),
                    request.resolved.map(|resolved| resolved.duration_ms),
                    trigger.session_id,
                    trigger.sequence
                );
                self.shock_status = Some(ShockStatus::Skipped {
                    request,
                    reason: "shock queue is full".to_owned(),
                });
            }
            ShockEnqueueResult::Disconnected => {
                log::error!(
                    target: "companion::app",
                    "shock_failed trigger={} provider={} target={:?} intensity={:?} duration_ms={:?} session_id={:?} sequence={} reason=worker_unavailable",
                    trigger.kind.label(),
                    request.provider.label(),
                    request.target.as_ref().map(ProviderTarget::id),
                    request.resolved.map(|resolved| resolved.intensity),
                    request.resolved.map(|resolved| resolved.duration_ms),
                    trigger.session_id,
                    trigger.sequence
                );
                self.shock_status = Some(ShockStatus::Failed {
                    request,
                    error: "shock worker is unavailable".to_owned(),
                });
            }
        }
    }

    fn queue_trigger_shock(&mut self, trigger: TriggerIdentity) {
        if !self.trigger_is_new(&trigger) {
            return;
        }
        let settings = self.triggers.get(trigger.kind);
        if !settings.enabled {
            log::info!(
                target: "companion::app",
                "trigger_disabled trigger={} session_id={:?} sequence={} ability_slot={:?} detection={:?}",
                trigger.kind.label(),
                trigger.session_id,
                trigger.sequence,
                trigger.ability_slot,
                trigger.detection
            );
            return;
        }
        if let (Some(filter), Some(ability_slot)) = (
            self.triggers.ability_filter(trigger.kind),
            trigger.ability_slot,
        ) && !filter.accepts(ability_slot)
        {
            log::info!(
                target: "companion::app",
                "trigger_filtered reason=ability_not_selected trigger={} session_id={:?} sequence={} ability_slot={} detection={:?}",
                trigger.kind.label(),
                trigger.session_id,
                trigger.sequence,
                ability_slot,
                trigger.detection
            );
            return;
        }
        let resolved = settings.shock.resolve();
        let request = ShockRequest {
            provider: self.provider,
            target: self.selected_device().cloned(),
            resolved,
            trigger,
        };
        let Some(client) = self.client.clone() else {
            log::warn!(
                target: "companion::app",
                "shock_skipped trigger={} provider={} target={:?} intensity={:?} duration_ms={:?} session_id={:?} sequence={} reason=provider_not_connected",
                request.trigger.kind.label(),
                request.provider.label(),
                request.target.as_ref().map(ProviderTarget::id),
                request.resolved.map(|resolved| resolved.intensity),
                request.resolved.map(|resolved| resolved.duration_ms),
                request.trigger.session_id,
                request.trigger.sequence
            );
            self.shock_status = Some(ShockStatus::Skipped {
                request,
                reason: "provider is not connected".to_owned(),
            });
            return;
        };
        if request.target.is_none() {
            log::warn!(
                target: "companion::app",
                "shock_skipped trigger={} provider={} target=none intensity={:?} duration_ms={:?} session_id={:?} sequence={} reason=no_target",
                request.trigger.kind.label(),
                request.provider.label(),
                request.resolved.map(|resolved| resolved.intensity),
                request.resolved.map(|resolved| resolved.duration_ms),
                request.trigger.session_id,
                request.trigger.sequence
            );
            self.shock_status = Some(ShockStatus::Skipped {
                request,
                reason: "no target is selected".to_owned(),
            });
            return;
        }
        if request.resolved.is_none() {
            log::warn!(
                target: "companion::app",
                "shock_skipped trigger={} provider={} target={:?} intensity=none duration_ms=none session_id={:?} sequence={} reason=invalid_settings",
                request.trigger.kind.label(),
                request.provider.label(),
                request.target.as_ref().map(ProviderTarget::id),
                request.trigger.session_id,
                request.trigger.sequence
            );
            self.shock_status = Some(ShockStatus::Skipped {
                request,
                reason: "shock settings are invalid".to_owned(),
            });
            return;
        }
        let queued_at = Instant::now();
        let enqueue_result = match self.shock_sender.try_send(ShockJob {
            client,
            request: request.clone(),
            queued_at,
        }) {
            Ok(()) => ShockEnqueueResult::Accepted,
            Err(TrySendError::Full(_job)) => ShockEnqueueResult::Full,
            Err(TrySendError::Disconnected(_job)) => ShockEnqueueResult::Disconnected,
        };
        self.apply_shock_enqueue_result(request, enqueue_result);
    }
    fn ensure_bridge_subscription(&mut self) {
        if self.bridge_events.is_none() {
            log::debug!(target: "companion::app", "bridge_subscription_created");
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
                    match &event {
                        BridgeEvent::HookReady(ready) => log::info!(
                            target: "companion::app",
                            "bridge_hook_ready session_id={:?} client_time_ms={} poll_interval_ms={}",
                            ready.session_id,
                            ready.client_time_ms,
                            ready.poll_interval_ms
                        ),
                        BridgeEvent::AbilityCatalog(catalog) => log::info!(
                            target: "companion::app",
                            "bridge_ability_catalog session_id={:?} client_time_ms={} abilities={}",
                            catalog.session_id,
                            catalog.client_time_ms,
                            catalog.abilities.len()
                        ),
                        BridgeEvent::LocalPlayerDeath(death) => log::info!(
                            target: "companion::app",
                            "bridge_trigger_received trigger=death session_id={:?} sequence={} client_time_ms={} detection={:?}",
                            death.session_id,
                            death.sequence,
                            death.client_time_ms,
                            death.detection
                        ),
                        BridgeEvent::AbilityUsed(ability) => log::info!(
                            target: "companion::app",
                            "bridge_trigger_received trigger=ability_use session_id={:?} sequence={} client_time_ms={} ability_slot={} detection={:?} charges_before={:?} charges_after={:?}",
                            ability.session_id,
                            ability.sequence,
                            ability.client_time_ms,
                            ability.ability_slot,
                            ability.detection,
                            ability.charges_before,
                            ability.charges_after
                        ),
                        BridgeEvent::AbilityCooldownReady(ability) => log::info!(
                            target: "companion::app",
                            "bridge_trigger_received trigger=ability_cooldown_ready session_id={:?} sequence={} client_time_ms={} ability_slot={} detection={:?} charges_before={:?} charges_after={:?}",
                            ability.session_id,
                            ability.sequence,
                            ability.client_time_ms,
                            ability.ability_slot,
                            ability.detection,
                            ability.charges_before,
                            ability.charges_after
                        ),
                    }
                    self.last_bridge_event = Some(event.clone());
                    let trigger = match event {
                        BridgeEvent::HookReady(_) => {
                            self.ability_catalog.clear();
                            None
                        }
                        BridgeEvent::AbilityCatalog(catalog) => {
                            self.replace_ability_catalog(catalog);
                            None
                        }
                        BridgeEvent::LocalPlayerDeath(death) => {
                            Some(TriggerIdentity::from_death(death))
                        }
                        BridgeEvent::AbilityUsed(ability) => Some(TriggerIdentity::from_ability(
                            TriggerKind::AbilityUse,
                            ability,
                        )),
                        BridgeEvent::AbilityCooldownReady(ability) => {
                            Some(TriggerIdentity::from_ability(
                                TriggerKind::AbilityCooldownReady,
                                ability,
                            ))
                        }
                    };
                    if let Some(trigger) = trigger {
                        self.queue_trigger_shock(trigger);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::warn!(
                        target: "companion::app",
                        "bridge_subscription_failed reason=channel_closed"
                    );
                    self.bridge_events = None;
                    break;
                }
            }
        }
    }

    fn start_listener_from_input(&mut self) {
        let path = self.log_path.trim();
        if path.is_empty() {
            log::warn!(target: "companion::app", "log_listener_start_skipped reason=empty_path");
            self.listener_action_error =
                Some("Enter a console.log path before starting the listener.".to_owned());
            return;
        }
        let path = PathBuf::from(path);
        log::info!(
            target: "companion::app",
            "log_listener_manual_start path={:?}",
            path
        );
        self.listener_action_error = self.start_log_listener(path.clone()).err().map(|error| {
            log::warn!(
                target: "companion::app",
                "log_listener_manual_failed path={:?} error={:?}",
                path,
                error
            );
            format!("Could not start listener: {error}")
        });
        if self.listener_action_error.is_none() {
            log::info!(
                target: "companion::app",
                "log_listener_manual_started path={:?}",
                path
            );
        }
    }

    fn auto_detect_log_path(&mut self) {
        self.listener_action_error = None;
        log::info!(target: "companion::app", "log_path_auto_detection_started");
        self.apply_log_detection(deadlock_path::detect());
    }

    fn apply_log_detection(&mut self, result: Result<Detection, DetectionError>) {
        match result {
            Ok(Detection::Ready { path }) => {
                log::info!(
                    target: "companion::app",
                    "log_path_auto_detection_found path={:?}",
                    path
                );
                self.log_path = path.display().to_string();
                self.log_detection_status = match self.start_log_listener(path.clone()) {
                    Ok(()) => Some(LogDetectionStatus::Found),
                    Err(error) => {
                        log::warn!(
                            target: "companion::app",
                            "log_listener_auto_start_failed path={:?} error={:?}",
                            path,
                            error
                        );
                        Some(LogDetectionStatus::Failed(format!(
                            "Deadlock console.log was found, but the listener could not start: {error}"
                        )))
                    }
                };
            }
            Ok(Detection::NotCreated { path }) => {
                log::info!(
                    target: "companion::app",
                    "log_path_auto_detection_not_created path={:?}",
                    path
                );
                self.log_path = path.display().to_string();
                self.log_detection_status = match self.start_log_listener(path.clone()) {
                    Ok(()) => Some(LogDetectionStatus::NotCreated),
                    Err(error) => {
                        log::warn!(
                            target: "companion::app",
                            "log_listener_auto_start_failed path={:?} error={:?}",
                            path,
                            error
                        );
                        Some(LogDetectionStatus::Failed(format!(
                            "Deadlock was found, but the listener could not start: {error}"
                        )))
                    }
                };
            }
            Err(error) => {
                log::warn!(
                    target: "companion::app",
                    "log_path_auto_detection_failed error={:?}",
                    error
                );
                self.log_detection_status = Some(LogDetectionStatus::Failed(format!(
                    "Auto-detect failed: {error}"
                )));
            }
        }
    }

    pub fn draw(&mut self, ui: &mut Ui) {
        self.poll_sound();
        self.poll_connection_test();
        self.poll_shock();
        self.poll_bridge_events();
        let busy = self.is_busy();

        ui.horizontal_wrapped(|ui| {
            for section in [
                AppSection::Setup,
                AppSection::Effects,
                AppSection::GameConnection,
            ] {
                ui.selectable_value(&mut self.selected_section, section, section.label());
            }
        });
        ui.separator();
        ui.add_space(6.0);

        match self.selected_section {
            AppSection::Setup => self.draw_setup(ui, busy),
            AppSection::Effects => self.draw_effects(ui, busy),
            AppSection::GameConnection => self.draw_game_connection(ui),
        }

        let listener_status = self.bridge_listener.status();
        if listener_status.phase != ListenerPhase::Stopped || self.shock_in_progress() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }

    fn draw_setup(&mut self, ui: &mut Ui, busy: bool) {
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

        ui.add_space(8.0);
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
        let selection_enabled = !self.devices.is_empty() && !busy;
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
        let mut selected_device = self.selected_device.clone();
        ui.add_enabled_ui(selection_enabled, |ui| {
            egui::ComboBox::from_id_salt("device")
                .selected_text(selected_name.as_str())
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for device in &self.devices {
                        selection_changed |= ui
                            .selectable_value(
                                &mut selected_device,
                                Some(device.id().clone()),
                                device.name(),
                            )
                            .changed();
                    }
                });
        });
        if selection_changed && let Some(target) = selected_device {
            self.select_device(target);
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
    }

    fn draw_effects(&mut self, ui: &mut Ui, busy: bool) {
        ui.heading("Effects");
        ui.label("Each trigger has its own shock settings.");
        ui.add_space(4.0);
        for kind in [
            TriggerKind::Death,
            TriggerKind::AbilityUse,
            TriggerKind::AbilityCooldownReady,
        ] {
            let summary = self.triggers.get(kind).shock.summary();
            let selected = self.selected_effect == kind;
            let frame = egui::Frame::group(ui.style()).inner_margin(8.0);
            let frame = if selected {
                frame
                    .fill(ui.visuals().selection.bg_fill.gamma_multiply(0.35))
                    .stroke(ui.visuals().selection.stroke)
            } else {
                frame
            };
            let mut configure_clicked = false;
            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.strong(trigger_display_label(kind));
                        ui.small(summary);
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        configure_clicked |= ui.button("⚙ Configure").clicked();
                        ui.add_enabled_ui(!busy, |ui| {
                            toggle_button(ui, &mut self.triggers.get_mut(kind).enabled);
                        });
                    });
                });
            });
            if configure_clicked {
                self.select_effect(kind);
            }
            ui.add_space(4.0);
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        let destination = self.selected_effect;
        ui.heading(format!("{} effect", trigger_display_label(destination)));
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.label("Trigger");
                toggle_button(ui, &mut self.triggers.get_mut(destination).enabled);
            });
        });

        if destination != TriggerKind::Death {
            self.draw_ability_filter(ui, destination, busy);
            if destination == TriggerKind::AbilityCooldownReady {
                ui.small(
                    "Cooldown ready includes a normal cooldown finishing and a charged ability restoring a charge.",
                );
            }
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Copy shock settings from");
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("copy-shock-source")
                    .selected_text(trigger_display_label(self.copy_source))
                    .show_ui(ui, |ui| {
                        for source in [
                            TriggerKind::Death,
                            TriggerKind::AbilityUse,
                            TriggerKind::AbilityCooldownReady,
                        ] {
                            if source != destination {
                                ui.selectable_value(
                                    &mut self.copy_source,
                                    source,
                                    trigger_display_label(source),
                                );
                            }
                        }
                    });
            });
        });
        if ui
            .add_enabled(
                !busy && self.copy_source != destination,
                egui::Button::new("Copy").min_size([ui.available_width(), 0.0].into()),
            )
            .clicked()
        {
            self.copy_shock_settings(self.copy_source, destination);
        }
        if let Some(feedback) = &self.copy_feedback {
            status_line(ui, feedback, [0.30, 0.78, 0.42, 1.0]);
        }

        ui.add_space(6.0);
        ui.add_enabled_ui(!busy, |ui| {
            draw_shock_settings_editor(ui, &mut self.triggers.get_mut(destination).shock);
        });
    }

    fn draw_ability_filter(&mut self, ui: &mut Ui, kind: TriggerKind, busy: bool) {
        let slots = (1..=4)
            .chain(self.ability_catalog.keys().copied())
            .collect::<BTreeSet<_>>();
        let names = &self.ability_catalog;
        let filter = self
            .triggers
            .ability_filter_mut(kind)
            .expect("ability trigger has an ability filter");

        ui.add_space(6.0);
        ui.label("Abilities");
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                if ui.button("All").clicked() {
                    *filter = AbilityFilter::All;
                }
                if ui.button("None").clicked() {
                    *filter = AbilityFilter::Selected(BTreeSet::new());
                }
            });
            ui.horizontal_wrapped(|ui| {
                for slot in &slots {
                    let mut selected = filter.accepts(*slot);
                    let label = names
                        .get(slot)
                        .and_then(Option::as_deref)
                        .map(|name| format!("Slot {slot} — {name}"))
                        .unwrap_or_else(|| format!("Slot {slot}"));
                    if ui.checkbox(&mut selected, label).changed() {
                        if matches!(&*filter, AbilityFilter::All) {
                            let selected_slots = slots
                                .iter()
                                .copied()
                                .filter(|candidate| *candidate != *slot)
                                .collect();
                            *filter = AbilityFilter::Selected(selected_slots);
                        } else if let AbilityFilter::Selected(selected_slots) = &mut *filter {
                            if selected {
                                selected_slots.insert(*slot);
                            } else {
                                selected_slots.remove(slot);
                            }
                        }
                    }
                }
            });
        });
        if matches!(&*filter, AbilityFilter::Selected(slots) if slots.is_empty()) {
            status_line(
                ui,
                "No abilities are selected; this trigger will not shock.",
                [0.92, 0.68, 0.22, 1.0],
            );
        }
        if self.ability_catalog.is_empty() {
            ui.small("Using numbered slots until the game reports ability names.");
        }
    }

    fn draw_game_connection(&mut self, ui: &mut Ui) {
        ui.heading("Game connection");
        ui.label("Deadlock must be launched with -condebug so it writes console.log.");
        text_input(ui, "Log path", &mut self.log_path, false);
        ui.horizontal(|ui| {
            let spacing = ui.spacing().item_spacing.x;
            let button_size = egui::vec2(
                (ui.available_width() - spacing) * 0.5,
                ui.spacing().interact_size.y,
            );
            if ui
                .add_sized(button_size, egui::Button::new("Auto-detect"))
                .clicked()
            {
                self.auto_detect_log_path();
            }
            if ui
                .add_sized(button_size, egui::Button::new("Start/Restart listener"))
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
        ui.label(format!(
            "Current ability catalogue: {} slot(s).",
            self.ability_catalog.len()
        ));
        if let Some(status) = &self.shock_status {
            let label = status.label();
            status_line(ui, &label, status.color());
        } else {
            ui.label("Last shock delivery: none since startup.");
        }
    }
}

pub struct CompanionApp {
    pub state: AppState,
    persistence: Persistence,
    reset_confirmation: bool,
    menu_error: Option<String>,
    version_check: VersionCheckOwner,
    version_warnings: WarningSelection,
}

impl CompanionApp {
    pub fn load() -> Self {
        match default_state_path() {
            Ok(path) => Self::load_from_path(path),
            Err(error) => {
                log::warn!(
                    target: "companion::app",
                    "settings_load_unavailable error={:?}",
                    error
                );
                let (persistence, state) = Persistence::unavailable(error);
                Self {
                    state: state.restore_app(),
                    persistence,
                    reset_confirmation: false,
                    menu_error: None,
                    version_check: VersionCheckOwner::with_client(LATEST_RELEASE_URL, None),
                    version_warnings: WarningSelection::default(),
                }
            }
        }
    }

    pub fn load_with_context(context: egui::Context) -> Self {
        let mut app = Self::load();
        app.version_check = VersionCheckOwner::new(&context);
        app
    }

    pub(crate) fn load_from_path(path: PathBuf) -> Self {
        let (persistence, state) = Persistence::open(path);
        Self {
            state: state.restore_app(),
            persistence,
            reset_confirmation: false,
            menu_error: None,
            version_check: VersionCheckOwner::with_client(LATEST_RELEASE_URL, None),
            version_warnings: WarningSelection::default(),
        }
    }

    pub fn draw(&mut self, ui: &mut Ui) {
        self.version_check.poll();
        let listener_status = self.state.bridge_listener.status();
        let remote = match &self.version_check.state {
            VersionCheckState::Current { latest }
            | VersionCheckState::UpdateAvailable { latest } => Some(latest),
            _ => None,
        };
        self.version_warnings =
            select_warnings(&app_version(), &listener_status.mod_version, remote);

        self.draw_menu(ui);
        if let Some(warning) = self.persistence.load_warning() {
            status_line(ui, warning, [0.92, 0.68, 0.22, 1.0]);
        }
        if let Some(error) = self.persistence.save_error() {
            status_line(ui, error, [0.92, 0.32, 0.28, 1.0]);
        }
        if let Some(error) = &self.menu_error {
            status_line(ui, error, [0.92, 0.32, 0.28, 1.0]);
        }
        self.draw_update_panel(ui);
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Frame::NONE.inner_margin(8.0).show(ui, |ui| {
                self.state.draw(ui);
            });
        });
        self.draw_reset_confirmation(ui);

        if let Some(delay) = self
            .persistence
            .observe(PersistedState::from_app(&self.state), Instant::now())
        {
            ui.ctx().request_repaint_after(delay);
        }
        if self.version_check.is_checking() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }

    fn draw_update_panel(&self, ui: &mut Ui) {
        let has_warning = self.version_warnings.companion_outdated.is_some()
            || self.version_warnings.mod_outdated.is_some()
            || self.version_warnings.mod_legacy
            || self.version_warnings.mod_invalid;
        if !has_warning {
            return;
        }
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(86, 64, 22))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.strong("Updates available");
                if let Some(target) = &self.version_warnings.companion_outdated {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "Companion {} is older than {}.",
                            app_version(),
                            target
                        ));
                        ui.hyperlink_to("Download companion", COMPANION_RELEASE_URL);
                    });
                }
                if let Some((installed, target)) = &self.version_warnings.mod_outdated {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "DeadlockShock mod {} is older than {}.",
                            installed, target
                        ));
                        ui.hyperlink_to("Update mod", MOD_RELEASE_URL);
                    });
                } else if self.version_warnings.mod_legacy {
                    ui.horizontal(|ui| {
                        ui.label("The last observed DeadlockShock mod predates version reporting.");
                        ui.hyperlink_to("Update mod", MOD_RELEASE_URL);
                    });
                } else if self.version_warnings.mod_invalid {
                    ui.horizontal(|ui| {
                        ui.label("The last observed DeadlockShock mod reported an invalid version; reinstall the latest mod.");
                        ui.hyperlink_to("Update mod", MOD_RELEASE_URL);
                    });
                }
            });
    }
    pub fn flush_pending(&mut self) {
        log::info!(target: "companion::app", "settings_flush_boundary reason=application_exit");
        let result = self
            .persistence
            .flush(PersistedState::from_app(&self.state));
        if result.is_err() {
            log::warn!(target: "companion::app", "settings_flush_boundary outcome=failed");
        }
    }
    fn draw_menu(&mut self, ui: &mut Ui) {
        let reset_available = !self.state.is_busy();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Menu", |ui| {
                ui.label(format!("Companion version: {}", app_version()));
                let mod_label = match &self.state.bridge_listener.status().mod_version {
                    ModVersionObservation::Unknown => "unknown".to_owned(),
                    ModVersionObservation::Legacy => "legacy (no version reporting)".to_owned(),
                    ModVersionObservation::Invalid => "invalid".to_owned(),
                    ModVersionObservation::Reported(version) => format!("last observed {version}"),
                };
                ui.label(format!("Mod version: {mod_label}"));
                match &self.version_check.state {
                    VersionCheckState::Checking => ui.label("Latest stable: checking…"),
                    VersionCheckState::Current { latest } => {
                        ui.label(format!("Latest stable: {latest} (current)"))
                    }
                    VersionCheckState::UpdateAvailable { latest } => {
                        ui.label(format!("Latest stable: {latest} (update available)"))
                    }
                    VersionCheckState::Unavailable { reason } => {
                        ui.label(format!("Latest stable: unavailable ({reason})"))
                    }
                };
                let checking = self.version_check.is_checking();
                if ui
                    .add_enabled(!checking, egui::Button::new("Check for updates"))
                    .clicked()
                {
                    self.version_check.start(ui.ctx().clone());
                }
                ui.separator();
                if ui.button("Open config folder").clicked() {
                    log::info!(target: "companion::app", "config_folder_open_requested");
                    self.menu_error = self.persistence.open_config_directory().err();
                    if let Some(error) = &self.menu_error {
                        log::warn!(
                            target: "companion::app",
                            "config_folder_open_failed error={:?}",
                            error
                        );
                    }
                }
                ui.separator();
                let response =
                    ui.add_enabled(reset_available, egui::Button::new("Reset saved state…"));
                if response.clicked() {
                    self.reset_confirmation = true;
                }
                if !reset_available {
                    response.on_disabled_hover_text(
                        "Wait for connection, sound, and shock work to finish before resetting.",
                    );
                }
            });
        });
    }

    fn draw_reset_confirmation(&mut self, ui: &mut Ui) {
        if !self.reset_confirmation {
            return;
        }

        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        let reset_available = !self.state.is_busy();
        egui::Window::new("Reset saved state?")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.label(
                    "This clears saved credentials, target preference, trigger and shock settings, and log path.",
                );
                ui.label("Any active log listener will be stopped.");
                if !reset_available {
                    status_line(
                        ui,
                        "Wait for connection, sound, and shock work to finish.",
                        [0.92, 0.68, 0.22, 1.0],
                    );
                }
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if ui
                        .add_enabled(reset_available, egui::Button::new("Reset"))
                        .clicked()
                    {
                        confirm = true;
                    }
                });
            });
        self.reset_confirmation = open && !cancel;
        if confirm && self.reset_and_save() {
            self.reset_confirmation = false;
        }
    }

    fn reset_and_save(&mut self) -> bool {
        log::info!(target: "companion::app", "settings_reset_requested");
        if !self.state.reset_saved_state() {
            log::warn!(
                target: "companion::app",
                "settings_reset_outcome outcome=skipped"
            );
            return false;
        }
        let result = self
            .persistence
            .save_reset_now(PersistedState::from_app(&self.state));
        log::info!(
            target: "companion::app",
            "settings_reset_outcome outcome=applied saved={}",
            result.is_ok()
        );
        true
    }
}
fn trigger_display_label(kind: TriggerKind) -> &'static str {
    match kind {
        TriggerKind::Death => "Death",
        TriggerKind::AbilityUse => "Ability use",
        TriggerKind::AbilityCooldownReady => "Cooldown ready",
    }
}

fn first_copy_source(destination: TriggerKind) -> TriggerKind {
    match destination {
        TriggerKind::Death => TriggerKind::AbilityUse,
        TriggerKind::AbilityUse | TriggerKind::AbilityCooldownReady => TriggerKind::Death,
    }
}

fn toggle_button(ui: &mut Ui, value: &mut bool) {
    let label = if *value { "On" } else { "Off" };
    if ui
        .add(
            egui::Button::new(label)
                .selected(*value)
                .min_size(egui::vec2(44.0, 0.0)),
        )
        .clicked()
    {
        *value = !*value;
    }
}

fn draw_shock_settings_editor(ui: &mut Ui, shock: &mut ShockSettings) {
    ui.heading("Shock mode");
    ui.horizontal(|ui| {
        ui.label("Mode");
        egui::ComboBox::from_id_salt("shock-mode")
            .selected_text(shock.mode.label())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut shock.mode, ShockMode::Interval, "Interval");
                ui.selectable_value(&mut shock.mode, ShockMode::Fixed, "Fixed");
            });
    });
    ui.add_space(4.0);
    match shock.mode {
        ShockMode::Interval => {
            slider_input(
                ui,
                "Minimum intensity",
                &mut shock.interval.minimum_intensity,
                MIN_SHOCK_INTENSITY..=MAX_SHOCK_INTENSITY,
                1.0,
                "%",
            );
            slider_input(
                ui,
                "Maximum intensity",
                &mut shock.interval.maximum_intensity,
                MIN_SHOCK_INTENSITY..=MAX_SHOCK_INTENSITY,
                1.0,
                "%",
            );
            slider_input(
                ui,
                "Minimum duration",
                &mut shock.interval.minimum_duration_seconds,
                MIN_SHOCK_DURATION..=MAX_SHOCK_DURATION,
                0.1,
                " s",
            );
            slider_input(
                ui,
                "Maximum duration",
                &mut shock.interval.maximum_duration_seconds,
                MIN_SHOCK_DURATION..=MAX_SHOCK_DURATION,
                0.1,
                " s",
            );
        }
        ShockMode::Fixed => {
            slider_input(
                ui,
                "Intensity",
                &mut shock.fixed.intensity,
                MIN_SHOCK_INTENSITY..=MAX_SHOCK_INTENSITY,
                1.0,
                "%",
            );
            slider_input(
                ui,
                "Duration",
                &mut shock.fixed.duration_seconds,
                MIN_SHOCK_DURATION..=MAX_SHOCK_DURATION,
                0.1,
                " s",
            );
        }
    }
    if shock.interval.minimum_intensity > shock.interval.maximum_intensity {
        shock.interval.maximum_intensity = shock.interval.minimum_intensity;
    }
    if shock.interval.minimum_duration_seconds > shock.interval.maximum_duration_seconds {
        shock.interval.maximum_duration_seconds = shock.interval.minimum_duration_seconds;
    }
}

fn portable_intensity(value: f32) -> Option<u8> {
    (value.is_finite()
        && value.fract() == 0.0
        && (MIN_SHOCK_INTENSITY..=MAX_SHOCK_INTENSITY).contains(&value))
    .then_some(value as u8)
}
fn portable_duration(value: f32) -> Option<u64> {
    if !value.is_finite() || !(MIN_SHOCK_DURATION..=MAX_SHOCK_DURATION).contains(&value) {
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
            bridge_event_description(event),
            format_duration(at.elapsed())
        ),
        _ => "Last bridge event: none since listener start.".to_owned(),
    };
    ui.label(event);
}
fn bridge_event_description(event: &BridgeEvent) -> String {
    let ability_description = |name: &str, ability: &AbilityTrigger| {
        let ability_name = ability
            .ability_name
            .as_deref()
            .map(|name| format!(" ({name})"))
            .unwrap_or_default();
        let charges = match (ability.charges_before, ability.charges_after) {
            (Some(before), Some(after)) => format!(", charges {before}→{after}"),
            _ => String::new(),
        };
        format!(
            "{name}, slot {}{ability_name}, detection {}{charges}",
            ability.ability_slot, ability.detection
        )
    };
    match event {
        BridgeEvent::HookReady(_) | BridgeEvent::LocalPlayerDeath(_) => {
            event.event_name().to_owned()
        }
        BridgeEvent::AbilityCatalog(catalog) => {
            format!("ability_catalog, {} slot(s)", catalog.abilities.len())
        }
        BridgeEvent::AbilityUsed(ability) => ability_description("ability_used", ability),
        BridgeEvent::AbilityCooldownReady(ability) => {
            ability_description("ability_cooldown_ready", ability)
        }
    }
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
    use rand::SeedableRng;
    use rand::rngs::StdRng;
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
    fn switching_provider_clears_active_and_preferred_targets_but_retains_forms() {
        let mut state = AppState {
            username: "user".into(),
            api_key: "key".into(),
            devices: vec![ProviderTarget::new(TargetId::PiShock(1), "hub")],
            selected_device: Some(TargetId::PiShock(1)),
            preferred_target: Some(TargetId::PiShock(1)),
            credential_state: CredentialState::Valid,
            ..AppState::default()
        };
        state.set_provider(ProviderKind::OpenShock);
        assert_eq!(state.provider, ProviderKind::OpenShock);
        assert!(state.devices.is_empty());
        assert!(state.selected_device.is_none());
        assert!(state.preferred_target.is_none());
        assert_eq!(state.username, "user");
        assert_eq!(state.api_key, "key");
    }
    #[test]
    fn preferred_target_is_reconciled_only_against_freshly_fetched_targets() {
        let preferred = TargetId::PiShock(2);
        let mut state = AppState {
            preferred_target: Some(preferred.clone()),
            ..AppState::default()
        };
        assert!(state.selected_device.is_none());
        state.apply_devices(vec![
            ProviderTarget::new(TargetId::PiShock(1), "Alpha"),
            ProviderTarget::new(preferred.clone(), "Beta"),
        ]);
        assert_eq!(state.selected_device, Some(preferred.clone()));
        assert_eq!(
            state.selected_device().map(ProviderTarget::name),
            Some("Beta")
        );

        state.reset_connection();
        assert!(state.selected_device.is_none());
        assert_eq!(state.preferred_target, Some(preferred.clone()));
        state.apply_connection_result(Err(ProviderError::NotConnected));
        assert_eq!(state.preferred_target, Some(preferred.clone()));

        state.apply_devices(vec![ProviderTarget::new(TargetId::PiShock(3), "Gamma")]);
        assert_eq!(state.selected_device, Some(TargetId::PiShock(3)));
        assert_eq!(state.preferred_target, Some(TargetId::PiShock(3)));
        assert!(state.select_device(TargetId::PiShock(3)));
        assert_eq!(state.preferred_target, Some(TargetId::PiShock(3)));
        assert!(!state.select_device(TargetId::PiShock(99)));
        assert_eq!(state.selected_device, Some(TargetId::PiShock(3)));
    }
    #[test]
    fn failed_connection_clears_stale_live_targets_but_preserves_preference() {
        let mut state = AppState {
            devices: vec![ProviderTarget::new(TargetId::PiShock(1), "hub")],
            selected_device: Some(TargetId::PiShock(1)),
            preferred_target: Some(TargetId::PiShock(1)),
            ..AppState::default()
        };
        state.apply_connection_result(Err(ProviderError::NotConnected));
        assert!(state.devices.is_empty());
        assert!(state.selected_device.is_none());
        assert_eq!(state.preferred_target, Some(TargetId::PiShock(1)));
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
    fn all_effect_editors_render_both_shock_modes() {
        for kind in [
            TriggerKind::Death,
            TriggerKind::AbilityUse,
            TriggerKind::AbilityCooldownReady,
        ] {
            for mode in [ShockMode::Interval, ShockMode::Fixed] {
                let context = egui::Context::default();
                let mut state = AppState::default();
                state.selected_section = AppSection::Effects;
                state.selected_effect = kind;
                state.triggers.get_mut(kind).shock.mode = mode;
                if kind != TriggerKind::Death {
                    state
                        .ability_catalog
                        .insert(1, Some("Power Slash".to_owned()));
                }
                let output = context.run_ui(egui::RawInput::default(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| state.draw(ui));
                });
                assert!(!output.shapes.is_empty());
            }
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
    fn trigger(kind: TriggerKind, session_id: &str, sequence: u64) -> TriggerIdentity {
        TriggerIdentity {
            kind,
            session_id: session_id.to_owned(),
            client_time_ms: sequence,
            sequence,
            detection: "test".to_owned(),
            ability_slot: (kind != TriggerKind::Death).then_some(2),
            ability_name: (kind != TriggerKind::Death).then(|| "Test Ability".to_owned()),
            charges_before: None,
            charges_after: None,
        }
    }

    fn death_event(session_id: &str, sequence: u64) -> LocalPlayerDeath {
        LocalPlayerDeath {
            schema: 1,
            session_id: session_id.to_owned(),
            client_time_ms: sequence,
            sequence,
            detection: "test".to_owned(),
        }
    }
    #[test]
    fn shock_profiles_resolve_portable_fixed_and_interval_values_independently() {
        let mut fixed = ShockSettings::default();
        fixed.mode = ShockMode::Fixed;
        fixed.fixed.intensity = 37.0;
        fixed.fixed.duration_seconds = 1.234;
        assert_eq!(
            fixed.resolve(),
            Some(ResolvedShock {
                intensity: 37,
                duration_ms: 1_234
            })
        );

        let mut interval = ShockSettings::default();
        interval.interval.minimum_intensity = 70.0;
        interval.interval.maximum_intensity = 72.0;
        interval.interval.minimum_duration_seconds = 2.5;
        interval.interval.maximum_duration_seconds = 2.6;
        let mut rng = StdRng::seed_from_u64(7);
        let resolved = interval.resolve_with(&mut rng).unwrap();
        assert!((70..=72).contains(&resolved.intensity));
        assert!((2_500..=2_600).contains(&resolved.duration_ms));
        assert_eq!(ShockSettings::default().resolve().unwrap().intensity, 1);

        fixed.fixed.intensity = 0.0;
        assert_eq!(fixed.resolve(), None);
        interval.interval.minimum_intensity = 90.0;
        interval.interval.maximum_intensity = 10.0;
        assert_eq!(interval.resolve(), None);
    }

    #[test]
    fn ability_filters_cover_all_selected_empty_and_unknown_slots() {
        assert!(AbilityFilter::All.accepts(1));
        assert!(AbilityFilter::All.accepts(999));
        let selected = AbilityFilter::Selected(BTreeSet::from([2, 5]));
        assert!(!selected.accepts(1));
        assert!(selected.accepts(2));
        assert!(selected.accepts(5));
        assert!(!selected.accepts(999));
        assert!(!AbilityFilter::Selected(BTreeSet::new()).accepts(2));
    }

    #[test]
    fn copy_transfers_both_shock_banks_without_enabled_state_or_filter() {
        let mut state = AppState::default();
        state.triggers.death.enabled = false;
        state.triggers.death.shock.mode = ShockMode::Fixed;
        state.triggers.death.shock.fixed.intensity = 61.0;
        state.triggers.death.shock.fixed.duration_seconds = 1.7;
        state.triggers.death.shock.interval.minimum_intensity = 11.0;
        state.triggers.death.shock.interval.maximum_intensity = 89.0;
        state.triggers.death.shock.interval.minimum_duration_seconds = 0.6;
        state.triggers.death.shock.interval.maximum_duration_seconds = 2.8;
        state.triggers.ability_use.trigger.enabled = true;
        state.triggers.ability_use.ability_filter = AbilityFilter::Selected(BTreeSet::from([2]));

        assert!(state.copy_shock_settings(TriggerKind::Death, TriggerKind::AbilityUse));
        assert_eq!(
            state.triggers.ability_use.trigger.shock,
            state.triggers.death.shock
        );
        assert!(state.triggers.ability_use.trigger.enabled);
        assert_eq!(
            state.triggers.ability_use.ability_filter,
            AbilityFilter::Selected(BTreeSet::from([2]))
        );
        assert!(!state.copy_shock_settings(TriggerKind::Death, TriggerKind::Death));
    }

    #[test]
    fn selecting_an_effect_updates_the_editor_and_copy_source() {
        let mut state = AppState {
            selected_effect: TriggerKind::Death,
            copy_source: TriggerKind::AbilityUse,
            copy_feedback: Some("old confirmation".to_owned()),
            ..AppState::default()
        };

        state.select_effect(TriggerKind::AbilityUse);

        assert_eq!(state.selected_effect, TriggerKind::AbilityUse);
        assert_eq!(state.copy_source, TriggerKind::Death);
        assert!(state.copy_feedback.is_none());
    }
    #[test]
    fn shock_queue_accepts_capacity_without_blocking() {
        let (sender, receiver) = mpsc::sync_channel(SHOCK_QUEUE_CAPACITY);
        for value in 0..SHOCK_QUEUE_CAPACITY {
            assert!(sender.try_send(value).is_ok());
        }
        assert!(matches!(
            sender.try_send(SHOCK_QUEUE_CAPACITY),
            Err(TrySendError::Full(_))
        ));
        drop(receiver);
        assert!(matches!(
            sender.try_send(0),
            Err(TrySendError::Disconnected(_))
        ));
    }
    #[test]
    fn expired_shock_jobs_are_skipped_before_dispatch() {
        let queued_at = Instant::now();
        let now = queued_at + MAX_SHOCK_QUEUE_AGE;
        assert!(shock_job_expired_at(queued_at, now));
        assert!(!shock_job_expired_at(
            queued_at,
            now - Duration::from_millis(1)
        ));
    }
    #[test]
    fn expired_completion_is_skipped_and_decrements_in_flight() {
        let request = ShockRequest {
            provider: ProviderKind::PiShock,
            target: None,
            resolved: None,
            trigger: trigger(TriggerKind::Death, "session", 1),
        };
        let (sender, receiver) = mpsc::channel();
        let mut state = AppState {
            shock_result: receiver,
            shock_in_flight: 1,
            ..AppState::default()
        };
        sender
            .send(ShockCompletion {
                request,
                result: ShockCompletionResult::Skipped { reason: "expired" },
            })
            .unwrap();
        state.poll_shock();
        assert_eq!(state.shock_in_flight, 0);
        let Some(ShockStatus::Skipped { reason, .. }) = state.shock_status else {
            panic!("expected skipped status");
        };
        assert_eq!(reason, "expired");
    }

    #[test]
    fn queue_submission_outcomes_update_status_and_in_flight_count() {
        let request = ShockRequest {
            provider: ProviderKind::PiShock,
            target: None,
            resolved: None,
            trigger: trigger(TriggerKind::Death, "session", 1),
        };

        let mut full = AppState::default();
        full.apply_shock_enqueue_result(request.clone(), ShockEnqueueResult::Full);
        assert_eq!(full.shock_in_flight, 0);
        assert!(matches!(
            full.shock_status,
            Some(ShockStatus::Skipped { reason, .. }) if reason == "shock queue is full"
        ));

        let mut disconnected = AppState::default();
        disconnected.apply_shock_enqueue_result(request.clone(), ShockEnqueueResult::Disconnected);
        assert_eq!(disconnected.shock_in_flight, 0);
        assert!(matches!(
            disconnected.shock_status,
            Some(ShockStatus::Failed { error, .. }) if error == "shock worker is unavailable"
        ));

        let mut accepted = AppState::default();
        accepted.apply_shock_enqueue_result(request, ShockEnqueueResult::Accepted);
        assert_eq!(accepted.shock_in_flight, 1);
        assert!(matches!(
            accepted.shock_status,
            Some(ShockStatus::Sending(_))
        ));
    }

    #[test]
    fn actionable_events_share_a_global_sequence_watermark_across_trigger_kinds() {
        let mut state = AppState::default();
        state.queue_trigger_shock(trigger(TriggerKind::AbilityUse, "first", 4));
        assert_eq!(state.last_sequence, Some(("first".to_owned(), 4)));
        assert!(state.shock_status.is_none());

        state.queue_trigger_shock(trigger(TriggerKind::Death, "first", 3));
        assert!(state.shock_status.is_none());
        state.queue_trigger_shock(trigger(TriggerKind::Death, "first", 5));
        let first = state.shock_status.clone();
        assert!(matches!(&first, Some(ShockStatus::Skipped { .. })));

        state.triggers.ability_use.trigger.enabled = true;
        state.queue_trigger_shock(trigger(TriggerKind::AbilityUse, "first", 5));
        assert_eq!(state.shock_status, first);
        state.queue_trigger_shock(trigger(TriggerKind::AbilityUse, "first", 6));
        assert_ne!(state.shock_status, first);

        state.queue_trigger_shock(trigger(TriggerKind::Death, "second", 1));
        assert_eq!(state.last_sequence, Some(("second".to_owned(), 1)));
    }

    #[test]
    fn trigger_defaults_and_cooldown_enablement_control_queueing() {
        let mut state = AppState::default();
        assert!(state.triggers.death.enabled);
        assert!(!state.triggers.ability_use.trigger.enabled);
        assert!(!state.triggers.ability_cooldown_ready.trigger.enabled);

        state.queue_trigger_shock(trigger(TriggerKind::AbilityCooldownReady, "session", 1));
        assert_eq!(state.last_sequence, Some(("session".to_owned(), 1)));
        assert!(state.shock_status.is_none());

        state.triggers.ability_cooldown_ready.trigger.enabled = true;
        state.queue_trigger_shock(trigger(TriggerKind::AbilityCooldownReady, "session", 2));
        assert!(matches!(
            state.shock_status,
            Some(ShockStatus::Skipped { .. })
        ));
    }

    #[test]
    fn hook_ready_is_observed_but_never_advances_the_actionable_watermark() {
        let (sender, receiver) = mpsc::channel();
        let mut state = AppState {
            bridge_events: Some(receiver),
            ..AppState::default()
        };
        state
            .ability_catalog
            .insert(1, Some("Stale name".to_owned()));
        sender
            .send(BridgeEvent::HookReady(crate::bridge_listener::HookReady {
                schema: 1,
                session_id: "session".to_owned(),
                client_time_ms: 1,
                poll_interval_ms: 100,
            }))
            .unwrap();
        state.poll_bridge_events();
        assert!(state.ability_catalog.is_empty());
        assert!(state.last_sequence.is_none());
        assert!(state.shock_status.is_none());
    }

    #[test]
    fn enabled_death_preserves_skipped_status_when_prerequisites_are_missing() {
        let (sender, receiver) = mpsc::channel();
        let mut state = AppState {
            bridge_events: Some(receiver),
            ..AppState::default()
        };
        sender
            .send(BridgeEvent::LocalPlayerDeath(death_event("session", 1)))
            .unwrap();
        state.poll_bridge_events();
        assert!(matches!(
            state.shock_status,
            Some(ShockStatus::Skipped { .. })
        ));
        assert_eq!(state.shock_in_flight, 0);
    }

    #[test]
    fn parsed_enabled_ability_event_reaches_the_shock_queue_path() {
        let event = crate::bridge_listener::parse_bridge_record(
            "[DEADLOCK_DEATH_HOOK]{\"schema\":1,\"event\":\"ability_cooldown_ready\",\"session_id\":\"session\",\"client_time_ms\":7,\"sequence\":3,\"ability_slot\":2,\"ability_name\":\"Bookwyrm\",\"detection\":\"charge_restored\",\"charges_before\":1,\"charges_after\":2}",
        )
        .expect("valid ability event");
        let (sender, receiver) = mpsc::channel();
        let mut state = AppState {
            bridge_events: Some(receiver),
            ..AppState::default()
        };
        state.triggers.ability_cooldown_ready.trigger.enabled = true;

        sender.send(event).unwrap();
        state.poll_bridge_events();

        assert_eq!(state.last_sequence, Some(("session".to_owned(), 3)));
        assert!(matches!(
            state.shock_status,
            Some(ShockStatus::Skipped { .. })
        ));
        assert_eq!(state.shock_in_flight, 0);
    }

    #[test]
    fn deduplication_advances_before_filtering_and_filters_are_independent() {
        let mut state = AppState::default();
        state.triggers.ability_use.trigger.enabled = true;
        state.triggers.ability_cooldown_ready.trigger.enabled = true;
        state.triggers.ability_use.ability_filter = AbilityFilter::Selected(BTreeSet::new());
        state.triggers.ability_cooldown_ready.ability_filter =
            AbilityFilter::Selected(BTreeSet::from([3]));

        state.queue_trigger_shock(trigger(TriggerKind::AbilityUse, "session", 1));
        assert_eq!(state.last_sequence, Some(("session".to_owned(), 1)));
        assert!(state.shock_status.is_none());

        state.triggers.ability_use.ability_filter = AbilityFilter::All;
        state.queue_trigger_shock(trigger(TriggerKind::AbilityUse, "session", 1));
        assert!(state.shock_status.is_none());
        state.queue_trigger_shock(trigger(TriggerKind::AbilityUse, "session", 2));
        let accepted_use = state.shock_status.clone();
        assert!(matches!(&accepted_use, Some(ShockStatus::Skipped { .. })));

        state.queue_trigger_shock(trigger(TriggerKind::AbilityCooldownReady, "session", 3));
        assert_eq!(state.shock_status, accepted_use);
        assert_eq!(state.last_sequence, Some(("session".to_owned(), 3)));
    }

    #[test]
    fn trigger_routing_resolves_the_selected_profile_before_status_is_recorded() {
        let mut state = AppState::default();
        state.triggers.death.shock.mode = ShockMode::Fixed;
        state.triggers.death.shock.fixed.intensity = 19.0;
        state.triggers.death.shock.fixed.duration_seconds = 0.7;
        state.triggers.ability_use.trigger.enabled = true;
        state.triggers.ability_use.trigger.shock.mode = ShockMode::Fixed;
        state.triggers.ability_use.trigger.shock.fixed.intensity = 73.0;
        state
            .triggers
            .ability_use
            .trigger
            .shock
            .fixed
            .duration_seconds = 2.1;

        state.queue_trigger_shock(trigger(TriggerKind::AbilityUse, "session", 1));
        let Some(status) = state.shock_status.clone() else {
            panic!("enabled trigger should record missing-provider status");
        };
        assert_eq!(
            status.request().resolved,
            Some(ResolvedShock {
                intensity: 73,
                duration_ms: 2_100,
            })
        );

        state.triggers.ability_use.trigger.shock.fixed.intensity = 5.0;
        assert_eq!(status.request().resolved.unwrap().intensity, 73);
    }

    #[test]
    fn ability_catalog_replaces_runtime_names_without_advancing_actionable_state() {
        let (sender, receiver) = mpsc::channel();
        let mut state = AppState {
            bridge_events: Some(receiver),
            ..AppState::default()
        };
        sender
            .send(BridgeEvent::AbilityCatalog(
                crate::bridge_listener::AbilityCatalog {
                    schema: 1,
                    session_id: "session".to_owned(),
                    client_time_ms: 1,
                    abilities: vec![
                        crate::bridge_listener::AbilityCatalogEntry {
                            ability_slot: 1,
                            ability_name: Some("First".to_owned()),
                        },
                        crate::bridge_listener::AbilityCatalogEntry {
                            ability_slot: 5,
                            ability_name: None,
                        },
                    ],
                },
            ))
            .unwrap();
        state.poll_bridge_events();
        assert_eq!(
            state.ability_catalog,
            BTreeMap::from([(1, Some("First".to_owned())), (5, None)])
        );
        assert!(state.last_sequence.is_none());
        assert!(state.shock_status.is_none());

        state.replace_ability_catalog(crate::bridge_listener::AbilityCatalog {
            schema: 1,
            session_id: "session".to_owned(),
            client_time_ms: 2,
            abilities: vec![crate::bridge_listener::AbilityCatalogEntry {
                ability_slot: 2,
                ability_name: Some("Replacement".to_owned()),
            }],
        });
        assert_eq!(
            state.ability_catalog,
            BTreeMap::from([(2, Some("Replacement".to_owned()))])
        );
    }

    #[test]
    fn shock_status_contains_generic_trigger_slot_and_detection_details() {
        let mut ability = trigger(TriggerKind::AbilityCooldownReady, "session", 9);
        ability.detection = "charge_restored".to_owned();
        ability.charges_before = Some(1);
        ability.charges_after = Some(2);
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
            trigger: ability,
        };
        let status = ShockStatus::Sent(request);
        let label = status.label();
        assert!(label.contains("OpenShock"));
        assert!(label.contains("group"));
        assert!(label.contains("20% for 0.5 s"));
        assert!(label.contains("ability cooldown ready slot 2"));
        assert!(label.contains("charge_restored"));
        assert!(label.contains("charges 1→2"));
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
    #[test]
    fn reset_is_blocked_by_each_in_flight_work_kind() {
        let mut connection_busy = AppState {
            username: "keep".to_owned(),
            ..AppState::default()
        };
        let (_sender, receiver) = mpsc::channel();
        connection_busy.connection_result = Some(receiver);
        assert!(!connection_busy.reset_saved_state());
        assert_eq!(connection_busy.username, "keep");

        let mut sound_busy = AppState::default();
        let (_sender, receiver) = mpsc::channel();
        sound_busy.sound_result = Some(receiver);
        assert!(!sound_busy.reset_saved_state());

        let mut shock_busy = AppState {
            shock_in_flight: 1,
            ..AppState::default()
        };
        assert!(!shock_busy.reset_saved_state());
        assert_eq!(shock_busy.shock_in_flight, 1);
    }

    #[test]
    fn confirmed_reset_stops_runtime_state_and_writes_durable_defaults_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let mut app = CompanionApp::load_from_path(path.clone());
        app.state.provider = ProviderKind::OpenShock;
        app.state.openshock_token = "secret".to_owned();
        app.state.preferred_target = Some(TargetId::OpenShock("group".to_owned()));
        app.state.triggers.death.shock.mode = ShockMode::Fixed;
        app.state.triggers.death.shock.fixed.intensity = 80.0;
        app.state.triggers.ability_use.trigger.enabled = true;
        app.state.triggers.ability_use.ability_filter =
            AbilityFilter::Selected(BTreeSet::from([2]));
        app.state.triggers.ability_cooldown_ready.trigger.enabled = true;
        let log_path = directory.path().join("console.log");
        app.state.log_path = log_path.display().to_string();
        app.state.start_log_listener(log_path).unwrap();
        app.state
            .queue_trigger_shock(trigger(TriggerKind::Death, "session", 1));
        assert!(app.state.listener_is_running());
        assert!(!app.state.runtime_trigger_and_shock_state_is_clear());

        assert!(app.reset_and_save());
        assert_eq!(
            PersistedState::from_app(&app.state),
            PersistedState::default()
        );
        assert!(!app.state.listener_is_running());
        assert!(app.state.runtime_trigger_and_shock_state_is_clear());
        assert_eq!(app.state.credential_state, CredentialState::Unknown);
        assert!(app.state.devices.is_empty());
        assert!(app.state.selected_device.is_none());
        let written = std::fs::read_to_string(path).unwrap();
        assert_eq!(
            serde_json::from_str::<PersistedState>(&written).unwrap(),
            PersistedState::default()
        );
    }

    #[test]
    fn persistence_aware_app_renders_with_an_injected_state_path() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = CompanionApp::load_from_path(directory.path().join("state.json"));
        let context = egui::Context::default();
        let output = context.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.draw(ui));
        });
        assert!(!output.shapes.is_empty());
    }
}
