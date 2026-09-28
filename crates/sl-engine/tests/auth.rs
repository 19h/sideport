use plist::{Dictionary, Value};
use sl_engine::{AnisetteSetting, Engine, EngineConfig, EngineError, JobEvent, PromptKind, PromptReply};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn gsa_error(code: i64) -> ResponseTemplate {
    let mut status = Dictionary::new();
    status.insert("ec".into(), Value::Integer(code.into()));

    let mut response = Dictionary::new();
    response.insert("Status".into(), Value::Dictionary(status));

    let mut envelope = Dictionary::new();
    envelope.insert("Response".into(), Value::Dictionary(response));

    let mut body = Vec::new();
    Value::Dictionary(envelope).to_writer_xml(&mut body).expect("GSA fixture");

    ResponseTemplate::new(200).set_body_bytes(body)
}

#[tokio::test]
async fn engine_password_prompt_reaches_remote_authentication_without_retaining_failure() {
    let server = MockServer::start().await;
    let anisette = serde_json::json!({
        "X-Apple-I-MD": "fixture-otp",
        "X-Apple-I-MD-M": "fixture-machine-token",
        "X-Mme-Device-Id": "fixture-device",
        "X-MMe-Client-Info": "fixture-client",
        "X-Apple-Locale": "en_US",
    });

    // The recovered cache refetches in the last 3 s of each 30 s window, so a second fetch can
    // occur when the sign-in straddles that boundary.
    Mock::given(method("GET"))
        .and(path("/anisette"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anisette))
        .expect(1..=2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/grandslam/GsService2"))
        .respond_with(gsa_error(-22406))
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("data directory");
    let engine = Engine::new(EngineConfig {
        data_dir: Some(directory.path().into()),
        auth_origin: Some(server.uri()),
        disable_scheduler: true,
        ..Default::default()
    })
    .expect("engine");
    let mut settings = engine.settings();
    settings.anisette = AnisetteSetting::Remote { url: format!("{}/anisette", server.uri()) };
    engine.update_settings(settings).expect("save settings");

    let job = engine.login("fixture@example.test".into(), None, false);
    let events = job.events();

    loop {
        match events.recv().await.expect("job event") {
            JobEvent::Prompt(prompt) => {
                assert!(matches!(&prompt.kind, PromptKind::Password { .. }));
                prompt.answer(PromptReply::Text { value: "private-fixture-password".into(), remember: false });
                break;
            }
            JobEvent::Stage(_) => {}
            event => panic!("unexpected pre-authentication event: {event:?}"),
        }
    }

    let error = job.result().await.expect_err("fixture service rejection");

    assert!(matches!(&error, EngineError::Auth(message) if message.contains("-22406")));
    assert!(!error.to_string().contains("private-fixture-password"));
    assert!(engine.accounts().expect("account list").is_empty());
}

#[tokio::test]
async fn a_rejected_remembered_login_stores_neither_account_nor_secrets() {
    let server = MockServer::start().await;
    let anisette = serde_json::json!({
        "X-Apple-I-MD": "fixture-otp",
        "X-Apple-I-MD-M": "fixture-machine-token",
        "X-Mme-Device-Id": "fixture-device",
        "X-MMe-Client-Info": "fixture-client",
        "X-Apple-Locale": "en_US",
    });

    Mock::given(method("GET"))
        .and(path("/anisette"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anisette))
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/grandslam/GsService2")).respond_with(gsa_error(-20209)).mount(&server).await;

    let directory = tempfile::tempdir().expect("data directory");
    let engine = Engine::new(EngineConfig {
        data_dir: Some(directory.path().into()),
        auth_origin: Some(server.uri()),
        file_secrets: true,
        disable_scheduler: true,
        ..Default::default()
    })
    .expect("engine");
    let mut settings = engine.settings();
    settings.anisette = AnisetteSetting::Remote { url: format!("{}/anisette", server.uri()) };
    engine.update_settings(settings).expect("save settings");

    let job = engine.login("fixture@example.test".into(), Some("private-fixture-password".into()), true);
    let error = job.result().await.expect_err("fixture service rejection");

    assert!(matches!(&error, EngineError::Auth(message) if message.contains("-20209")));
    assert!(engine.accounts().expect("account list").is_empty());

    let secrets = std::fs::read(directory.path().join("secrets.json")).unwrap_or_default();
    assert!(!String::from_utf8_lossy(&secrets).contains("private-fixture-password"));
    assert!(!secrets.windows(8).any(|window| window == b"password"));
}
