mod common;

use plist::{Dictionary, Value};
use sl_bundle::{
    ArchiveLimits, BundleArchive, Control, Injection, PatchOptions, ProfileRequirements, PropertyEdit, Replacement,
    SigningRequest,
};
use sl_codesign::Signer;
use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

fn unpack(root: &Path) -> BundleArchive {
    BundleArchive::unpack(root, ArchiveLimits::default(), Control::default()).expect("unpack")
}

#[test]
fn metadata_edits_preserve_suffixes_original_ids_and_documented_child_policy() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.old.app", "Test", "APPL");
    common::synthetic_bundle(&root.join("PlugIns/Widget.appex"), "com.old.app.widget", "Widget", "XPC!");
    common::synthetic_bundle(&root.join("Extensions/Other.appex"), "org.other.extension", "Other", "XPC!");
    common::write_info(&root.join("Pack.stickerpack"), common::info("com.old.app.pack", "Pack", "BNDL"));

    let mut info = sl_bundle::read_dictionary(&root.join("Info.plist")).expect("info");
    let mut url = Dictionary::new();
    url.insert("CFBundleURLName".into(), "com.old.app".into());
    info.insert("CFBundleURLTypes".into(), Value::Array(vec![Value::Dictionary(url)]));
    info.insert("CFBundleDisplayName".into(), "Original".into());
    common::write_info(&root, info);

    fs::create_dir(root.join("en.lproj")).expect("localization");
    fs::write(
        root.join("en.lproj/InfoPlist.strings"),
        r#"/* localized */ "CFBundleDisplayName" = "Localized"; "Keep" = "Yes";"#,
    )
    .expect("strings");

    let mut archive = unpack(&root);
    let edits = BTreeMap::from([
        (
            "CFBundleIdentifier".into(),
            PropertyEdit::Transform(Arc::new(|original| {
                let identifier = original.and_then(Value::as_string).expect("original identifier");

                Ok(Some(format!("{identifier}.TEAM123456").into()))
            })),
        ),
        ("ALTBundleIdentifier".into(), PropertyEdit::Set("ignored supplied value".into())),
        ("CFBundleDisplayName".into(), PropertyEdit::Set("New Name".into())),
        ("UIFileSharingEnabled".into(), PropertyEdit::Set(true.into())),
    ]);
    let options = PatchOptions { info: edits, ..PatchOptions::default() };

    let report = archive.patch(&options, Control::default()).expect("patch");
    let root = archive.bundle_path();
    let main = sl_bundle::read_dictionary(&root.join("Info.plist")).expect("main");
    let widget = sl_bundle::read_dictionary(&root.join("PlugIns/Widget.appex/Info.plist")).expect("widget");
    let other = sl_bundle::read_dictionary(&root.join("Extensions/Other.appex/Info.plist")).expect("other");
    let pack = sl_bundle::read_dictionary(&root.join("Pack.stickerpack/Info.plist")).expect("pack");
    let localized = sl_bundle::read_dictionary(&root.join("en.lproj/InfoPlist.strings")).expect("strings");

    assert_eq!(main["CFBundleIdentifier"].as_string(), Some("com.old.app.TEAM123456"));
    assert_eq!(widget["CFBundleIdentifier"].as_string(), Some("com.old.app.TEAM123456.widget"));
    assert_eq!(other["CFBundleIdentifier"].as_string(), Some("org.other.extension"));
    assert_eq!(pack["CFBundleIdentifier"].as_string(), Some("com.old.app.TEAM123456.pack"));
    assert_eq!(widget["ALTBundleIdentifier"].as_string(), Some("com.old.app.widget"));
    assert_eq!(
        main["CFBundleURLTypes"].as_array().expect("URLs")[0].as_dictionary().expect("URL")["CFBundleURLName"]
            .as_string(),
        Some("com.old.app.TEAM123456")
    );
    assert!(!localized.contains_key("CFBundleDisplayName"));
    assert_eq!(localized["Keep"].as_string(), Some("Yes"));
    assert!(report.metadata_changed.contains(&"PlugIns/Widget.appex/Info.plist".into()));
    assert!(fs::read(root.join("Info.plist")).expect("binary plist").starts_with(b"bplist"));
}

