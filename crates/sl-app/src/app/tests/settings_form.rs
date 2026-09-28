//! Anisette, refresh and default settings through the rendered form.

use super::*;
use crate::app::services::update_message;
use sl_engine::{AnisetteSetting, ServiceConfig, UpdateEndpoints, UpdateStatus};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[gpui::test]
async fn settings_validate_then_save_provider_refresh_and_defaults(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let engine = engine(temporary.path());
    let (view, mut cx) = window(engine.clone(), cx);
    let before = engine.settings();

    click(&mut cx, "nav:Settings");
    click(&mut cx, "anisette:remote");

    let (remote, alternate, threshold, interval) = cx.read(|cx| {
        let form = &view.read(cx).settings;

        (form.remote_url.clone(), form.alternate_url.clone(), form.threshold.clone(), form.interval.clone())
    });
    set_input(&mut cx, &remote, "https://anisette.example.test/v3");
    set_input(&mut cx, &alternate, "http://127.0.0.1:6969");
    set_input(&mut cx, &threshold, "0");

    click(&mut cx, "save-settings");
    assert!(cx.read(|cx| view.read(cx).error.clone()).is_some_and(|error| error.contains("threshold")));
    assert_eq!(engine.settings(), before, "invalid settings are not saved");

    set_input(&mut cx, &threshold, "24");
    set_input(&mut cx, &interval, "15");
    click(&mut cx, "refresh-network");
    click(&mut cx, "default-stream");
    click(&mut cx, "default-remember");
    click(&mut cx, "save-settings");

    let saved = engine.settings();
    assert_eq!(cx.read(|cx| view.read(cx).error.clone()), None);
    assert_eq!(saved.anisette, AnisetteSetting::Remote { url: "https://anisette.example.test/v3".into() });
    assert_eq!(saved.alternate_anisette, Some(AnisetteSetting::Remote { url: "http://127.0.0.1:6969".into() }));
    assert_eq!((saved.refresh.threshold_hours, saved.refresh.check_interval_minutes), (24, 15));
    assert!(saved.refresh.enabled && !saved.refresh.allow_network);
    assert!(saved.stream_upload && saved.remember_passwords);
    assert_eq!(saved.theme, before.theme);
    assert!(cx.read(|cx| view.read(cx).accounts.remember), "the sign-in form follows the new default");

    click(&mut cx, "anisette:local");
    click(&mut cx, "nav:Settings");
    assert!(cx.read(|cx| view.read(cx).settings.remote), "reopening Settings shows the saved provider");
}

#[gpui::test]
async fn the_autostart_toggle_installs_and_removes_the_login_item(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let agents = temporary.path().join("LaunchAgents");
    let config = EngineConfig { autostart_dir: Some(agents.clone()), ..EngineConfig::default() };
    let engine = isolated(temporary.path(), &FakeDevice::iphone(UDID), config);
    let program = temporary.path().join("bin/sideport");
    let (view, mut cx) = window(engine.clone(), cx);

    cx.update(|_, cx| view.update(cx, |view, _| view.daemon_program = Some(program.clone())));
    click(&mut cx, "nav:Settings");
    click(&mut cx, "autostart");

    assert!(engine.autostart(), "the login item is installed at once");
    let entry = fs::read_dir(&agents).expect("login items").next().expect("entry").expect("entry").path();
    let written = fs::read_to_string(&entry).expect("login item");
    assert!(written.contains(program.to_str().expect("path")) && written.contains("daemon"), "{written}");

    click(&mut cx, "autostart");
    assert!(!engine.autostart());
    assert!(!entry.exists());

    let demo = demo_engine(&temporary.path().join("demo"));
    assert!(demo.set_autostart(true, &program).is_err(), "the demo never installs login items");
}

#[gpui::test]
async fn services_are_unconfigured_by_default_and_the_update_check_contacts_nothing(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (view, mut cx) = window(engine(temporary.path()), cx);

    click(&mut cx, "nav:Settings");
    assert!(rendered(&mut cx, "services-status"));

    let status = cx.read(|cx| view.read(cx).engine.services_status());
    assert!(!status.updates_configured && !status.token_verifier_configured && !status.feature_state.token_present);

    click(&mut cx, "check-update");
    until(&mut cx, &view, "update check", |view| view.updates.result.is_some()).await;

    assert_eq!(cx.read(|cx| view.read(cx).updates.result.clone()), Some(Ok(UpdateStatus::NotConfigured)));
    assert!(rendered(&mut cx, "update-result"));
}

#[gpui::test]
async fn a_configured_update_service_reports_available_current_and_failed_checks(cx: &mut TestAppContext) {
    const MANIFEST: &str = "/info/exe/darwin-arm64.json";

    let temporary = tempfile::tempdir().expect("tempdir");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let server = runtime.block_on(MockServer::start());

    // Each manifest answers once, in order; a third check finds no manifest.
    for version in ["1.1.0", "1.0.0"] {
        let body = serde_json::json!({ "version": version, "sha256": format!("{}=", "A".repeat(43)) });
        let response = ResponseTemplate::new(200).set_body_json(body);
        let manifest = Mock::given(method("GET")).and(path(MANIFEST)).respond_with(response);

        runtime.block_on(manifest.up_to_n_times(1).mount(&server));
    }

    let updates = UpdateEndpoints {
        info_base: format!("{}/info/", server.uri()),
        diff_base: format!("{}/diff/", server.uri()),
        binary_base: format!("{}/binary/", server.uri()),
        command: "exe".into(),
        platform: "darwin-arm64".into(),
    };
    let services = ServiceConfig {
        updates: Some(updates),
        token_public_key_pem: None,
        current_version: "1.0.0".into(),
        user_agent: None,
    };
    let config = EngineConfig { services: Some(services), ..EngineConfig::default() };
    let (view, mut cx) = window(isolated(temporary.path(), &FakeDevice::iphone(UDID), config), cx);

    click(&mut cx, "nav:Settings");
    assert!(cx.read(|cx| view.read(cx).engine.services_status().updates_configured));

    let mut outcomes = Vec::new();

    for _ in 0..3 {
        click(&mut cx, "check-update");
        until(&mut cx, &view, "update check", |view| !view.updates.checking && view.updates.result.is_some()).await;
        outcomes.push(cx.read(|cx| view.read(cx).updates.result.clone()).expect("result"));
    }

    let messages: Vec<_> = outcomes.iter().map(|outcome| outcome.as_ref().map(update_message)).collect();
    let available = messages[0].as_ref().is_ok_and(|message| message.starts_with("Version 1.1.0 is available"));

    assert!(available, "{messages:?}");
    assert_eq!(messages[1], Ok("Sideport 1.0.0 is up to date.".into()));
    assert!(messages[2].is_err(), "a missing manifest is reported: {messages:?}");

    let requests = runtime.block_on(server.received_requests()).expect("recorded requests");
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| request.url.path() == MANIFEST), "nothing is downloaded");
}
