use std::fmt;
use std::time::Duration;

use openshock::{
    Credentials as OpenShockCredentials, DeviceGroup, Error as OpenShockError, OpenShockClient,
};
use pishock::{Credentials as PiShockCredentials, Device, Error as PiShockError, WebSocketClient};
use thiserror::Error;

pub const SENDER_NAME: &str = "deadlockshock-companion";
pub const TEST_SOUND_DURATION: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProviderKind {
    #[default]
    PiShock,
    OpenShock,
}
impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::PiShock => "PiShock",
            Self::OpenShock => "OpenShock",
        }
    }
}

#[derive(Clone, Default)]
pub struct ProviderCredentials {
    pub pishock_username: String,
    pub pishock_api_key: String,
    pub openshock_token: String,
}
impl fmt::Debug for ProviderCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCredentials")
            .field("pishock_username", &self.pishock_username)
            .field("pishock_api_key", &"[REDACTED]")
            .field("openshock_token", &"[REDACTED]")
            .finish()
    }
}
impl ProviderCredentials {
    pub fn present(&self, provider: ProviderKind) -> bool {
        match provider {
            ProviderKind::PiShock => {
                !self.pishock_username.trim().is_empty() && !self.pishock_api_key.trim().is_empty()
            }
            ProviderKind::OpenShock => !self.openshock_token.trim().is_empty(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetId {
    PiShock(u64),
    OpenShock(String),
}

#[derive(Clone)]
enum TargetData {
    PiShock(Device),
    OpenShock(DeviceGroup),
}

#[derive(Clone)]
pub struct ProviderTarget {
    id: TargetId,
    name: String,
    data: Option<TargetData>,
}
impl fmt::Debug for ProviderTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderTarget")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish()
    }
}
impl PartialEq for ProviderTarget {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.name == other.name
    }
}
impl Eq for ProviderTarget {}
impl ProviderTarget {
    pub fn id(&self) -> &TargetId {
        &self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    #[cfg(test)]
    pub(crate) fn new(id: TargetId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            data: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("PiShock: {0}")]
    PiShock(#[from] PiShockError),
    #[error("OpenShock: {0}")]
    OpenShock(#[from] OpenShockError),
    #[error("provider target does not belong to the active provider")]
    TargetProviderMismatch,
    #[error("no provider is connected")]
    NotConnected,
}

pub enum ConnectedProvider {
    PiShock(WebSocketClient),
    OpenShock(OpenShockClient),
}
impl ConnectedProvider {
    pub fn kind(&self) -> ProviderKind {
        match self {
            Self::PiShock(_) => ProviderKind::PiShock,
            Self::OpenShock(_) => ProviderKind::OpenShock,
        }
    }
    pub fn connect(
        kind: ProviderKind,
        credentials: &ProviderCredentials,
    ) -> Result<Self, ProviderError> {
        match kind {
            ProviderKind::PiShock => Ok(Self::PiShock(WebSocketClient::connect(
                PiShockCredentials::new(
                    credentials.pishock_username.trim(),
                    credentials.pishock_api_key.trim(),
                ),
                SENDER_NAME,
            )?)),
            ProviderKind::OpenShock => Ok(Self::OpenShock(OpenShockClient::connect(
                OpenShockCredentials::new(credentials.openshock_token.trim()),
                SENDER_NAME,
            )?)),
        }
    }
    pub fn list_targets(&self) -> Result<Vec<ProviderTarget>, ProviderError> {
        match self {
            Self::PiShock(client) => Ok(client
                .list_devices()?
                .into_iter()
                .map(target_from_pishock)
                .collect()),
            Self::OpenShock(client) => Ok(client
                .list_devices()?
                .into_iter()
                .map(target_from_openshock)
                .collect()),
        }
    }
    pub fn test_sound(&self, target: &ProviderTarget) -> Result<(), ProviderError> {
        if !target_matches(self.kind(), target.id()) {
            return Err(ProviderError::TargetProviderMismatch);
        }
        match (self, target.data.as_ref()) {
            (Self::PiShock(client), Some(TargetData::PiShock(device))) => {
                client.beep_device(device, TEST_SOUND_DURATION.as_secs() as u8)?;
                Ok(())
            }
            (Self::OpenShock(client), Some(TargetData::OpenShock(group))) => {
                client.test_sound(group, TEST_SOUND_DURATION.as_millis() as u64)?;
                Ok(())
            }
            _ => Err(ProviderError::NotConnected),
        }
    }
    pub fn disconnect(self) -> Result<(), ProviderError> {
        match self {
            Self::PiShock(_) => Ok(()),
            Self::OpenShock(client) => Ok(client.disconnect()?),
        }
    }
}
fn target_matches(provider: ProviderKind, target: &TargetId) -> bool {
    matches!(
        (provider, target),
        (ProviderKind::PiShock, TargetId::PiShock(_))
            | (ProviderKind::OpenShock, TargetId::OpenShock(_))
    )
}
fn target_from_pishock(device: Device) -> ProviderTarget {
    ProviderTarget {
        id: TargetId::PiShock(device.client_id),
        name: device.name.clone(),
        data: Some(TargetData::PiShock(device)),
    }
}
fn target_from_openshock(group: DeviceGroup) -> ProviderTarget {
    ProviderTarget {
        id: TargetId::OpenShock(group.id.clone()),
        name: group.name.clone(),
        data: Some(TargetData::OpenShock(group)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_provider_is_pishock() {
        assert_eq!(ProviderKind::default(), ProviderKind::PiShock);
    }
    #[test]
    fn credentials_are_provider_specific() {
        let mut credentials = ProviderCredentials::default();
        assert!(!credentials.present(ProviderKind::PiShock));
        assert!(!credentials.present(ProviderKind::OpenShock));
        credentials.pishock_username = "user".into();
        credentials.pishock_api_key = "key".into();
        assert!(credentials.present(ProviderKind::PiShock));
        assert!(!credentials.present(ProviderKind::OpenShock));
        credentials.openshock_token = "token".into();
        assert!(credentials.present(ProviderKind::OpenShock));
    }
    #[test]
    fn target_mismatch_is_rejected() {
        assert!(!target_matches(
            ProviderKind::PiShock,
            &TargetId::OpenShock("group".into())
        ));
        assert!(target_matches(
            ProviderKind::OpenShock,
            &TargetId::OpenShock("group".into())
        ));
    }
    #[test]
    fn credentials_debug_redacts_secrets() {
        let credentials = ProviderCredentials {
            pishock_username: "account".into(),
            pishock_api_key: "api-secret-123".into(),
            openshock_token: "token-secret-456".into(),
        };
        let rendered = format!("{credentials:?}");
        assert!(!rendered.contains("api-secret-123"));
        assert!(!rendered.contains("token-secret-456"));
        assert!(rendered.contains("account"));
    }
}