#[test]
fn watch_plugins_replacements_and_sinf_cleanup_preserve_surviving_data() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    common::synthetic_bundle(&root.join("PlugIns/Keep.appex"), "com.example.test.keep", "Keep", "XPC!");
    common::synthetic_bundle(&root.join("Extensions/Drop.appex"), "com.example.test.drop", "Drop", "XPC!");
    common::synthetic_bundle(&root.join("Watch/Watch.app"), "com.example.watch", "Watch", "APPL");

    let watch = root.join("Watch/Watch.app");
    let mut info = sl_bundle::read_dictionary(&watch.join("Info.plist")).expect("watch");
    info.insert("WKWatchKitApp".into(), true.into());
    common::write_info(&watch, info);

    fs::create_dir(root.join("SC_Info")).expect("SINF");
    fs::write(root.join("SC_Info/main.sinf"), b"retained").expect("SINF file");
    let mut manifest = Dictionary::new();
    manifest.insert(
        "SinfReplicationPaths".into(),
        Value::Array(vec![
            "SC_Info/main.sinf".into(),
            "Watch/Watch.app/removed.sinf".into(),
            "Extensions/Drop.appex/removed.sinf".into(),
        ]),
    );
    Value::Dictionary(manifest).to_file_xml(root.join("SC_Info/Manifest.plist")).expect("manifest");

    let replacement = temporary.path().join("replacement");
    fs::write(&replacement, b"new resource").expect("replacement source");
    let mut archive = unpack(&root);
    let options = PatchOptions {
        remove_extensions: vec!["Drop.appex".into()],
        replacements: vec![Replacement { target: "resource".into(), source: Some(replacement) }],
        ..PatchOptions::default()
    };

    let report = archive.patch(&options, Control::default()).expect("patch");
    let prepared = archive.bundle_path();

    assert!(!prepared.join("Watch").exists());
    assert!(!prepared.join("Extensions/Drop.appex").exists());
    assert!(prepared.join("PlugIns/Keep.appex").exists());
    assert_eq!(fs::read(prepared.join("resource")).expect("replacement"), b"new resource");
    assert!(report.removed.contains(&"Watch".into()));

    let manifest = sl_bundle::read_dictionary(&prepared.join("SC_Info/Manifest.plist")).expect("manifest");

    assert_eq!(
        manifest["SinfReplicationPaths"].as_array().expect("paths"),
        &vec![Value::String("SC_Info/main.sinf".into())]
    );
    assert!(root.join("Watch").exists());

    archive
        .patch(&PatchOptions { drop_plugins: true, ..PatchOptions::default() }, Control::default())
        .expect("drop plugins");

    assert!(!prepared.join("PlugIns").exists());
}

#[test]
fn deep_signing_and_stripping_cover_nested_bundles_and_loose_arm64_code() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    common::synthetic_bundle(&root.join("Frameworks/Kit.framework"), "com.example.kit", "Kit", "FMWK");
    common::synthetic_bundle(&root.join("PlugIns/Widget.appex"), "com.example.test.widget", "Widget", "XPC!");
    fs::write(root.join("Frameworks/libExtra.dylib"), common::macho()).expect("dylib");
    fs::create_dir(root.join("Helpers")).expect("helpers");
    fs::write(root.join("Helpers/ArmHelper"), common::macho()).expect("helper");

    let mut archive = unpack(&root);
    let signer = Signer::AdHoc;
    let request = SigningRequest {
        signer: Some(&signer),
        profile: None,
        profiles: None,
        entitlements: None,
        deep: true,
        requirements: ProfileRequirements::default(),
    };

    let report = archive.sign(request, Control::default()).expect("sign");

    assert_eq!(report.signed.len(), 5);
    assert!(report.skipped.is_empty());
    assert_eq!(report.signed.last().expect("main").file_name().expect("filename"), "Test");

    for path in &report.signed {
        let bytes = fs::read(path).expect("binary");

        assert!(sl_macho::MachO::parse(&bytes).expect("Mach-O").signature.is_some());
    }

    let request = SigningRequest { signer: None, ..request };
    let stripped = archive.sign(request, Control::default()).expect("strip");

    assert_eq!(stripped.signed.len(), 5);

    for path in stripped.signed {
        let bytes = fs::read(path).expect("binary");

        assert!(sl_macho::MachO::parse(&bytes).expect("Mach-O").signature.is_none());
    }

    assert!(!archive.bundle_path().join("_CodeSignature").exists());
    assert!(!archive.bundle_path().join("Frameworks/Kit.framework/_CodeSignature").exists());
}

