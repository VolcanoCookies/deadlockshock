use mockito::{Matcher, Mock, Server};
use serde_json::{Value, json};

use super::*;

const API_KEY: &str = "test-secret-key";
const SENDER: &str = "deadlockshock-companion";

fn get_mock(server: &mut Server, path: &str, body: Value) -> Mock {
    server
        .mock("GET", path)
        .match_header(API_KEY_HEADER, API_KEY)
        .match_header("X-PiShock-UserId", Matcher::Missing)
        .match_header("X-PiShock-Token", Matcher::Missing)
        .match_query(Matcher::Missing)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body.to_string())
        .expect(1)
        .create()
}

fn connect(server: &mut Server) -> (HttpClient, Mock) {
    let auth = get_mock(
        server,
        "/Account",
        json!({"UserId":42,"Username":"ignored"}),
    );
    let client = HttpClient::connect_to(API_KEY, SENDER.into(), server.url()).unwrap();
    (client, auth)
}

fn share(id: u64) -> Value {
    json!({
        "ClientId":7,"Id":id,"Name":"Collar","Paused":false,
        "SharePaused":false,"Locked":false,"IsV3":true,
        "CanShock":true,"CanVibrate":true,"CanBeep":true,"MaxIntensity":50,"MaxDuration":3000,
        "ShareCode":format!("CODE-{id}")
    })
}

fn group() -> DeviceGroup {
    DeviceGroup {
        client_id: 7,
        name: "Hub".into(),
        shockers: vec![serde_json::from_value(share(9)).unwrap()],
    }
}

fn command_mock(server: &mut Server, id: u64, body: Value, status: usize) -> Mock {
    server
        .mock(
            "POST",
            format!("/Shockers/OperateByShare/CODE-{id}").as_str(),
        )
        .match_header(API_KEY_HEADER, API_KEY)
        .match_header("X-PiShock-UserId", Matcher::Missing)
        .match_header("X-PiShock-Token", Matcher::Missing)
        .match_query(Matcher::Missing)
        .match_body(Matcher::Json(body))
        .with_status(status)
        .expect(1)
        .create()
}

fn command(operation: u8, intensity: u8, duration: u64) -> Value {
    json!({"AgentName":SENDER,"Operation":operation,"Intensity":intensity,"Duration":duration,"IntensityAsPercentage":false})
}

#[test]
fn connects_with_only_a_trimmed_api_key_and_no_command() {
    let mut server = Server::new();
    let auth = get_mock(&mut server, "/Account", json!({"UserId":42}));
    let no_command = server.mock("POST", Matcher::Any).expect(0).create();
    let client =
        HttpClient::connect_to(" \ttest-secret-key\n", format!(" {SENDER} "), server.url())
            .unwrap();
    assert_eq!(client.sender, SENDER);
    assert!(client.headers[API_KEY_HEADER].is_sensitive());
    assert!(!format!("{:?}", client.headers).contains(API_KEY));
    auth.assert();
    no_command.assert();
}

#[test]
fn rejects_empty_credentials_and_invalid_header_values_before_network_requests() {
    for (key, sender, expected) in [
        (" \t", SENDER, Error::EmptyApiKey),
        (API_KEY, " \t", Error::EmptySender),
        ("key\r\nInjected: value", SENDER, Error::Transport),
    ] {
        assert_eq!(
            HttpClient::connect_to(key, sender.into(), "http://127.0.0.1:1".into()).err(),
            Some(expected)
        );
    }
}

#[test]
fn rejected_credentials_and_malformed_auth_responses_never_expose_the_key() {
    for (status, body, expected) in [
        (
            401,
            API_KEY,
            Error::ApiKeyRejected {
                method: "GET",
                endpoint: "/Account",
            },
        ),
        (
            403,
            API_KEY,
            Error::ApiAccessDenied {
                method: "GET",
                endpoint: "/Account",
            },
        ),
        (
            200,
            API_KEY,
            Error::Decode {
                operation: "authentication",
            },
        ),
        (
            200,
            "{}",
            Error::Decode {
                operation: "authentication",
            },
        ),
        (
            500,
            API_KEY,
            Error::HttpStatus {
                operation: "/Account",
                status: 500,
            },
        ),
    ] {
        let mut server = Server::new();
        let auth = server
            .mock("GET", "/Account")
            .with_status(status)
            .with_body(body)
            .expect(1)
            .create();
        let error = HttpClient::connect_to(API_KEY, SENDER.into(), server.url())
            .err()
            .unwrap();
        assert!(!format!("{error:?} {error}").contains(API_KEY));
        assert_eq!(error, expected);
        if matches!(status, 401 | 403) {
            assert!(error.to_string().contains("GET /Account"));
            assert!(error.to_string().contains(&status.to_string()));
        }
        auth.assert();
    }
}

