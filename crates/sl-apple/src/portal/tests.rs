use super::*;
use crate::anisette::AnisetteHeaders;
use futures::FutureExt;
use futures::future::BoxFuture;
use plist::Dictionary;
use std::collections::BTreeMap;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const USERNAME: &str = "fixture@example.test";
const DSID: &str = "123456789";
const TOKEN: &str = "private-fixture-token";
const TEAM: &str = "TEAM123456";
/// Framing matches a PEM CSR; the multi-line body must reach the service unchanged.
const FIXTURE_CSR: &str = "-----BEGIN CERTIFICATE REQUEST-----\nZml4dHVyZQ==\n-----END CERTIFICATE REQUEST-----\n";

#[derive(Debug)]
struct FixedAnisette;

impl AnisetteProvider for FixedAnisette {
    fn headers<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<AnisetteHeaders>> {
        let values: BTreeMap<_, _> = [
            ("X-Apple-I-MD", "fixture-otp"),
            ("X-Apple-I-MD-M", "fixture-machine-token"),
            ("X-Mme-Device-Id", "fixture-device"),
            ("X-MMe-Client-Info", "fixture-client"),
            ("X-Apple-Locale", "en_US"),
            ("X-Apple-GS-Token", "untrusted-anisette-override"),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect();

        async move { AnisetteHeaders::new(values) }.boxed()
    }
}

fn fixture() -> (AuthSession, FixedAnisette, CancellationToken) {
    (AuthSession::fixture(USERNAME, DSID, TOKEN), FixedAnisette, CancellationToken::new())
}

fn client(server: &MockServer) -> PortalClient {
    PortalClient::with_origin(&format!("{}/services/QH65B2/", server.uri())).expect("fixture portal")
}

fn status(code: i64, fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut response = wire::dict(&dictionary(fields)).expect("fixture dictionary").clone();
    response.insert("resultCode".into(), Value::Integer(code.into()));

    Value::Dictionary(response)
}

fn plist_response(value: Value) -> ResponseTemplate {
    let mut body = Vec::new();
    value.to_writer_xml(&mut body).expect("fixture plist");

    ResponseTemplate::new(200).set_body_bytes(body)
}

async fn mount(server: &MockServer, action: &str, value: Value) {
    let endpoint = format!("/services/QH65B2/{action}.action");

    Mock::given(method("POST")).and(path(endpoint)).respond_with(plist_response(value)).expect(1).mount(server).await;
}

fn request_fields(request: &wiremock::Request) -> Dictionary {
    let value = Value::from_reader_xml(request.body.as_slice()).expect("request plist");

    wire::dict(&value).expect("request dictionary").clone()
}

#[tokio::test]
async fn team_request_uses_session_headers_and_classifies_memberships() {
    let server = MockServer::start().await;
    let teams = Value::Array(vec![
        dictionary([
            ("teamId", Value::String(TEAM.into())),
            ("name", Value::String("Personal Team".into())),
            ("type", Value::String("Individual".into())),
            ("memberships", Value::Array(vec![dictionary([("name", Value::String("Apple Developer Free".into()))])])),
        ]),
        dictionary([
            ("teamId", Value::String("ORG123456".into())),
            ("name", Value::String("Fixture Laboratory".into())),
            ("type", Value::String("Company/Organization".into())),
            ("memberships", Value::Array(vec![])),
        ]),
    ]);
    mount(&server, "listTeams", status(0, [("teams", teams)])).await;

    let (session, provider, cancellation) = fixture();
    let access = PortalAccess::new(&session, &provider, &cancellation);
    let teams = client(&server).list_teams(access).await.expect("portal teams");

    assert_eq!(teams.len(), 2);
    assert_eq!(teams[0].kind, TeamKind::Free);
    assert_eq!(teams[1].kind, TeamKind::Organization);

    let requests = server.received_requests().await.expect("requests");
    let request = &requests[0];
    let fields = request_fields(request);

    assert_eq!(request.url.query(), Some("clientId=XABBG36SBA"));
    assert_eq!(fields.get("clientId").and_then(Value::as_string), Some(CLIENT_ID));
    assert_eq!(fields.get("protocolVersion").and_then(Value::as_string), Some(PROTOCOL));
    assert_eq!(fields.get("userLocale").and_then(Value::as_string), Some("en_US"));
    assert!(Uuid::parse_str(fields.get("requestId").and_then(Value::as_string).expect("request ID")).is_ok());
    assert!(!fields.contains_key("teamId"));
    assert_eq!(request.headers.get("x-apple-i-identity-id").expect("DSID"), DSID);
    assert_eq!(request.headers.get("x-apple-gs-token").expect("session token"), TOKEN);
    assert_eq!(request.headers.get("x-apple-i-locale").expect("locale"), "en_US");
    assert_eq!(request.headers.get("x-apple-app-info").expect("app"), XCODE_APP);
    assert!(!format!("{access:?}").contains(TOKEN));
    assert!(!format!("{:?}", client(&server)).contains(&server.uri()));
}

#[tokio::test]
async fn system_actions_preserve_paths_platform_fields_and_typed_responses() {
    let server = MockServer::start().await;
    let app_id = dictionary([
        ("appIdId", Value::String("APP123".into())),
        ("identifier", Value::String("com.example.fixture".into())),
        ("name", Value::String("Fixture".into())),
        ("expirationDate", Value::String("2026-10-01T12:00:00Z".into())),
    ]);
    let certificate = dictionary([
        ("serialNumber", Value::String("ABC123".into())),
        ("machineName", Value::String("Fixture Mac".into())),
        ("certContent", Value::Data(vec![0x30, 0x01, 0x00])),
    ]);

    for (action, response) in [
        (
            "listDevices",
            status(
                0,
                [(
                    "devices",
                    Value::Array(vec![dictionary([
                        ("deviceNumber", Value::String("UDID123".into())),
                        ("name", Value::String("Fixture TV".into())),
                    ])]),
                )],
            ),
        ),
        ("addDevice", status(0, [])),
        ("listAppIds", status(0, [("appIds", Value::Array(vec![app_id.clone()]))])),
        ("addAppId", status(0, [("appId", app_id)])),
        ("listAllDevelopmentCerts", status(0, [("certificates", Value::Array(vec![certificate]))])),
        (
            "submitDevelopmentCSR",
            status(0, [("certRequest", dictionary([("serialNum", Value::String("ABC123".into()))]))]),
        ),
        ("revokeDevelopmentCert", status(0, [])),
        (
            "downloadTeamProvisioningProfile",
            status(0, [("provisioningProfile", dictionary([("encodedProfile", Value::Data(vec![1, 2, 3]))]))]),
        ),
    ] {
        mount(&server, &format!("ios/{action}"), response).await;
    }

    let (session, provider, cancellation) = fixture();
    let access = PortalAccess::new(&session, &provider, &cancellation);
    let portal = client(&server);
    let devices = portal.list_devices(TEAM, Platform::Tvos, access).await.expect("devices");
    portal.add_device(TEAM, Platform::Ios, "UDID456", "Test Phone", access).await.expect("register device");
    let app_ids = portal.list_app_ids(TEAM, Platform::Ios, access).await.expect("app IDs");
    let created =
        portal.add_app_id(TEAM, Platform::Ios, "com.example.fixture", "Fixture", access).await.expect("app ID");
    let certificates = portal.list_certificates(TEAM, Platform::Ios, access).await.expect("certificates");
    let machine_id = Uuid::parse_str("d584b0a7-2613-4806-b631-c3ab34f8b14a").expect("machine ID");
    let serial = portal
        .submit_development_csr(TEAM, Platform::Ios, FIXTURE_CSR, machine_id, "Fixture Mac", access)
        .await
        .expect("certificate request");
    portal.revoke_development_certificate(TEAM, Platform::Ios, &serial, access).await.expect("revoke");
    let profile = portal.download_profile(TEAM, Platform::Ios, "APP123", access).await.expect("profile");

    assert_eq!(devices[0].device_number, "UDID123");
    assert_eq!(app_ids[0].expiration.expect("expiry").to_rfc3339(), "2026-10-01T12:00:00+00:00");
    assert_eq!(created.app_id_id, "APP123");
    assert_eq!(certificates[0].content_der.as_deref(), Some([0x30, 0x01, 0x00].as_slice()));
    assert_eq!(serial, "ABC123");
    assert_eq!(profile.as_slice(), &[1, 2, 3]);

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 8);

