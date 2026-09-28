//! Device utilities through the rendered Devices section, with the real engine against the fake
//! device layer and a wiremock Developer Disk Image mirror: connection kind and heartbeat, image
//! mounting with progress, JIT for developer-signed apps only, app-change notifications and a
//! confirmed pairing repair. These fixtures establish the desktop flow, not physical devices.

use super::*;
use sl_device::{InstalledApp, jit::AppLaunch};
use sl_engine::DdiConfig;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const DEVELOPER_APP: &str = "com.example.developer";
const STORE_APP: &str = "com.example.store";
const UNLAUNCHABLE_APP: &str = "com.example.unlaunchable";
const IMAGE: &str = "/pdso/DeveloperDiskImage/master/16.5/DeveloperDiskImage.dmg";

/// A mirror serving the legacy iOS 16.5 image slowly enough to observe the running job.
fn image_mirror() -> (tokio::runtime::Runtime, MockServer) {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let server = runtime.block_on(MockServer::start());

    runtime.block_on(async {
        for repository in ["xushuduo/Xcode-iOS-Developer-Disk-Image", "mspvirajpatel/Xcode_Developer_Disk_Images"] {
            let release = format!("/repos/{repository}/releases/tags/16.5");
            Mock::given(method("GET")).and(path(release)).respond_with(ResponseTemplate::new(404)).mount(&server).await;
        }

        let image = ResponseTemplate::new(200).set_body_bytes(b"image".to_vec()).set_delay(Duration::from_millis(600));
        Mock::given(method("GET")).and(path(IMAGE)).respond_with(image).mount(&server).await;

        let signature = ResponseTemplate::new(200).set_body_bytes(b"signature".to_vec());
        Mock::given(method("GET")).and(path(format!("{IMAGE}.signature"))).respond_with(signature).mount(&server).await;
    });

    (runtime, server)
}

fn installed(bundle_id: &str, name: &str, developer: bool) -> InstalledApp {
    InstalledApp { bundle_id: bundle_id.into(), name: name.into(), version: Some("1.0".into()), developer }
}

fn logged(view: &Entity<Sideport>, cx: &mut VisualTestContext, text: &str) -> bool {
    cx.read(|cx| view.read(cx).logs.iter().any(|(_, message)| message.contains(text)))
}

