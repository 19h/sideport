//! Device installation through the real engine with a fake portal and a fake device layer.
//! These fixtures establish the engine's install, resume, record and refresh behavior; physical
//! installation is outside their scope.

#[path = "../../sl-bundle/tests/common/mod.rs"]
mod common;

use futures::executor::block_on;
use sl_bundle::{ArchiveLimits, BundleArchive, Control};
use sl_codesign::ProvisioningProfile;
use sl_engine::{
    AnisetteSetting, AppOptions, DeviceBackend, Engine, EngineConfig, EngineError, Fact, JobEvent, JobOutcome, JobSpec,
    PromptKind, PromptReply, SigningMode, Stage, Target,
};
use sl_testkit::{FakeDevice, FakePortal, ProfileChain};
use std::fs;
use std::path::{Path, PathBuf};
use wiremock::MockServer;

const APPLE_ID: &str = "fixture@example.test";
const TEAM: &str = "TEAM123456";
const UDID: &str = "00008030-001A2D0C0E38802E";

struct Harness {
    server: MockServer,
    portal: FakePortal,
    device: FakeDevice,
    temporary: tempfile::TempDir,
}

impl Harness {
    async fn new() -> Self {
        let server = MockServer::start().await;
        let portal = FakePortal::new(TEAM, true);
        portal.mount(&server).await;

        Self { server, portal, device: FakeDevice::iphone(UDID), temporary: tempfile::tempdir().expect("tempdir") }
    }

    fn data_dir(&self) -> PathBuf {
        self.temporary.path().join("data")
    }

