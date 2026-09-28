//! Session import, Apple ID provisioning, device installation and refresh through the rendered
//! controls, with the real engine against the fake developer portal and a fake device layer.
//! These fixtures establish the desktop flow; Apple's service and physical devices are not used.

use super::*;
use sl_engine::AnisetteSetting;
use sl_testkit::{FakePortal, ProfileChain};
use wiremock::MockServer;

const APPLE_ID: &str = "fixture@example.test";
const TEAM: &str = "TEAM123456";

struct Portal {
    server: MockServer,
    portal: FakePortal,
    /// Keeps the runtime that mounted the fake alive for the test.
    _runtime: tokio::runtime::Runtime,
}

impl Portal {
    fn start() -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let portal = FakePortal::new(TEAM, true);

        let server = runtime.block_on(async {
            let server = MockServer::start().await;
            portal.mount(&server).await;

            server
        });

        Self { server, portal, _runtime: runtime }
    }

    fn engine(&self, root: &Path, device: &FakeDevice) -> Engine {
        let config = EngineConfig {
            portal_origin: Some(FakePortal::origin(&self.server)),
            profile_trust: Some(ProfileChain::shared().trust()),
            ..EngineConfig::default()
        };
        let engine = isolated(root, device, config);

        let mut settings = engine.settings();
        settings.anisette = AnisetteSetting::Remote { url: format!("{}/anisette", self.server.uri()) };
        engine.update_settings(settings).expect("settings");

        engine
    }
}

/// A recovered Sideloadly `sessions.json` with one GrandSlam session and one legacy entry.
fn sessions_file(root: &Path) -> PathBuf {
    let sessions = serde_json::json!({
        format!("{APPLE_ID}:a"): { "_type": "GsaAuthenticator", "dsid": "42", "gs_token": "fixture-token" },
        "legacy@example.test:i": { "_type": "IdmsAuthenticator", "myacinfo": "legacy" },
    });
    let path = root.join("sessions.json");

    fs::write(&path, serde_json::to_vec(&sessions).expect("JSON")).expect("sessions");

    path
}

#[gpui::test]
async fn imported_sessions_provision_install_and_refresh_through_the_ui(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let portal = Portal::start();
    let device = FakeDevice::iphone(UDID);
    let source = temporary.path().join("App.app");
    common::synthetic_bundle(&source, "com.example.app", "App", "APPL");

    let engine = portal.engine(temporary.path(), &device);
    let (view, mut cx) = window(engine, cx);
    let sessions = sessions_file(temporary.path());
    cx.update(|_, cx| view.update(cx, |view, _| view.accounts.sessions_path = Some(sessions)));

    click(&mut cx, "nav:Accounts");
    click(&mut cx, "import-sessions");
    until(&mut cx, &view, "session import", |view| view.accounts.import.is_some()).await;

    cx.read(|cx| {
        let report = view.read(cx).accounts.import.clone().expect("report");

        assert_eq!(report.imported, [APPLE_ID]);
        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
    });
    assert!(rendered(&mut cx, "import-report"));
    assert!(rendered(&mut cx, &format!("account:{APPLE_ID}")));

    cx.update(|window, cx| view.update(cx, |view, cx| view.load_path(source.clone(), window, cx)));
    until(&mut cx, &view, "inspection", |view| view.app.is_some() && !view.busy).await;

    click(&mut cx, "mode:Apple ID");
    click(&mut cx, "destination:Install on device");
    until(&mut cx, &view, "device list", |view| view.devices.selected.as_deref() == Some(UDID)).await;

    assert_eq!(cx.read(|cx| view.read(cx).draft.apple_id.clone()).as_deref(), Some(APPLE_ID));
    assert_eq!(cx.read(|cx| view.read(cx).action_blocker()), None);

    click(&mut cx, "primary-action");
    until(&mut cx, &view, "installation", |view| !view.busy).await;

    let bundle_id = format!("com.example.app.{TEAM}");
    let installation_id = cx.read(|cx| {
        let view = view.read(cx);
        let facts = &view.facts;

        assert_eq!(view.error, None, "{:?}", view.logs);
        assert_eq!(view.status, "Installed");
        assert_eq!(facts.team.as_ref().map(|team| team.team_id.as_str()), Some(TEAM));
        assert_eq!(facts.bundle_id.as_deref(), Some(bundle_id.as_str()));
        assert!(facts.quota.is_some(), "free teams report their App ID quota");
        assert!(facts.expires.is_some());

        view.outcome.as_ref().and_then(|outcome| outcome.installation_id).expect("tracked installation")
    });

    {
        let state = device.state();
        let [installed] = state.installed.as_slice() else { panic!("one installed package") };

        assert_eq!(installed.bundle_id, bundle_id);
    }
    assert_eq!(portal.portal.state().count("ios/addDevice"), 1, "the device was registered");

    click(&mut cx, "nav:Installations");
    assert!(rendered(&mut cx, &format!("installation:{installation_id}")));
    cx.read(|cx| {
        let view = view.read(cx);
        let [installation] = view.installations.list.as_slice() else { panic!("one installation") };

        assert_eq!((installation.device_name.as_str(), installation.apple_id.as_str()), ("Fixture iPhone", APPLE_ID));
        assert!(installation.auto_refresh);
    });

    click(&mut cx, &format!("refresh:{installation_id}"));
    until(&mut cx, &view, "refresh", |view| !view.busy).await;
    until(&mut cx, &view, "refresh notice", |view| view.installations.notice.as_deref() == Some("Refreshed App")).await;

    assert_eq!(cx.read(|cx| view.read(cx).error.clone()), None);
    assert_eq!(device.state().installed.len(), 2);
    assert_eq!(portal.portal.state().count("ios/submitDevelopmentCSR"), 1, "refresh reuses the certificate");
}