#[test]
fn library_injection_rewrites_sibling_dependencies_and_is_idempotent() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");

    let first = temporary.path().join("libFirst.dylib");
    let second = temporary.path().join("libSecond.dylib");
    let bytes = common::macho();
    let parsed = sl_macho::MachO::parse(&bytes).expect("Mach-O");
    let dependent = parsed.add_dylib("/usr/lib/libSecond.dylib", false).expect("dependency");
    fs::write(&first, dependent).expect("first");
    fs::write(&second, &bytes).expect("second");

    let mut archive = unpack(&root);
    let injections = [Injection { source: first, name: None }, Injection { source: second, name: None }];

    let report = archive.inject(&injections, Control::default()).expect("inject");

    assert_eq!(report.load_paths.len(), 2);

    let main = archive.bundle_path().join("Test");
    let first_bytes = fs::read(&main).expect("main");
    let image = sl_macho::MachO::parse(&first_bytes).expect("main Mach-O");

    assert_eq!(image.commands.iter().filter(|command| command.kind == sl_macho::LC_LOAD_DYLIB).count(), 2);
    assert_eq!(image.commands.iter().filter(|command| command.kind == sl_macho::LC_RPATH).count(), 1);

    let library = fs::read(archive.bundle_path().join("Frameworks/libFirst.dylib")).expect("first");
    let image = sl_macho::MachO::parse(&library).expect("library Mach-O");
    let command = image.commands.iter().find(|command| command.kind == sl_macho::LC_LOAD_DYLIB).expect("dependency");
    let offset = image.endian.u32(command.bytes, 8).expect("name") as usize;
    let name = command.bytes[offset..].split(|byte| *byte == 0).next().expect("path");

    assert_eq!(name, b"@executable_path/Frameworks/libSecond.dylib");

    archive.inject(&injections, Control::default()).expect("repeat");

    assert_eq!(fs::read(main).expect("same main"), first_bytes);

    let duplicate = [
        Injection { source: injections[0].source.clone(), name: Some("SAME.dylib".into()) },
        Injection { source: injections[1].source.clone(), name: Some("same.dylib".into()) },
    ];

    assert!(archive.inject(&duplicate, Control::default()).is_err());
}

#[test]
fn malformed_metadata_and_path_edits_fail_without_touching_original() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let mut archive = unpack(&root);
    let before = fs::read(root.join("Info.plist")).expect("source");

    let options = PatchOptions {
        info: BTreeMap::from([("CFBundleIdentifier".into(), PropertyEdit::Remove)]),
        ..PatchOptions::default()
    };

    assert!(archive.patch(&options, Control::default()).is_err());

    let options = PatchOptions {
        replacements: vec![Replacement { target: "../outside".into(), source: None }],
        ..PatchOptions::default()
    };

    assert!(archive.patch(&options, Control::default()).is_err());
    assert_eq!(fs::read(root.join("Info.plist")).expect("source unchanged"), before);
}

#[cfg(unix)]
#[test]
fn folder_output_copies_the_prepared_app_with_symlinks_and_refuses_existing_paths() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.folder", "Test", "APPL");
    fs::create_dir(root.join("Resources")).expect("resources");
    fs::write(root.join("Resources/data.txt"), b"data").expect("resource");
    std::os::unix::fs::symlink("Resources/data.txt", root.join("link.txt")).expect("symlink");

    let archive = unpack(&root);
    let output = temporary.path().join("folder");
    archive.save_folder(&output, Control::default()).expect("folder output");

    let app = output.join("Payload/Test.app");
    assert_eq!(fs::read(app.join("Resources/data.txt")).expect("copied"), b"data");
    assert_eq!(fs::read_link(app.join("link.txt")).expect("link"), Path::new("Resources/data.txt"));
    assert_eq!(fs::metadata(app.join("Test")).expect("executable").permissions().mode() & 0o777, 0o755);

    assert!(archive.save_folder(&output, Control::default()).is_err(), "an existing destination is refused");
    assert!(archive.save_folder(&archive.root().join("inside"), Control::default()).is_err());
    assert_eq!(fs::read_dir(temporary.path()).expect("entries").count(), 2, "no temporary directory is left behind");
}
