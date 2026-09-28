#[path = "../../sl-bundle/tests/common/mod.rs"]
mod common;

use futures::executor::block_on;
use sl_bundle::{ArchiveLimits, BundleArchive, Control, OutputLayout, PackOptions};
use sl_engine::{
    AppOptions, BundleIdPolicy, Engine, EngineConfig, EngineError, ExtensionRemoval, FileReplacement, InfoOverride,
    InfoValue, JobEvent, JobSpec, PromptKind, PromptReply, SigningMode, Stage, Target,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn engine(root: &Path) -> Engine {
    Engine::new(EngineConfig { data_dir: Some(root.join("data")), disable_scheduler: true, ..EngineConfig::default() })
        .expect("engine")
}

fn spec(source: PathBuf, output: Option<PathBuf>, signing: SigningMode) -> JobSpec {
    JobSpec { source, target: Target::ExportIpa { path: output }, signing, options: AppOptions::default() }
}

#[test]
fn inspection_jobs_use_the_real_reader_and_close_their_event_stream() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let engine = engine(temporary.path());
    let job = engine.inspect_job(root.clone());
    let events = job.events();
    let summary = block_on(job.result()).expect("inspection job");

    assert_eq!(summary, block_on(engine.inspect(root)).expect("inspection future"));
    assert!(matches!(events.try_recv(), Ok(JobEvent::Stage(Stage::Preparing))));
    assert!(block_on(events.recv()).is_err());
}

#[test]
fn real_engine_inspects_and_exports_metadata_edits_without_mutating_the_original() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    common::synthetic_bundle(&root.join("PlugIns/Widget.appex"), "com.example.test.widget", "Widget", "XPC!");
    common::synthetic_bundle(&root.join("Extensions/Other.appex"), "com.example.test.other", "Other", "XPC!");
    let original_info = fs::read(root.join("Info.plist")).expect("original");
    let original_executable = fs::read(root.join("Test")).expect("executable");
    let replacement = temporary.path().join("replacement");
    fs::write(&replacement, b"new resource").expect("replacement");
    let engine = engine(temporary.path());
    let summary = block_on(engine.inspect(root.clone())).expect("inspect");

    assert_eq!(summary.bundle_id, "com.example.test");
    assert_eq!(summary.extensions.len(), 2);

    let output = temporary.path().join("signed.ipa");
    let mut job = spec(root.clone(), Some(output.clone()), SigningMode::AdHoc);
    job.options = AppOptions {
        bundle_id: BundleIdPolicy::Custom("com.changed.test".into()),
        display_name: Some("Changed".into()),
        enable_file_sharing: true,
        remove_extensions: ExtensionRemoval::All,
        replacements: vec![FileReplacement { target: "resource".into(), source: Some(replacement) }],
        extra_info: vec![InfoOverride { key: "CustomInteger".into(), value: InfoValue::Integer(42) }],
        ..AppOptions::default()
    };
    let handle = engine.start(job);
    let events = handle.events();
    let outcome = block_on(handle.result()).expect("export");
    let stages: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            JobEvent::Stage(stage) => Some(stage),
            _ => None,
        })
        .collect();
    let archive = BundleArchive::unpack(&output, ArchiveLimits::default(), Control::default()).expect("output");
    let info = sl_bundle::read_dictionary(&archive.bundle_path().join("Info.plist")).expect("info");
    let bytes = fs::read(archive.bundle_path().join("Test")).expect("signed executable");

    assert_eq!(outcome.bundle_id, "com.changed.test");
    assert_eq!(outcome.exported_to, Some(output));
    assert_eq!(info["CFBundleDisplayName"].as_string(), Some("Changed"));
    assert_eq!(info["UIFileSharingEnabled"].as_boolean(), Some(true));
    assert_eq!(info["CustomInteger"].as_signed_integer(), Some(42));
    assert!(!archive.bundle_path().join("PlugIns").exists());
    assert!(!archive.bundle_path().join("Extensions").exists());
    assert_eq!(fs::read(archive.bundle_path().join("resource")).expect("resource"), b"new resource");
    assert!(sl_macho::Binary::parse(&bytes).expect("Mach-O").slices[0].image.signature.is_some());
    assert_eq!(stages, [Stage::Preparing, Stage::Patching, Stage::Signing, Stage::Packaging, Stage::Done]);
    assert_eq!(fs::read(root.join("Info.plist")).expect("original info"), original_info);
    assert_eq!(fs::read(root.join("Test")).expect("original binary"), original_executable);
}

