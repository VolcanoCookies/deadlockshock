//! A small synchronous client for PiShock's current discovery APIs and legacy
//! share-code API.
//!
//! A [`Device`] is a PiShock hub and contains its owned [`Shocker`]s. Legacy
//! information and commands instead address a shocker through a share code.
//! Every client method blocks the calling thread; GUI applications should call
//! it away from the UI thread.
//!
//! # Example
//!
//! ```no_run
//! use pishock::{Credentials, PiShockClient};
//!
//! let credentials = Credentials::new("username", "api-key");
//! let client = PiShockClient::connect(credentials, "deadlockshock-companion")?;
//! let devices = client.list_devices()?;
//! if let Some(device) = devices.first() {
//!     println!("{} has {} shockers", device.name, device.shockers.len());
//! }
//! client.beep("share-code", 1)?;
//! # Ok::<(), pishock::Error>(())
//! ```

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_DURATION: u8 = 15;
const OPERATION_SUCCEEDED: &str = "Operation Succeeded.";

/// A PiShock username and API key.
///
/// Debug formatting always redacts the API key.
#[derive(Clone)]
pub struct Credentials {
    username: String,
    api_key: String,
}

impl Credentials {
    /// Creates credentials. Values are validated by [`PiShockClient::connect`].
    pub fn new(username: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            api_key: api_key.into(),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("username", &self.username)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

/// An owned PiShock hub and the shockers paired to it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Device {
    /// The hub's client ID.
    pub client_id: u64,
    /// The hub's display name.
    pub name: String,
    /// The owning user's ID.
    pub user_id: u64,
    /// The owning user's username.
    pub username: String,
    /// Shockers paired to this hub.
    pub shockers: Vec<Shocker>,
}

/// A shocker paired to an owned hub.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shocker {
    /// The shocker's display name.
    pub name: String,
    /// The shocker's ID within PiShock.
    pub shocker_id: u64,
    /// Whether commands to the shocker are paused.
    pub is_paused: bool,
}

/// Information and share limits for a share-code command target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShockerInfo {
    /// The client ID of the hub hosting the shocker.
    pub client_id: u64,
    /// The shocker's ID within PiShock.
    pub id: u64,
    /// The shocker's display name.
    pub name: String,
    /// Whether commands are paused.
    pub paused: bool,
    /// The maximum intensity allowed by the share code.
    pub max_intensity: u8,
    /// The maximum duration allowed by the share code.
    pub max_duration: u8,
    /// Whether the hosting hub is connected, when reported by PiShock.
    pub online: Option<bool>,
}

/// An operation sent to a shocker through a share code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Shock at `intensity` for `duration` seconds.
    Shock { intensity: u8, duration: u8 },
    /// Vibrate at `intensity` for `duration` seconds.
    Vibrate { intensity: u8, duration: u8 },
    /// Beep for `duration` seconds.
    Beep { duration: u8 },
}

