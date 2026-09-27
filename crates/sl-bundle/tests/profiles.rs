//! Identity-signing profile preflight. Profiles here are decoded-field fixtures: they establish
//! target matching and ordering before mutation, not Apple trust or device acceptance.

mod common;

use der::{Decode, Encode};
use rsa::pkcs1v15::SigningKey;
use sha2::Sha256;
use sl_bundle::{ArchiveLimits, BundleArchive, Control, Error, ProfileRequirements, SigningRequest};
use sl_codesign::{ProfileTrust, ProvisioningProfile, Signer, SigningIdentity};
use std::{collections::BTreeMap, fs, path::Path, sync::Arc};
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::time::Validity;

const TEAM: &str = "TEAM123456";
const DEVICE: &str = "00008030-001A2D0C0E38802E";

fn identity() -> Arc<SigningIdentity> {
    let key = sl_codesign::generate_signing_key().expect("RSA key");
    let signer = SigningKey::<Sha256>::new(key.clone());

    let public_key = rsa::pkcs8::EncodePublicKey::to_public_key_der(&key.to_public_key()).expect("SPKI");
    let public_key = spki::SubjectPublicKeyInfoOwned::from_der(public_key.as_bytes()).expect("SPKI");
    let subject: Name =
        format!("CN=Apple Development: Sideport Test,OU={TEAM},O=Sideport,C=US").parse().expect("subject");
    let validity = Validity::from_now(std::time::Duration::from_secs(3600)).expect("validity");

    let builder =
        CertificateBuilder::new(Profile::Root, SerialNumber::from(7u32), validity, subject, public_key, &signer)
            .expect("builder");
    let certificate = builder.build::<rsa::pkcs1v15::Signature>().expect("certificate").to_der().expect("DER");

    Arc::new(SigningIdentity::with_chain(&certificate, key, "").expect("identity"))
}

fn profile(app_id: &str, identity: &SigningIdentity, raw: &[u8]) -> ProvisioningProfile {
    let mut entitlements = plist::Dictionary::new();
    entitlements.insert("application-identifier".into(), format!("{TEAM}.{app_id}").into());

    ProvisioningProfile {
        raw: raw.to_vec(),
        name: format!("Fixture {app_id}"),
        uuid: format!("UUID-{app_id}"),
        team_identifiers: vec![TEAM.into()],
        application_identifier_prefixes: vec![TEAM.into()],
        app_id_name: None,
        entitlements,
        creation_date: "2026-01-01T00:00:00Z".parse().expect("creation"),
        expiration_date: "2030-01-01T00:00:00Z".parse().expect("expiration"),
        time_to_live_days: Some(7),
        local_provision: true,
        platforms: vec!["iOS".into()],
        provisions_all_devices: false,
        provisioned_devices: vec![DEVICE.into()],
        developer_certificates: vec![identity.certificate_der().to_vec()],
    }
}

fn fixture(root: &Path) {
    common::synthetic_bundle(root, "com.example.app", "App", "APPL");
    common::synthetic_bundle(&root.join("Frameworks/Kit.framework"), "com.example.kit", "Kit", "FMWK");
    common::synthetic_bundle(&root.join("PlugIns/Widget.appex"), "com.example.app.widget", "Widget", "XPC!");
}

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    walkdir(root)
        .into_iter()
        .map(|path| {
            let name = path.strip_prefix(root).expect("relative").to_string_lossy().into_owned();

            (name, fs::read(&path).expect("file"))
        })
        .collect()
}

fn walkdir(root: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();

    for entry in fs::read_dir(root).expect("directory") {
        let path = entry.expect("entry").path();

        if path.is_dir() { files.extend(walkdir(&path)) } else { files.push(path) }
    }

    files
}

fn request<'a>(
    signer: &'a Signer,
    main: &'a ProvisioningProfile,
    children: &'a BTreeMap<String, ProvisioningProfile>,
    requirements: ProfileRequirements<'a>,
) -> SigningRequest<'a> {
    SigningRequest {
        signer: Some(signer),
        profile: Some(main),
        profiles: Some(children),
        entitlements: None,
        deep: true,
        requirements,
    }
}

