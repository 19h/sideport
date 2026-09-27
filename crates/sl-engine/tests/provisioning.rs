//! Apple ID provisioning through the real engine against a stateful developer-portal fake.
//! These fixtures establish the recovered client flow and signed output; Apple's current
//! service behavior and device acceptance are not established here.

#[path = "../../sl-bundle/tests/common/mod.rs"]
mod common;

use chrono::{Duration, Utc};
use futures::executor::block_on;
use sl_bundle::{ArchiveLimits, BundleArchive, Control};
use sl_codesign::{ProvisioningProfile, blob};
use sl_engine::{
    AnisetteSetting, AppOptions, BundleIdPolicy, Engine, EngineConfig, EngineError, Fact, JobEvent, JobOutcome,
    JobSpec, PromptKind, PromptReply, SigningMode, Target,
};
use sl_testkit::{FakePortal, ProfileChain};
use std::fs;
use std::path::{Path, PathBuf};
use wiremock::MockServer;

const APPLE_ID: &str = "fixture@example.test";
const TEAM: &str = "TEAM123456";

struct Harness {
    server: MockServer,
    portal: FakePortal,
    temporary: tempfile::TempDir,
}

impl Harness {
    async fn new(free: bool) -> Self {
        let server = MockServer::start().await;
        let portal = FakePortal::new(TEAM, free);
        portal.mount(&server).await;

        let temporary = tempfile::tempdir().expect("tempdir");

        Self { server, portal, temporary }
    }

    fn data_dir(&self) -> PathBuf {
        self.temporary.path().join("data")
    }

    fn engine(&self) -> Engine {
        let config = EngineConfig {
            data_dir: Some(self.data_dir()),
            portal_origin: Some(FakePortal::origin(&self.server)),
            profile_trust: Some(ProfileChain::shared().trust()),
            file_secrets: true,
            disable_scheduler: true,
            ..EngineConfig::default()
        };
        let engine = Engine::new(config).expect("engine");

        let mut settings = engine.settings();
        settings.anisette = AnisetteSetting::Remote { url: format!("{}/anisette", self.server.uri()) };
        engine.update_settings(settings).expect("settings");

        engine
    }

    /// A recovered-client `sessions.json` with one GSA and one legacy IDMS session.
    fn sessions_file(&self) -> PathBuf {
        let sessions = serde_json::json!({
            format!("{APPLE_ID}:a"): {
                "_type": "GsaAuthenticator",
                "dsid": "123456789",
                "gs_token": "fixture-private-token",
                "userhash": "unused",
                "anisette_provider": { "_type": "RemoteAnisette", "url": "https://example.invalid" },
                "using_alt_anisette": false,
            },
            "legacy@example.test:i": { "_type": "IdmsAuthenticator", "myacinfo": "legacy" },
            "latest:a": APPLE_ID,
        });

        let path = self.temporary.path().join("sessions.json");
        fs::write(&path, serde_json::to_vec(&sessions).expect("sessions JSON")).expect("sessions file");

        path
    }

    fn app(&self) -> PathBuf {
        let root = self.temporary.path().join("App.app");

        if !root.exists() {
            common::synthetic_bundle(&root, "com.example.app", "App", "APPL");
            common::synthetic_bundle(&root.join("PlugIns/Widget.appex"), "com.example.app.widget", "Widget", "XPC!");
            common::synthetic_bundle(&root.join("Frameworks/Kit.framework"), "com.example.kit", "Kit", "FMWK");
        }

        root
    }

    fn spec(&self, output: &str, options: AppOptions) -> JobSpec {
        JobSpec {
            source: self.app(),
            target: Target::ExportIpa { path: Some(self.temporary.path().join(output)) },
            signing: SigningMode::AppleId { apple_id: APPLE_ID.into() },
            options,
        }
    }
}

/// Run a job, answering Confirm prompts with `confirm` and collecting facts.
fn run(engine: &Engine, spec: JobSpec, confirm: bool) -> (Result<JobOutcome, EngineError>, Vec<Fact>, Vec<PromptKind>) {
    let job = engine.start(spec);
    let events = job.events();
    let collector = std::thread::spawn(move || {
        let mut facts = Vec::new();
        let mut prompts = Vec::new();

        while let Ok(event) = block_on(events.recv()) {
            match event {
                JobEvent::Fact(fact) => facts.push(fact),
                JobEvent::Prompt(prompt) => {
                    prompts.push(prompt.kind.clone());

                    match &prompt.kind {
                        PromptKind::Confirm { .. } => prompt.answer(PromptReply::Confirmed(confirm)),
                        _ => prompt.cancel(),
                    }
                }
                _ => {}
            }
        }

        (facts, prompts)
    });

    let result = block_on(job.result());
    let (facts, prompts) = collector.join().expect("event collector");

    (result, facts, prompts)
}

