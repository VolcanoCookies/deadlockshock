use std::cmp::Ordering;
use std::io::Read;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use crate::bridge_listener::ModVersionObservation;
use egui::Context;
use reqwest::blocking::{Client, Response};
use semver::{Version, VersionReq};
use serde::Deserialize;

pub const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/VolcanoCookies/deadlockshock/releases/latest";
pub const COMPANION_RELEASE_URL: &str =
    "https://github.com/VolcanoCookies/deadlockshock/releases/latest";
pub const MOD_RELEASE_URL: &str = "https://gamebanana.com/mods/700758";
pub const USER_AGENT: &str = concat!("DeadlockShock-Companion/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
struct LatestReleaseResponse {
    tag_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VersionCheckState {
    Checking,
    Current { latest: Version },
    UpdateAvailable { latest: Version },
    Unavailable { reason: String },
}

impl Default for VersionCheckState {
    fn default() -> Self {
        Self::Checking
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VersionCheckError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("response body read failed: {0}")]
    Read(#[from] std::io::Error),
    #[error("latest release returned HTTP status {0}")]
    HttpStatus(reqwest::StatusCode),
    #[error("latest release response exceeded {MAX_RESPONSE_BYTES} bytes")]
    OversizedResponse,
    #[error("latest release response was malformed: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("latest release tag is invalid: {0}")]
    InvalidTag(String),
    #[error("latest release response omitted tag_name")]
    MissingTag,
    #[error("could not construct HTTP client: {0}")]
    Client(String),
}

impl VersionCheckError {
    fn category(&self) -> &'static str {
        match self {
            Self::Request(error) if error.is_timeout() => "timeout",
            Self::Request(error) if error.is_connect() => "connect",
            Self::Request(_) => "request",
            Self::Read(_) => "body_read",
            Self::HttpStatus(_) => "http_status",
            Self::OversizedResponse => "oversized_response",
            Self::Malformed(_) => "malformed_response",
            Self::InvalidTag(_) => "invalid_tag",
            Self::MissingTag => "missing_tag",
            Self::Client(_) => "client",
        }
    }
}

pub fn normalize_release_tag(tag: &str) -> Result<Version, VersionCheckError> {
    let Some(version) = tag.strip_prefix('v') else {
        return Err(VersionCheckError::InvalidTag(tag.to_owned()));
    };
    if version.is_empty() || version.len() > 128 {
        return Err(VersionCheckError::InvalidTag(tag.to_owned()));
    }
    Version::parse(version).map_err(|_| VersionCheckError::InvalidTag(tag.to_owned()))
}

fn read_bounded_body(mut response: Response) -> Result<Vec<u8>, VersionCheckError> {
    let mut body = Vec::new();
    response
        .by_ref()
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(VersionCheckError::OversizedResponse);
    }
    Ok(body)
}

pub fn check_latest_release_with(
    client: &Client,
    endpoint: &str,
) -> Result<Version, VersionCheckError> {
    let response = client
        .get(endpoint)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()?;
    if !response.status().is_success() {
        return Err(VersionCheckError::HttpStatus(response.status()));
    }
    let body = read_bounded_body(response)?;
    let release: LatestReleaseResponse = serde_json::from_slice(&body)?;
    let tag = release.tag_name.ok_or(VersionCheckError::MissingTag)?;
    normalize_release_tag(&tag)
}

pub fn app_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo package version must be valid semver")
}

/// The same declaration is checked against the mod's output by release tests.
pub fn supported_mod_versions() -> &'static VersionReq {
    static SUPPORTED: OnceLock<VersionReq> = OnceLock::new();
    SUPPORTED.get_or_init(|| {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ModCompatibility {
            supported_mod_versions: String,
        }
        let declaration: ModCompatibility =
            serde_json::from_str(include_str!("../mod-compatibility.json"))
                .expect("mod compatibility declaration must be valid JSON");
        VersionReq::parse(&declaration.supported_mod_versions)
            .expect("supported mod versions must be a valid semver requirement")
    })
}

fn precedence_cmp(left: &Version, right: &Version) -> Ordering {
    left.cmp_precedence(right)
}

