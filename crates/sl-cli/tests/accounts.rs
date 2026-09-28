//! Account, provisioning and device commands through the real executable. The fake portal runs
//! in the test process; the CLI reaches it over loopback HTTP.

#[path = "../../sl-bundle/tests/common/mod.rs"]
mod common;

use assert_cmd::Command;
use der::EncodePem;
use predicates::prelude::PredicateBooleanExt;
use sl_testkit::{FakePortal, ProfileChain};
use std::path::PathBuf;
use wiremock::MockServer;

const APPLE_ID: &str = "fixture@example.test";
const TEAM: &str = "TEAM123456";

struct Fixture {
    temporary: tempfile::TempDir,
    portal_origin: String,
    anisette: String,
}

impl Fixture {
    fn new(server: &MockServer) -> Self {
        let temporary = tempfile::tempdir().expect("tempdir");

        let anchors = ProfileChain::shared().root.certificate.to_pem(der::pem::LineEnding::LF).expect("anchor PEM");
        std::fs::write(temporary.path().join("anchors.pem"), anchors).expect("anchors");

        let sessions = serde_json::json!({
            format!("{APPLE_ID}:a"): { "_type": "GsaAuthenticator", "dsid": "42", "gs_token": "fixture-token" },
            "legacy@example.test:i": { "_type": "IdmsAuthenticator" },
        });
        std::fs::write(temporary.path().join("sessions.json"), serde_json::to_vec(&sessions).expect("JSON"))
            .expect("sessions");

        common::synthetic_bundle(&temporary.path().join("App.app"), "com.example.app", "App", "APPL");

        Self { temporary, portal_origin: FakePortal::origin(server), anisette: format!("{}/anisette", server.uri()) }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.temporary.path().join(name)
    }

    fn cli(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("sideport"));

        command
            .arg("--data-dir")
            .arg(self.path("data"))
            .args(["--portal-origin", &self.portal_origin, "--file-secrets"])
            .arg("--profile-anchors")
            .arg(self.path("anchors.pem"));

        command
    }

    fn json(&self, arguments: &[&str]) -> serde_json::Value {
        let output = self.cli().arg("--json").args(arguments).assert().success().get_output().stdout.clone();

        serde_json::from_slice(&output).expect("JSON output")
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(work).await.expect("CLI worker")
}

#[tokio::test(flavor = "multi_thread")]
async fn imported_sessions_sign_an_export_and_report_certificate_ownership() {
    let server = MockServer::start().await;
    let portal = FakePortal::new(TEAM, true);
    portal.mount(&server).await;

    let fixture = Fixture::new(&server);

    let fixture = blocking(move || {
        let settings = fixture.json(&["settings", "anisette", "--remote", &fixture.anisette.clone()]);
        assert_eq!(settings["anisette"]["Remote"]["url"], fixture.anisette);

        let sessions = fixture.path("sessions.json");
        let imported = fixture.json(&["account", "import", sessions.to_str().expect("path")]);
        assert_eq!(imported["imported"], serde_json::json!([APPLE_ID]));
        assert_eq!(imported["skipped"].as_array().map(Vec::len), Some(1));

        let accounts = fixture.json(&["account", "list"]);
        assert_eq!(accounts[0]["apple_id"], APPLE_ID);

        let output = fixture.path("signed.ipa");
        let source = fixture.path("App.app");
        let outcome = fixture.json(&[
            "export",
            source.to_str().expect("path"),
            "--signing",
            "apple-id",
            "--apple-id",
            APPLE_ID,
            "--output",
            output.to_str().expect("path"),
        ]);

        assert_eq!(outcome["bundle_id"], format!("com.example.app.{TEAM}"));
        assert!(outcome["expires"].is_string());
        assert!(output.is_file());

        let certificates = fixture.json(&["certificates", APPLE_ID]);
        assert_eq!(certificates.as_array().map(Vec::len), Some(1));
        assert_eq!(certificates[0]["is_ours"], true);

        let app_ids = fixture.json(&["app-ids", APPLE_ID]);
        assert_eq!(app_ids[0]["identifier"], format!("com.example.app.{TEAM}"));

        let installations = fixture.json(&["installations"]);
        assert_eq!(installations, serde_json::json!([]));

        fixture
    })
    .await;

    let requests = portal.state().actions().into_iter().map(str::to_owned).collect::<Vec<_>>();
    assert!(requests.contains(&"ios/submitDevelopmentCSR".to_owned()));
    assert!(requests.contains(&"ios/downloadTeamProvisioningProfile".to_owned()));

    blocking(move || {
        let signed_out = fixture.json(&["account", "logout", APPLE_ID]);
        assert_eq!(signed_out["signed_out"], APPLE_ID);
        assert_eq!(fixture.json(&["account", "list"]), serde_json::json!([]));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unattended_prompts_and_missing_device_services_fail_with_explanations() {
    let server = MockServer::start().await;
    let portal = FakePortal::new(TEAM, false);
    portal.mount(&server).await;

    let fixture = Fixture::new(&server);

    blocking(move || {
        let source = fixture.path("App.app");

        fixture
            .cli()
            .args(["export", source.to_str().expect("path"), "--signing", "apple-id"])
            .assert()
            .failure()
            .stderr(predicates::str::contains("--apple-id"));

        fixture
            .cli()
            .env("USBMUXD_SOCKET_ADDRESS", "127.0.0.1:1")
            .arg("devices")
            .assert()
            .failure()
            .stderr(predicates::str::contains("not reachable").or(predicates::str::contains("unavailable")));

        fixture
            .cli()
            .args([
                "install",
                source.to_str().expect("path"),
                "--device",
                "00008030-001A2D0C0E38802E",
                "--signing",
                "unsigned",
            ])
            .assert()
            .failure();

        let settings = fixture.json(&["settings", "refresh", "--threshold-hours", "12", "--allow-network", "false"]);
        assert_eq!(settings["refresh"]["threshold_hours"], 12);
        assert_eq!(settings["refresh"]["allow_network"], false);

        let shown = fixture.json(&["settings", "show"]);
        assert_eq!(shown["refresh"]["threshold_hours"], 12);
    })
    .await;
}