#[test]
fn discovery_reports_the_failing_endpoint_and_distinguishes_forbidden_from_invalid_keys() {
    for (status, expected) in [
        (
            401,
            Error::ApiKeyRejected {
                method: "GET",
                endpoint: "/Hub",
            },
        ),
        (
            403,
            Error::ApiAccessDenied {
                method: "GET",
                endpoint: "/Hub",
            },
        ),
    ] {
        let mut server = Server::new();
        let (client, auth) = connect(&mut server);
        let hubs = server
            .mock("GET", "/Hub")
            .with_status(status)
            .with_body(API_KEY)
            .expect(1)
            .create();
        assert_eq!(client.list_devices(), Err(expected));
        auth.assert();
        hubs.assert();
    }
}

#[test]
fn discovery_preserves_owned_hub_ids_and_includes_shared_hubs_without_duplicate_shares() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let hubs = get_mock(
        &mut server,
        "/Hub",
        json!([
            {"HubId":7,"Name":"Existing Hub"},{"HubId":8,"Name":"Empty Hub"}
        ]),
    );
    let mut unrelated = share(11);
    unrelated["ClientId"] = json!(99);
    let mut paused = share(10);
    paused["SharePaused"] = json!(true);
    let shares = get_mock(
        &mut server,
        "/Share/GetShared",
        json!([share(9), share(9), paused, unrelated]),
    );
    let devices = client.list_devices().unwrap();
    assert_eq!(devices.len(), 3);
    assert_eq!(devices[0].client_id, 7);
    assert_eq!(devices[0].name, "Existing Hub");
    assert_eq!(devices[0].shockers.len(), 2);
    assert!(devices[0].shockers[1].share_paused);
    assert!(devices[1].shockers.is_empty());
    assert_eq!(devices[2].client_id, 99);
    assert_eq!(devices[2].name, "Shared PiShock hub 99");
    assert_eq!(devices[2].shockers.len(), 1);
    auth.assert();
    hubs.assert();
    shares.assert();
}

#[test]
fn discovery_rejects_missing_permission_fields_instead_of_granting_access() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let hubs = get_mock(&mut server, "/Hub", json!([{"HubId":7,"Name":"Hub"}]));
    let mut malformed = share(9);
    malformed.as_object_mut().unwrap().remove("CanShock");
    let shares = get_mock(&mut server, "/Share/GetShared", json!([malformed]));
    assert_eq!(
        client.list_devices(),
        Err(Error::Decode {
            operation: "shocker listing"
        })
    );
    auth.assert();
    hubs.assert();
    shares.assert();
}

#[test]
fn sends_sound_and_shock_with_key_only_headers_and_millisecond_durations() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let sound = command_mock(&mut server, 9, command(2, 0, 1000), 204);
    let shock = command_mock(&mut server, 9, command(0, 25, 350), 204);
    client.beep_device(&group(), 1000).unwrap();
    client.shock_device(&group(), 25, 350).unwrap();
    auth.assert();
    sound.assert();
    shock.assert();
}

#[test]
fn skips_paused_and_locked_shockers_and_sends_once_to_each_active_shocker() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let mut device = group();
    for (id, field) in [(10, "Paused"), (11, "SharePaused"), (12, "Locked")] {
        let mut paused = share(id);
        paused[field] = json!(true);
        device
            .shockers
            .push(serde_json::from_value(paused).unwrap());
    }
    let active = command_mock(&mut server, 9, command(0, 25, 300), 204);
    let no_paused = server
        .mock(
            "POST",
            Matcher::Regex("/Shockers/OperateByShare/CODE-1[012]".into()),
        )
        .expect(0)
        .create();
    client.shock_device(&device, 25, 300).unwrap();
    device.shockers[0].paused = true;
    assert_eq!(
        client.shock_device(&device, 25, 300),
        Err(Error::NoAvailableShockers)
    );
    auth.assert();
    active.assert();
    no_paused.assert();
}

