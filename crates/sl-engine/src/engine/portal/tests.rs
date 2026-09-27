use super::*;
use crate::engine::{Engine, EngineConfig, LiveAccount};
use crate::job::JobEvent;
use crate::types::{AccountSummary, AnisetteSetting, TeamKind};
use plist::{Dictionary, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroize::Zeroizing;

const APPLE_ID: &str = "fixture@example.test";
const TEAM_ID: &str = "TEAM123456";

fn session() -> Arc<AuthSession> {
    let dsid = Zeroizing::new("123456789".into());
    let token = Zeroizing::new("fixture-private-token".into());

    Arc::new(AuthSession::restore(APPLE_ID.into(), dsid, token, false).expect("fixture session"))
}

fn engine(server: &MockServer, teams: Vec<TeamSummary>) -> (Engine, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("data directory");
    let portal_origin = format!("{}/services/QH65B2/", server.uri());
    let config = EngineConfig {
        data_dir: Some(directory.path().into()),
        portal_origin: Some(portal_origin),
        disable_scheduler: true,
        file_secrets: true,
        ..EngineConfig::default()
    };
    let engine = Engine::new(config).expect("engine");

    let mut settings = engine.settings();
    settings.anisette = AnisetteSetting::Remote { url: format!("{}/anisette", server.uri()) };
    engine.update_settings(settings).expect("settings");

    let summary = AccountSummary {
        apple_id: APPLE_ID.into(),
        teams,
        default_team: None,
        has_session: true,
        remembers_password: false,
        last_login: None,
    };
    engine.inner.accounts.lock().insert(APPLE_ID.into(), LiveAccount { summary, session: Some(session()) });

    (engine, directory)
}

fn response(fields: impl IntoIterator<Item = (&'static str, Value)>) -> ResponseTemplate {
    let mut body = Dictionary::new();
    body.insert("resultCode".into(), Value::Integer(0.into()));

    for (name, value) in fields {
        body.insert(name.into(), value);
    }

    let mut bytes = Vec::new();
    Value::Dictionary(body).to_writer_xml(&mut bytes).expect("portal fixture");

    ResponseTemplate::new(200).set_body_bytes(bytes)
}

fn record(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut record = Dictionary::new();

    for (name, value) in fields {
        record.insert(name.into(), value);
    }

    Value::Dictionary(record)
}

async fn mount(server: &MockServer, action: &str, response: ResponseTemplate, count: u64) {
    let endpoint = format!("/services/QH65B2/{action}.action");

    Mock::given(method("POST")).and(path(endpoint)).respond_with(response).expect(count).mount(server).await;
}

async fn anisette(server: &MockServer) {
    let headers = serde_json::json!({
        "X-Apple-I-MD": "fixture-otp",
        "X-Apple-I-MD-M": "fixture-machine-token",
        "X-Mme-Device-Id": "fixture-device",
        "X-MMe-Client-Info": "fixture-client",
        "X-Apple-Locale": "en_US",
    });

    Mock::given(method("GET"))
        .and(path("/anisette"))
        .respond_with(ResponseTemplate::new(200).set_body_json(headers))
        .mount(server)
        .await;
}

fn team(id: &str) -> TeamSummary {
    TeamSummary { team_id: id.into(), name: format!("Team {id}"), kind: TeamKind::Individual }
}

#[tokio::test]
async fn account_views_refresh_teams_and_revoke_only_a_listed_certificate() {
    let server = MockServer::start().await;
    anisette(&server).await;

    let team = record([
        ("teamId", Value::String(TEAM_ID.into())),
        ("name", Value::String("Fixture Team".into())),
        ("type", Value::String("Individual".into())),
    ]);
    mount(&server, "listTeams", response([("teams", Value::Array(vec![team]))]), 1).await;

    let certificate = record([
        ("serialNumber", Value::String("ABC123".into())),
        ("machineName", Value::String("Fixture Mac".into())),
        ("expirationDate", Value::String("2026-10-01T12:00:00Z".into())),
    ]);
    mount(&server, "ios/listAllDevelopmentCerts", response([("certificates", Value::Array(vec![certificate]))]), 3)
        .await;

    let app_id = record([
        ("appIdId", Value::String("APP123".into())),
        ("identifier", Value::String("com.example.fixture".into())),
        ("name", Value::String("Fixture".into())),
    ]);
    mount(&server, "ios/listAppIds", response([("appIds", Value::Array(vec![app_id]))]), 1).await;
    mount(&server, "ios/revokeDevelopmentCert", response([]), 1).await;

    let (engine, _directory) = engine(&server, Vec::new());
    let certificates = engine.certificates(APPLE_ID.into()).result().await.expect("certificates");

    assert_eq!(certificates.len(), 1);
    assert_eq!(certificates[0].serial, "ABC123");
    assert_eq!(certificates[0].name, "ABC123");
    assert_eq!(certificates[0].machine_name.as_deref(), Some("Fixture Mac"));
    assert!(!certificates[0].is_ours);

    let app_ids = engine.app_ids(APPLE_ID.into()).result().await.expect("app IDs");
    assert_eq!(app_ids[0].identifier, "com.example.fixture");

    let missing = engine.revoke_certificate(APPLE_ID.into(), "NOT_LISTED".into()).result().await;
    assert!(matches!(missing, Err(EngineError::Signing(_))));

    engine.revoke_certificate(APPLE_ID.into(), "ABC123".into()).result().await.expect("revoke");

    let accounts = engine.accounts().expect("accounts");
    assert_eq!(accounts[0].teams[0].team_id, TEAM_ID);
    assert_eq!(accounts[0].default_team.as_deref(), Some(TEAM_ID));

    let requests = server.received_requests().await.expect("requests");
    let portal_requests: Vec<_> = requests.iter().filter(|request| request.url.path().ends_with(".action")).collect();
    assert_eq!(portal_requests.len(), 6);

    for request in &portal_requests {
        assert_eq!(request.headers.get("x-apple-gs-token").expect("token"), "fixture-private-token");

        if request.url.path().contains("/ios/") {
            let body = Value::from_reader_xml(request.body.as_slice()).expect("plist body");
            let fields = body.as_dictionary().expect("fields");
            assert_eq!(fields.get("teamId").and_then(Value::as_string), Some(TEAM_ID));
        }
    }
}

#[tokio::test]
async fn team_choice_is_saved_and_logout_invalidates_a_waiting_job() {
    let server = MockServer::start().await;
    anisette(&server).await;
    mount(&server, "ios/listAppIds", response([("appIds", Value::Array(vec![]))]), 2).await;

    let (engine, _directory) = engine(&server, vec![team("FIRST"), team("SECOND")]);
    let job = engine.app_ids(APPLE_ID.into());
    let events = job.events();

    loop {
        let event = events.recv().await.expect("team event");

        if let JobEvent::Prompt(prompt) = event {
            assert!(matches!(&prompt.kind, PromptKind::ChooseTeam { teams, .. } if teams.len() == 2));
            prompt.answer(PromptReply::Choice(1));
            break;
        }
    }

    assert!(job.result().await.expect("selected team").is_empty());
    assert_eq!(engine.accounts().expect("accounts")[0].default_team.as_deref(), Some("SECOND"));
    assert!(engine.app_ids(APPLE_ID.into()).result().await.expect("saved choice").is_empty());

    let requests = server.received_requests().await.expect("requests");
    let app_requests: Vec<_> = requests.iter().filter(|request| request.url.path().contains("listAppIds")).collect();
    assert_eq!(app_requests.len(), 2);

    for request in app_requests {
        let body = Value::from_reader_xml(request.body.as_slice()).expect("plist body");
        assert_eq!(body.as_dictionary().expect("fields").get("teamId").and_then(Value::as_string), Some("SECOND"));
    }

    engine.inner.accounts.lock().get_mut(APPLE_ID).expect("account").summary.default_team = None;
    let pending = engine.revoke_certificate(APPLE_ID.into(), "ABC123".into());
    let events = pending.events();

    loop {
        if let JobEvent::Prompt(prompt) = events.recv().await.expect("team prompt") {
            engine.logout(APPLE_ID.into()).await.expect("logout");
            prompt.answer(PromptReply::Choice(0));
            break;
        }
    }

    assert!(matches!(pending.result().await, Err(EngineError::Auth(_))));
}

#[tokio::test]
async fn portal_service_errors_keep_the_code_and_redact_response_details() {
    let server = MockServer::start().await;
    anisette(&server).await;

    let mut body = Dictionary::new();
    body.insert("resultCode".into(), Value::Integer(7460.into()));
    body.insert("resultString".into(), Value::String("private-server-detail".into()));

    let mut bytes = Vec::new();
    Value::Dictionary(body).to_writer_xml(&mut bytes).expect("portal error fixture");
    let rejected = ResponseTemplate::new(200).set_body_bytes(bytes);
    mount(&server, "ios/listAppIds", rejected, 1).await;

    let (engine, _directory) = engine(&server, vec![team(TEAM_ID)]);
    let missing = engine.app_ids("other@example.test".into()).result().await;
    assert!(matches!(missing, Err(EngineError::Auth(_))));

    let error = engine.app_ids(APPLE_ID.into()).result().await.expect_err("portal rejection");
    assert!(matches!(&error, EngineError::Portal { code: 7460, .. }));
    assert!(!error.to_string().contains("private-server-detail"));
}