#[test]
fn original_archive_exports_are_byte_identical_and_save_prompts_use_the_selected_path() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let archive = BundleArchive::unpack(&root, ArchiveLimits::default(), Control::default()).expect("unpack");
    let source = temporary.path().join("original.ipa");
    archive.save(&source, OutputLayout::Ipa, PackOptions::default(), Control::default()).expect("pack");
    let output = temporary.path().join("copy.ipa");
    let engine = engine(temporary.path());
    let handle = engine.start(spec(source.clone(), None, SigningMode::Original));
    let events = handle.events();

    block_on(async {
        while let Ok(event) = events.recv().await {
            if let JobEvent::Prompt(prompt) = event {
                assert!(matches!(prompt.kind, PromptKind::SaveFile { .. }));
                prompt.answer(PromptReply::Path(output.clone()));
            }
        }

        handle.result().await.expect("original export");
    });

    assert_eq!(fs::read(&source).expect("source"), fs::read(&output).expect("output"));
    assert!(matches!(
        block_on(engine.start(spec(source.clone(), Some(source), SigningMode::Original)).result()),
        Err(EngineError::Storage(_))
    ));
}

#[test]
fn cancelled_pack_preserves_existing_output_and_waits_for_worker_cleanup() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    fs::File::create(root.join("large-resource")).expect("resource").set_len(64 * 1024 * 1024).expect("length");
    let output = temporary.path().join("output.ipa");
    fs::write(&output, b"previous export").expect("existing output");
    let engine = engine(temporary.path());
    let handle = engine.start(spec(root, Some(output.clone()), SigningMode::Original));
    let events = handle.events();
    let cancellation = handle.cancellation_token();

    block_on(async {
        while let Ok(event) = events.recv().await {
            if matches!(event, JobEvent::Stage(Stage::Packaging)) {
                cancellation.cancel();
            }
        }

        assert_eq!(handle.result().await, Err(EngineError::Cancelled));
    });

    assert_eq!(fs::read(&output).expect("unchanged output"), b"previous export");
    assert!(
        !fs::read_dir(temporary.path()).expect("directory").any(|entry| entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp"))
    );
}

#[test]
fn dropping_the_engine_keeps_an_active_job_alive_and_subscription_channels_close() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let engine = engine(temporary.path());
    let devices = engine.subscribe_devices();
    let refresh = engine.subscribe_refresh();
    let handle = engine.start(spec(root, Some(temporary.path().join("output.ipa")), SigningMode::Unsigned));
    drop(engine);

    assert!(devices.is_closed());
    assert!(refresh.is_closed());
    assert!(block_on(handle.result()).is_ok());
}

#[test]
fn settings_survive_restart_and_failed_updates_preserve_memory_and_disk() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let engine = engine(temporary.path());
    let mut settings = engine.settings();
    settings.theme = sl_engine::ThemePreference::Dark;
    engine.update_settings(settings.clone()).expect("save");
    let mut too_large = settings.clone();
    too_large.anisette = sl_engine::AnisetteSetting::Remote { url: "x".repeat(65536) };

    assert!(engine.update_settings(too_large).is_err());
    assert_eq!(engine.settings(), settings);
    let directory = engine.data_dir();
    drop(engine);
    let restarted =
        Engine::new(EngineConfig { data_dir: Some(directory), ..EngineConfig::default() }).expect("restart");
    assert_eq!(restarted.settings(), settings);
    assert!(serde_json::from_str::<AppOptions>("{}").expect("default options").remove_watch_app);
}

#[tokio::test]
async fn engine_can_be_dropped_from_an_async_caller() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let engine = engine(temporary.path());
    drop(engine);

    let invalid = temporary.path().join("invalid");
    fs::create_dir(&invalid).expect("directory");
    fs::write(invalid.join("settings.json"), b"invalid JSON").expect("settings");
    assert!(Engine::new(EngineConfig { data_dir: Some(invalid), ..EngineConfig::default() }).is_err());
}