#[test]
fn validates_all_group_permissions_and_limits_before_sending_any_command() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let no_command = server.mock("POST", Matcher::Any).expect(0).create();
    for (field, value, expected) in [
        ("IsV3", json!(false), Error::UnsupportedFirmware),
        ("CanShock", json!(false), Error::OperationNotAllowed),
        (
            "MaxIntensity",
            json!(20),
            Error::IntensityRejected {
                message: "exceeds the share limit".into(),
            },
        ),
        (
            "MaxDuration",
            json!(100),
            Error::DurationRejected {
                message: "exceeds the share limit".into(),
            },
        ),
    ] {
        let mut device = group();
        let mut restricted = share(10);
        restricted[field] = value;
        device
            .shockers
            .push(serde_json::from_value(restricted).unwrap());
        assert_eq!(client.shock_device(&device, 25, 300), Err(expected));
    }
    let mut device = group();
    device.shockers[0].can_beep = false;
    assert_eq!(
        client.beep_device(&device, 1000),
        Err(Error::OperationNotAllowed)
    );
    for intensity in [0, 101] {
        assert_eq!(
            client.shock_device(&group(), intensity, 300),
            Err(Error::InvalidIntensity)
        );
    }
    for duration in [0, 15, 15_001] {
        assert_eq!(
            client.shock_device(&group(), 25, duration),
            Err(Error::InvalidHttpDuration)
        );
    }
    auth.assert();
    no_command.assert();
}

#[test]
fn command_failures_are_not_retried_and_stop_the_group_without_exposing_response_bodies() {
    for (status, expected) in [
        (
            401,
            Error::ApiKeyRejected {
                method: "POST",
                endpoint: BY_SHARE,
            },
        ),
        (
            403,
            Error::ApiAccessDenied {
                method: "POST",
                endpoint: BY_SHARE,
            },
        ),
        (404, Error::ShareCodeNotFound),
        (405, Error::OperationNotAllowed),
        (406, Error::UnsupportedFirmware),
        (410, Error::ShareLocked),
        (
            412,
            Error::IntensityRejected {
                message: "exceeds the share limit".into(),
            },
        ),
        (
            416,
            Error::DurationRejected {
                message: "exceeds the share limit".into(),
            },
        ),
        (503, Error::ShockerPaused),
        (
            500,
            Error::HttpStatus {
                operation: BY_SHARE,
                status: 500,
            },
        ),
    ] {
        let mut server = Server::new();
        let (client, auth) = connect(&mut server);
        let mut device = group();
        device
            .shockers
            .push(serde_json::from_value(share(10)).unwrap());
        let failed = server
            .mock("POST", "/Shockers/OperateByShare/CODE-9")
            .with_status(status)
            .with_body(API_KEY)
            .expect(1)
            .create();
        let no_second = server
            .mock("POST", "/Shockers/OperateByShare/CODE-10")
            .expect(0)
            .create();
        let error = client.shock_device(&device, 25, 300).unwrap_err();
        assert!(!format!("{error:?} {error}").contains(API_KEY));
        assert_eq!(error, expected);
        auth.assert();
        failed.assert();
        no_second.assert();
    }
}

#[test]
fn lists_shared_only_accounts_and_selects_a_group_by_its_hub_id() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let hubs = get_mock(&mut server, "/Hub", json!([]));
    let shares = get_mock(&mut server, "/Share/GetShared", json!([share(9)]));
    let device = client.get_device(7).unwrap().unwrap();
    assert_eq!(device.client_id, 7);
    assert_eq!(device.shockers[0].id, 9);
    assert_eq!(device.name, "Shared PiShock hub 7");
    auth.assert();
    hubs.assert();
    shares.assert();
}

#[test]
fn gets_claimed_shocker_information_by_share_code_and_redacts_the_code() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let shares = get_mock(&mut server, "/Share/GetShared", json!([share(9)])).expect(2);
    let info = client.get_shocker_info(" CODE-9 ").unwrap();
    assert_eq!(info.id, 9);
    assert_eq!(info.max_duration, 3000);
    assert!(info.can_vibrate);
    assert!(!format!("{info:?}").contains("CODE-9"));
    assert_eq!(
        client.get_shocker_info("unclaimed"),
        Err(Error::ShareCodeNotFound)
    );
    auth.assert();
    shares.assert();
}

#[test]
fn share_code_commands_use_the_new_route_for_every_operation() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let shock = command_mock(&mut server, 9, command(0, 50, 500), 204);
    let vibrate = command_mock(&mut server, 9, command(1, 80, 1500), 204);
    let beep = command_mock(&mut server, 9, command(2, 0, 250), 204);
    client.shock(" CODE-9 ", 50, 500).unwrap();
    client.vibrate("CODE-9", 80, 1500).unwrap();
    client.beep("CODE-9", 250).unwrap();
    auth.assert();
    shock.assert();
    vibrate.assert();
    beep.assert();
}

