//! Device utilities through the engine with a fake device layer: Developer Disk Image mounting
//! (legacy download via wiremock), JIT command sequencing, and pairing repair. These fixtures
//! establish the engine's orchestration; physical mounting/JIT/pairing are outside their scope.

use futures::executor::block_on;
use sl_engine::{DdiConfig, DeviceBackend, Engine, EngineConfig, JobEvent, JobHandle};
use sl_testkit::FakeDevice;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const UDID: &str = "00008030-001A2D0C0E38802E";

fn engine(device: &FakeDevice, data_dir: std::path::PathBuf, ddi: DdiConfig) -> Engine {
    let config = EngineConfig {
        data_dir: Some(data_dir),
        device_backend: Some(DeviceBackend(device.backend())),
        file_secrets: true,
        disable_scheduler: true,
        ddi,
        ..EngineConfig::default()
    };

    Engine::new(config).expect("engine")
}

/// Drain a job's result, ignoring events (prompts are never expected here).
fn finish<T: Send + 'static>(job: JobHandle<T>) -> Result<T, sl_engine::EngineError> {
    let events = job.events();
    let collector = std::thread::spawn(move || while block_on(events.recv()).is_ok() {});
    let result = block_on(job.result());
    collector.join().expect("collector");

    result
}

#[tokio::test(flavor = "multi_thread")]
async fn mounting_a_legacy_developer_image_downloads_and_mounts_it() {
    let server = MockServer::start().await;

    for repo in ["xushuduo/Xcode-iOS-Developer-Disk-Image", "mspvirajpatel/Xcode_Developer_Disk_Images"] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo}/releases/tags/16.5")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
    }

    Mock::given(method("GET"))
        .and(path("/pdso/DeveloperDiskImage/master/16.5/DeveloperDiskImage.dmg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"image".to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pdso/DeveloperDiskImage/master/16.5/DeveloperDiskImage.dmg.signature"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"sig".to_vec()))
        .mount(&server)
        .await;

    let device = FakeDevice::new(UDID, "Fixture iPhone", "iPhone", "iPhone12,1", "16.5");
    let temporary = tempfile::tempdir().expect("tempdir");
    let ddi = DdiConfig { github_api: Some(server.uri()), raw_content: Some(server.uri()), tss_endpoint: None };
    let engine = engine(&device, temporary.path().join("data"), ddi);

    let mount = finish(engine.mount_developer_image(UDID.into())).expect("mount");
    assert!(!mount.already_mounted);
    assert!(!mount.personalized);

    let state = device.state();
    assert_eq!(state.mounts.len(), 1, "the developer image was uploaded once");
    assert_eq!(state.mounts[0].0, "Developer");
    assert_eq!(state.mounts[0].1, b"image");
}

#[tokio::test(flavor = "multi_thread")]
async fn enabling_jit_mounts_then_launches_and_detaches() {
    let device = FakeDevice::iphone(UDID);
    {
        // A developer image is already mounted, so JIT goes straight to debugserver.
        let mut state = device.state();
        state.mounted = Some(sl_device::mounter::Mounted::Developer);
        state.launch.insert(
            "com.example.app".into(),
            sl_device::jit::AppLaunch {
                path: "/private/var/containers/Bundle/Application/AAAA/App.app".into(),
                container: Some("/private/var/mobile/Containers/Data/Application/BBBB".into()),
                executable: "App".into(),
            },
        );
    }

    let temporary = tempfile::tempdir().expect("tempdir");
    let engine = engine(&device, temporary.path().join("data"), DdiConfig::default());

    finish(engine.enable_jit(UDID.into(), "com.example.app".into(), true)).expect("jit");

    let commands = device.state().jit_commands.clone();
    assert_eq!(
        commands,
        vec![
            "QSetLogging:bitmask=LOG_ALL|LOG_RNB_REMOTE|LOG_RNB_PACKETS".to_string(),
            "QSetMaxPacketSize: 1024".into(),
            "QSetWorkingDir: /private/var/mobile/Containers/Data/Application/BBBB".into(),
            "A /private/var/containers/Bundle/Application/AAAA/App.app".into(),
            "qLaunchSuccess".into(),
            "D".into(),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repairing_pairing_unpairs_then_pairs_again() {
    let device = FakeDevice::iphone(UDID);
    let temporary = tempfile::tempdir().expect("tempdir");
    let engine = engine(&device, temporary.path().join("data"), DdiConfig::default());

    let job = engine.repair_pairing(UDID.into());
    let events = job.events();

    let collector = std::thread::spawn(move || {
        let mut logs = Vec::new();

        while let Ok(event) = block_on(events.recv()) {
            if let JobEvent::Log { message, .. } = event {
                logs.push(message);
            }
        }

        logs
    });

    finish_result(block_on(job.result()));
    let logs = collector.join().expect("collector");

    let state = device.state();
    assert!(state.unpaired, "the device was unpaired");
    assert!(state.paired, "and paired again");
    assert!(logs.iter().any(|line| line.contains("Removing the existing pairing")));
    assert!(logs.iter().any(|line| line.contains("Pairing repaired")));
}

fn finish_result(result: Result<(), sl_engine::EngineError>) {
    result.expect("repair pairing");
}