#[test]
fn dropping_a_job_handle_cancels_a_pending_prompt_and_closes_its_events() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let engine = engine(temporary.path());
    let handle = engine.start(spec(root, None, SigningMode::Unsigned));
    let events = handle.events();
    let cancellation = handle.cancellation_token();

    block_on(async {
        while let Ok(event) = events.recv().await {
            if let JobEvent::Prompt(prompt) = event {
                drop(handle);
                assert!(cancellation.is_cancelled());

                // Keep the prompt alive: cancellation must finish without an answer or drop.
                let withdrawn = events.recv().await.expect("withdrawal");
                assert!(matches!(withdrawn, JobEvent::PromptWithdrawn { id } if id == prompt.id));
                assert!(events.recv().await.is_err());
                drop(prompt);
                break;
            }
        }
    });
}

#[test]
fn unavailable_backends_and_identity_only_edits_fail_before_output_mutation() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let output = temporary.path().join("existing.ipa");
    fs::write(&output, b"original output").expect("output");
    let engine = engine(temporary.path());
    let base = spec(root, Some(output.clone()), SigningMode::Unsigned);
    let mut apple = base.clone();
    apple.signing = SigningMode::AppleId { apple_id: "test@example.invalid".into() };
    let mut device = base.clone();
    device.target = Target::Device { udid: "test".into(), prefer_network: false };
    let mut icon = base.clone();
    icon.options.icon = Some("icon.png".into());
    let mut entitlements = base;
    entitlements.options.entitlements = Some("entitlements.plist".into());

    for job in [device, icon, entitlements] {
        assert!(matches!(block_on(engine.start(job).result()), Err(EngineError::Unsupported(_))));
        assert_eq!(fs::read(&output).expect("output"), b"original output");
    }

    let signed_out = block_on(engine.start(apple).result());
    assert!(
        matches!(&signed_out, Err(EngineError::Auth(message)) if message.contains("not signed in")),
        "{signed_out:?}"
    );
    assert_eq!(fs::read(&output).expect("output"), b"original output");
}

#[test]
fn concurrent_progress_is_monotonic_and_does_not_enqueue_one_event_per_chunk() {
    let (context, events, _) = sl_engine::JobContext::channel();
    context.stage(Stage::Preparing);
    context.progress(0, 16000);

    std::thread::scope(|scope| {
        for worker in 0..16 {
            let context = &context;
            scope.spawn(move || {
                for chunk in 1..=1000 {
                    context.progress(worker * 1000 + chunk, 16000);
                }
            });
        }
    });
    context.progress(16000, 16000);
    let progress: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            JobEvent::Progress { done, total: 16000 } => Some(done),
            _ => None,
        })
        .collect();

    assert_eq!(progress.first(), Some(&0));
    assert_eq!(progress.last(), Some(&16000));
    assert!(progress.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(progress.len() < 100, "progress event count: {}", progress.len());
}

#[cfg(target_os = "macos")]
#[test]
fn apple_codesign_accepts_an_ipa_processed_through_the_real_engine() {
    use std::process::Command;

    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::write_info(&root, common::info("com.example.test", "Test", "APPL"));
    let source = temporary.path().join("main.c");
    fs::write(&source, "int main(void) { return 0; }\n").expect("source");
    common::compile(&source, &root.join("Test"), false);
    let engine = engine(temporary.path());
    let output = temporary.path().join("signed.ipa");
    let mut job = spec(root.clone(), Some(output.clone()), SigningMode::AdHoc);
    job.options.display_name = Some("Engine Export".into());
    block_on(engine.start(job).result()).expect("engine export");
    let unpacked = BundleArchive::unpack(&output, ArchiveLimits::default(), Control::default()).expect("output");

    common::run(
        Command::new("codesign")
            .args(["--verify", "--strict", "--deep", "--all-architectures"])
            .arg(unpacked.bundle_path()),
    );
    common::run(&mut Command::new(unpacked.bundle_path().join("Test")));
    fs::write(unpacked.bundle_path().join("Info.plist"), b"tampered").expect("tamper");
    let tampered =
        Command::new("codesign").args(["--verify", "--strict"]).arg(unpacked.bundle_path()).output().expect("verify");

    assert!(!tampered.status.success());
}