/// An error produced by validation or a PiShock API request.
///
/// Transport and decoding errors intentionally omit request URLs so API keys
/// embedded in PiShock query strings cannot be exposed through formatting.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum Error {
    /// The username was empty.
    #[error("username must not be empty")]
    EmptyUsername,
    /// The API key was empty.
    #[error("API key must not be empty")]
    EmptyApiKey,
    /// The operation-log sender name was empty.
    #[error("sender name must not be empty")]
    EmptySender,
    /// A share code was empty.
    #[error("share code must not be empty")]
    EmptyShareCode,
    /// An intensity was outside 1 through 100.
    #[error("intensity must be between 1 and 100")]
    InvalidIntensity,
    /// A duration was outside 1 through 15 seconds.
    #[error("duration must be between 1 and 15 seconds")]
    InvalidDuration,
    /// The API key authentication request was rejected.
    #[error("PiShock authentication was rejected")]
    AuthenticationRejected,
    /// The share code does not exist.
    #[error("the share code does not exist")]
    ShareCodeNotFound,
    /// The username or API key was not authorized.
    #[error("PiShock did not authorize the request")]
    NotAuthorized,
    /// The target shocker is paused or unavailable.
    #[error("the target shocker is paused or unavailable")]
    ShockerPaused,
    /// The target hub is offline.
    #[error("the target PiShock hub is not connected")]
    DeviceOffline,
    /// The target device or share code is already in use elsewhere.
    #[error("the target PiShock device is already in use")]
    ShareCodeInUse,
    /// PiShock rejected the command operation code.
    #[error("PiShock rejected the operation code")]
    InvalidOperation,
    /// The share code does not allow the selected operation.
    #[error("the share code does not allow this operation")]
    OperationNotAllowed,
    /// PiShock rejected the requested intensity against share limits.
    #[error("PiShock rejected the intensity: {message}")]
    IntensityRejected {
        /// The rejection returned by PiShock.
        message: String,
    },
    /// PiShock rejected the requested duration against share limits.
    #[error("PiShock rejected the duration: {message}")]
    DurationRejected {
        /// The rejection returned by PiShock.
        message: String,
    },
    /// PiShock returned an otherwise unknown operation rejection.
    #[error("PiShock rejected the operation: {message}")]
    OperationRejected {
        /// The trimmed rejection returned by PiShock.
        message: String,
    },
    /// A server returned a non-success HTTP status.
    #[error("{operation} returned HTTP status {status}")]
    HttpStatus {
        /// The operation that failed, without its URL.
        operation: &'static str,
        /// The numeric HTTP status.
        status: u16,
    },
    /// A request could not be sent or its response could not be read.
    #[error("PiShock transport error")]
    Transport,
    /// A successful HTTP response did not match PiShock's documented schema.
    #[error("invalid PiShock response for {operation}")]
    Decode {
        /// The operation whose response could not be decoded.
        operation: &'static str,
    },
}

/// A connected, blocking PiShock API client.
///
/// The connection authenticates the API key once and stores the resulting user
/// ID. Requests use finite connect and total timeouts and never retry commands.
pub struct PiShockClient {
    http: Client,
    credentials: Credentials,
    sender: String,
    user_id: u64,
    urls: BaseUrls,
}

impl PiShockClient {
    /// Validates credentials and sender name, authenticates, and extracts the user ID.
    pub fn connect(
        credentials: Credentials,
        sender_name: impl Into<String>,
    ) -> Result<Self, Error> {
        Self::connect_to(credentials, sender_name.into(), BaseUrls::production())
    }

    fn connect_to(credentials: Credentials, sender: String, urls: BaseUrls) -> Result<Self, Error> {
        validate_credentials(&credentials, &sender)?;

        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Transport)?;

        let response = http
            .get(format!("{}/Auth/GetUserIfAPIKeyValid", urls.auth))
            .query(&[
                ("apikey", credentials.api_key.as_str()),
                ("username", credentials.username.as_str()),
            ])
            .send()
            .map_err(|_| Error::Transport)?;
        let response = expect_status(response, "authentication")?;
        let auth: AuthResponse = response.json().map_err(|_| Error::Decode {
            operation: "authentication",
        })?;