fn is_older(left: &Version, right: &Version) -> bool {
    precedence_cmp(left, right).is_lt()
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WarningSelection {
    pub companion_outdated: Option<Version>,
    pub mod_unsupported: Option<Version>,
    pub mod_legacy: bool,
    pub mod_invalid: bool,
}

pub fn select_warnings(
    app: &Version,
    mod_observation: &ModVersionObservation,
    remote: Option<&Version>,
) -> WarningSelection {
    let observed = match mod_observation {
        ModVersionObservation::Reported(version) => Version::parse(version).ok(),
        _ => None,
    };
    let companion_outdated = remote.filter(|latest| is_older(app, latest)).cloned();
    let mod_unsupported = observed.filter(|version| !supported_mod_versions().matches(version));
    WarningSelection {
        companion_outdated,
        mod_unsupported,
        mod_legacy: matches!(mod_observation, ModVersionObservation::Legacy),
        mod_invalid: matches!(mod_observation, ModVersionObservation::Invalid)
            || matches!(mod_observation, ModVersionObservation::Reported(version) if Version::parse(version).is_err()),
    }
}

struct WorkerResult {
    result: Result<Version, VersionCheckError>,
}

pub struct VersionCheckOwner {
    endpoint: String,
    client: Option<Client>,
    completion: Option<Receiver<WorkerResult>>,
    pub state: VersionCheckState,
}

impl VersionCheckOwner {
    pub fn new(context: &Context) -> Self {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| VersionCheckError::Client(error.to_string()));
        let mut owner = Self::with_client(LATEST_RELEASE_URL, client.ok());
        owner.start(context.clone());
        owner
    }

    pub fn with_client(endpoint: impl Into<String>, client: Option<Client>) -> Self {
        Self {
            endpoint: endpoint.into(),
            client,
            completion: None,
            state: VersionCheckState::Checking,
        }
    }

    pub fn start(&mut self, context: Context) -> bool {
        if self.completion.is_some() {
            return false;
        }
        self.state = VersionCheckState::Checking;
        let endpoint = self.endpoint.clone();
        let client = self.client.clone();
        let (sender, receiver) = mpsc::channel();
        self.completion = Some(receiver);
        match thread::Builder::new()
            .name("deadlock-version-check".to_owned())
            .spawn(move || {
                let result = client
                    .ok_or_else(|| VersionCheckError::Client("client unavailable".to_owned()))
                    .and_then(|client| check_latest_release_with(&client, &endpoint));
                let _ = sender.send(WorkerResult { result });
                context.request_repaint();
            }) {
            Ok(_) => true,
            Err(error) => {
                log::error!(
                    target: "companion::version_check",
                    "version_check_worker_spawn_failed error={error:?}"
                );
                self.completion = None;
                self.state = VersionCheckState::Unavailable {
                    reason: error.to_string(),
                };
                false
            }
        }
    }

    pub fn poll(&mut self) -> bool {
        let Some(receiver) = self.completion.as_ref() else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result.result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err(VersionCheckError::Client(
                "worker channel disconnected".to_owned(),
            )),
        };
        self.completion = None;
        self.state = match result {
            Ok(latest) if is_older(&app_version(), &latest) => {
                VersionCheckState::UpdateAvailable { latest }
            }
            Ok(latest) => VersionCheckState::Current { latest },
            Err(error) => {
                let category = error.category();
                log::warn!(
                    target: "companion::version_check",
                    "version_check_failed error_kind={category}"
                );
                VersionCheckState::Unavailable {
                    reason: category.to_owned(),
                }
            }
        };
        true
    }

    pub fn is_checking(&self) -> bool {
        self.completion.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Instant;

    fn version(value: &str) -> Version {
        Version::parse(value).expect("valid test version")
    }

    fn test_client(timeout: Duration) -> Client {
        Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .expect("test client")
    }

    fn serve_once(status: &str, body: Vec<u8>, delay: Duration) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let endpoint = format!("http://{}", listener.local_addr().expect("mock address"));
        let (request_sender, request_receiver) = mpsc::channel();
        let status = status.to_owned();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept mock request");
            let mut request = [0_u8; 8192];
            let read = stream.read(&mut request).expect("read mock request");
            let _ = request_sender.send(String::from_utf8_lossy(&request[..read]).into_owned());
            thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
        });
        (endpoint, request_receiver)
    }

    fn wait_for_completion(owner: &mut VersionCheckOwner) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !owner.poll() {
            assert!(Instant::now() < deadline, "version-check worker timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn strict_tag_normalization_and_semver_precedence() {
        assert_eq!(normalize_release_tag("v0.2.0").unwrap(), version("0.2.0"));
        assert!(normalize_release_tag("0.2.0").is_err());
        assert!(normalize_release_tag("vv0.2.0").is_err());
        assert!(normalize_release_tag("v0.2").is_err());
        assert!(is_older(&version("1.0.0-alpha"), &version("1.0.0")));
        assert_eq!(
            precedence_cmp(&version("1.0.0+build.1"), &version("1.0.0+build.2")),
            Ordering::Equal
        );
    }

    #[test]
    fn declared_mod_range_accepts_compatible_stable_versions() {
        let supported = supported_mod_versions();
        for compatible in ["0.1.0", "0.1.9", "0.1.42", "0.1.0+build.2"] {
            assert!(supported.matches(&version(compatible)), "{compatible}");
        }
        for unsupported in ["0.0.9", "0.2.0", "1.0.0", "0.1.1-alpha"] {
            assert!(!supported.matches(&version(unsupported)), "{unsupported}");
        }
    }

    #[test]
    fn companion_releases_do_not_make_a_supported_mod_outdated() {
        let installed_mod = ModVersionObservation::Reported("0.1.0".to_owned());
        let older_companion =
            select_warnings(&version("0.1.0"), &installed_mod, Some(&version("0.2.0")));
        assert_eq!(older_companion.companion_outdated, Some(version("0.2.0")));
        assert!(older_companion.mod_unsupported.is_none());

        for companion in ["0.1.1", "0.2.0", "2.0.0"] {
            let current = version(companion);
            let warnings = select_warnings(&current, &installed_mod, Some(&current));
            assert_eq!(warnings, WarningSelection::default());
        }

        let offline = select_warnings(&version("0.2.0"), &installed_mod, None);
        assert_eq!(offline, WarningSelection::default());
    }

    #[test]
    fn unsupported_mods_do_not_invent_a_companion_release() {
        for installed in ["0.0.9", "0.2.0", "1.0.0"] {
            let observation = ModVersionObservation::Reported(installed.to_owned());
            let warnings = select_warnings(&version("0.1.0"), &observation, None);
            assert_eq!(warnings.mod_unsupported, Some(version(installed)));
            assert!(warnings.companion_outdated.is_none());
        }
        let future_mod = ModVersionObservation::Reported("5.0.0".to_owned());
        let warnings = select_warnings(&version("0.1.0"), &future_mod, Some(&version("0.2.0")));
        assert_eq!(warnings.companion_outdated, Some(version("0.2.0")));
        assert_eq!(warnings.mod_unsupported, Some(version("5.0.0")));
    }

    #[test]
    fn warning_selection_handles_prerelease_build_legacy_invalid_and_unknown() {
        let prerelease = select_warnings(
            &version("1.0.0-alpha"),
            &ModVersionObservation::Unknown,
            Some(&version("1.0.0")),
        );
        assert!(prerelease.companion_outdated.is_some());

        let build_only = select_warnings(
            &version("1.0.0+app"),
            &ModVersionObservation::Reported("0.1.0+mod".to_owned()),
            Some(&version("1.0.0+release")),
        );
        assert_eq!(build_only, WarningSelection::default());

        let legacy = select_warnings(&version("1.0.0"), &ModVersionObservation::Legacy, None);
        assert!(legacy.mod_legacy);
        assert!(!legacy.mod_invalid);
        assert!(legacy.mod_unsupported.is_none());

        for observation in [
            ModVersionObservation::Invalid,
            ModVersionObservation::Reported("invalid".to_owned()),
        ] {
            let invalid = select_warnings(&version("1.0.0"), &observation, None);
            assert!(invalid.mod_invalid);
            assert!(!invalid.mod_legacy);
            assert!(invalid.mod_unsupported.is_none());
        }

        let unknown = select_warnings(&version("1.0.0"), &ModVersionObservation::Unknown, None);
        assert_eq!(unknown, WarningSelection::default());
    }

    #[test]
    fn github_response_success_and_required_headers() {
        let (endpoint, request) = serve_once(
            "200 OK",
            br#"{"tag_name":"v0.2.0"}"#.to_vec(),
            Duration::ZERO,
        );
        assert_eq!(
            check_latest_release_with(&test_client(Duration::from_secs(1)), &endpoint).unwrap(),
            version("0.2.0")
        );
        let request = request
            .recv_timeout(Duration::from_secs(1))
            .expect("captured request")
            .to_ascii_lowercase();
        assert!(request.contains(&format!("user-agent: {}", USER_AGENT.to_ascii_lowercase())));
        assert!(request.contains("accept: application/vnd.github+json"));
    }

    #[test]
    fn github_response_rejects_status_oversize_and_bad_payloads() {
        let cases = [
            (
                "503 Service Unavailable",
                br#"{"tag_name":"v0.2.0"}"#.to_vec(),
                "http_status",
            ),
            (
                "200 OK",
                vec![b'x'; MAX_RESPONSE_BYTES + 1],
                "oversized_response",
            ),
            ("200 OK", b"{bad".to_vec(), "malformed_response"),
            ("200 OK", b"{}".to_vec(), "missing_tag"),
            ("200 OK", br#"{"tag_name":"0.2.0"}"#.to_vec(), "invalid_tag"),
        ];
        for (status, body, category) in cases {
            let (endpoint, _) = serve_once(status, body, Duration::ZERO);
            let error = check_latest_release_with(&test_client(Duration::from_secs(1)), &endpoint)
                .expect_err("response must fail");
            assert_eq!(error.category(), category);
        }
    }

    #[test]
    fn github_request_reports_connect_and_timeout_failures() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve unused port");
        let endpoint = format!("http://{}", listener.local_addr().expect("unused address"));
        drop(listener);
        let connect =
            check_latest_release_with(&test_client(Duration::from_millis(100)), &endpoint)
                .expect_err("connection must fail");
        assert_eq!(connect.category(), "connect");

        let (endpoint, _) = serve_once(
            "200 OK",
            br#"{"tag_name":"v0.2.0"}"#.to_vec(),
            Duration::from_millis(100),
        );
        let timeout = check_latest_release_with(&test_client(Duration::from_millis(20)), &endpoint)
            .expect_err("request must time out");
        assert_eq!(timeout.category(), "timeout");
    }

    #[test]
    fn worker_completes_repaints_suppresses_concurrency_and_retries() {
        let context = Context::default();
        let repaint_count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&repaint_count);
        context.set_request_repaint_callback(move |_| {
            callback_count.fetch_add(1, AtomicOrdering::Relaxed);
        });

        let (endpoint, _) = serve_once(
            "200 OK",
            br#"{"tag_name":"v0.2.0"}"#.to_vec(),
            Duration::ZERO,
        );
        let mut owner =
            VersionCheckOwner::with_client(endpoint, Some(test_client(Duration::from_secs(1))));
        assert!(owner.start(context.clone()));
        assert!(!owner.start(context.clone()));
        assert!(matches!(owner.state, VersionCheckState::Checking));
        wait_for_completion(&mut owner);
        assert!(matches!(
            owner.state,
            VersionCheckState::UpdateAvailable { ref latest } if latest == &version("0.2.0")
        ));
        assert!(repaint_count.load(AtomicOrdering::Relaxed) > 0);

        let (retry_endpoint, _) = serve_once(
            "200 OK",
            br#"{"tag_name":"v0.1.0"}"#.to_vec(),
            Duration::ZERO,
        );
        owner.endpoint = retry_endpoint;
        assert!(owner.start(context));
        assert!(matches!(owner.state, VersionCheckState::Checking));
        wait_for_completion(&mut owner);
        assert!(matches!(
            owner.state,
            VersionCheckState::Current { ref latest } if latest == &version("0.1.0")
        ));
    }
}