#[test]
fn remote_sources_are_downloaded_verified_and_removed_after_the_job() {
    use sha1::Digest as _;

    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.remote", "Test", "APPL");

    let archive =
        BundleArchive::unpack(&root, ArchiveLimits::default(), sl_bundle::Control::default()).expect("unpack");
    let ipa = temporary.path().join("remote.ipa");
    archive.save(&ipa, OutputLayout::Ipa, PackOptions::default(), sl_bundle::Control::default()).expect("pack");
    let bytes = fs::read(&ipa).expect("IPA bytes");
    let digest = hex::encode(sha1::Sha1::digest(&bytes));

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let server = runtime.block_on(wiremock::MockServer::start());
    runtime.block_on(
        wiremock::Mock::given(wiremock::matchers::path("/remote.ipa"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .mount(&server),
    );

    let engine = engine(temporary.path());
    let link = format!("sideloadly:?dn=Remote.ipa&xs={}/remote.ipa&h={digest}", server.uri());
    let output = temporary.path().join("exported.ipa");

    let outcome = block_on(engine.start(spec(link.into(), Some(output.clone()), SigningMode::Unsigned)).result());
    assert_eq!(outcome.expect("remote export").bundle_id, "com.example.remote");
    assert!(output.is_file());

    let downloads = temporary.path().join("data/downloads");
    assert_eq!(fs::read_dir(&downloads).expect("downloads").count(), 0, "the job's download is removed");

    let cached = block_on(engine.download(format!("{}/remote.ipa", server.uri())).result()).expect("download");
    assert_eq!(sl_acquire::download::read_plain(&cached, true).expect("plain"), bytes);

    let wrong = format!("sideloadly:?xs={}/remote.ipa&h={}", server.uri(), "0".repeat(40));
    let rejected = block_on(
        engine.start(spec(wrong.into(), Some(temporary.path().join("x.ipa")), SigningMode::Unsigned)).result(),
    );
    assert!(matches!(rejected, Err(EngineError::Network(message)) if message.contains("Hash mismatch")));

    let store = block_on(
        engine.start(spec("sideloadly:?c=US&bi=com.example.app".into(), None, SigningMode::Unsigned)).result(),
    );
    assert!(matches!(store, Err(EngineError::Unsupported(_))));
}

#[test]
fn engines_sharing_a_data_directory_keep_each_others_setting_changes() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let desktop = engine(temporary.path());
    let daemon = engine(temporary.path());

    assert_eq!(daemon.settings().theme, sl_engine::ThemePreference::System);

    desktop
        .modify_settings(|settings| {
            settings.theme = sl_engine::ThemePreference::Dark;

            Ok(())
        })
        .expect("desktop change");

    assert_eq!(daemon.settings().theme, sl_engine::ThemePreference::Dark, "the other engine reloads");

    daemon
        .modify_settings(|settings| {
            settings.refresh.threshold_hours = 30;

            Ok(())
        })
        .expect("daemon change");

    let merged = desktop.settings();
    assert_eq!((merged.theme, merged.refresh.threshold_hours), (sl_engine::ThemePreference::Dark, 30));

    let refused = desktop.modify_settings(|settings| {
        settings.stream_upload = !settings.stream_upload;

        Err(EngineError::Other("invalid form".into()))
    });
    assert!(refused.is_err());
    assert_eq!(daemon.settings(), merged, "a failed change saves nothing");

    std::fs::write(temporary.path().join("data/settings.json"), b"{ not json").expect("corrupt");
    assert_eq!(desktop.settings(), merged, "unreadable settings keep the last good ones");
}

#[test]
fn an_input_rewritten_while_the_job_runs_fails_the_job_before_output() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let root = temporary.path().join("Test.app");
    common::synthetic_bundle(&root, "com.example.test", "Test", "APPL");
    let output = temporary.path().join("output.ipa");
    let engine = engine(temporary.path());

    let handle = engine.start(spec(root.clone(), None, SigningMode::AdHoc));
    let events = handle.events();

    block_on(async {
        while let Ok(event) = events.recv().await {
            if let JobEvent::Prompt(prompt) = event {
                // Same length, new bytes: only the file's times reveal the rewrite.
                std::thread::sleep(std::time::Duration::from_millis(20));
                let info = fs::read(root.join("Info.plist")).expect("plist");
                fs::write(root.join("Info.plist"), &info).expect("rewrite");

                prompt.answer(PromptReply::Path(output.clone()));
            }
        }
    });

    let result = block_on(handle.result());
    assert!(
        matches!(&result, Err(EngineError::InvalidApp(message)) if message.contains("Info.plist")),
        "{result:?}"
    );
    assert!(!output.exists(), "no output is written from a changed input");
}