#[test]
fn id_commands_use_the_new_route_and_accept_duration_boundaries() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    for (cmd, operation, intensity, duration) in [
        (
            Command::Shock {
                intensity: 1,
                duration_ms: 16,
            },
            0,
            1,
            16,
        ),
        (
            Command::Vibrate {
                intensity: 100,
                duration_ms: 15_000,
            },
            1,
            100,
            15_000,
        ),
        (Command::Beep { duration_ms: 16 }, 2, 0, 16),
    ] {
        let operation_mock = server
            .mock("POST", "/Shockers/OperateById/9")
            .match_header(API_KEY_HEADER, API_KEY)
            .match_query(Matcher::Missing)
            .match_body(Matcher::Json(command(operation, intensity, duration)))
            .with_status(204)
            .expect(1)
            .create();
        client.send_command_by_id(9, cmd).unwrap();
        operation_mock.assert();
    }
    auth.assert();
}

#[test]
fn share_codes_are_encoded_as_one_url_path_segment() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let operation = server
        .mock(
            "POST",
            "/Shockers/OperateByShare/code%2Fwith%3Fquery%23fragment",
        )
        .match_query(Matcher::Missing)
        .match_header(API_KEY_HEADER, API_KEY)
        .with_status(204)
        .expect(1)
        .create();
    client.beep("code/with?query#fragment", 250).unwrap();
    auth.assert();
    operation.assert();
}

#[test]
fn multiple_shares_select_one_permitted_share_and_send_once_per_shocker() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let mut device = group();
    device.shockers[0].can_shock = false;
    let mut allowed = share(9);
    allowed["ShareCode"] = json!("permitted-share");
    device
        .shockers
        .push(serde_json::from_value(allowed).unwrap());
    let selected = server
        .mock("POST", "/Shockers/OperateByShare/permitted-share")
        .match_body(Matcher::Json(command(0, 25, 300)))
        .with_status(204)
        .expect(1)
        .create();
    let no_restricted = server
        .mock("POST", "/Shockers/OperateByShare/CODE-9")
        .expect(0)
        .create();
    client.shock_device(&device, 25, 300).unwrap();
    auth.assert();
    selected.assert();
    no_restricted.assert();
}

#[test]
fn limits_from_different_shares_are_never_combined() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let mut device = group();
    device.shockers[0].max_duration = 100;
    let mut other = share(9);
    other["ShareCode"] = json!("longer-weaker-share");
    other["MaxIntensity"] = json!(10);
    device.shockers.push(serde_json::from_value(other).unwrap());
    let no_command = server.mock("POST", Matcher::Any).expect(0).create();
    assert!(matches!(
        client.shock_device(&device, 25, 300),
        Err(Error::IntensityRejected { .. })
    ));
    auth.assert();
    no_command.assert();
}

#[test]
fn group_vibration_respects_its_mode_permission_and_preserves_intensity_units() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let vibration = command_mock(&mut server, 9, command(1, 80, 1000), 204);
    let mut device = group();
    client.vibrate_device(&device, 80, 1000).unwrap();
    device.shockers[0].can_vibrate = false;
    assert_eq!(
        client.vibrate_device(&device, 80, 1000),
        Err(Error::OperationNotAllowed)
    );
    auth.assert();
    vibration.assert();
}

#[test]
fn invalid_share_codes_and_direct_commands_never_reach_the_network() {
    let mut server = Server::new();
    let (client, auth) = connect(&mut server);
    let no_command = server.mock("POST", Matcher::Any).expect(0).create();
    assert_eq!(client.beep(" \t", 1000), Err(Error::EmptyShareCode));
    assert_eq!(client.get_shocker_info(" \t"), Err(Error::EmptyShareCode));
    for invalid in [
        Command::Shock {
            intensity: 0,
            duration_ms: 1000,
        },
        Command::Vibrate {
            intensity: 101,
            duration_ms: 1000,
        },
    ] {
        assert_eq!(
            client.send_command_by_id(9, invalid),
            Err(Error::InvalidIntensity)
        );
        assert_eq!(
            client.send_command("CODE-9", invalid),
            Err(Error::InvalidIntensity)
        );
    }
    for duration_ms in [0, 15, 15_001] {
        assert_eq!(
            client.beep("CODE-9", duration_ms),
            Err(Error::InvalidHttpDuration)
        );
    }
    auth.assert();
    no_command.assert();
}

#[test]
fn authentication_headers_are_not_forwarded_to_redirects() {
    let mut redirected = Server::new();
    let no_request = redirected.mock("GET", Matcher::Any).expect(0).create();
    let mut server = Server::new();
    let redirect = server
        .mock("GET", "/Account")
        .with_status(302)
        .with_header("location", &redirected.url())
        .expect(1)
        .create();
    assert_eq!(
        HttpClient::connect_to(API_KEY, SENDER.into(), server.url()).err(),
        Some(Error::HttpStatus {
            operation: "/Account",
            status: 302
        })
    );
    redirect.assert();
    no_request.assert();
}