    for request in &requests {
        let fields = request_fields(request);
        assert_eq!(fields.get("teamId").and_then(Value::as_string), Some(TEAM));
        assert!(request.url.path().starts_with("/services/QH65B2/ios/"));
    }

    let tv = request_fields(&requests[0]);
    let phone = request_fields(&requests[1]);
    let csr = request_fields(&requests[5]);

    assert_eq!(tv.get("DTDK_Platform").and_then(Value::as_string), Some("tvos"));
    assert_eq!(tv.get("subPlatform").and_then(Value::as_string), Some("tvOS"));
    assert_eq!(phone.get("DTDK_Platform").and_then(Value::as_string), Some("ios"));
    assert!(!phone.contains_key("subPlatform"));
    assert_eq!(phone.get("deviceNumber").and_then(Value::as_string), Some("UDID456"));
    assert_eq!(csr.get("machineId").and_then(Value::as_string), Some(machine_id.to_string().as_str()));
    assert_eq!(csr.get("csrContent").and_then(Value::as_string), Some(FIXTURE_CSR));
}

#[tokio::test]
async fn portal_errors_and_cancellation_do_not_expose_response_bodies() {
    let server = MockServer::start().await;
    let private_body = "private-server-detail";
    let response = plist_response(status(1100, [("resultString", Value::String(private_body.into()))]));
    Mock::given(method("POST"))
        .and(path("/services/QH65B2/listTeams.action"))
        .respond_with(response)
        .mount(&server)
        .await;

    let (session, provider, cancellation) = fixture();
    let access = PortalAccess::new(&session, &provider, &cancellation);
    let error = client(&server).list_teams(access).await.expect_err("expired service session");

    assert!(matches!(error, Error::Service { operation: "listTeams", code: 1100 }));
    assert!(!error.to_string().contains(private_body));

    let delayed = MockServer::start().await;
    let response = plist_response(status(0, [("teams", Value::Array(vec![]))])).set_delay(Duration::from_secs(2));
    Mock::given(method("POST")).respond_with(response).mount(&delayed).await;
    let portal = client(&delayed);
    let cancelled = CancellationToken::new();
    let access = PortalAccess::new(&session, &provider, &cancelled);
    let request = portal.list_teams(access);
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancelled.cancel();
    };
    let (result, ()) = tokio::join!(request, cancel);

    assert!(matches!(result, Err(Error::Cancelled)));
}