        Ok(Self {
            http,
            credentials,
            sender,
            user_id: auth.user_id,
            urls,
        })
    }

    /// Lists the authenticated user's owned hubs and their paired shockers.
    pub fn list_devices(&self) -> Result<Vec<Device>, Error> {
        let response = self
            .http
            .get(format!("{}/PiShock/GetUserDevices", self.urls.platform))
            .query(&[
                ("UserId", self.user_id.to_string()),
                ("Token", self.credentials.api_key.clone()),
                ("api", "true".to_owned()),
            ])
            .send()
            .map_err(|_| Error::Transport)?;
        let response = expect_status(response, "device listing")?;
        let devices: Vec<DeviceResponse> = response.json().map_err(|_| Error::Decode {
            operation: "device listing",
        })?;
        Ok(devices.into_iter().map(Device::from).collect())
    }

    /// Lists owned hubs and returns the one with `client_id`, if present.
    pub fn get_device(&self, client_id: u64) -> Result<Option<Device>, Error> {
        Ok(self
            .list_devices()?
            .into_iter()
            .find(|device| device.client_id == client_id))
    }

    /// Gets share limits and status for a legacy share code.
    pub fn get_shocker_info(&self, share_code: &str) -> Result<ShockerInfo, Error> {
        validate_share_code(share_code)?;
        let request = ShockerInfoRequest {
            username: &self.credentials.username,
            code: share_code,
            api_key: &self.credentials.api_key,
        };
        let response = self
            .http
            .post(format!("{}/api/GetShockerInfo", self.urls.legacy))
            .json(&request)
            .send()
            .map_err(|_| Error::Transport)?;
        let response = expect_shocker_info_status(response)?;
        let info: ShockerInfoResponse = response.json().map_err(|_| Error::Decode {
            operation: "shocker information",
        })?;
        Ok(info.into())
    }

    /// Sends one command request to a legacy share-code target.
    pub fn send_command(&self, share_code: &str, command: Command) -> Result<(), Error> {
        validate_share_code(share_code)?;
        validate_command(command)?;
        let (operation, intensity, duration) = match command {
            Command::Shock {
                intensity,
                duration,
            } => (0, Some(intensity), duration),
            Command::Vibrate {
                intensity,
                duration,
            } => (1, Some(intensity), duration),
            Command::Beep { duration } => (2, None, duration),
        };
        let request = OperationRequest {
            username: &self.credentials.username,
            name: &self.sender,
            code: share_code,
            intensity,
            duration,
            api_key: &self.credentials.api_key,
            operation,
        };
        let response = self
            .http
            .post(format!("{}/api/apioperate/", self.urls.legacy))
            .json(&request)
            .send()
            .map_err(|_| Error::Transport)?;
        let response = expect_status(response, "command")?;
        let body = response.text().map_err(|_| Error::Transport)?;
        parse_operation_response(body.trim(), &self.credentials.api_key)
    }

    /// Shocks a share-code target at `intensity` for `duration` seconds.
    pub fn shock(&self, share_code: &str, intensity: u8, duration: u8) -> Result<(), Error> {
        self.send_command(
            share_code,
            Command::Shock {
                intensity,
                duration,
            },
        )
    }

    /// Vibrates a share-code target at `intensity` for `duration` seconds.
    pub fn vibrate(&self, share_code: &str, intensity: u8, duration: u8) -> Result<(), Error> {
        self.send_command(
            share_code,
            Command::Vibrate {
                intensity,
                duration,
            },
        )
    }

    /// Beeps a share-code target for `duration` seconds.
    pub fn beep(&self, share_code: &str, duration: u8) -> Result<(), Error> {
        self.send_command(share_code, Command::Beep { duration })
    }
}

fn validate_credentials(credentials: &Credentials, sender: &str) -> Result<(), Error> {
    if credentials.username.trim().is_empty() {
        return Err(Error::EmptyUsername);
    }
    if credentials.api_key.trim().is_empty() {
        return Err(Error::EmptyApiKey);
    }
    if sender.trim().is_empty() {
        return Err(Error::EmptySender);
    }
    Ok(())
}

fn validate_share_code(share_code: &str) -> Result<(), Error> {
    if share_code.trim().is_empty() {
        Err(Error::EmptyShareCode)
    } else {
        Ok(())
    }
}

fn validate_command(command: Command) -> Result<(), Error> {
    let (intensity, duration) = match command {
        Command::Shock {
            intensity,
            duration,
        }
        | Command::Vibrate {
            intensity,
            duration,
        } => (Some(intensity), duration),
        Command::Beep { duration } => (None, duration),
    };
    if intensity.is_some_and(|value| !(1..=100).contains(&value)) {
        return Err(Error::InvalidIntensity);
    }
    if !(1..=MAX_DURATION).contains(&duration) {
        return Err(Error::InvalidDuration);
    }
    Ok(())
}

fn expect_shocker_info_status(response: Response) -> Result<Response, Error> {
    match response.status() {
        StatusCode::NOT_FOUND => Err(Error::ShareCodeNotFound),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::NotAuthorized),
        _ => expect_status(response, "shocker information"),
    }
}

