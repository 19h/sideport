#[path = "../../sl-bundle/tests/common/mod.rs"]
mod common;

use futures::executor::block_on;
use sl_app::{Draft, ExportMode, IdentifierPolicy};
use sl_engine::{
    AppSummary, BundleIdPolicy, DeviceBackend, Engine, EngineConfig, ExtensionRemoval, FileReplacement, InfoValue,
    SigningMode, Target,
};
use sl_testkit::FakeDevice;
use std::path::{Path, PathBuf};

/// Inspect with an engine that uses no keychain, system device service or scheduler.
fn inspect(directory: &Path, root: PathBuf) -> AppSummary {
    let config = EngineConfig {
        data_dir: Some(directory.join("data")),
        file_secrets: true,
        disable_scheduler: true,
        device_backend: Some(DeviceBackend(FakeDevice::iphone("00008030-001A2D0C0E38802E").backend())),
        ..EngineConfig::default()
    };
    let engine = Engine::new(config).expect("engine");

    block_on(engine.inspect(root)).expect("inspect")
}

#[test]
fn the_form_preserves_unchanged_fields_and_maps_typed_overrides() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let app = inspect(temporary.path(), root);
    let mut draft = Draft::for_app(&app);

    let unchanged = draft.job(&app, ExportMode::Unsigned, None).expect("unchanged");
    assert_eq!(unchanged.options.bundle_id, BundleIdPolicy::Original);
    assert_eq!(unchanged.options.display_name, None);
    assert_eq!(unchanged.options.version, None);
    assert_eq!(unchanged.options.minimum_os, None);

    draft.identifier = "com.example.changed".into();
    draft.name = "Prepared Test".into();
    draft.extra_info = r#"{"Text":"value","Enabled":true,"Count":-9,"Removed":null}"#.into();
    let changed = draft.job(&app, ExportMode::AdHoc, Some("output.ipa".into())).expect("changed");

    assert_eq!(changed.signing, SigningMode::AdHoc);
    assert_eq!(changed.options.bundle_id, BundleIdPolicy::Custom("com.example.changed".into()));
    assert_eq!(changed.options.display_name.as_deref(), Some("Prepared Test"));

    for (key, expected) in [
        ("Text", InfoValue::String("value".into())),
        ("Enabled", InfoValue::Bool(true)),
        ("Count", InfoValue::Integer(-9)),
        ("Removed", InfoValue::Remove),
    ] {
        assert_eq!(changed.options.extra_info.iter().find(|item| item.key == key).expect("override").value, expected);
    }
}

#[test]
fn invalid_metadata_and_relative_paths_fail_before_a_job_is_created() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let app = inspect(temporary.path(), root);

    for identifier in ["", "com..test", ".test", "test.", "com.test space", "com.test/other", "com.é"] {
        let mut draft = Draft::for_app(&app);
        draft.identifier = identifier.into();
        assert!(draft.job(&app, ExportMode::Unsigned, None).is_err(), "{identifier}");
    }

    for overrides in [
        "[]",
        "{",
        r#"{"Value":1.25}"#,
        r#"{"Value":9223372036854775808}"#,
        r#"{"Value":{}}"#,
        r#"{"Value":"\u0000"}"#,
        r#"{"":1}"#,
    ] {
        let mut draft = Draft::for_app(&app);
        draft.extra_info = overrides.into();
        assert!(draft.job(&app, ExportMode::Unsigned, None).is_err(), "{overrides}");
    }

    for target in ["../outside", "/absolute", "Frameworks/../outside", "a//b", "a/./b", "a/", "C:/file", "a\\b"] {
        let mut draft = Draft::for_app(&app);
        draft.options.replacements.push(FileReplacement { target: PathBuf::from(target), source: None });
        assert!(draft.job(&app, ExportMode::Unsigned, None).is_err(), "{target}");
    }

    let mut draft = Draft::for_app(&app);
    draft.options.remove_extensions = ExtensionRemoval::Selected(vec!["Absent.appex".into()]);
    assert!(draft.job(&app, ExportMode::Unsigned, None).is_err());
}

#[test]
fn original_export_ignores_edits_including_an_invalid_draft() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let app = inspect(temporary.path(), root);
    let mut draft = Draft::for_app(&app);
    draft.identifier.clear();
    draft.extra_info = "invalid JSON".into();
    draft.options.enable_file_sharing = true;
    draft.options.replacements.push(FileReplacement { target: "../outside".into(), source: None });

    let job = draft.job(&app, ExportMode::Original, None).expect("original");

    assert_eq!(job.signing, SigningMode::Original);
    assert_eq!(job.options, sl_engine::AppOptions::default());
}

