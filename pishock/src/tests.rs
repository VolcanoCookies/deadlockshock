use mockito::{Matcher, Mock, Server};

use super::*;

const USERNAME: &str = "api-user";
const API_KEY: &str = "super-secret-key";
const SENDER: &str = "desktop-companion";
const SHARE_CODE: &str = "SHARE-CODE";

fn credentials() -> Credentials {
    Credentials::new(USERNAME, API_KEY)
}

fn auth_mock(server: &mut Server) -> Mock {
    server
        .mock("GET", "/Auth/GetUserIfAPIKeyValid")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("apikey".into(), API_KEY.into()),
            Matcher::UrlEncoded("username".into(), USERNAME.into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"UserID":42,"Ignored":"value"}"#)
        .expect(1)
        .create()
}

fn connect(server: &mut Server) -> (PiShockClient, Mock) {
    let authentication = auth_mock(server);
    let url = server.url();
    let client = PiShockClient::connect_to(
        credentials(),
        SENDER.into(),
        BaseUrls {
            auth: url.clone(),
            platform: url.clone(),
            legacy: url,
        },
    )
    .unwrap();
    (client, authentication)
}

fn operation_mock(server: &mut Server, body: &str, response: &str) -> Mock {
    server
        .mock("POST", "/api/apioperate/")
        .match_header("content-type", Matcher::Regex("^application/json".into()))
        .match_body(Matcher::JsonString(body.into()))
        .with_status(200)
        .with_header("content-type", "text/plain")
        .with_body(response)
        .expect(1)
        .create()
}

#[test]
fn credentials_debug_redacts_api_key() {
    let rendered = format!("{:?}", credentials());
    assert!(rendered.contains("[REDACTED]"));
    assert!(!rendered.contains(API_KEY));
}

