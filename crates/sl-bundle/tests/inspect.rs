mod common;

use sl_bundle::{ArchiveLimits, BundleArchive, Control, OutputLayout, PackOptions, inspect};
use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

#[test]
fn inspects_directory_ipa_and_flipped_ipa_with_extensions_and_largest_declared_icon() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    common::synthetic_bundle(&root.join("PlugIns/Widget.appex"), "com.example.test.widget", "Widget", "XPC!");
    common::write_info(&root.join("Watch/Watch.app"), {
        let mut info = common::info("com.example.watch", "Watch", "APPL");
        info.insert("WKWatchKitApp".into(), true.into());
        info
    });

    let mut info = sl_bundle::read_dictionary(&root.join("Info.plist")).expect("info");
    info.insert("CFBundleIconFiles".into(), plist::Value::Array(vec!["AppIcon".into()]));
    common::write_info(&root, info);

    for (filename, dimension) in [("AppIcon.png", 20), ("AppIcon@2x.png", 40), ("Other.png", 100)] {
        image::RgbaImage::from_pixel(dimension, dimension, image::Rgba([10, 20, 30, 255]))
            .save(root.join(filename))
            .expect("icon");
    }

    let archive = BundleArchive::unpack(&root, ArchiveLimits::default(), Control::default()).expect("unpack");
    let ipa = temporary.path().join("input.ipa");
    archive.save(&ipa, OutputLayout::Ipa, PackOptions::default(), Control::default()).expect("pack");
    let flipped = temporary.path().join("flipped.ipa");
    fs::write(&flipped, fs::read(&ipa).expect("IPA").into_iter().map(|byte| byte ^ 0xaa).collect::<Vec<_>>())
        .expect("flip");

    for source in [&root, &ipa, &flipped] {
        let result = inspect(source, ArchiveLimits::default(), Control::default()).expect("inspect");
        let icon = image::load_from_memory(result.icon_png.as_ref().expect("icon")).expect("PNG");

        assert_eq!(result.info["CFBundleIdentifier"].as_string(), Some("com.example.test"));
        assert_eq!(result.extensions[0].file_name, "Widget.appex");
        assert!(result.has_watch_app);
        assert!(!result.encrypted);
        assert_eq!(icon.width(), 40);
        assert!(result.warnings.is_empty());
    }
}

#[test]
fn malformed_optional_icon_is_reported_and_inspection_cancellation_is_observed() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    fs::write(root.join("Icon.png"), b"not a PNG").expect("icon");
    let result = inspect(&root, ArchiveLimits::default(), Control::default()).expect("inspect");

    assert!(result.icon_png.is_none());
    assert_eq!(result.warnings.len(), 1);

    let checks = AtomicUsize::new(0);
    let cancelled = || checks.fetch_add(1, Ordering::Relaxed) > 4;
    let control = Control { is_cancelled: Some(&cancelled), on_progress: None };

    assert!(matches!(inspect(&root, ArchiveLimits::default(), control), Err(sl_bundle::Error::Cancelled)));
}

#[cfg(unix)]
#[test]
fn metadata_symlinks_resolve_inside_the_app_and_escape_is_rejected() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    fs::rename(root.join("Test"), root.join("Actual")).expect("rename");
    symlink("Actual", root.join("Test")).expect("symlink");
    let archive = BundleArchive::unpack(&root, ArchiveLimits::default(), Control::default()).expect("unpack");
    let ipa = temporary.path().join("linked.ipa");
    archive.save(&ipa, OutputLayout::Ipa, PackOptions::default(), Control::default()).expect("pack");

    assert!(inspect(&ipa, ArchiveLimits::default(), Control::default()).is_ok());

    fs::remove_file(root.join("Test")).expect("unlink");
    fs::write(temporary.path().join("Outside"), common::macho()).expect("outside");
    symlink("../Outside", root.join("Test")).expect("escape");

    assert!(inspect(&root, ArchiveLimits::default(), Control::default()).is_err());
}

#[test]
fn asset_catalog_icons_are_explicitly_pending_and_invalid_executable_types_fail() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    fs::write(root.join("Assets.car"), b"catalog placeholder").expect("catalog");
    let result = inspect(&root, ArchiveLimits::default(), Control::default()).expect("inspect");

    assert!(result.icon_png.is_none());
    assert!(result.warnings.iter().any(|warning| warning.contains("Asset-catalog")));

    let mut info = sl_bundle::read_dictionary(&root.join("Info.plist")).expect("info");
    info.insert("CFBundleExecutable".into(), 42.into());
    common::write_info(&root, info);
    assert!(inspect(&root, ArchiveLimits::default(), Control::default()).is_err());
}

#[cfg(unix)]
#[test]
fn nonregular_archive_and_metadata_files_are_rejected_without_opening_their_streams() {
    use std::process::Command;

    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    fs::remove_file(root.join("Test")).expect("remove executable");
    common::run(Command::new("mkfifo").arg(root.join("Test")));
    common::run(Command::new("mkfifo").arg(temporary.path().join("input.ipa")));

    assert!(inspect(&root, ArchiveLimits::default(), Control::default()).is_err());
    assert!(inspect(&temporary.path().join("input.ipa"), ArchiveLimits::default(), Control::default()).is_err());
}