fn now() -> Option<chrono::DateTime<chrono::Utc>> {
    Some("2026-09-04T12:00:00Z".parse().expect("fixture time"))
}

#[test]
fn a_mismatched_child_profile_fails_before_any_file_changes() {
    let temporary = tempfile::tempdir().expect("tempdir");
    fixture(&temporary.path().join("App.app"));

    let identity = identity();
    let signer = Signer::Identity(identity.clone());
    let main = profile("com.example.app", &identity, b"main profile");
    let children = BTreeMap::from([(
        "com.example.app.widget".to_owned(),
        profile("com.example.other", &identity, b"widget profile"),
    )]);

    let mut archive =
        BundleArchive::unpack(&temporary.path().join("App.app"), ArchiveLimits::default(), Control::default())
            .expect("unpack");
    let before = snapshot(&archive.bundle_path());

    let requirements = ProfileRequirements { now: now(), ..ProfileRequirements::default() };
    let error =
        archive.sign(request(&signer, &main, &children, requirements), Control::default()).expect_err("mismatch");

    assert!(matches!(&error, Error::Profile { bundle, .. } if bundle == "Widget.appex"), "{error}");
    assert_eq!(snapshot(&archive.bundle_path()), before, "preflight must precede nested signing changes");
}

#[test]
fn matching_child_profiles_are_embedded_per_bundle() {
    let temporary = tempfile::tempdir().expect("tempdir");
    fixture(&temporary.path().join("App.app"));

    let identity = identity();
    let signer = Signer::Identity(identity.clone());
    let main = profile("com.example.app", &identity, b"main profile");
    let children = BTreeMap::from([(
        "com.example.app.widget".to_owned(),
        profile("com.example.app.*", &identity, b"widget profile"),
    )]);

    let mut archive =
        BundleArchive::unpack(&temporary.path().join("App.app"), ArchiveLimits::default(), Control::default())
            .expect("unpack");
    let requirements =
        ProfileRequirements { device_udid: Some(DEVICE), platform: Some("iOS"), now: now(), trust: None };

    archive.sign(request(&signer, &main, &children, requirements), Control::default()).expect("sign");

    let root = archive.bundle_path();
    assert_eq!(fs::read(root.join("embedded.mobileprovision")).expect("main"), b"main profile");
    assert_eq!(fs::read(root.join("PlugIns/Widget.appex/embedded.mobileprovision")).expect("child"), b"widget profile");
    assert!(!root.join("Frameworks/Kit.framework/embedded.mobileprovision").exists());
}

#[test]
fn device_platform_and_trust_requirements_apply_to_the_main_profile() {
    let temporary = tempfile::tempdir().expect("tempdir");
    fixture(&temporary.path().join("App.app"));

    let identity = identity();
    let signer = Signer::Identity(identity.clone());
    let main = profile("com.example.app", &identity, b"main profile");
    let children = BTreeMap::new();

    let mut archive =
        BundleArchive::unpack(&temporary.path().join("App.app"), ArchiveLimits::default(), Control::default())
            .expect("unpack");
    let before = snapshot(&archive.bundle_path());
    let apple = ProfileTrust::apple().expect("Apple trust");

    let base = ProfileRequirements { now: now(), ..ProfileRequirements::default() };
    let failing = [
        ProfileRequirements { device_udid: Some("00008030-001A2D0C0E38802F"), ..base },
        ProfileRequirements { device_udid: Some("not-a-udid"), ..base },
        ProfileRequirements { platform: Some("tvOS"), ..base },
        ProfileRequirements { trust: Some(&apple), ..base },
        ProfileRequirements { now: Some("2031-01-01T00:00:00Z".parse().expect("expired")), ..base },
    ];

    for requirements in failing {
        let result = archive.sign(request(&signer, &main, &children, requirements), Control::default());

        assert!(matches!(&result, Err(Error::Profile { bundle, .. }) if bundle == "App.app"), "{requirements:?}");
    }

    assert_eq!(snapshot(&archive.bundle_path()), before);

    let missing = SigningRequest { profile: None, ..request(&signer, &main, &children, base) };
    assert!(matches!(archive.sign(missing, Control::default()), Err(Error::Bundle(_))));
}
