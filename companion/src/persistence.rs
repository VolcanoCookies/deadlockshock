use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::app::{
    AppState, MAX_SHOCK_DURATION, MAX_SHOCK_INTENSITY, MIN_SHOCK_DURATION, MIN_SHOCK_INTENSITY,
    ShockMode,
};
use crate::provider::{ProviderKind, TargetId};

pub const SCHEMA_VERSION: u32 = 1;
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedState {
    schema_version: u32,
    provider: PersistedProvider,
    credentials: PersistedCredentials,
    preferred_target: Option<PersistedTarget>,
    shock: PersistedShock,
    log_path: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PersistedProvider {
    PiShock,
    OpenShock,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedCredentials {
    pishock: PersistedPiShockCredentials,
    openshock: PersistedOpenShockCredentials,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedPiShockCredentials {
    username: String,
    api_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedOpenShockCredentials {
    token: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedTarget {
    provider: PersistedProvider,
    id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedShock {
    mode: PersistedShockMode,
    interval: PersistedShockInterval,
    fixed: PersistedShockFixed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PersistedShockMode {
    Interval,
    Fixed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedShockInterval {
    minimum_intensity: f32,
    maximum_intensity: f32,
    minimum_duration_seconds: f32,
    maximum_duration_seconds: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedShockFixed {
    intensity: f32,
    duration_seconds: f32,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            provider: PersistedProvider::PiShock,
            credentials: PersistedCredentials {
                pishock: PersistedPiShockCredentials {
                    username: String::new(),
                    api_key: String::new(),
                },
                openshock: PersistedOpenShockCredentials {
                    token: String::new(),
                },
            },
            preferred_target: None,
            shock: PersistedShock {
                mode: PersistedShockMode::Interval,
                interval: PersistedShockInterval {
                    minimum_intensity: MIN_SHOCK_INTENSITY,
                    maximum_intensity: MIN_SHOCK_INTENSITY,
                    minimum_duration_seconds: MIN_SHOCK_DURATION,
                    maximum_duration_seconds: MIN_SHOCK_DURATION,
                },
                fixed: PersistedShockFixed {
                    intensity: MIN_SHOCK_INTENSITY,
                    duration_seconds: MIN_SHOCK_DURATION,
                },
            },
            log_path: String::new(),
        }
    }
}

impl PersistedState {
    pub(crate) fn from_app(app: &AppState) -> Self {
        let preferred_target = app
            .preferred_target
            .as_ref()
            .map(PersistedTarget::from_target_id);
        let state = Self {
            schema_version: SCHEMA_VERSION,
            provider: app.provider.into(),
            credentials: PersistedCredentials {
                pishock: PersistedPiShockCredentials {
                    username: app.username.clone(),
                    api_key: app.api_key.clone(),
                },
                openshock: PersistedOpenShockCredentials {
                    token: app.openshock_token.clone(),
                },
            },
            preferred_target,
            shock: PersistedShock {
                mode: app.shock_mode.into(),
                interval: PersistedShockInterval {
                    minimum_intensity: app.min_intensity,
                    maximum_intensity: app.max_intensity,
                    minimum_duration_seconds: app.min_duration,
                    maximum_duration_seconds: app.max_duration,
                },
                fixed: PersistedShockFixed {
                    intensity: app.intensity,
                    duration_seconds: app.duration,
                },
            },
            log_path: app.log_path.clone(),
        };
        state.normalized().unwrap_or_default()
    }

    pub(crate) fn restore_app(&self) -> AppState {
        let mut app = AppState::default();
        app.provider = self.provider.into();
        app.username = self.credentials.pishock.username.clone();
        app.api_key = self.credentials.pishock.api_key.clone();
        app.openshock_token = self.credentials.openshock.token.clone();
        app.preferred_target = self
            .preferred_target
            .as_ref()
            .and_then(|target| target.to_target_id().ok());
        app.shock_mode = self.shock.mode.into();
        app.min_intensity = self.shock.interval.minimum_intensity;
        app.max_intensity = self.shock.interval.maximum_intensity;
        app.min_duration = self.shock.interval.minimum_duration_seconds;
        app.max_duration = self.shock.interval.maximum_duration_seconds;
        app.intensity = self.shock.fixed.intensity;
        app.duration = self.shock.fixed.duration_seconds;
        app.log_path = self.log_path.clone();
        app
    }

    fn normalized(mut self) -> Result<Self, String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema version {}; expected {SCHEMA_VERSION}",
                self.schema_version
            ));
        }

        self.shock.interval.minimum_intensity = normalize_value(
            self.shock.interval.minimum_intensity,
            MIN_SHOCK_INTENSITY,
            MAX_SHOCK_INTENSITY,
            MIN_SHOCK_INTENSITY,
        );
        self.shock.interval.maximum_intensity = normalize_value(
            self.shock.interval.maximum_intensity,
            MIN_SHOCK_INTENSITY,
            MAX_SHOCK_INTENSITY,
            MIN_SHOCK_INTENSITY,
        )
        .max(self.shock.interval.minimum_intensity);
        self.shock.fixed.intensity = normalize_value(
            self.shock.fixed.intensity,
            MIN_SHOCK_INTENSITY,
            MAX_SHOCK_INTENSITY,
            MIN_SHOCK_INTENSITY,
        );
        self.shock.interval.minimum_duration_seconds = normalize_value(
            self.shock.interval.minimum_duration_seconds,
            MIN_SHOCK_DURATION,
            MAX_SHOCK_DURATION,
            MIN_SHOCK_DURATION,
        );
        self.shock.interval.maximum_duration_seconds = normalize_value(
            self.shock.interval.maximum_duration_seconds,
            MIN_SHOCK_DURATION,
            MAX_SHOCK_DURATION,
            MIN_SHOCK_DURATION,
        )
        .max(self.shock.interval.minimum_duration_seconds);
        self.shock.fixed.duration_seconds = normalize_value(
            self.shock.fixed.duration_seconds,
            MIN_SHOCK_DURATION,
            MAX_SHOCK_DURATION,
            MIN_SHOCK_DURATION,
        );

        if let Some(target) = &self.preferred_target {
            let canonical = PersistedTarget::from_target_id(&target.to_target_id()?);
            self.preferred_target = (canonical.provider == self.provider).then_some(canonical);
        }

        Ok(self)
    }
}

impl From<ProviderKind> for PersistedProvider {
    fn from(provider: ProviderKind) -> Self {
        match provider {
            ProviderKind::PiShock => Self::PiShock,
            ProviderKind::OpenShock => Self::OpenShock,
        }
    }
}

impl From<PersistedProvider> for ProviderKind {
    fn from(provider: PersistedProvider) -> Self {
        match provider {
            PersistedProvider::PiShock => Self::PiShock,
            PersistedProvider::OpenShock => Self::OpenShock,
        }
    }
}

impl From<ShockMode> for PersistedShockMode {
    fn from(mode: ShockMode) -> Self {
        match mode {
            ShockMode::Interval => Self::Interval,
            ShockMode::Fixed => Self::Fixed,
        }
    }
}

impl From<PersistedShockMode> for ShockMode {
    fn from(mode: PersistedShockMode) -> Self {
        match mode {
            PersistedShockMode::Interval => Self::Interval,
            PersistedShockMode::Fixed => Self::Fixed,
        }
    }
}

impl PersistedTarget {
    fn from_target_id(target: &TargetId) -> Self {
        match target {
            TargetId::PiShock(id) => Self {
                provider: PersistedProvider::PiShock,
                id: id.to_string(),
            },
            TargetId::OpenShock(id) => Self {
                provider: PersistedProvider::OpenShock,
                id: id.clone(),
            },
        }
    }

    fn to_target_id(&self) -> Result<TargetId, String> {
        match self.provider {
            PersistedProvider::PiShock => {
                self.id.parse::<u64>().map(TargetId::PiShock).map_err(|_| {
                    "PiShock preferred target ID is not an unsigned integer".to_owned()
                })
            }
            PersistedProvider::OpenShock if self.id.trim().is_empty() => {
                Err("OpenShock preferred target ID is empty".to_owned())
            }
            PersistedProvider::OpenShock => Ok(TargetId::OpenShock(self.id.clone())),
        }
    }
}

fn normalize_value(value: f32, minimum: f32, maximum: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(minimum, maximum)
    } else {
        fallback
    }
}

pub(crate) struct LoadOutcome {
    pub state: PersistedState,
    pub warning: Option<String>,
}

pub(crate) fn default_state_path() -> Result<PathBuf, String> {
    let result = dirs::config_dir()
        .map(|directory| directory.join("deadlockshock-companion").join("state.json"))
        .ok_or_else(|| {
            "The operating system did not provide a per-user config directory.".to_owned()
        });
    if let Ok(path) = &result {
        log::info!(
            target: "companion::persistence",
            "settings_path_resolved path={:?}",
            path
        );
    }
    result
}

pub(crate) fn load_from_path(path: &Path) -> LoadOutcome {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            log::info!(
                target: "companion::persistence",
                "settings_load_outcome path={:?} outcome=missing_defaults",
                path
            );
            return LoadOutcome {
                state: PersistedState::default(),
                warning: None,
            };
        }
        Err(error) => {
            log::warn!(
                target: "companion::persistence",
                "settings_load_failed path={:?} stage=read error={:?}",
                path,
                error
            );
            return LoadOutcome {
                state: PersistedState::default(),
                warning: Some(format!(
                    "Could not read saved state at {}: {error}. Defaults were restored.",
                    path.display()
                )),
            };
        }
    };

    let loaded = serde_json::from_str::<PersistedState>(&source)
        .map_err(|error| error.to_string())
        .and_then(PersistedState::normalized);
    match loaded {
        Ok(state) => {
            log::info!(
                target: "companion::persistence",
                "settings_load_outcome path={:?} outcome=loaded",
                path
            );
            LoadOutcome {
                state,
                warning: None,
            }
        }
        Err(error) => {
            let preservation = match preserve_invalid_file(path) {
                Ok(backup) => {
                    log::warn!(
                        target: "companion::persistence",
                        "settings_load_failed path={:?} stage=parse backup={:?}",
                        path,
                        backup
                    );
                    format!("The invalid file was preserved at {}.", backup.display())
                }
                Err(backup_error) => {
                    log::warn!(
                        target: "companion::persistence",
                        "settings_load_failed path={:?} stage=parse backup_failed error={:?}",
                        path,
                        backup_error
                    );
                    format!(
                        "The invalid file could not be moved to a backup ({backup_error}); it remains at {}.",
                        path.display()
                    )
                }
            };
            LoadOutcome {
                state: PersistedState::default(),
                warning: Some(format!(
                    "Saved state was invalid ({error}). {preservation} Defaults were restored."
                )),
            }
        }
    }
}

