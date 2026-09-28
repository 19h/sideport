//! Several `sideport` processes sharing one data directory.

use std::process::{Command, Stdio};

/// Each process changes one field; every process's change must survive the others.
#[test]
fn concurrent_processes_keep_each_others_setting_changes() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let data_dir = temporary.path().join("data");
    let program = env!("CARGO_BIN_EXE_sideport");

    let base = || {
        let mut command = Command::new(program);
        command.arg("--data-dir").arg(&data_dir).arg("--file-secrets").stdout(Stdio::null()).stderr(Stdio::piped());

        command
    };

    // One run creates the data directory and database before the concurrent writers start.
    assert!(base().args(["settings", "show"]).status().expect("show").success());

    let mut children = Vec::new();

    for round in 0..8 {
        let mut refresh = base();
        refresh.args(["settings", "refresh", "--threshold-hours", &(20 + round).to_string()]);
        children.push(refresh.spawn().expect("refresh writer"));

        let mut anisette = base();
        anisette.args(["settings", "alternate-anisette", "--remote", &format!("https://ani{round}.example.test")]);
        children.push(anisette.spawn().expect("anisette writer"));
    }

    for child in children {
        let output = child.wait_with_output().expect("writer");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    let shown = base().args(["--json", "settings", "show"]).stdout(Stdio::piped()).output().expect("final settings");
    let settings: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("settings JSON");

    let threshold = settings["refresh"]["threshold_hours"].as_u64().expect("threshold");
    let alternate = settings["alternate_anisette"].to_string();

    assert!((20..28).contains(&threshold), "a refresh writer's value survived: {threshold}");
    assert!(alternate.contains(".example.test"), "an anisette writer's value survived: {alternate}");
}