#[test]
fn apple_id_installs_map_account_identifier_policy_and_device_options() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let app = inspect(temporary.path(), root);
    let device = Target::Device { udid: "00008030-001A2D0C0E38802E".into(), prefer_network: false };

    let mut draft = Draft::for_app(&app);
    draft.options.entitlements = Some("extra.entitlements".into());
    draft.options.provision_extensions = true;
    draft.options.stream_upload = true;
    draft.options.track_for_refresh = true;
    draft.options.tvos_for_apple_tv = true;
    draft.upload_chunk = " 8 ".into();

    let missing = draft.spec(&app, ExportMode::AppleId, device.clone());
    assert!(missing.is_err_and(|error| error.contains("Apple ID")), "an account is required");

    draft.apple_id = Some("fixture@example.test".into());
    let install = draft.spec(&app, ExportMode::AppleId, device.clone()).expect("Apple ID install");

    assert_eq!(install.target, device);
    assert_eq!(install.signing, SigningMode::AppleId { apple_id: "fixture@example.test".into() });
    assert_eq!(install.options.bundle_id, BundleIdPolicy::Auto);
    assert_eq!(install.options.entitlements, Some("extra.entitlements".into()));
    assert!(install.options.provision_extensions && install.options.stream_upload);
    assert!(install.options.track_for_refresh && install.options.tvos_for_apple_tv);
    assert_eq!(install.options.upload_chunk_mib, Some(8));

    draft.identifier_policy = IdentifierPolicy::Original;
    let original = draft.spec(&app, ExportMode::AppleId, device.clone()).expect("original identifier");
    assert_eq!(original.options.bundle_id, BundleIdPolicy::Original);

    draft.identifier_policy = IdentifierPolicy::Custom;
    draft.identifier = "com.example.custom".into();
    let custom = draft.spec(&app, ExportMode::AppleId, device.clone()).expect("custom identifier");
    assert_eq!(custom.options.bundle_id, BundleIdPolicy::Custom("com.example.custom".into()));

    draft.identifier = "not valid".into();
    assert!(draft.spec(&app, ExportMode::AppleId, device.clone()).is_err(), "custom identifiers are validated");

    draft.identifier_policy = IdentifierPolicy::Automatic;
    let export = draft.job(&app, ExportMode::AppleId, None).expect("Apple ID export ignores the unused field");
    assert_eq!(export.options.bundle_id, BundleIdPolicy::Auto);
    assert_eq!(export.options.entitlements, Some("extra.entitlements".into()));
    assert!(!export.options.stream_upload && !export.options.track_for_refresh && !export.options.tvos_for_apple_tv);
    assert_eq!(export.options.upload_chunk_mib, None, "device options stay off for exports");
}

#[test]
fn device_options_apply_only_where_the_engine_accepts_them() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let app = inspect(temporary.path(), root);
    let device = Target::Device { udid: "00008030-001A2D0C0E38802E".into(), prefer_network: true };

    let mut draft = Draft::for_app(&app);
    draft.options.entitlements = Some("extra.entitlements".into());
    draft.options.provision_extensions = true;
    draft.options.stream_upload = true;
    draft.options.track_for_refresh = true;
    draft.options.tvos_for_apple_tv = true;

    let unsigned = draft.spec(&app, ExportMode::Unsigned, device.clone());
    assert!(unsigned.is_err_and(|error| error.contains("cannot be installed")));

    let adhoc = draft.spec(&app, ExportMode::AdHoc, device.clone()).expect("ad-hoc install");
    assert_eq!(adhoc.signing, SigningMode::AdHoc);
    assert_eq!(adhoc.options.entitlements, None, "entitlements need Apple ID signing");
    assert!(!adhoc.options.provision_extensions && !adhoc.options.track_for_refresh);
    assert!(!adhoc.options.tvos_for_apple_tv && adhoc.options.stream_upload);

    let original = draft.spec(&app, ExportMode::Original, device.clone()).expect("original install");
    assert_eq!(original.signing, SigningMode::Original);
    assert!(original.options.stream_upload && original.options.entitlements.is_none());

    for chunk in ["0", "65", "1.5", "big"] {
        draft.upload_chunk = chunk.into();
        assert!(draft.spec(&app, ExportMode::AdHoc, device.clone()).is_err(), "{chunk}");
    }

    draft.upload_chunk = "64".into();
    let largest = draft.spec(&app, ExportMode::AdHoc, device).expect("64 MiB chunks");
    assert_eq!(largest.options.upload_chunk_mib, Some(64));
}