fn expect_status(response: Response, operation: &'static str) -> Result<Response, Error> {
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else if operation == "authentication"
        && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
    {
        Err(Error::AuthenticationRejected)
    } else {
        Err(Error::HttpStatus {
            operation,
            status: status.as_u16(),
        })
    }
}

fn parse_operation_response(body: &str, api_key: &str) -> Result<(), Error> {
    match body {
        OPERATION_SUCCEEDED => Ok(()),
        "This code doesn’t exist." | "This code doesn't exist." => Err(Error::ShareCodeNotFound),
        "Not Authorized." => Err(Error::NotAuthorized),
        "Shocker is Paused, unable to send command."
        | "Shocker is Paused or does not exist. Unpause to send command." => {
            Err(Error::ShockerPaused)
        }
        "Device currently not connected." => Err(Error::DeviceOffline),
        "This share code has already been used by somebody else." | "Device in Use." => {
            Err(Error::ShareCodeInUse)
        }
        "Unknown Op, use 0 for shock, 1 for vibrate and 2 for beep." => {
            Err(Error::InvalidOperation)
        }
        "Shock not allowed." | "Vibrate not allowed." | "Beep not allowed." => {
            Err(Error::OperationNotAllowed)
        }
        message if message.starts_with("Intensity must be between 0 and ") => {
            Err(Error::IntensityRejected {
                message: redact_api_key(message, api_key),
            })
        }
        message if message.starts_with("Duration must be between 1 and ") => {
            Err(Error::DurationRejected {
                message: redact_api_key(message, api_key),
            })
        }
        message => Err(Error::OperationRejected {
            message: redact_api_key(message, api_key),
        }),
    }
}

fn redact_api_key(message: &str, api_key: &str) -> String {
    message.replace(api_key, "[REDACTED]")
}

#[derive(Clone)]
struct BaseUrls {
    auth: String,
    platform: String,
    legacy: String,
}

impl BaseUrls {
    fn production() -> Self {
        Self {
            auth: "https://auth.pishock.com".to_owned(),
            platform: "https://ps.pishock.com".to_owned(),
            legacy: "https://do.pishock.com".to_owned(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthResponse {
    #[serde(rename = "UserID", alias = "UserId", alias = "userId")]
    user_id: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceResponse {
    client_id: u64,
    name: String,
    user_id: u64,
    username: String,
    shockers: Vec<ShockerResponse>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShockerResponse {
    name: String,
    shocker_id: u64,
    is_paused: bool,
}

impl From<DeviceResponse> for Device {
    fn from(value: DeviceResponse) -> Self {
        Self {
            client_id: value.client_id,
            name: value.name,
            user_id: value.user_id,
            username: value.username,
            shockers: value.shockers.into_iter().map(Shocker::from).collect(),
        }
    }
}

impl From<ShockerResponse> for Shocker {
    fn from(value: ShockerResponse) -> Self {
        Self {
            name: value.name,
            shocker_id: value.shocker_id,
            is_paused: value.is_paused,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ShockerInfoRequest<'a> {
    username: &'a str,
    code: &'a str,
    #[serde(rename = "Apikey")]
    api_key: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShockerInfoResponse {
    client_id: u64,
    id: u64,
    name: String,
    paused: bool,
    max_intensity: u8,
    max_duration: u8,
    #[serde(default, alias = "isOnline")]
    online: Option<bool>,
}

impl From<ShockerInfoResponse> for ShockerInfo {
    fn from(value: ShockerInfoResponse) -> Self {
        Self {
            client_id: value.client_id,
            id: value.id,
            name: value.name,
            paused: value.paused,
            max_intensity: value.max_intensity,
            max_duration: value.max_duration,
            online: value.online,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct OperationRequest<'a> {
    username: &'a str,
    name: &'a str,
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    intensity: Option<u8>,
    duration: u8,
    #[serde(rename = "Apikey")]
    api_key: &'a str,
    #[serde(rename = "Op")]
    operation: u8,
}

#[cfg(test)]
mod tests;
