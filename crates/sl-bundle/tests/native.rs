#![cfg(target_os = "macos")]

mod common;

use sl_bundle::{ArchiveLimits, BundleArchive, Control, Injection, OutputLayout, PackOptions, SigningRequest};
use sl_codesign::{ProvisioningProfile, Signer, SigningIdentity, blob};
use std::{fs, path::Path, process::Command, sync::Arc};

fn native_fixture(root: &Path, work: &Path) {
    common::write_info(root, common::info("com.example.test", "Test", "APPL"));
    let source = work.join("main.c");
    fs::write(&source, "int main(void) { return 0; }\n").expect("source");
    common::compile(&source, &root.join("Test"), false);

    let framework = root.join("Frameworks/Kit.framework");
    common::write_info(&framework, common::info("com.example.kit", "Kit", "FMWK"));
    let source = work.join("kit.c");
    fs::write(&source, "int kit(void) { return 42; }\n").expect("source");
    common::compile(&source, &framework.join("Kit"), true);
    fs::write(framework.join("resource"), b"original framework resource").expect("framework resource");

    let extension = root.join("PlugIns/Widget.appex");
    common::write_info(&extension, common::info("com.example.test.widget", "Widget", "XPC!"));
    let source = work.join("extension.c");
    fs::write(&source, "int main(void) { return 0; }\n").expect("source");
    common::compile(&source, &extension.join("Widget"), false);
}

#[test]
fn apple_codesign_verifies_nested_universal_bundles_and_injected_code_executes() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    native_fixture(&root, temporary.path());

    let extension = root.join("PlugIns/Widget.appex");
    common::run(Command::new("codesign").args(["--force", "--sign", "-"]).arg(&extension));
    let extension_bytes = fs::read(extension.join("Widget")).expect("Apple-signed extension");
    let extension_binary = sl_macho::Binary::parse(&extension_bytes).expect("extension Mach-O");

    for slice in &extension_binary.slices {
        let range = slice.image.signature.clone().expect("Apple signature");
        let blobs = blob::parse_superblob(&slice.image.bytes[range], blob::EMBEDDED_SIGNATURE).expect("Apple blobs");

        assert_eq!(sl_macho::Endian::Big.u64(blobs[&0], 80).expect("Apple exec flags") & 1, 1);
    }

    let source = temporary.path().join("injected.c");
    let library = temporary.path().join("libInjected.dylib");
    fs::write(&source, "int injected(void) { return 7; }\n").expect("source");
    common::compile(&source, &library, true);

    let mut archive = BundleArchive::unpack(&root, ArchiveLimits::default(), Control::default()).expect("unpack");
    archive.inject(&[Injection { source: library, name: None }], Control::default()).expect("inject");

    let signer = Signer::AdHoc;
    let request =
        SigningRequest { signer: Some(&signer), profile: None, profiles: None, entitlements: None, deep: true };
    let report = archive.sign(request, Control::default()).expect("sign");

    assert_eq!(report.signed.len(), 4);
    assert!(report.skipped.is_empty());

    common::run(
        Command::new("codesign")
            .args(["--verify", "--strict", "--deep", "--all-architectures", "--verbose=4"])
            .arg(archive.bundle_path()),
    );
    common::run(&mut Command::new(archive.bundle_path().join("Test")));

    let output = temporary.path().join("signed.ipa");
    archive.save(&output, OutputLayout::Ipa, PackOptions::default(), Control::default()).expect("pack");
    let unpacked = BundleArchive::unpack(&output, ArchiveLimits::default(), Control::default()).expect("roundtrip");

    common::run(
        Command::new("codesign")
            .args(["--verify", "--strict", "--deep", "--all-architectures"])
            .arg(unpacked.bundle_path()),
    );

    let resource = unpacked.bundle_path().join("Frameworks/Kit.framework/resource");
    fs::write(&resource, b"tampered").expect("tamper");
    let result = Command::new("codesign")
        .args(["--verify", "--strict", "--deep"])
        .arg(unpacked.bundle_path())
        .output()
        .expect("verify tamper");

    assert!(!result.status.success());

    archive.sign(SigningRequest { signer: None, ..request }, Control::default()).expect("strip");
    archive.sign(request, Control::default()).expect("sign again");

    common::run(
        Command::new("codesign")
            .args(["--verify", "--strict", "--deep", "--all-architectures"])
            .arg(archive.bundle_path()),
    );
}

