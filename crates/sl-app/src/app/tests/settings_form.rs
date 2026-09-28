//! Anisette, refresh and default settings through the rendered form.

use super::*;
use sl_engine::AnisetteSetting;

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
