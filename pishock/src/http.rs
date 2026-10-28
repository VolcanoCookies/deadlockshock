//! PiShock's API-key-only public HTTP API.
//!
//! Protocol reference: <https://api.pishock.com/swagger/v1/swagger.json>.
//! Methods block and must be called away from the GUI thread.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use reqwest::Url;
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::{Command, Error, build_http_client, expect_status};

const API_URL: &str = "https://api.pishock.com";
const API_KEY_HEADER: &str = "X-PiShock-Api-Key";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const BY_ID: &str = "/Shockers/OperateById/{ShockerId}";
const BY_SHARE: &str = "/Shockers/OperateByShare/{ShareCode}";

/// An owned or shared hub and its shockers accessible through claimed shares.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceGroup {
    pub client_id: u64,
    pub name: String,
    pub shockers: Vec<SharedShocker>,
}

/// A shocker and the permissions granted by one claimed share.
/// Multiple shares for a shocker remain separate so their limits are not combined.
#[derive(Clone, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct SharedShocker {
    pub client_id: u64,
    pub id: u64,
    pub name: String,
    pub paused: bool,
    pub share_paused: bool,
    pub locked: bool,
    pub is_v3: bool,
    pub can_shock: bool,
    pub can_vibrate: bool,
    pub can_beep: bool,
    pub max_intensity: u8,
    pub max_duration: u64,
    share_code: String,
}

