//! Synchronous client for PiShock's public HTTP API.
//!
//! [`HttpClient`] authenticates with an API key alone, discovers owned and shared
//! device groups, and sends shock, vibration and beep commands. All durations are
//! milliseconds. Methods block, so GUI applications must call them off the UI thread.
//!
//! ```no_run
//! use pishock::HttpClient;
//!
//! let client = HttpClient::connect("api-key", "deadlockshock-companion")?;
//! for device in client.list_devices()? {
//!     println!("{}: {}", device.client_id, device.name);
//! }
//! # Ok::<(), pishock::Error>(())
//! ```

use std::time::Duration;

use reqwest::blocking::{Client, Response};
use thiserror::Error;

pub mod http;
pub use http::{DeviceGroup, HttpClient, SharedShocker};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A command for either a shocker ID, share code, or device group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Shock { intensity: u8, duration_ms: u64 },
    Vibrate { intensity: u8, duration_ms: u64 },
    Beep { duration_ms: u64 },
}

impl Command {
    pub(crate) fn parameters(self) -> Result<(u8, u8, u64), Error> {
        let (operation, intensity, duration) = match self {
            Self::Shock {
                intensity,
                duration_ms,
            } => (0, intensity, duration_ms),
            Self::Vibrate {
                intensity,
                duration_ms,
            } => (1, intensity, duration_ms),
            Self::Beep { duration_ms } => (2, 0, duration_ms),
        };
        if operation != 2 && !(1..=100).contains(&intensity) {
            return Err(Error::InvalidIntensity);
        }
        if !(16..=15_000).contains(&duration) {
            return Err(Error::InvalidHttpDuration);
        }
        Ok((operation, intensity, duration))
    }
}

/// Request errors omit credentials, share codes, and untrusted response bodies.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum Error {
    #[error("API key must not be empty")]
    EmptyApiKey,
    #[error("sender name must not be empty")]
    EmptySender,
    #[error("share code must not be empty")]
    EmptyShareCode,
    #[error("intensity must be between 1 and 100")]
    InvalidIntensity,
    #[error("HTTP duration must be between 16 and 15000 milliseconds")]
    InvalidHttpDuration,
    #[error("the target PiShock hub requires V3 firmware")]
    UnsupportedFirmware,
    #[error(
        "PiShock rejected the API key at {method} {endpoint} (HTTP 401 Unauthorized). Check that the key is current and has not expired."
    )]
    ApiKeyRejected {
        method: &'static str,
        endpoint: &'static str,
    },
    #[error(
        "PiShock denied access to {method} {endpoint} (HTTP 403 Forbidden). Check API-key permissions and account access."
    )]
    ApiAccessDenied {
        method: &'static str,
        endpoint: &'static str,
    },
    #[error("no accessible share was found for the target shocker")]
    ShareCodeNotFound,
    #[error("the PiShock share is locked")]
    ShareLocked,
    #[error("the share code does not allow this operation")]
    OperationNotAllowed,
    #[error("the selected PiShock hub has no available shockers")]
    NoAvailableShockers,
    #[error("PiShock rejected the intensity: {message}")]
    IntensityRejected { message: String },
    #[error("PiShock rejected the duration: {message}")]
    DurationRejected { message: String },
    #[error("the target shocker or share is paused")]
    ShockerPaused,
    #[error("{operation} returned HTTP status {status}")]
    HttpStatus {
        operation: &'static str,
        status: u16,
    },
    #[error("PiShock transport error")]
    Transport,
    #[error("invalid PiShock response for {operation}")]
    Decode { operation: &'static str },
}

fn build_http_client() -> Result<Client, Error> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::Transport)
}

fn expect_status(
    response: Response,
    method: &'static str,
    endpoint: &'static str,
) -> Result<Response, Error> {
    match response.status().as_u16() {
        401 => Err(Error::ApiKeyRejected { method, endpoint }),
        403 => Err(Error::ApiAccessDenied { method, endpoint }),
        _ if response.status().is_success() => Ok(response),
        status => Err(Error::HttpStatus {
            operation: endpoint,
            status,
        }),
    }
}
