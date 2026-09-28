//! The Devices section against the real engine and a fake device layer.

use super::*;
use chrono::{Duration as Days, Utc};
use sl_device::InstalledApp;
use sl_testkit::{ProfileChain, profile::ProfileFields};

const PROFILE_UUID: &str = "6B0E4F3A-2C1D-4E5F-8A9B-0C1D2E3F4A5B";

fn fixture_profile() -> Vec<u8> {
    let now = Utc::now();
    let fields = ProfileFields {
        team_id: "TEAM123456".into(),
        identifier: "com.example.sideloaded".into(),
        name: "Sideloaded".into(),
        uuid: PROFILE_UUID.into(),
        created: now - Days::days(1),
        expires: now + Days::days(6),
        free: true,
        platform: "iOS",
        devices: vec![UDID.into()],
        certificates: Vec::new(),
    };

    fields.signed(ProfileChain::shared())
}

#[gpui::test]
async fn devices_pair_then_list_apps_and_profiles_and_confirm_removals(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let device = FakeDevice::iphone(UDID);

    {
        let mut state = device.state();
        state.paired = false;
        state.apps.push(InstalledApp {
            bundle_id: "com.example.sideloaded".into(),
            name: "Sideloaded".into(),
            version: Some("1.0".into()),
            developer: true,
        });
        state.profiles.push(fixture_profile());
    }

    let engine = isolated(temporary.path(), &device, EngineConfig::default());
    let (view, mut cx) = window(engine, cx);

    click(&mut cx, "nav:Devices");
    until(&mut cx, &view, "device list", |view| !view.devices.list.is_empty()).await;

    cx.read(|cx| {
        let view = view.read(cx);

        assert_eq!(view.devices.selected.as_deref(), Some(UDID));
        assert!(!view.devices.list[0].paired, "a device refusing lockdown is listed unpaired");
        assert!(view.devices.apps.is_none(), "unpaired devices are not queried");
    });

    click(&mut cx, &format!("pair:{UDID}"));
    until(&mut cx, &view, "pairing", |view| view.devices.list.first().is_some_and(|device| device.paired)).await;
    until(&mut cx, &view, "device contents", |view| view.devices.apps.is_some() && view.devices.profiles.is_some())
        .await;

    assert!(device.state().paired);

    click(&mut cx, "uninstall:com.example.sideloaded");
    assert!(rendered(&mut cx, "dialog-submit-danger"));
    cx.simulate_keystrokes("enter");
    assert!(device.state().uninstalled.is_empty(), "Enter does not confirm an uninstall");

    click(&mut cx, "dialog-submit-danger");
    until(&mut cx, &view, "uninstall", |view| {
        view.devices.apps.as_ref().is_some_and(|(_, apps)| apps.is_empty()) && view.status.starts_with("Uninstalled")
    })
    .await;
    assert_eq!(device.state().uninstalled, ["com.example.sideloaded"]);

    click(&mut cx, &format!("remove-profile:{PROFILE_UUID}"));
    click(&mut cx, "dialog-cancel");
    assert!(device.state().removed_profiles.is_empty(), "cancel keeps the profile");

    click(&mut cx, &format!("remove-profile:{PROFILE_UUID}"));
    click(&mut cx, "dialog-submit-danger");
    until(&mut cx, &view, "profile removal", |view| view.status.starts_with("Removed profile")).await;
    assert_eq!(device.state().removed_profiles, [PROFILE_UUID]);
}

#[gpui::test]
async fn detached_devices_are_explained_and_block_installation(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let source = temporary.path().join("Test.app");
    common::synthetic_bundle(&source, "com.example.test", "Test", "APPL");
    let device = FakeDevice::iphone(UDID);
    device.state().attached = false;

    let engine = isolated(temporary.path(), &device, EngineConfig::default());
    let (view, mut cx) = window(engine, cx);

    cx.update(|window, cx| view.update(cx, |view, cx| view.load_path(source.clone(), window, cx)));
    until(&mut cx, &view, "inspection", |view| view.app.is_some() && !view.busy).await;

    click(&mut cx, "mode:Ad-hoc signed");
    click(&mut cx, "destination:Install on device");
    until(&mut cx, &view, "device list", |view| view.devices.listed).await;

    assert!(rendered(&mut cx, "action-guidance"));
    assert!(cx.read(|cx| view.read(cx).action_blocker()).is_some_and(|reason| reason.contains("Connect a device")));

    click(&mut cx, "primary-action");
    assert!(!cx.read(|cx| view.read(cx).busy), "the disabled action does not start a job");
}