#[gpui::test]
async fn device_utilities_mount_enable_jit_follow_app_changes_and_repair_pairing(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (_runtime, mirror) = image_mirror();
    let device = FakeDevice::new(UDID, "Fixture iPhone", "iPhone", "iPhone12,1", "16.5");

    {
        let mut state = device.state();
        let launch = AppLaunch {
            path: "/private/var/containers/Bundle/Application/AAAA/Developer.app".into(),
            container: Some("/private/var/mobile/Containers/Data/Application/BBBB".into()),
            executable: "Developer".into(),
        };

        state.apps = vec![
            installed(DEVELOPER_APP, "Developer", true),
            installed(STORE_APP, "Store", false),
            installed(UNLAUNCHABLE_APP, "Unlaunchable", true),
        ];
        state.launch.insert(DEVELOPER_APP.into(), launch);
    }

    let mirror = Some(mirror.uri());
    let ddi = DdiConfig { github_api: mirror.clone(), raw_content: mirror.clone(), tss_endpoint: mirror };
    let engine = isolated(temporary.path(), &device, EngineConfig { ddi, ..EngineConfig::default() });
    let (view, mut cx) = window(engine, cx);

    click(&mut cx, "nav:Devices");
    until(&mut cx, &view, "device contents", |view| view.devices.apps.is_some()).await;

    // The connection kind is shown; a heartbeat the device does not answer is reported.
    assert!(rendered(&mut cx, &format!("connection:{UDID}")));

    click(&mut cx, "check-connection");
    until(&mut cx, &view, "unanswered heartbeat", |view| view.devices.tools.checking.is_none()).await;

    let error = cx.read(|cx| view.read(cx).error.clone()).expect("unanswered heartbeat");
    assert!(error.starts_with("Connection check failed"), "{error}");
    assert!(rendered(&mut cx, "error"));

    device.state().heartbeat = Some(2);
    click(&mut cx, "check-connection");
    until(&mut cx, &view, "heartbeat", |view| view.devices.tools.reachable.is_some()).await;
    assert_eq!(cx.read(|cx| view.read(cx).devices.tools.reachable.clone()), Some((UDID.into(), 2)));
    assert_eq!(cx.read(|cx| view.read(cx).error.clone()), None);

    // Mounting runs in the job slot; its stage and latest step show while the image downloads.
    click(&mut cx, "mount-ddi");
    assert!(cx.read(|cx| view.read(cx).busy), "mounting occupies the job slot");
    assert!(rendered(&mut cx, "utility-progress") && rendered(&mut cx, "cancel-job"));

    until(&mut cx, &view, "mounted image", |view| !view.busy).await;

    cx.read(|cx| {
        let view = view.read(cx);

        assert_eq!(view.error, None, "{:?}", view.logs);
        assert_eq!(view.status, "Mounted the Developer Disk Image on Fixture iPhone.");
        assert!(view.devices.tools.running.is_none());
    });
    assert!(logged(&view, &mut cx, "Resolving a Developer Disk Image for iOS 16.5"));
    assert!(rendered(&mut cx, "utility-notice"));

    {
        let state = device.state();
        let [(image_type, image, _)] = state.mounts.as_slice() else { panic!("one mount") };

        assert_eq!((image_type.as_str(), image.as_slice()), ("Developer", &b"image"[..]));
    }

    // JIT is offered for developer-signed apps only, and launches then detaches.
    assert!(rendered(&mut cx, &format!("jit:{DEVELOPER_APP}")));
    assert!(!rendered(&mut cx, &format!("jit:{STORE_APP}")), "App Store apps are not offered JIT");

    click(&mut cx, &format!("jit:{DEVELOPER_APP}"));
    until(&mut cx, &view, "JIT", |view| !view.busy).await;

    assert_eq!(cx.read(|cx| view.read(cx).error.clone()), None);
    assert_eq!(cx.read(|cx| view.read(cx).status.clone()), "JIT enabled for Developer");

    {
        let state = device.state();
        let launch = ["A /private/var/containers/Bundle/Application/AAAA/Developer.app", "qLaunchSuccess", "D"];

        assert_eq!(state.mounts.len(), 1, "the mounted image is reused");
        assert!(state.jit_commands.ends_with(&launch.map(String::from)), "{:?}", state.jit_commands);
    }

    click(&mut cx, &format!("jit:{UNLAUNCHABLE_APP}"));
    until(&mut cx, &view, "failed JIT", |view| !view.busy).await;

    let error = cx.read(|cx| view.read(cx).error.clone()).expect("failed JIT");
    assert!(error.contains("not installed"), "{error}");

    // Following app changes reloads the list when the device reports an installation.
    {
        let mut state = device.state();
        state.apps.push(installed("com.example.new", "New", true));
        state.notifications = vec!["com.apple.mobile.application_installed".into()];
    }

    let lists_new_app = |view: &Sideport| {
        view.devices.apps.as_ref().is_some_and(|(_, apps)| apps.iter().any(|app| app.bundle_id == "com.example.new"))
    };

    click(&mut cx, "follow-apps");
    until(&mut cx, &view, "reloaded apps", lists_new_app).await;
    assert!(logged(&view, &mut cx, "Fixture iPhone: com.apple.mobile.application_installed"));

    click(&mut cx, "follow-apps");
    assert_eq!(cx.read(|cx| view.read(cx).devices.tools.following.clone()), None);

    // Repairing pairing asks first, explains the trust dialog and ignores Enter.
    click(&mut cx, "repair-pairing");
    assert!(rendered(&mut cx, "dialog-submit-danger"));
    cx.read(|cx| match &view.read(cx).dialog {
        Some(Dialog::Confirm(confirmation)) => assert!(confirmation.message.contains("trust this computer again")),
        _ => panic!("pairing repair confirmation"),
    });

    cx.simulate_keystrokes("enter");
    assert!(!device.state().unpaired, "Enter does not confirm a pairing repair");

    let repaired = |view: &Sideport| view.devices.pairing.is_none() && view.status == "Pairing repaired";

    click(&mut cx, "dialog-submit-danger");
    until(&mut cx, &view, "pairing repair", repaired).await;

    let repaired_state = {
        let state = device.state();

        state.unpaired && state.paired
    };
    assert!(repaired_state, "the device was unpaired, then paired again");
    assert!(logged(&view, &mut cx, "Removing the existing pairing"));
}

#[gpui::test]
async fn demo_devices_show_their_connection_and_simulated_utilities(cx: &mut TestAppContext) {
    const WIFI_IPAD: &str = "00008103-000E4C1A0C38801E";

    let temporary = tempfile::tempdir().expect("tempdir");
    let (view, mut cx) = window(demo_engine(temporary.path()), cx);

    click(&mut cx, "nav:Devices");
    until(&mut cx, &view, "demo devices", |view| view.devices.listed).await;

    click(&mut cx, &format!("device:{WIFI_IPAD}"));

    let listed_for_ipad = |view: &Sideport| view.devices.apps.as_ref().is_some_and(|(udid, _)| udid == WIFI_IPAD);
    until(&mut cx, &view, "iPad contents", listed_for_ipad).await;

    assert!(rendered(&mut cx, &format!("connection:{WIFI_IPAD}")));

    click(&mut cx, "mount-ddi");
    until(&mut cx, &view, "the simulated mount", |view| !view.busy).await;
    assert!(logged(&view, &mut cx, "Mounted the developer image"), "the demo engine simulated the mount");

    let connections = cx.read(|cx| view.read(cx).selected_device().map(|device| device.connections.clone()));
    assert_eq!(connections.as_deref().map(crate::app::utilities::connection_label).as_deref(), Some("Wi-Fi"));
}