#[tokio::test]
async fn malformed_portal_status_and_oversized_response_are_rejected() {
    let (session, provider, cancellation) = fixture();
    let access = PortalAccess::new(&session, &provider, &cancellation);
    let missing_status = MockServer::start().await;

    mount(&missing_status, "listTeams", dictionary([("teams", Value::Array(vec![]))])).await;
    assert!(matches!(client(&missing_status).list_teams(access).await, Err(Error::Invalid("resultCode"))));

    let oversized = MockServer::start().await;
    let body = vec![b'X'; wire::MAX_BODY_BYTES + 1];
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_bytes(body)).mount(&oversized).await;

    assert!(matches!(client(&oversized).list_teams(access).await, Err(Error::ResponseTooLarge(wire::MAX_BODY_BYTES))));
}

#[test]
fn duplicate_team_identifiers_and_invalid_expirations_fail_schema_validation() {
    let team = dictionary([
        ("teamId", Value::String(TEAM.into())),
        ("name", Value::String("Fixture Team".into())),
        ("type", Value::String("Individual".into())),
    ]);
    let response = dictionary([("teams", Value::Array(vec![team.clone(), team]))]);

    assert!(matches!(records::teams(&response), Err(Error::Invalid("duplicate team ID"))));

    let app_id = dictionary([
        ("appIdId", Value::String("APP123".into())),
        ("identifier", Value::String("com.example.fixture".into())),
        ("name", Value::String("Fixture".into())),
        ("expirationDate", Value::String("not-a-date".into())),
    ]);

    assert!(matches!(records::app_id(&app_id), Err(Error::Invalid("expirationDate"))));

    let mut app_id = wire::dict(&app_id).expect("app ID dictionary").clone();
    app_id.insert("expirationDate".into(), Value::String("never".into()));
    assert!(records::app_id(&Value::Dictionary(app_id)).expect("non-expiring app ID").expiration.is_none());
}

#[test]
fn portal_origin_requires_https_or_literal_loopback() {
    assert!(PortalClient::new().is_ok());

    for origin in [
        "http://example.test/services/QH65B2/",
        "http://localhost/services/QH65B2/",
        "https://user:secret@example.test/services/QH65B2/",
        "https://example.test/services/QH65B2/other/",
    ] {
        assert!(PortalClient::with_origin(origin).is_err());
    }
}
