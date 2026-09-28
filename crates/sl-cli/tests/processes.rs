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

/// A process killed while it holds a refresh claim leaves the claim behind; once the claim is
/// older than the one-hour timeout, another process takes the entry over and finishes it.
#[test]
fn a_refresh_claimed_by_a_killed_process_is_taken_over() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let server = runtime.block_on(wiremock::MockServer::start());
    let stalled = wiremock::ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(120));
    runtime.block_on(wiremock::Mock::given(wiremock::matchers::any()).respond_with(stalled).mount(&server));

    let temporary = tempfile::tempdir().expect("tempdir");
    let data_dir = temporary.path().join("data");
    let program = env!("CARGO_BIN_EXE_sideport");

    let base = || {
        let mut command = Command::new(program);
        command.arg("--data-dir").arg(&data_dir).arg("--file-secrets").stdout(Stdio::null()).stderr(Stdio::null());

        command
    };

    assert!(base().args(["settings", "show"]).status().expect("create state").success());

    // A tracked installation whose stored source is a URL on the local server, already queued.
    let database = rusqlite::Connection::open(data_dir.join("state.sqlite3")).expect("state database");
    let now = unix_seconds();
    let record = serde_json::json!({
        "id": 0, "app_name": "App", "bundle_id": "com.example.app", "original_bundle_id": "com.example.app",
        "version": null, "device_udid": "00000000-0000000000000000", "device_name": "Fixture",
        "apple_id": "fixture@example.test", "team_id": "TEAM123456",
        "installed_at": "2026-09-01T00:00:00Z", "expires_at": "2026-09-08T00:00:00Z", "auto_refresh": true,
        "last_error": null, "consecutive_failures": 0, "icon_png": null,
        "spec": {
            "source": format!("{}/App.ipa", server.uri()),
            "target": { "Device": { "udid": "00000000-0000000000000000", "prefer_network": false } },
            "signing": "AdHoc",
            "options": {},
        },
    });
    database
        .execute(
            "INSERT INTO installations (device_udid, bundle_id, record, expires_at, auto_refresh, enqueued_at, enqueue_token)
             VALUES ('00000000-0000000000000000', 'com.example.app', ?1, ?2, 1, ?2, 'queued')",
            rusqlite::params![record.to_string(), now],
        )
        .expect("queued installation");

    let claim = || -> (Option<i64>, Option<String>) {
        database
            .query_row("SELECT claimed_at, enqueue_token FROM installations", [], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("queue entry")
    };

    // The first process claims the entry and stalls on the download; it is then killed.
    let mut crashed = base().arg("refresh-due").spawn().expect("first scheduler pass");
    let claimed = (0..200).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        claim().0.is_some()
    });
    assert!(claimed, "the first process claimed the entry");

    crashed.kill().expect("kill");
    crashed.wait().expect("reap");

    let (claimed_at, token) = claim();
    assert!(
        claimed_at.is_some() && token.as_deref().is_some_and(|token| token.starts_with("claim:")),
        "the claim remains"
    );

    // A second pass within the hour leaves the claim alone.
    assert!(base().arg("refresh-due").status().expect("second pass").success());
    assert_eq!(claim().1, token, "a fresh claim is not taken over");

    // After the timeout the claim is stale: the next pass takes over, the download is refused
    // (a web page instead of an IPA) and the failure is recorded.
    database.execute("UPDATE installations SET claimed_at = claimed_at - 7200", []).expect("age the claim");
    runtime.block_on(server.reset());
    let page = wiremock::ResponseTemplate::new(200).set_body_raw("<html></html>", "text/html");
    runtime.block_on(wiremock::Mock::given(wiremock::matchers::any()).respond_with(page).mount(&server));

    assert!(base().arg("refresh-due").status().expect("third pass").success());

    let (enqueued, stored): (Option<i64>, String) = database
        .query_row("SELECT enqueued_at, record FROM installations", [], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("installation");
    let stored: serde_json::Value = serde_json::from_str(&stored).expect("record");

    assert_eq!(enqueued, None, "the entry was run and dequeued");
    assert_eq!(stored["consecutive_failures"], 1, "{stored}");
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_secs() as i64
}
