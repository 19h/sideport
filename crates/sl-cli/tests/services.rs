use assert_cmd::Command;

/// With no service endpoints configured (the default), the CLI reports everything as not
/// configured and contacts nothing.
#[test]
fn services_status_and_check_update_report_not_configured() {
    let directory = tempfile::tempdir().expect("data directory");

    let status = Command::new(assert_cmd::cargo::cargo_bin!("sideport"))
        .arg("--data-dir")
        .arg(directory.path())
        .args(["--json", "services", "status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status JSON");

    assert_eq!(status["updates_configured"], false);
    assert_eq!(status["token_verifier_configured"], false);
    assert_eq!(status["token_present"], false);

    let update = Command::new(assert_cmd::cargo::cargo_bin!("sideport"))
        .arg("--data-dir")
        .arg(directory.path())
        .args(["--json", "services", "check-update"])
        .assert()
        .success()
        .get_output()
        .clone();
    let update: serde_json::Value = serde_json::from_slice(&update.stdout).expect("update JSON");

    assert_eq!(update["status"], "not-configured");
}
