use sl_engine::{AnisetteSetting, Engine, EngineConfig, EngineError};
use wiremock::matchers::{method, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn real_engine_remote_anisette_describes_machine_without_tokens() {
    let server = MockServer::start().await;
    let headers = serde_json::json!({
        "X-Apple-I-MD": "fixture-otp",
        "X-Apple-I-MD-M": "fixture-machine-token",
        "X-Mme-Device-Id": "fixture-device",
        "X-MMe-Client-Info": "<Mac14,1> <macOS;15.0;24A335> <com.apple.AuthKit/1>",
        "X-Apple-Locale": "en_US",
        "X-Apple-I-SRL-NO": "fixture-serial",
    });
    Mock::given(method("GET"))
        .and(query_param("u", ""))
        .respond_with(ResponseTemplate::new(200).set_body_json(headers))
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("data directory");
    let config =
        EngineConfig { data_dir: Some(directory.path().into()), disable_scheduler: true, ..Default::default() };
    let engine = Engine::new(config).expect("engine");
    let setting = AnisetteSetting::Remote { url: server.uri() };
    let description = engine.test_anisette(setting).await.expect("real provider");

    assert_eq!(description, "Mac14,1 with serial number fixture-serial running macOS 15.0 24A335");
    assert!(!description.contains("fixture-otp"));
    assert!(!description.contains("fixture-machine-token"));
    assert_eq!(engine.settings().anisette, AnisetteSetting::Local);
}

#[tokio::test]
async fn engine_reports_remote_failure_without_response_body_or_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string("private-server-detail"))
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("data directory");
    let config = EngineConfig { data_dir: Some(directory.path().into()), ..Default::default() };
    let engine = Engine::new(config).expect("engine");
    let setting = AnisetteSetting::Remote { url: format!("{}?token=private-query", server.uri()) };
    let error = engine.test_anisette(setting).await.expect_err("remote failure");

    assert!(matches!(&error, EngineError::Anisette(message) if message.contains("HTTP 403")));
    assert!(!error.to_string().contains("private-"));
}
