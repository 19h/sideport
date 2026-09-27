use assert_cmd::Command;
use wiremock::matchers::{method, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn anisette_cli_json_uses_real_transport_and_omits_otp_headers() {
    let server = MockServer::start().await;
    let headers = serde_json::json!({
        "X-Apple-I-MD": "private-otp",
        "X-Apple-I-MD-M": "private-machine-token",
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

    let endpoint = server.uri();
    let output = tokio::task::spawn_blocking(move || {
        let directory = tempfile::tempdir().expect("data directory");

        Command::new(assert_cmd::cargo::cargo_bin!("sideport"))
            .arg("--data-dir")
            .arg(directory.path())
            .args(["--json", "anisette", "--remote", &endpoint])
            .assert()
            .success()
            .get_output()
            .clone()
    })
    .await
    .expect("CLI worker");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("CLI JSON");

    assert_eq!(json["description"], "Mac14,1 with serial number fixture-serial running macOS 15.0 24A335");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-"));
    assert!(output.stderr.is_empty());
}