#[test]
fn connect_and_list_devices_obey_discovery_contracts() {
    let mut server = Server::new();
    let (client, authentication) = connect(&mut server);
    authentication.assert();
    let listing = server
        .mock("GET", "/PiShock/GetUserDevices")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("UserId".into(), "42".into()),
            Matcher::UrlEncoded("Token".into(), API_KEY.into()),
            Matcher::UrlEncoded("api".into(), "true".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"[{"clientId":7,"name":"Hub","userId":42,"username":"api-user","shockers":[{"name":"Collar","shockerId":9,"isPaused":false}]}]"#)
        .expect(1)
        .create();

    let devices = client.list_devices().unwrap();

    listing.assert();
    assert_eq!(devices[0].client_id, 7);
    assert_eq!(devices[0].shockers[0].shocker_id, 9);
}

#[test]
fn get_device_lists_and_selects_by_client_id() {
    let mut server = Server::new();
    let (client, _authentication) = connect(&mut server);
    let listing = server
        .mock("GET", "/PiShock/GetUserDevices")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("UserId".into(), "42".into()),
            Matcher::UrlEncoded("Token".into(), API_KEY.into()),
            Matcher::UrlEncoded("api".into(), "true".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"[{"clientId":1,"name":"First","userId":42,"username":"api-user","shockers":[]},{"clientId":2,"name":"Second","userId":42,"username":"api-user","shockers":[]}]"#)
        .expect(2)
        .create();

    assert_eq!(client.get_device(2).unwrap().unwrap().name, "Second");
    assert_eq!(client.get_device(99), Ok(None));
    listing.assert();
}

#[test]
fn get_shocker_info_uses_pascal_request_and_parses_camel_response() {
    let mut server = Server::new();
    let (client, _authentication) = connect(&mut server);
    let info_request = server
        .mock("POST", "/api/GetShockerInfo")
        .match_header("content-type", Matcher::Regex("^application/json".into()))
        .match_body(Matcher::JsonString(format!(r#"{{"Username":"{USERNAME}","Code":"{SHARE_CODE}","Apikey":"{API_KEY}"}}"#)))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"name":"Collar","clientId":7,"id":9,"paused":false,"maxIntensity":60,"maxDuration":12}"#)
        .expect(1)
        .create();

    let info = client.get_shocker_info(SHARE_CODE).unwrap();

    info_request.assert();
    assert_eq!(
        info,
        ShockerInfo {
            client_id: 7,
            id: 9,
            name: "Collar".into(),
            paused: false,
            max_intensity: 60,
            max_duration: 12,
            online: None,
        }
    );
}

#[test]
fn get_shocker_info_maps_not_found_and_authorization_statuses() {
    for (status, expected) in [
        (404, Error::ShareCodeNotFound),
        (401, Error::NotAuthorized),
        (403, Error::NotAuthorized),
    ] {
        let mut server = Server::new();
        let (client, _authentication) = connect(&mut server);
        let response = server
            .mock("POST", "/api/GetShockerInfo")
            .with_status(status)
            .expect(1)
            .create();

        assert_eq!(client.get_shocker_info(SHARE_CODE), Err(expected));
        response.assert();
    }
}

#[test]
fn convenience_commands_emit_exact_operation_payloads() {
    let mut server = Server::new();
    let (client, _authentication) = connect(&mut server);

    let shock = operation_mock(
        &mut server,
        &format!(
            r#"{{"Username":"{USERNAME}","Name":"{SENDER}","Code":"{SHARE_CODE}","Intensity":25,"Duration":3,"Apikey":"{API_KEY}","Op":0}}"#
        ),
        "Operation Succeeded.\n",
    );
    client.shock(SHARE_CODE, 25, 3).unwrap();
    shock.assert();
    drop(shock);

    let vibrate = operation_mock(
        &mut server,
        &format!(
            r#"{{"Username":"{USERNAME}","Name":"{SENDER}","Code":"{SHARE_CODE}","Intensity":80,"Duration":4,"Apikey":"{API_KEY}","Op":1}}"#
        ),
        " Operation Succeeded. ",
    );
    client.vibrate(SHARE_CODE, 80, 4).unwrap();
    vibrate.assert();
    drop(vibrate);

    let beep = operation_mock(
        &mut server,
        &format!(
            r#"{{"Username":"{USERNAME}","Name":"{SENDER}","Code":"{SHARE_CODE}","Duration":2,"Apikey":"{API_KEY}","Op":2}}"#
        ),
        "Operation Succeeded.",
    );
    client.beep(SHARE_CODE, 2).unwrap();
    beep.assert();
}

#[test]
fn unified_command_path_sends_the_selected_command() {
    let mut server = Server::new();
    let (client, _authentication) = connect(&mut server);
    let request = operation_mock(
        &mut server,
        &format!(
            r#"{{"Username":"{USERNAME}","Name":"{SENDER}","Code":"{SHARE_CODE}","Intensity":1,"Duration":15,"Apikey":"{API_KEY}","Op":0}}"#
        ),
        OPERATION_SUCCEEDED,
    );

    client
        .send_command(
            SHARE_CODE,
            Command::Shock {
                intensity: 1,
                duration: 15,
            },
        )
        .unwrap();
    request.assert();
}

#[test]
fn invalid_values_are_rejected_without_networking() {
    let mut server = Server::new();
    let (client, _authentication) = connect(&mut server);
    let no_operation = server.mock("POST", "/api/apioperate/").expect(0).create();
    let no_info = server
        .mock("POST", "/api/GetShockerInfo")
        .expect(0)
        .create();

    assert_eq!(client.shock(" ", 1, 1), Err(Error::EmptyShareCode));
    assert_eq!(client.shock(SHARE_CODE, 0, 1), Err(Error::InvalidIntensity));
    assert_eq!(
        client.vibrate(SHARE_CODE, 101, 1),
        Err(Error::InvalidIntensity)
    );
    assert_eq!(client.beep(SHARE_CODE, 0), Err(Error::InvalidDuration));
    assert_eq!(client.beep(SHARE_CODE, 16), Err(Error::InvalidDuration));
    assert_eq!(client.get_shocker_info("\t"), Err(Error::EmptyShareCode));
    no_operation.assert();
    no_info.assert();
}

#[test]
fn credentials_are_validated_before_authentication_networking() {
    let cases = [
        (Credentials::new("", API_KEY), SENDER, Error::EmptyUsername),
        (Credentials::new(USERNAME, " "), SENDER, Error::EmptyApiKey),
        (
            Credentials::new(USERNAME, API_KEY),
            "\t",
            Error::EmptySender,
        ),
    ];

    for (credentials, sender, expected) in cases {
        let mut server = Server::new();
        let no_auth = server
            .mock("GET", "/Auth/GetUserIfAPIKeyValid")
            .expect(0)
            .create();
        let url = server.url();
        let result = PiShockClient::connect_to(
            credentials,
            sender.into(),
            BaseUrls {
                auth: url.clone(),
                platform: url.clone(),
                legacy: url,
            },
        );
        assert_eq!(result.err(), Some(expected));
        no_auth.assert();
    }
}

#[test]
fn documented_operation_rejections_map_to_typed_errors() {
    let cases = [
        ("This code doesn’t exist.", Error::ShareCodeNotFound),
        ("Not Authorized.", Error::NotAuthorized),
        (
            "Shocker is Paused, unable to send command.",
            Error::ShockerPaused,
        ),
        ("Device currently not connected.", Error::DeviceOffline),
        (
            "This share code has already been used by somebody else.",
            Error::ShareCodeInUse,
        ),
        (
            "Unknown Op, use 0 for shock, 1 for vibrate and 2 for beep.",
            Error::InvalidOperation,
        ),
    ];

    for (body, expected) in cases {
        let mut server = Server::new();
        let (client, _authentication) = connect(&mut server);
        let rejection = operation_mock(
            &mut server,
            &format!(
                r#"{{"Username":"{USERNAME}","Name":"{SENDER}","Code":"{SHARE_CODE}","Duration":1,"Apikey":"{API_KEY}","Op":2}}"#
            ),
            body,
        );
        assert_eq!(client.beep(SHARE_CODE, 1), Err(expected));
        rejection.assert();
    }
}

#[test]
fn bounded_live_and_unknown_rejections_are_preserved() {
    assert_eq!(
        parse_operation_response("Intensity must be between 0 and 50", API_KEY),
        Err(Error::IntensityRejected {
            message: "Intensity must be between 0 and 50".into()
        })
    );
    assert_eq!(
        parse_operation_response("Duration must be between 1 and 8", API_KEY),
        Err(Error::DurationRejected {
            message: "Duration must be between 1 and 8".into()
        })
    );
    assert_eq!(
        parse_operation_response("Unexpected policy rejection", API_KEY),
        Err(Error::OperationRejected {
            message: "Unexpected policy rejection".into()
        })
    );
    assert_eq!(
        parse_operation_response("Shock not allowed.", API_KEY),
        Err(Error::OperationNotAllowed)
    );
    assert_eq!(
        parse_operation_response("Device in Use.", API_KEY),
        Err(Error::ShareCodeInUse)
    );
    assert_eq!(
        parse_operation_response(&format!("Rejected key {API_KEY}"), API_KEY),
        Err(Error::OperationRejected {
            message: "Rejected key [REDACTED]".into()
        })
    );
}

#[test]
fn a_rejected_command_is_sent_exactly_once() {
    let mut server = Server::new();
    let (client, _authentication) = connect(&mut server);
    let rejection = operation_mock(
        &mut server,
        &format!(
            r#"{{"Username":"{USERNAME}","Name":"{SENDER}","Code":"{SHARE_CODE}","Duration":1,"Apikey":"{API_KEY}","Op":2}}"#
        ),
        "Device in Use.",
    );

    assert_eq!(client.beep(SHARE_CODE, 1), Err(Error::ShareCodeInUse));
    rejection.assert();
}

#[test]
fn http_and_decode_failures_are_typed_and_redacted() {
    let mut status_server = Server::new();
    let status_auth = status_server
        .mock("GET", "/Auth/GetUserIfAPIKeyValid")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("apikey".into(), API_KEY.into()),
            Matcher::UrlEncoded("username".into(), USERNAME.into()),
        ]))
        .with_status(403)
        .expect(1)
        .create();
    let status_url = status_server.url();
    let error = PiShockClient::connect_to(
        credentials(),
        SENDER.into(),
        BaseUrls {
            auth: status_url.clone(),
            platform: status_url.clone(),
            legacy: status_url,
        },
    )
    .err()
    .unwrap();
    assert_eq!(error, Error::AuthenticationRejected);
    assert!(!format!("{error:?} {error}").contains(API_KEY));
    status_auth.assert();

    let mut decode_server = Server::new();
    let malformed = decode_server
        .mock("GET", "/Auth/GetUserIfAPIKeyValid")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("apikey".into(), API_KEY.into()),
            Matcher::UrlEncoded("username".into(), USERNAME.into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("not-json")
        .expect(1)
        .create();
    let decode_url = decode_server.url();
    let error = PiShockClient::connect_to(
        credentials(),
        SENDER.into(),
        BaseUrls {
            auth: decode_url.clone(),
            platform: decode_url.clone(),
            legacy: decode_url,
        },
    )
    .err()
    .unwrap();
    assert_eq!(
        error,
        Error::Decode {
            operation: "authentication"
        }
    );
    assert!(!format!("{error:?} {error}").contains(API_KEY));
    malformed.assert();
}
