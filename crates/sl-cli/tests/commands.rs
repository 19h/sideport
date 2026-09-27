#[path = "../../sl-bundle/tests/common/mod.rs"]
mod common;

use assert_cmd::Command;
use std::fs;

fn cli() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("sideport"))
}

#[test]
fn inspect_json_and_original_export_work_with_real_inputs() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let data = temporary.path().join("data");
    let inspected = cli()
        .arg("--data-dir")
        .arg(&data)
        .args(["--json", "inspect"])
        .arg(&root)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&inspected).expect("JSON");

    assert_eq!(json["bundle_id"], "com.example.test");

    let output = temporary.path().join("exported.ipa");
    let exported = cli()
        .arg("--data-dir")
        .arg(&data)
        .args(["--json", "export"])
        .arg(&root)
        .arg("--output")
        .arg(&output)
        .args(["--signing", "unsigned", "--name", "CLI Export", "--set-integer", "Custom=7"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&exported).expect("outcome JSON");

    assert_eq!(json["bundle_id"], "com.example.test");
    assert!(output.is_file());

    let inspected = cli()
        .arg("--data-dir")
        .arg(&data)
        .args(["--json", "inspect"])
        .arg(&output)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&inspected).expect("exported metadata");
    assert_eq!(json["name"], "CLI Export");

    let copy = temporary.path().join("copied.ipa");
    cli()
        .arg("--data-dir")
        .arg(&data)
        .arg("export")
        .arg(&output)
        .arg("--output")
        .arg(&copy)
        .args(["--signing", "original"])
        .assert()
        .success();
    assert_eq!(fs::read(&output).expect("source"), fs::read(&copy).expect("copy"));
}

#[test]
fn invalid_typed_overrides_fail_before_creating_output() {
    cli()
        .args(["export", "missing.ipa", "--set-bool", "Key=yes"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("boolean must be true or false"));
    cli()
        .args(["export", "missing.ipa", "--set-integer", "Key=9999999999999999999999999"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("signed 64-bit"));
    cli()
        .args(["export", "missing.ipa", "--remove-all-extensions", "--remove-extension", "Widget.appex"])
        .assert()
        .failure();
}

#[test]
fn missing_input_reports_a_failure_instead_of_a_demo_result() {
    let temporary = tempfile::tempdir().expect("tempdir");
    cli()
        .arg("--data-dir")
        .arg(temporary.path().join("data"))
        .args(["inspect", "absent.ipa"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("No such file"));
}

#[cfg(unix)]
#[test]
fn sigint_cancels_the_worker_and_preserves_an_existing_export() {
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command as ProcessCommand, Stdio};

    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    fs::File::create(root.join("large-resource")).expect("resource").set_len(256 * 1024 * 1024).expect("length");
    let output = temporary.path().join("existing.ipa");
    fs::write(&output, b"previous export").expect("output");
    let mut child = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("sideport"))
        .arg("--data-dir")
        .arg(temporary.path().join("data"))
        .arg("export")
        .arg(root)
        .arg("--output")
        .arg(&output)
        .args(["--signing", "original"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI");
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut line = String::new();

    loop {
        line.clear();
        assert!(stderr.read_line(&mut line).expect("stage") != 0, "CLI ended before packaging");

        if line.trim() == "Packaging" {
            break;
        }
    }

    assert!(ProcessCommand::new("kill").arg("-INT").arg(child.id().to_string()).status().expect("SIGINT").success());
    assert_eq!(child.wait().expect("worker termination").code(), Some(1));
    let mut remainder = String::new();
    stderr.read_to_string(&mut remainder).expect("cancellation diagnostic");
    assert!(remainder.contains("Cancelled"));
    assert_eq!(fs::read(output).expect("preserved output"), b"previous export");
}