    fn engine(&self) -> Engine {
        let config = EngineConfig {
            data_dir: Some(self.data_dir()),
            portal_origin: Some(FakePortal::origin(&self.server)),
            profile_trust: Some(ProfileChain::shared().trust()),
            device_backend: Some(DeviceBackend(self.device.backend())),
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

    fn signed_in(&self) -> Engine {
        let engine = self.engine();

        let sessions = serde_json::json!({
            format!("{APPLE_ID}:a"): { "_type": "GsaAuthenticator", "dsid": "42", "gs_token": "fixture-token" },
        });
        let path = self.temporary.path().join("sessions.json");
        fs::write(&path, serde_json::to_vec(&sessions).expect("JSON")).expect("sessions");
        engine.import_sessions(path).expect("import");

        engine
    }

    fn app(&self) -> PathBuf {
        let root = self.temporary.path().join("App.app");

        if !root.exists() {
            common::synthetic_bundle(&root, "com.example.app", "App", "APPL");
            common::synthetic_bundle(&root.join("PlugIns/Widget.appex"), "com.example.app.widget", "Widget", "XPC!");
        }

        root
    }

    fn ipa(&self) -> PathBuf {
        let path = self.temporary.path().join("App.ipa");

        if !path.exists() {
            let archive =
                BundleArchive::unpack(&self.app(), ArchiveLimits::default(), Control::default()).expect("unpack");
            archive.save(&path, sl_bundle::OutputLayout::Ipa, Default::default(), Control::default()).expect("pack");
        }

        path
    }

    fn spec(&self, source: PathBuf, signing: SigningMode, options: AppOptions) -> JobSpec {
        JobSpec { source, target: Target::Device { udid: UDID.into(), prefer_network: false }, signing, options }
    }
}

struct Run {
    result: Result<JobOutcome, EngineError>,
    facts: Vec<Fact>,
    stages: Vec<Stage>,
    prompts: Vec<PromptKind>,
}

fn run(engine: &Engine, spec: JobSpec) -> Run {
    let job = engine.start(spec);
    let events = job.events();

    let collector = std::thread::spawn(move || {
        let (mut facts, mut stages, mut prompts) = (Vec::new(), Vec::new(), Vec::new());

        while let Ok(event) = block_on(events.recv()) {
            match event {
                JobEvent::Fact(fact) => facts.push(fact),
                JobEvent::Stage(stage) => stages.push(stage),
                JobEvent::Prompt(prompt) => {
                    prompts.push(prompt.kind.clone());
                    prompt.answer(PromptReply::Cancel);
                }
                _ => {}
            }
        }

        (facts, stages, prompts)
    });

    let result = block_on(job.result());
    let (facts, stages, prompts) = collector.join().expect("collector");

    Run { result, facts, stages, prompts }
}

fn installed_app(bytes: &[u8], directory: &Path) -> BundleArchive {
    let path = directory.join(format!("installed-{}.ipa", bytes.len()));
    fs::write(&path, bytes).expect("installed package");

    BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).expect("installed package unpacks")
}

#[tokio::test(flavor = "multi_thread")]
async fn apple_id_installs_register_the_device_embed_its_udid_and_record_the_installation() {
    let harness = Harness::new().await;
    let engine = harness.signed_in();

    let options = AppOptions { track_for_refresh: true, ..AppOptions::default() };
    let outcome =
        run(&engine, harness.spec(harness.ipa(), SigningMode::AppleId { apple_id: APPLE_ID.into() }, options));
    let result = outcome.result.expect("Apple ID device install");

    assert_eq!(result.bundle_id, format!("com.example.app.{TEAM}"));
    assert!(result.exported_to.is_none());
    assert!(outcome.prompts.is_empty(), "{:?}", outcome.prompts);
    assert!(outcome.stages.contains(&Stage::Uploading) && outcome.stages.contains(&Stage::Installing));
    assert!(outcome.facts.iter().any(|fact| matches!(fact, Fact::BundleId(id) if id == &result.bundle_id)));

    {
        let portal = harness.portal.state();
        let added =
            portal.requests.iter().find(|request| request.action == "ios/addDevice").expect("device registration");

        assert_eq!(added.field("deviceNumber"), Some(UDID));
        assert_eq!(added.field("name"), Some("Fixture iPhone"));
    }

    let device = harness.device.state();
    let [installed] = device.installed.as_slice() else { panic!("one installed package: {}", device.installed.len()) };
    assert_eq!(installed.bundle_id, result.bundle_id);
    assert!(device.files.is_empty(), "installation consumed the staged package");

    let app = installed_app(&installed.bytes, harness.temporary.path());
    let profile = fs::read(app.bundle_path().join("embedded.mobileprovision")).expect("embedded profile");
    let profile = ProvisioningProfile::parse(&profile).expect("profile");
    assert!(profile.provisioned_devices.iter().any(|device| device == UDID));
    drop(device);

    let installations = engine.installations().expect("installations");
    let [installation] = installations.as_slice() else { panic!("one installation") };

    assert_eq!(Some(installation.id), result.installation_id);
    assert_eq!(installation.device_udid, UDID);
    assert_eq!(installation.team_id, TEAM);
    assert_eq!(installation.expires_at, result.expires);
    assert!(installation.auto_refresh);

    let stored = installation.spec.source.clone();
    assert!(stored.starts_with(harness.data_dir().join("files")), "{}", stored.display());
    assert_eq!(fs::read(&stored).expect("stored copy"), fs::read(harness.ipa()).expect("input"));

    engine.set_auto_refresh(installation.id, false).expect("toggle");

    let refreshed = run_refresh(&engine, installation.id);
    assert_eq!(refreshed.expect("refresh").installation_id, Some(installation.id));
    assert_eq!(harness.device.state().installed.len(), 2);

    let after = engine.installations().expect("installations");
    assert_eq!(after.len(), 1);
    assert!(!after[0].auto_refresh, "refresh keeps the user's automatic-refresh choice");
    assert!(after[0].installed_at >= installation.installed_at);

    let certificate_requests = harness.portal.state().count("ios/submitDevelopmentCSR");
    assert_eq!(certificate_requests, 1, "refresh reuses the certificate");

    engine.forget_installation(installation.id).expect("forget");
    assert!(engine.installations().expect("installations").is_empty());
    assert!(!stored.exists(), "the unreferenced stored copy is deleted");
}

fn run_refresh(engine: &Engine, id: i64) -> Result<JobOutcome, EngineError> {
    let job = engine.refresh(id);
    let events = job.events();

    std::thread::spawn(move || {
        while let Ok(event) = block_on(events.recv()) {
            if let JobEvent::Prompt(prompt) = event {
                prompt.answer(PromptReply::Cancel);
            }
        }
    });

    block_on(job.result())
}

#[tokio::test(flavor = "multi_thread")]
async fn adhoc_and_original_installs_need_no_account() {
    let harness = Harness::new().await;
    let engine = harness.engine();

    run(&engine, harness.spec(harness.app(), SigningMode::AdHoc, AppOptions::default())).result.expect("ad-hoc");
    run(&engine, harness.spec(harness.ipa(), SigningMode::Original, AppOptions::default())).result.expect("original");

    let device = harness.device.state();
    assert_eq!(device.installed.len(), 2);
    assert_eq!(device.installed[1].bytes, fs::read(harness.ipa()).expect("input"), "original IPAs upload unchanged");
    assert!(harness.portal.state().requests.is_empty());
    drop(device);

    let unsigned = run(&engine, harness.spec(harness.app(), SigningMode::Unsigned, AppOptions::default()));
    assert!(matches!(unsigned.result, Err(EngineError::Unsupported(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_upload_resumes_after_an_interruption_with_identical_bytes() {
    let harness = Harness::new().await;
    let engine = harness.engine();
    let options = AppOptions { stream_upload: true, ..AppOptions::default() };

    run(&engine, harness.spec(harness.app(), SigningMode::AdHoc, options.clone())).result.expect("clean stream");
    harness.device.state().interrupt_after = Some(1500);
    run(&engine, harness.spec(harness.app(), SigningMode::AdHoc, options)).result.expect("resumed stream");

    let device = harness.device.state();
    let [clean, resumed] = device.installed.as_slice() else { panic!("two installs") };

    assert_eq!(clean.bytes, resumed.bytes, "deterministic packing makes the resumed stream identical");
    assert!(device.connections >= 3, "the interruption caused a second connection");
    installed_app(&resumed.bytes, harness.temporary.path());
}

#[tokio::test(flavor = "multi_thread")]
async fn device_utilities_list_apps_profiles_and_pairing_state() {
    let harness = Harness::new().await;
    let engine = harness.signed_in();

    let devices = engine.devices().await.expect("devices");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].model_name.as_deref(), Some("iPhone 14 Pro"));
    assert!(devices[0].paired);

    let options = AppOptions::default();
    run(&engine, harness.spec(harness.ipa(), SigningMode::AppleId { apple_id: APPLE_ID.into() }, options))
        .result
        .expect("install");

    let installed = harness.device.state().installed[0].bytes.clone();
    let app = installed_app(&installed, harness.temporary.path());
    let profile = fs::read(app.bundle_path().join("embedded.mobileprovision")).expect("profile");
    harness.device.state().profiles.push(profile);
    harness.device.state().profiles.push(b"not a profile".to_vec());

    let profiles = engine.device_profiles(UDID.into()).await.expect("profiles");
    assert_eq!(profiles.len(), 1, "undecodable profiles are skipped");
    assert!(profiles[0].is_free);
    assert_eq!(profiles[0].team_id.as_deref(), Some(TEAM));

    engine.remove_profile(UDID.into(), profiles[0].uuid.clone()).await.expect("remove profile");
    assert_eq!(harness.device.state().removed_profiles, [profiles[0].uuid.clone()]);

    let apps = engine.device_apps(UDID.into()).await.expect("apps");
    assert!(apps.iter().any(|app| app.bundle_id.ends_with(TEAM) && app.is_developer_app));

    engine.uninstall_app(UDID.into(), format!("com.example.app.{TEAM}")).await.expect("uninstall");
    assert!(engine.device_apps(UDID.into()).await.expect("apps").is_empty());

    harness.device.state().paired = false;
    let unpaired = engine.devices().await.expect("devices");
    assert!(unpaired[0].paired, "values are cached for a known device");

    engine.pair_device(UDID.into()).await.expect("pair");
    assert!(harness.device.state().paired);

    harness.device.state().attached = false;
    let missing = run(&engine, harness.spec(harness.app(), SigningMode::AdHoc, AppOptions::default()));
    assert!(matches!(missing.result, Err(EngineError::DeviceUnavailable(_))), "{:?}", missing.result);
}

#[tokio::test(flavor = "multi_thread")]
async fn apple_silicon_installs_register_the_mac_and_place_a_tagged_wrapper() {
    let harness = Harness::new().await;
    let applications = harness.temporary.path().join("Applications");
    std::fs::create_dir(&applications).expect("applications");

    let mac = sl_engine::MacTarget {
        udid: "00006041-001A2B3C4D5E6F70".into(),
        name: "Fixture Mac".into(),
        model: "Mac16,5".into(),
        os_version: "27.2".into(),
        applications: applications.clone(),
    };

    let config = EngineConfig {
        data_dir: Some(harness.data_dir()),
        portal_origin: Some(FakePortal::origin(&harness.server)),
        profile_trust: Some(ProfileChain::shared().trust()),
        device_backend: Some(DeviceBackend(harness.device.backend())),
        mac_target: sl_engine::MacTargetSetting::Fixed(mac.clone()),
        file_secrets: true,
        disable_scheduler: true,
        ..EngineConfig::default()
    };
    let engine = Engine::new(config).expect("engine");
    let mut settings = engine.settings();
    settings.anisette = AnisetteSetting::Remote { url: format!("{}/anisette", harness.server.uri()) };
    engine.update_settings(settings).expect("settings");

    let sessions =
        serde_json::json!({ format!("{APPLE_ID}:a"): { "_type": "GsaAuthenticator", "dsid": "42", "gs_token": "t" } });
    let path = harness.temporary.path().join("sessions.json");
    fs::write(&path, serde_json::to_vec(&sessions).expect("JSON")).expect("sessions");
    engine.import_sessions(path).expect("import");

    let listed = engine.devices().await.expect("devices");
    assert!(listed.iter().any(|device| device.udid == mac.udid
        && device.device_class == "Mac"
        && device.model_name.as_deref() == Some("This Mac")));

    let spec = |track: bool| JobSpec {
        source: harness.ipa(),
        target: Target::Device { udid: mac.udid.clone(), prefer_network: false },
        signing: SigningMode::AppleId { apple_id: APPLE_ID.into() },
        options: AppOptions { track_for_refresh: track, ..AppOptions::default() },
    };

    let first = run(&engine, spec(true)).result.expect("Mac install");
    let placed = first.exported_to.clone().expect("installed path");

    assert_eq!(placed, applications.join("App.app"));
    assert_eq!(fs::read_link(placed.join("WrappedBundle")).expect("link"), Path::new("Wrapper/App.app"));
    assert!(placed.join("Wrapper/App.app/embedded.mobileprovision").is_file());
    assert!(!fs::read_to_string(placed.join("sideloadly.tag")).expect("tag").is_empty());

    let profile = ProvisioningProfile::parse(
        &fs::read(placed.join("Wrapper/App.app/embedded.mobileprovision")).expect("profile"),
    )
    .expect("profile decodes");
    assert!(profile.provisioned_devices.iter().any(|device| device == &mac.udid));
    assert!(
        harness.portal.state().requests.iter().any(
            |request| request.action == "ios/addDevice" && request.field("deviceNumber") == Some(mac.udid.as_str())
        )
    );
    assert_eq!(
        first.bundle_id,
        format!("com.example.app.{TEAM}"),
        "Apple Silicon targets always mangle for free teams"
    );

    fs::rename(&placed, applications.join("Renamed.app")).expect("user renames the app");
    let refreshed = run_refresh(&engine, first.installation_id.expect("tracked"));
    assert_eq!(
        refreshed.expect("refresh").exported_to,
        Some(applications.join("Renamed.app")),
        "the tag finds the renamed app"
    );

    let untracked = run(&engine, spec(false)).result.expect("one-off install");
    assert_eq!(
        untracked.exported_to,
        Some(applications.join("App.app")),
        "no tag match; the display name is free again"
    );

    let adhoc = JobSpec { signing: SigningMode::AdHoc, ..spec(false) };
    assert!(matches!(run(&engine, adhoc).result, Err(EngineError::Unsupported(_))));
    assert!(harness.device.state().installed.is_empty(), "nothing went to the iPhone fixture");
}