fn unpack(path: &Path) -> BundleArchive {
    BundleArchive::unpack(path, ArchiveLimits::default(), Control::default()).expect("unpack output")
}

fn signature_blobs(path: &Path) -> std::collections::BTreeMap<u32, Vec<u8>> {
    let bytes = fs::read(path).expect("binary");
    let binary = sl_macho::Binary::parse(&bytes).expect("Mach-O");
    let slice = &binary.slices[0];
    let range = slice.image.signature.clone().expect("signature");

    blob::parse_superblob(&slice.image.bytes[range], blob::EMBEDDED_SIGNATURE)
        .expect("signature blobs")
        .into_iter()
        .map(|(slot, bytes)| (slot, bytes.to_vec()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_free_account_provisions_signs_and_reuses_its_certificate_after_restart() {
    let harness = Harness::new(true).await;
    let foreign = harness.portal.add_foreign_certificate("Other Mac", Utc::now() + Duration::days(30));

    let engine = harness.engine();
    let import = engine.import_sessions(harness.sessions_file()).expect("import");

    assert_eq!(import.imported, [APPLE_ID]);
    assert_eq!(import.skipped.len(), 1, "{:?}", import.skipped);
    assert!(engine.accounts().expect("accounts")[0].has_session);

    let (result, facts, prompts) = run(&engine, harness.spec("first.ipa", AppOptions::default()), false);
    let outcome = result.expect("Apple ID export");

    assert!(prompts.is_empty(), "{prompts:?}");
    assert_eq!(outcome.bundle_id, format!("com.example.app.{TEAM}"));
    assert!(outcome.expires.is_some_and(|expires| expires > Utc::now() + Duration::days(6)));
    assert!(facts.iter().any(|fact| matches!(fact, Fact::AppIdQuota { remaining: 10, .. })));
    assert!(facts.iter().any(|fact| matches!(fact, Fact::ProfileExpiry { ttl_days: Some(7), .. })));
    assert!(facts.iter().any(|fact| matches!(fact, Fact::Team(team) if team.team_id == TEAM)));

    {
        let state = harness.portal.state();
        let actions = state.actions();

        assert_eq!(
            actions,
            [
                "listTeams",
                "ios/listAllDevelopmentCerts",
                "ios/submitDevelopmentCSR",
                "ios/listAllDevelopmentCerts",
                "ios/listAppIds",
                "ios/addAppId",
                "ios/downloadTeamProvisioningProfile",
            ]
        );

        let csr = state.requests.iter().find(|request| request.action == "ios/submitDevelopmentCSR").expect("CSR");
        assert!(csr.field("machineId").is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()));
        assert!(csr.field("machineName").is_some_and(|name| !name.is_empty()));

        let app_id = state.requests.iter().find(|request| request.action == "ios/addAppId").expect("App ID");
        assert_eq!(app_id.field("identifier"), Some(format!("com.example.app.{TEAM}").as_str()));
        assert_eq!(app_id.field("name"), Some("App"));
        assert_eq!(app_id.field("teamId"), Some(TEAM));
    }

    let output = unpack(&harness.temporary.path().join("first.ipa"));
    let root = output.bundle_path();
    let info = sl_bundle::read_dictionary(&root.join("Info.plist")).expect("info");
    let widget = sl_bundle::read_dictionary(&root.join("PlugIns/Widget.appex/Info.plist")).expect("widget");

    assert_eq!(info["CFBundleIdentifier"].as_string(), Some(format!("com.example.app.{TEAM}").as_str()));
    assert_eq!(info["ALTBundleIdentifier"].as_string(), Some("com.example.app"));
    assert_eq!(widget["CFBundleIdentifier"].as_string(), Some(format!("com.example.app.{TEAM}.widget").as_str()));

    let embedded = fs::read(root.join("embedded.mobileprovision")).expect("embedded profile");
    let profile = ProvisioningProfile::parse(&embedded).expect("embedded profile decodes");
    profile.verify_trust(&ProfileChain::shared().trust()).expect("embedded profile is the signed download");
    assert_eq!(profile.bundle_id(), Some(format!("com.example.app.{TEAM}").as_str()));
    assert!(!root.join("PlugIns/Widget.appex/embedded.mobileprovision").exists());

    let issued = harness.portal.state().certificates.iter().find(|certificate| certificate.serial != foreign).cloned();
    let issued = issued.expect("issued certificate");

    for executable in [root.join("App"), root.join("PlugIns/Widget.appex/Widget")] {
        let blobs = signature_blobs(&executable);
        let entitlements = plist::Value::from_reader(std::io::Cursor::new(&blobs[&5][8..])).expect("entitlements");
        let entitlements = entitlements.as_dictionary().expect("entitlement fields");

        assert_eq!(
            entitlements["application-identifier"].as_string(),
            Some(format!("{TEAM}.com.example.app.{TEAM}").as_str())
        );

        let cms = &blobs[&0x10000][8..];
        assert!(cms.windows(issued.der.len()).any(|window| window == issued.der), "issued certificate signs the code");
    }

    let certificates = block_on(engine.certificates(APPLE_ID.into()).result()).expect("certificates");
    let ours: Vec<_> = certificates.iter().filter(|certificate| certificate.is_ours).collect();
    assert_eq!(ours.len(), 1);
    assert_eq!(ours[0].serial, issued.serial);
    assert!(certificates.iter().any(|certificate| certificate.serial == foreign && !certificate.is_ours));

    drop(engine);

    let restarted = harness.engine();
    let accounts = restarted.accounts().expect("restored accounts");
    assert_eq!(accounts.len(), 1);
    assert!(accounts[0].has_session);
    assert_eq!(accounts[0].default_team.as_deref(), Some(TEAM));

    let before = harness.portal.state().requests.len();
    let (result, _, _) = run(&restarted, harness.spec("second.ipa", AppOptions::default()), false);
    result.expect("second export after restart");

    let state = harness.portal.state();
    let second: Vec<_> = state.requests[before..].iter().map(|request| request.action.as_str()).collect();

    assert_eq!(
        second,
        ["ios/listAllDevelopmentCerts", "ios/listAppIds", "ios/downloadTeamProvisioningProfile"],
        "restart reuses the stored key's certificate, the team choice and the App ID"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_certificate_limit_requires_confirmation_before_revoking_the_oldest() {
    let harness = Harness::new(false).await;
    let oldest = harness.portal.add_foreign_certificate("Oldest Mac", Utc::now() + Duration::days(10));
    let newer = harness.portal.add_foreign_certificate("Newer Mac", Utc::now() + Duration::days(200));

    let engine = harness.engine();
    engine.import_sessions(harness.sessions_file()).expect("import");

    let (declined, _, prompts) = run(&engine, harness.spec("declined.ipa", AppOptions::default()), false);

    assert!(matches!(declined, Err(EngineError::Portal { code: 7460, .. })), "{declined:?}");
    assert!(
        matches!(prompts.as_slice(), [PromptKind::Confirm { destructive: true, message, .. }] if message.contains(&oldest))
    );
    assert_eq!(harness.portal.state().certificates.len(), 2, "declining revokes nothing");
    assert!(!harness.temporary.path().join("declined.ipa").exists());

    let (accepted, _, _) = run(&engine, harness.spec("paid.ipa", AppOptions::default()), true);
    let outcome = accepted.expect("export after confirmed revocation");

    let state = harness.portal.state();
    let serials: Vec<_> = state.certificates.iter().map(|certificate| certificate.serial.as_str()).collect();

    assert!(!serials.contains(&oldest.as_str()));
    assert!(serials.contains(&newer.as_str()));
    assert_eq!(state.count("ios/revokeDevelopmentCert"), 1);
    assert_eq!(outcome.bundle_id, "com.example.app", "paid teams keep the original identifier");

    let output = unpack(&harness.temporary.path().join("paid.ipa"));
    let info = sl_bundle::read_dictionary(&output.bundle_path().join("Info.plist")).expect("info");
    assert!(!info.contains_key("ALTBundleIdentifier"));
}

#[tokio::test(flavor = "multi_thread")]
async fn extension_provisioning_and_custom_identifiers_embed_one_profile_per_bundle() {
    let harness = Harness::new(true).await;
    let engine = harness.engine();
    engine.import_sessions(harness.sessions_file()).expect("import");

    let options = AppOptions {
        bundle_id: BundleIdPolicy::Custom("org.custom.app".into()),
        provision_extensions: true,
        ..AppOptions::default()
    };
    let (result, _, _) = run(&engine, harness.spec("custom.ipa", options), false);
    assert_eq!(result.expect("custom export").bundle_id, "org.custom.app");

    let identifiers: Vec<_> = harness.portal.state().app_ids.iter().map(|app_id| app_id.identifier.clone()).collect();
    assert_eq!(identifiers, ["org.custom.app", "org.custom.app.widget"]);

    let output = unpack(&harness.temporary.path().join("custom.ipa"));
    let root = output.bundle_path();
    let widget = fs::read(root.join("PlugIns/Widget.appex/embedded.mobileprovision")).expect("widget profile");
    let widget = ProvisioningProfile::parse(&widget).expect("widget profile decodes");
    assert_eq!(widget.bundle_id(), Some("org.custom.app.widget"));

    let info = sl_bundle::read_dictionary(&root.join("Info.plist")).expect("info");
    assert_eq!(info["ALTBundleIdentifier"].as_string(), Some("com.example.app"));

    let blobs = signature_blobs(&root.join("PlugIns/Widget.appex/Widget"));
    let entitlements = plist::Value::from_reader(std::io::Cursor::new(&blobs[&5][8..])).expect("entitlements");
    assert_eq!(
        entitlements.as_dictionary().expect("fields")["application-identifier"].as_string(),
        Some(format!("{TEAM}.org.custom.app.widget").as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_sessions_and_untrusted_profiles_stop_before_output() {
    let harness = Harness::new(true).await;
    let engine = harness.engine();
    engine.import_sessions(harness.sessions_file()).expect("import");

    harness.portal.state().expire_sessions = 1;
    let (expired, _, prompts) = run(&engine, harness.spec("expired.ipa", AppOptions::default()), false);

    assert!(matches!(expired, Err(EngineError::Cancelled)), "{expired:?}");
    assert!(matches!(prompts.as_slice(), [PromptKind::Password { apple_id, .. }] if apple_id == APPLE_ID));
    assert!(!harness.temporary.path().join("expired.ipa").exists());

    let untrusting = Engine::new(EngineConfig {
        data_dir: Some(harness.data_dir()),
        portal_origin: Some(FakePortal::origin(&harness.server)),
        profile_trust: None,
        file_secrets: true,
        disable_scheduler: true,
        ..EngineConfig::default()
    })
    .expect("engine with Apple trust");

    let (untrusted, _, _) = run(&untrusting, harness.spec("untrusted.ipa", AppOptions::default()), false);

    assert!(matches!(&untrusted, Err(EngineError::Signing(message)) if message.contains("trust")), "{untrusted:?}");
    assert!(!harness.temporary.path().join("untrusted.ipa").exists());
}

/// Apple's codesign decodes the identity-signed output (identifier, team, entitlements,
/// Info.plist slot) and stops only at trust evaluation, because the fixture authority is not
/// Apple-anchored. OpenSSL independently verifies the CMS signature over the CodeDirectory.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn native_tools_decode_and_verify_an_apple_id_export_up_to_trust() {
    use sha2::{Digest, Sha256};
    use std::process::Command;

    let harness = Harness::new(true).await;
    let root = harness.temporary.path().join("App.app");

    common::write_info(&root, common::info("com.example.app", "App", "APPL"));
    let source = harness.temporary.path().join("main.c");
    fs::write(&source, "int main(void) { return 0; }\n").expect("source");
    common::compile(&source, &root.join("App"), false);

    let engine = harness.engine();
    engine.import_sessions(harness.sessions_file()).expect("import");

    let (result, _, _) = run(&engine, harness.spec("native.ipa", AppOptions::default()), false);
    result.expect("native Apple ID export");

    let output = unpack(&harness.temporary.path().join("native.ipa"));
    let bundle = output.bundle_path();

    let display =
        common::run(Command::new("codesign").args(["--display", "--verbose=6", "--entitlements", "-"]).arg(&bundle));
    let display = format!("{}{}", String::from_utf8_lossy(&display.stdout), String::from_utf8_lossy(&display.stderr));

    let info_digest: String = Sha256::digest(fs::read(bundle.join("Info.plist")).expect("Info.plist"))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    assert!(display.contains(&format!("Identifier=com.example.app.{TEAM}")), "{display}");
    assert!(display.contains(&format!("TeamIdentifier={TEAM}")), "{display}");
    assert!(display.contains(&format!("{TEAM}.com.example.app.{TEAM}")), "{display}");
    assert!(display.contains(&format!("-1={info_digest}")), "{display}");

    let verify =
        Command::new("codesign").args(["--verify", "--strict"]).arg(&bundle).output().expect("codesign verify");
    let verify_error = String::from_utf8_lossy(&verify.stderr);

    assert!(!verify.status.success());
    assert!(verify_error.contains("CSSMERR_TP_NOT_TRUSTED"), "{verify_error}");

    let blobs = signature_blobs(&bundle.join("App"));
    let work = harness.temporary.path();

    fs::write(work.join("signature.der"), &blobs[&0x10000][8..]).expect("CMS");
    fs::write(work.join("code-directory"), &blobs[&0]).expect("CodeDirectory");

    let openssl = |content: &str| {
        Command::new("openssl")
            .args(["cms", "-verify", "-noverify", "-binary", "-inform", "DER", "-in"])
            .arg(work.join("signature.der"))
            .arg("-content")
            .arg(work.join(content))
            .args(["-out", "/dev/null"])
            .output()
            .expect("openssl")
    };

    assert!(openssl("code-directory").status.success(), "OpenSSL verifies the CMS over the CodeDirectory");

    let mut tampered = blobs[&0].clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    fs::write(work.join("tampered"), tampered).expect("tampered CodeDirectory");
    assert!(!openssl("tampered").status.success());
}