fn preserve_invalid_file(path: &Path) -> io::Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("state");
    let extension = path.extension().and_then(|extension| extension.to_str());
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    for collision in 0_u32.. {
        let suffix = if collision == 0 {
            String::new()
        } else {
            format!("-{collision}")
        };
        let mut file_name = format!(
            "{stem}.invalid-{}-{:09}{suffix}",
            timestamp.as_secs(),
            timestamp.subsec_nanos()
        );
        if let Some(extension) = extension {
            file_name.push('.');
            file_name.push_str(extension);
        }
        let backup = parent.join(file_name);
        if !backup.exists() {
            fs::rename(path, &backup)?;
            return Ok(backup);
        }
    }
    unreachable!("the invalid-state backup suffix space is inexhaustible")
}

fn write_state(path: &Path, state: &PersistedState) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Could not create the saved-state directory {}: {error}",
            parent.display()
        )
    })?;
    set_private_directory_permissions(parent)?;

    let mut temporary = NamedTempFile::new_in(parent).map_err(|error| {
        format!(
            "Could not create a temporary saved-state file in {}: {error}",
            parent.display()
        )
    })?;
    set_private_file_permissions(temporary.as_file())?;
    serde_json::to_writer_pretty(&mut temporary, state)
        .map_err(|error| format!("Could not serialize saved state: {error}"))?;
    temporary
        .write_all(b"\n")
        .map_err(|error| format!("Could not finish writing saved state: {error}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("Could not synchronize saved state: {error}"))?;
    temporary.persist(path).map_err(|error| {
        format!(
            "Could not atomically replace {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "Could not restrict saved-state directory permissions for {}: {error}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &fs::File) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not restrict saved-state file permissions: {error}"))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &fs::File) -> Result<(), String> {
    Ok(())
}

#[derive(Clone, Copy)]
enum SaveReason {
    Autosave,
    Reset,
}

pub(crate) struct Persistence {
    path: Option<PathBuf>,
    saved: PersistedState,
    observed: PersistedState,
    pending: Option<PersistedState>,
    pending_reason: SaveReason,
    deadline: Option<Instant>,
    debounce: Duration,
    load_warning: Option<String>,
    save_error: Option<String>,
}

impl Persistence {
    pub(crate) fn open(path: PathBuf) -> (Self, PersistedState) {
        let outcome = load_from_path(&path);
        let state = outcome.state;
        log::info!(
            target: "companion::persistence",
            "settings_opened path={:?} load_warning={}",
            path,
            outcome.warning.is_some()
        );
        (
            Self {
                path: Some(path),
                saved: state.clone(),
                observed: state.clone(),
                pending: None,
                pending_reason: SaveReason::Autosave,
                deadline: None,
                debounce: SAVE_DEBOUNCE,
                load_warning: outcome.warning,
                save_error: None,
            },
            state,
        )
    }

    pub(crate) fn unavailable(message: String) -> (Self, PersistedState) {
        log::warn!(
            target: "companion::persistence",
            "settings_unavailable reason={:?}",
            message
        );
        let state = PersistedState::default();
        (
            Self {
                path: None,
                saved: state.clone(),
                observed: state.clone(),
                pending: None,
                pending_reason: SaveReason::Autosave,
                deadline: None,
                debounce: SAVE_DEBOUNCE,
                load_warning: Some(format!(
                    "Saved state is unavailable: {message} Settings will remain in memory for this session."
                )),
                save_error: None,
            },
            state,
        )
    }

    pub(crate) fn load_warning(&self) -> Option<&str> {
        self.load_warning.as_deref()
    }

    pub(crate) fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    pub(crate) fn observe(&mut self, state: PersistedState, now: Instant) -> Option<Duration> {
        if state != self.observed {
            self.observed = state.clone();
            if state == self.saved {
                self.pending = None;
                self.deadline = None;
                self.save_error = None;
                log::debug!(
                    target: "companion::persistence",
                    "settings_autosave_cancelled reason=reverted_to_saved"
                );
            } else {
                let coalesced = self.pending.is_some();
                self.pending = Some(state);
                self.pending_reason = SaveReason::Autosave;
                self.deadline = Some(now + self.debounce);
                log::debug!(
                    target: "companion::persistence",
                    "settings_autosave_scheduled coalesced={coalesced}"
                );
            }
        } else if self.pending.is_some() && self.deadline.is_none() {
            self.deadline = Some(now + self.debounce);
            log::debug!(
                target: "companion::persistence",
                "settings_autosave_rescheduled"
            );
        }

        if self.deadline.is_some_and(|deadline| deadline <= now) {
            let state = self
                .pending
                .clone()
                .expect("a save deadline requires pending state");
            let reason = self.pending_reason;
            if self.commit(state, reason).is_err() {
                self.deadline = Some(now + self.debounce);
            }
        }

        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    pub(crate) fn save_reset_now(&mut self, state: PersistedState) -> Result<(), ()> {
        log::info!(target: "companion::persistence", "settings_reset_save_started");
        self.observed = state.clone();
        self.commit(state, SaveReason::Reset)
    }

    pub(crate) fn flush(&mut self, state: PersistedState) -> Result<(), ()> {
        if state == self.saved && self.pending.is_none() {
            log::debug!(
                target: "companion::persistence",
                "settings_flush_noop reason=clean"
            );
            return Ok(());
        }
        log::debug!(target: "companion::persistence", "settings_flush_started");
        self.observed = state.clone();
        let reason = self
            .pending
            .as_ref()
            .map(|_| self.pending_reason)
            .unwrap_or(SaveReason::Autosave);
        self.commit(state, reason)
    }

    fn commit(&mut self, state: PersistedState, reason: SaveReason) -> Result<(), ()> {
        let result = self
            .path
            .as_deref()
            .ok_or_else(|| "No per-user saved-state path is available.".to_owned())
            .and_then(|path| write_state(path, &state));
        match result {
            Ok(()) => {
                let recovered = self.save_error.is_some();
                self.saved = state;
                self.pending = None;
                self.deadline = None;
                self.save_error = None;
                log::info!(
                    target: "companion::persistence",
                    "settings_save_committed reason={} recovered={}",
                    match reason {
                        SaveReason::Autosave => "autosave",
                        SaveReason::Reset => "reset",
                    },
                    recovered
                );
                Ok(())
            }
            Err(error) => {
                let first_failure = self.save_error.is_none();
                self.pending = Some(state);
                self.pending_reason = reason;
                self.deadline = None;
                self.save_error = Some(match reason {
                    SaveReason::Autosave => format!(
                        "Could not save settings: {error} Changes remain unsaved and will be retried."
                    ),
                    SaveReason::Reset => format!(
                        "Current settings were reset in memory, but saved state could not be replaced: {error} The previous disk state may return after restart."
                    ),
                });
                if first_failure {
                    log::warn!(
                        target: "companion::persistence",
                        "settings_save_failed reason={} error={:?}",
                        match reason {
                            SaveReason::Autosave => "autosave",
                            SaveReason::Reset => "reset",
                        },
                        error
                    );
                }
                Err(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{CredentialState, ShockMode};
    use crate::provider::ProviderTarget;

    #[test]
    fn json_roundtrip_is_readable_versioned_and_restores_only_durable_state() {
        let mut original = AppState::default();
        original.provider = ProviderKind::OpenShock;
        original.username = "pi-user".to_owned();
        original.api_key = "pi-key".to_owned();
        original.openshock_token = "open-token".to_owned();
        original.preferred_target = Some(TargetId::OpenShock("group-id".to_owned()));
        original.shock_mode = ShockMode::Fixed;
        original.min_intensity = 11.0;
        original.max_intensity = 72.0;
        original.intensity = 43.0;
        original.min_duration = 0.5;
        original.max_duration = 2.6;
        original.duration = 1.4;
        original.log_path = "/logs/console.log".to_owned();
        original.credential_state = CredentialState::Valid;
        original.devices = vec![ProviderTarget::new(
            TargetId::OpenShock("group-id".to_owned()),
            "Fetched group",
        )];
        original.selected_device = Some(TargetId::OpenShock("group-id".to_owned()));
        let persisted = PersistedState::from_app(&original);
        let json = serde_json::to_string_pretty(&persisted).unwrap();
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"provider\": \"openshock\""));
        assert!(json.contains("\"id\": \"group-id\""));
        assert!(json.contains("\"interval\""));
        assert!(json.contains("\"fixed\""));
        assert!(!json.contains("Fetched group"));

        let decoded = serde_json::from_str::<PersistedState>(&json)
            .unwrap()
            .normalized()
            .unwrap();
        assert_eq!(decoded, persisted);
        let restored = decoded.restore_app();
        assert_eq!(PersistedState::from_app(&restored), persisted);
        assert_eq!(restored.credential_state, CredentialState::Unknown);
        assert!(restored.devices.is_empty());
        assert!(restored.selected_device.is_none());
        assert!(!restored.listener_is_running());
        assert!(restored.runtime_death_and_shock_state_is_clear());
    }

    #[test]
    fn missing_file_loads_current_defaults_without_warning() {
        let directory = tempfile::tempdir().unwrap();
        let outcome = load_from_path(&directory.path().join("state.json"));
        assert_eq!(outcome.state, PersistedState::default());
        assert!(outcome.warning.is_none());
    }

    #[test]
    fn corrupt_file_is_preserved_before_defaults_are_returned() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(&path, "{not json").unwrap();

        let outcome = load_from_path(&path);
        assert_eq!(outcome.state, PersistedState::default());
        assert!(outcome.warning.as_deref().unwrap().contains("preserved"));
        assert!(!path.exists());
        let backups = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert!(
            backups[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("state.invalid-")
        );
        assert_eq!(fs::read_to_string(&backups[0]).unwrap(), "{not json");
    }

    #[test]
    fn loaded_shock_values_are_normalized_and_intervals_are_ordered() {
        let mut state = PersistedState::default();
        state.shock.interval.minimum_intensity = 120.0;
        state.shock.interval.maximum_intensity = -5.0;
        state.shock.interval.minimum_duration_seconds = 2.7;
        state.shock.interval.maximum_duration_seconds = 0.1;
        state.shock.fixed.intensity = -1.0;
        state.shock.fixed.duration_seconds = 9.0;

        let normalized = state.normalized().unwrap();
        assert_eq!(normalized.shock.interval.minimum_intensity, 100.0);
        assert_eq!(normalized.shock.interval.maximum_intensity, 100.0);
        assert_eq!(normalized.shock.interval.minimum_duration_seconds, 2.7);
        assert_eq!(normalized.shock.interval.maximum_duration_seconds, 2.7);
        assert_eq!(normalized.shock.fixed.intensity, 1.0);
        assert_eq!(normalized.shock.fixed.duration_seconds, 3.0);
    }

    #[test]
    fn debounce_writes_once_and_save_now_flushes_immediately() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let (mut persistence, initial) = Persistence::open(path.clone());
        let mut changed = initial.clone();
        changed.log_path = "/changed".to_owned();
        let start = Instant::now();

        assert_eq!(
            persistence.observe(changed.clone(), start),
            Some(SAVE_DEBOUNCE)
        );
        assert!(!path.exists());
        persistence.observe(changed.clone(), start + SAVE_DEBOUNCE / 2);
        assert!(!path.exists());
        assert_eq!(
            persistence.observe(changed.clone(), start + SAVE_DEBOUNCE),
            None
        );
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<PersistedState>(&written).unwrap(),
            changed
        );

        changed.log_path = "/exit-flush".to_owned();
        persistence.observe(changed.clone(), start + SAVE_DEBOUNCE * 2);
        persistence.flush(changed.clone()).unwrap();
        assert_eq!(
            serde_json::from_str::<PersistedState>(&fs::read_to_string(&path).unwrap()).unwrap(),
            changed
        );
        changed.log_path = "/save-now".to_owned();
        persistence.save_reset_now(changed.clone()).unwrap();
        assert_eq!(
            serde_json::from_str::<PersistedState>(&fs::read_to_string(path).unwrap()).unwrap(),
            changed
        );
    }

    #[test]
    fn unsupported_schema_is_backed_up_like_malformed_json() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let state = PersistedState {
            schema_version: SCHEMA_VERSION + 1,
            ..PersistedState::default()
        };
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();

        let outcome = load_from_path(&path);
        assert_eq!(outcome.state, PersistedState::default());
        assert!(
            outcome
                .warning
                .unwrap()
                .contains("unsupported schema version")
        );
        assert!(!path.exists());
    }

    #[test]
    fn failed_reset_save_stays_dirty_and_reports_that_disk_state_may_return() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::create_dir(&path).unwrap();
        let (mut persistence, mut state) = Persistence::open(path);
        state.log_path = "/reset-in-memory".to_owned();

        assert!(persistence.save_reset_now(state.clone()).is_err());
        assert_eq!(persistence.pending, Some(state));
        assert!(
            persistence
                .save_error()
                .unwrap()
                .contains("previous disk state may return")
        );
    }
}