impl fmt::Debug for SharedShocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedShocker")
            .field("client_id", &self.client_id)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("share_code", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl SharedShocker {
    fn available(&self) -> bool {
        !self.paused && !self.share_paused && !self.locked
    }

    fn validate_command(&self, operation: u8, intensity: u8, duration: u64) -> Result<(), Error> {
        if !self.is_v3 {
            return Err(Error::UnsupportedFirmware);
        }
        let allowed = match operation {
            0 => self.can_shock,
            1 => self.can_vibrate,
            2 => self.can_beep,
            _ => false,
        };
        if !allowed {
            return Err(Error::OperationNotAllowed);
        }
        if operation == 0 && intensity > self.max_intensity {
            return Err(Error::IntensityRejected {
                message: "exceeds the share limit".into(),
            });
        }
        if duration > self.max_duration {
            return Err(Error::DurationRejected {
                message: "exceeds the share limit".into(),
            });
        }
        Ok(())
    }
}

/// A client authenticated with the X-PiShock-Api-Key header alone.
pub struct HttpClient {
    http: Client,
    headers: HeaderMap,
    sender: String,
    base_url: String,
}

impl HttpClient {
    /// Checks the API key using GET /Account, without operating any device.
    pub fn connect(api_key: impl AsRef<str>, sender: impl Into<String>) -> Result<Self, Error> {
        Self::connect_to(api_key.as_ref(), sender.into(), API_URL.to_owned())
    }

    fn connect_to(api_key: &str, sender: String, base_url: String) -> Result<Self, Error> {
        let api_key = api_key.trim();
        let sender = sender.trim().to_owned();
        if api_key.is_empty() {
            return Err(Error::EmptyApiKey);
        }
        if sender.is_empty() {
            return Err(Error::EmptySender);
        }
        let mut key = HeaderValue::from_str(api_key).map_err(|_| Error::Transport)?;
        key.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(API_KEY_HEADER, key);
        let client = Self {
            http: build_http_client()?,
            headers,
            sender,
            base_url,
        };
        let _: AccountResponse = client.get("/Account")?.json().map_err(|_| Error::Decode {
            operation: "authentication",
        })?;
        Ok(client)
    }

    /// Lists owned and shared hubs, preserving hub IDs used by saved selections.
    /// Shockers must have a claimed share, including ones owned by the account.
    pub fn list_devices(&self) -> Result<Vec<DeviceGroup>, Error> {
        let hubs: Vec<HubResponse> = self.get("/Hub")?.json().map_err(|_| Error::Decode {
            operation: "device listing",
        })?;
        let mut devices: Vec<DeviceGroup> = hubs
            .into_iter()
            .map(|hub| DeviceGroup {
                client_id: hub.hub_id,
                name: hub.name,
                shockers: Vec::new(),
            })
            .collect();
        for share in self.list_shockers()? {
            let index = devices
                .iter()
                .position(|hub| hub.client_id == share.client_id)
                .unwrap_or_else(|| {
                    devices.push(DeviceGroup {
                        client_id: share.client_id,
                        name: format!("Shared PiShock hub {}", share.client_id),
                        shockers: Vec::new(),
                    });
                    devices.len() - 1
                });
            let shockers = &mut devices[index].shockers;
            if !shockers
                .iter()
                .any(|existing| existing.id == share.id && existing.share_code == share.share_code)
            {
                shockers.push(share);
            }
        }
        Ok(devices)
    }

    pub fn get_device(&self, client_id: u64) -> Result<Option<DeviceGroup>, Error> {
        Ok(self
            .list_devices()?
            .into_iter()
            .find(|device| device.client_id == client_id))
    }

    /// Returns the documented shared-shocker records, including permissions and limits.
    pub fn list_shockers(&self) -> Result<Vec<SharedShocker>, Error> {
        let mut shares: Vec<SharedShocker> =
            self.get("/Share/GetShared")?
                .json()
                .map_err(|_| Error::Decode {
                    operation: "shocker listing",
                })?;
        for share in &mut shares {
            share.share_code = normalize_share_code(&share.share_code)
                .map_err(|_| Error::Decode {
                    operation: "shocker listing",
                })?
                .to_owned();
        }
        Ok(shares)
    }

    /// Looks up a claimed share without the obsolete GetShockerInfo request.
    pub fn get_shocker_info(&self, share_code: &str) -> Result<SharedShocker, Error> {
        let share_code = normalize_share_code(share_code)?;
        self.list_shockers()?
            .into_iter()
            .find(|share| share.share_code == share_code)
            .ok_or(Error::ShareCodeNotFound)
    }

    /// Sends one command by shocker ID. The server enforces the applicable share.
    pub fn send_command_by_id(&self, shocker_id: u64, command: Command) -> Result<(), Error> {
        self.send_to("OperateById", &shocker_id.to_string(), BY_ID, command)
    }

    /// Sends one command through a specific share, using its exact permissions.
    pub fn send_command(&self, share_code: &str, command: Command) -> Result<(), Error> {
        let share_code = normalize_share_code(share_code)?;
        self.send_to("OperateByShare", share_code, BY_SHARE, command)
    }

    pub fn shock(&self, share_code: &str, intensity: u8, duration_ms: u64) -> Result<(), Error> {
        self.send_command(
            share_code,
            Command::Shock {
                intensity,
                duration_ms,
            },
        )
    }

    pub fn vibrate(&self, share_code: &str, intensity: u8, duration_ms: u64) -> Result<(), Error> {
        self.send_command(
            share_code,
            Command::Vibrate {
                intensity,
                duration_ms,
            },
        )
    }

    pub fn beep(&self, share_code: &str, duration_ms: u64) -> Result<(), Error> {
        self.send_command(share_code, Command::Beep { duration_ms })
    }

    pub fn beep_device(&self, device: &DeviceGroup, duration_ms: u64) -> Result<(), Error> {
        self.operate_device(device, Command::Beep { duration_ms })
    }

    pub fn vibrate_device(
        &self,
        device: &DeviceGroup,
        intensity: u8,
        duration_ms: u64,
    ) -> Result<(), Error> {
        self.operate_device(
            device,
            Command::Vibrate {
                intensity,
                duration_ms,
            },
        )
    }

    pub fn shock_device(
        &self,
        device: &DeviceGroup,
        intensity: u8,
        duration_ms: u64,
    ) -> Result<(), Error> {
        self.operate_device(
            device,
            Command::Shock {
                intensity,
                duration_ms,
            },
        )
    }

    fn operate_device(&self, device: &DeviceGroup, command: Command) -> Result<(), Error> {
        let (operation, intensity, duration) = command.parameters()?;
        let mut alternatives: BTreeMap<u64, Vec<&SharedShocker>> = BTreeMap::new();
        for share in device.shockers.iter().filter(|share| share.available()) {
            alternatives.entry(share.id).or_default().push(share);
        }
        if alternatives.is_empty() {
            return Err(Error::NoAvailableShockers);
        }

        // Preflight the entire group before its first POST. Select one complete
        // share per shocker rather than combining limits from different shares.
        let mut selected = Vec::new();
        for shares in alternatives.values() {
            let mut rejection = Error::NoAvailableShockers;
            let mut permitted = None;
            for share in shares {
                match share.validate_command(operation, intensity, duration) {
                    Ok(()) => {
                        permitted = Some(*share);
                        break;
                    }
                    Err(error) => rejection = error,
                }
            }
            selected.push(permitted.ok_or(rejection)?);
        }
        for share in selected {
            // One POST per physical shocker, never replayed after failure.
            self.send_command(&share.share_code, command)?;
        }
        Ok(())
    }

    fn send_to(
        &self,
        route: &str,
        target: &str,
        endpoint: &'static str,
        command: Command,
    ) -> Result<(), Error> {
        let (operation, intensity, duration) = command.parameters()?;
        let mut url = Url::parse(&self.base_url).map_err(|_| Error::Transport)?;
        url.path_segments_mut()
            .map_err(|_| Error::Transport)?
            .push("Shockers")
            .push(route)
            .push(target);
        let response = self
            .http
            .post(url)
            .headers(self.headers.clone())
            .timeout(COMMAND_TIMEOUT)
            .json(&OperateRequest {
                agent_name: &self.sender,
                operation,
                duration,
                intensity,
                intensity_as_percentage: false,
            })
            .send()
            .map_err(|_| Error::Transport)?;
        command_status(response, endpoint)
    }

    fn get(&self, path: &'static str) -> Result<Response, Error> {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .headers(self.headers.clone())
            .send()
            .map_err(|_| Error::Transport)?;
        expect_status(response, "GET", path)
    }
}

fn normalize_share_code(share_code: &str) -> Result<&str, Error> {
    let share_code = share_code.trim();
    if share_code.is_empty() {
        Err(Error::EmptyShareCode)
    } else {
        Ok(share_code)
    }
}

fn command_status(response: Response, endpoint: &'static str) -> Result<(), Error> {
    match response.status().as_u16() {
        204 => Ok(()),
        401 => Err(Error::ApiKeyRejected {
            method: "POST",
            endpoint,
        }),
        403 => Err(Error::ApiAccessDenied {
            method: "POST",
            endpoint,
        }),
        404 => Err(Error::ShareCodeNotFound),
        405 => Err(Error::OperationNotAllowed),
        406 => Err(Error::UnsupportedFirmware),
        410 => Err(Error::ShareLocked),
        412 => Err(Error::IntensityRejected {
            message: "exceeds the share limit".into(),
        }),
        416 => Err(Error::DurationRejected {
            message: "exceeds the share limit".into(),
        }),
        503 => Err(Error::ShockerPaused),
        status => Err(Error::HttpStatus {
            operation: endpoint,
            status,
        }),
    }
}

#[derive(Deserialize)]
struct AccountResponse {
    #[serde(rename = "UserId")]
    _user_id: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct HubResponse {
    hub_id: u64,
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct OperateRequest<'a> {
    agent_name: &'a str,
    operation: u8,
    duration: u64,
    intensity: u8,
    intensity_as_percentage: bool,
}

#[cfg(test)]
mod tests;