#[test]
fn identity_frameworks_and_extensions_inherit_and_merge_entitlements() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    native_fixture(&root, temporary.path());

    let certificate = temporary.path().join("certificate.pem");
    let key = temporary.path().join("key.pem");
    common::run(
        Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-sha256",
                "-days",
                "1",
                "-subj",
                "/CN=Apple Development: Sideport Test/OU=TEAM123456/O=Sideport/C=US",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&certificate),
    );

    let identity = SigningIdentity::from_pem(
        &fs::read_to_string(certificate).expect("certificate"),
        &fs::read_to_string(key).expect("key"),
    )
    .expect("identity");
    let signer = Signer::Identity(Arc::new(identity));
    let mut entitlements = plist::Dictionary::new();
    entitlements.insert("application-identifier".into(), "TEAM123456.com.example.test".into());
    entitlements.insert("get-task-allow".into(), true.into());
    let mut overrides = plist::Dictionary::new();
    overrides.insert("custom-value".into(), "override".into());

    // Explicit fixture profile: native display checks encoding, not Apple trust.
    let profile = ProvisioningProfile {
        raw: b"fixture profile".to_vec(),
        name: "Fixture".into(),
        uuid: "FIXTURE".into(),
        team_identifiers: vec!["TEAM123456".into()],
        application_identifier_prefixes: vec!["TEAM123456".into()],
        app_id_name: None,
        entitlements,
        creation_date: "2026-01-01T00:00:00Z".parse().expect("creation"),
        expiration_date: "2030-01-01T00:00:00Z".parse().expect("expiration"),
        time_to_live_days: None,
        local_provision: false,
        provisioned_devices: Vec::new(),
        developer_certificates: vec![match &signer {
            Signer::Identity(identity) => identity.certificate_der().to_vec(),
            Signer::AdHoc => unreachable!("fixture identity"),
        }],
    };

    let mut archive = BundleArchive::unpack(&root, ArchiveLimits::default(), Control::default()).expect("unpack");
    let request = SigningRequest {
        signer: Some(&signer),
        profile: Some(&profile),
        profiles: None,
        entitlements: Some(&overrides),
        deep: true,
    };
    let report = archive.sign(request, Control::default()).expect("identity sign");

    assert_eq!(fs::read(archive.bundle_path().join("embedded.mobileprovision")).expect("profile"), profile.raw);

    for path in report.signed {
        let bytes = fs::read(&path).expect("binary");
        let binary = sl_macho::Binary::parse(&bytes).expect("Mach-O");

        for slice in &binary.slices {
            let range = slice.image.signature.clone().expect("signature");
            let blobs = blob::parse_superblob(&slice.image.bytes[range], blob::EMBEDDED_SIGNATURE).expect("blobs");

            assert!(blobs.contains_key(&0x10000));
            assert!(blobs.contains_key(&7));

            let value = plist::Value::from_reader(std::io::Cursor::new(&blobs[&5][8..])).expect("entitlements");
            let fields = value.as_dictionary().expect("fields");

            assert_eq!(fields["custom-value"].as_string(), Some("override"));
            assert_eq!(fields["get-task-allow"].as_boolean(), Some(true));
        }

        let display = common::run(Command::new("codesign").args(["--display", "--entitlements", "-"]).arg(&path));
        let display =
            format!("{}{}", String::from_utf8_lossy(&display.stdout), String::from_utf8_lossy(&display.stderr));

        assert!(display.contains("custom-value"));
    }
}
