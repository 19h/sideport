#[path = "../../sl-bundle/tests/common/mod.rs"]
mod common;

use futures::executor::block_on;
use sl_app::{Draft, ExportMode};
use sl_engine::{BundleIdPolicy, Engine, EngineConfig, ExtensionRemoval, FileReplacement, InfoValue, SigningMode};
use std::path::PathBuf;

#[test]
fn the_form_preserves_unchanged_fields_and_maps_typed_overrides() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let engine = Engine::new(EngineConfig { data_dir: Some(temporary.path().join("data")), ..EngineConfig::default() })
        .expect("engine");
    let app = block_on(engine.inspect(root)).expect("inspect");
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
    let engine = Engine::new(EngineConfig { data_dir: Some(temporary.path().join("data")), ..EngineConfig::default() })
        .expect("engine");
    let app = block_on(engine.inspect(root)).expect("inspect");

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
    let engine = Engine::new(EngineConfig { data_dir: Some(temporary.path().join("data")), ..EngineConfig::default() })
        .expect("engine");
    let app = block_on(engine.inspect(root)).expect("inspect");
    let mut draft = Draft::for_app(&app);
    draft.identifier.clear();
    draft.extra_info = "invalid JSON".into();
    draft.options.enable_file_sharing = true;
    draft.options.replacements.push(FileReplacement { target: "../outside".into(), source: None });

    let job = draft.job(&app, ExportMode::Original, None).expect("original");

    assert_eq!(job.signing, SigningMode::Original);
    assert_eq!(job.options, sl_engine::AppOptions::default());
}
