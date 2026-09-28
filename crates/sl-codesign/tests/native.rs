#![cfg(target_os = "macos")]
mod common;

use sl_codesign::{CodeKind, SignOptions, Signer, blob, sign_macho, strip_signature};
use sl_macho::Binary;
use std::{io::Cursor, process::Command};

fn options() -> SignOptions<'static> {
    SignOptions {
        identifier: "com.example.sideport",
        kind: CodeKind::MainExecutable,
        entitlements: None,
        info_plist: None,
        code_resources: None,
        is_cancelled: None,
    }
}

#[test]
fn codesign_accepts_both_universal_slices_and_detects_code_tampering() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("sample");
    let original = common::compile(&path, true);

    let signed = sign_macho(&original, &Signer::AdHoc, &options()).expect("Rust signer");
    std::fs::write(&path, &signed).expect("write");

    common::run(
        Command::new("codesign").args(["--verify", "--strict", "--all-architectures", "--verbose=4"]).arg(&path),
    );
    common::run(&mut Command::new(&path)); // Native ARM64 execution also exercises kernel validation.

    let parsed = Binary::parse(&signed).expect("Mach-O");
    let mut damaged = signed.clone();
    let first = &parsed.slices[0];
    let code = first
        .image
        .segments
        .iter()
        .flat_map(|segment| &segment.sections)
        .find(|section| section.name == "__text")
        .expect("text");

    damaged[first.offset + code.offset as usize] ^= 1;
    std::fs::write(&path, damaged).expect("tamper");

    let result =
        Command::new("codesign").args(["--verify", "--all-architectures"]).arg(&path).output().expect("codesign");

    assert!(!result.status.success());

    let stripped = strip_signature(&signed).expect("strip");
    let parsed = Binary::parse(&stripped).expect("Mach-O");

    assert!(parsed.slices.iter().all(|slice| slice.image.signature.is_none()));

    let resigned = sign_macho(&stripped, &Signer::AdHoc, &options()).expect("sign again");
    std::fs::write(&path, resigned).expect("write");

    common::run(Command::new("codesign").args(["--verify", "--strict", "--all-architectures"]).arg(&path));
}

#[test]
fn codesign_decodes_identity_cms_xml_der_and_requirements() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("sample");
    let original = common::compile(&path, false);
    let (identity, _) = common::signing_material();

    let mut entitlements = plist::Dictionary::new();
    entitlements.insert("get-task-allow".into(), true.into());
    entitlements.insert("application-identifier".into(), "TEAM123456.com.example.sideport".into());
    entitlements
        .insert("keychain-access-groups".into(), plist::Value::Array(vec!["TEAM123456.com.example.sideport".into()]));

    let opts = SignOptions { entitlements: Some(&entitlements), ..options() };
    let signed = sign_macho(&original, &Signer::Identity(identity.clone()), &opts).expect("sign");
    std::fs::write(&path, &signed).expect("write");

    let result = common::run(
        Command::new("codesign")
            .args(["--display", "--verbose=4", "--entitlements", "-", "--requirements", "-"])
            .arg(&path),
    );
    let display = format!("{}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));

    assert!(display.contains("TEAM123456"), "{display}");
    assert!(display.contains("get-task-allow"), "{display}");
    assert!(display.contains("subject.CN"), "{display}");
    assert!(display.contains("1.2.840.113635.100.6.2.1"), "{display}");

    let parsed = Binary::parse(&signed).expect("Mach-O");
    let image = &parsed.slices[0].image;
    let signature_range = image.signature.clone().expect("sig");
    let blobs = blob::parse_superblob(&image.bytes[signature_range], blob::EMBEDDED_SIGNATURE).expect("blobs");

    std::fs::write(tmp.path().join("cms.der"), &blobs[&0x10000][8..]).expect("CMS");
    std::fs::write(tmp.path().join("cd"), blobs[&0]).expect("CD");

    common::run(
        Command::new("openssl")
            .args(["cms", "-verify", "-noverify", "-binary", "-inform", "DER", "-in"])
            .arg(tmp.path().join("cms.der"))
            .arg("-content")
            .arg(tmp.path().join("cd"))
            .arg("-out")
            .arg(tmp.path().join("verified")),
    );
}

#[test]
fn codesign_accepts_resource_seal_and_rejects_resource_tampering() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("Test.app");

    std::fs::create_dir_all(root.join("_CodeSignature")).expect("bundle");

    let path = root.join("Test");
    let original = common::compile(&path, false);

    // Remove the generated source before sealing the bundle.
    std::fs::remove_file(path.with_extension("c")).expect("remove source");

    let mut info = plist::Dictionary::new();
    for (key, value) in [
        ("CFBundleIdentifier", "com.example.sideport"),
        ("CFBundleExecutable", "Test"),
        ("CFBundleName", "Test"),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleVersion", "1"),
    ] {
        info.insert(key.into(), value.into());
    }

    let info = sl_codesign::entitlements::to_xml(&info).expect("plist");
    std::fs::write(root.join("Info.plist"), &info).expect("info");
    std::fs::write(root.join("resource"), b"original").expect("resource");

    let resources = sl_codesign::code_resources::build_seal(&root, Some("Test")).expect("seal");
    std::fs::write(root.join("_CodeSignature/CodeResources"), &resources).expect("seal file");

    let opts = SignOptions { info_plist: Some(&info), code_resources: Some(&resources), ..options() };
    let signed = sign_macho(&original, &Signer::AdHoc, &opts).expect("sign");
    std::fs::write(&path, signed).expect("binary");

    common::run(Command::new("codesign").args(["--verify", "--strict", "--verbose=4"]).arg(&root));

    std::fs::write(root.join("resource"), b"tampered").expect("tamper");

    let result = Command::new("codesign").arg("--verify").arg(&root).output().expect("codesign");

    assert!(!result.status.success());

    let value = plist::Value::from_reader(Cursor::new(resources)).expect("seal");

    assert!(value.as_dictionary().expect("dict").contains_key("files2"));
}

#[test]
fn der_entitlements_match_apples_encoder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("sample");
    common::compile(&path, false);

    let mut entitlements = plist::Dictionary::new();
    entitlements.insert("get-task-allow".into(), true.into());
    entitlements.insert("application-identifier".into(), "TEAM123456.com.example.sideport".into());
    entitlements
        .insert("keychain-access-groups".into(), plist::Value::Array(vec!["TEAM123456.com.example.sideport".into()]));
    entitlements.insert("integer-value".into(), 128i64.into());

    let ent_path = tmp.path().join("entitlements.plist");
    let xml = sl_codesign::entitlements::to_xml(&entitlements).expect("XML");
    std::fs::write(&ent_path, xml).expect("entitlements");

    common::run(
        Command::new("codesign")
            .args(["--force", "--sign", "-", "--generate-entitlement-der", "--entitlements"])
            .arg(&ent_path)
            .arg(&path),
    );

    let signed = std::fs::read(&path).expect("binary");
    let parsed = Binary::parse(&signed).expect("Mach-O");
    let image = &parsed.slices[0].image;
    let signature_range = image.signature.clone().expect("sig");
    let blobs = blob::parse_superblob(&image.bytes[signature_range], blob::EMBEDDED_SIGNATURE).expect("blobs");
    let expected = sl_codesign::entitlements::to_der(&entitlements).expect("Rust DER");

    assert_eq!(&blobs[&7][8..], expected);
}
