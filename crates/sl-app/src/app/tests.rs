use super::*;
use gpui::{EventEmitter, TestAppContext, VisualTestContext};
use gpui_component::Root;
use sl_bundle::{ArchiveLimits, BundleArchive, Control};
use sl_engine::{AppOptions, EngineConfig, JobSpec, Prompt, PromptKind, Target};
use std::fs;

#[path = "../../../sl-bundle/tests/common/mod.rs"]
mod common;

impl EventEmitter<()> for Sideport {}

fn window(engine: Engine, cx: &mut TestAppContext) -> (Entity<Sideport>, VisualTestContext) {
    // The engine uses real Tokio workers rather than GPUI's deterministic test executor.
    cx.executor().allow_parking();
    cx.update(gpui_component::init);
    cx.update(|cx| cx.bind_keys([gpui::KeyBinding::new("cmd-e", ExportApp, Some("Sideport"))]));
    let mut view = None;
    let window = cx.add_window(|window, cx| {
        let sideport = cx.new(|cx| Sideport::new(engine, window, cx));
        view = Some(sideport.clone());

        Root::new(sideport, window, cx)
    });

    (view.expect("view"), VisualTestContext::from_window(window.into(), cx))
}

fn engine(root: &Path) -> Engine {
    Engine::new(EngineConfig { data_dir: Some(root.join("data")), ..EngineConfig::default() }).expect("engine")
}

#[gpui::test]
fn file_edits_validate_on_enter_and_cancel_on_escape(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (view, mut cx) = window(engine(temporary.path()), cx);

    cx.update(|window, cx| view.update(cx, |view, cx| view.file_edit(None, window, cx)));
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_input("../outside");
    cx.simulate_keystrokes("enter");
    assert!(cx.read(|cx| view.read(cx).dialog_error.is_some()));

    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            let Some(Dialog::File(dialog)) = &view.dialog else { panic!("file dialog") };
            dialog.target.update(cx, |input, cx| input.set_value("Resources/obsolete.txt", window, cx));
        })
    });
    cx.simulate_keystrokes("enter");

    cx.read(|cx| {
        let view = view.read(cx);
        assert!(view.dialog.is_none());
        assert_eq!(
            view.draft.options.replacements,
            vec![FileReplacement { target: "Resources/obsolete.txt".into(), source: None }]
        );
    });

    cx.update(|window, cx| view.update(cx, |view, cx| view.file_edit(None, window, cx)));
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_keystrokes("escape");
    assert!(cx.read(|cx| view.read(cx).dialog.is_none()));
    assert_eq!(cx.read(|cx| view.read(cx).draft.options.replacements.len()), 1);
}

#[gpui::test]
async fn the_editor_exports_real_metadata_and_preserves_the_source(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let source = temporary.path().join("Test.app");
    let destination = temporary.path().join("prepared.ipa");
    common::synthetic_bundle(&source, "com.example.test", "Test", "APPL");
    let original_info = fs::read(source.join("Info.plist")).expect("source plist");
    let original_binary = fs::read(source.join("Test")).expect("source executable");
    let (view, mut cx) = window(engine(temporary.path()), cx);

    cx.update(|window, cx| view.update(cx, |view, cx| view.load_path(source.clone(), window, cx)));
    view.condition::<()>(&cx, |view, _| !view.busy).await;
    assert!(cx.read(|cx| view.read(cx).app.is_some()));

    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let mode = cx.debug_bounds("mode:Ad-hoc signed").expect("mode bounds");
    cx.simulate_click(mode.center(), gpui::Modifiers::none());
    assert_eq!(cx.read(|cx| view.read(cx).mode), ExportMode::AdHoc);

    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.fields.name.update(cx, |input, cx| input.set_value("Desktop Export", window, cx));
            view.fields.identifier.update(cx, |input, cx| input.set_value("com.example.desktop", window, cx));
        })
    });
    cx.update(|window, cx| {
        let _ = window.draw(cx);
    });
    cx.simulate_keystrokes("cmd-e");
    cx.simulate_new_path_selection(|_| Some(destination.clone()));
    view.condition::<()>(&cx, |view, _| !view.busy).await;

    cx.read(|cx| {
        let view = view.read(cx);
        assert_eq!(view.error, None);
        assert_eq!(view.outcome.as_ref().and_then(|outcome| outcome.exported_to.as_ref()), Some(&destination));
    });
    let unpacked = BundleArchive::unpack(&destination, ArchiveLimits::default(), Control::default()).expect("export");
    let info = plist::Value::from_file(unpacked.bundle_path().join("Info.plist")).expect("plist");
    let fields = info.as_dictionary().expect("dictionary");

    assert_eq!(fields["CFBundleIdentifier"].as_string(), Some("com.example.desktop"));
    assert_eq!(fields["CFBundleDisplayName"].as_string(), Some("Desktop Export"));
    assert!(unpacked.bundle_path().join("_CodeSignature/CodeResources").exists());
    assert_eq!(fs::read(source.join("Info.plist")).expect("source plist"), original_info);
    assert_eq!(fs::read(source.join("Test")).expect("source executable"), original_binary);
}

#[gpui::test]
async fn closing_a_window_cancels_and_joins_a_real_job_waiting_for_a_prompt(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let source = temporary.path().join("Test.app");
    common::synthetic_bundle(&source, "com.example.test", "Test", "APPL");
    let (view, mut cx) = window(engine(temporary.path()), cx);
    let job = JobSpec {
        source,
        target: Target::ExportIpa { path: None },
        signing: sl_engine::SigningMode::Unsigned,
        options: AppOptions::default(),
    };

    cx.update(|window, cx| view.update(cx, |view, cx| view.start_job(job, window, cx)));
    view.condition::<()>(&cx, |view, _| view.dialog.is_some()).await;

    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            assert!(!view.prepare_close(window, cx));
            assert!(view.cancellation.as_ref().expect("token").is_cancelled());
            assert!(view.busy);
        })
    });
    view.condition::<()>(&cx, |view, _| !view.busy).await;

    cx.read(|cx| {
        assert_eq!(view.read(cx).status, "Cancelled");
        assert!(cx.windows().is_empty());
    });
}

#[gpui::test]
async fn a_failed_inspection_clears_the_previous_app(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let source = temporary.path().join("Test.app");
    common::synthetic_bundle(&source, "com.example.test", "Test", "APPL");
    let (view, mut cx) = window(engine(temporary.path()), cx);

    cx.update(|window, cx| view.update(cx, |view, cx| view.load_path(source, window, cx)));
    view.condition::<()>(&cx, |view, _| !view.busy).await;
    assert!(cx.read(|cx| view.read(cx).app.is_some()));

    cx.update(|window, cx| view.update(cx, |view, cx| view.load_path(temporary.path().join("absent.ipa"), window, cx)));
    view.condition::<()>(&cx, |view, _| !view.busy).await;

    cx.read(|cx| {
        let view = view.read(cx);
        assert!(view.app.is_none());
        assert!(view.error.is_some());
    });
}

#[gpui::test]
async fn verification_codes_require_exact_ascii_digits_and_debug_excludes_secrets(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (view, mut cx) = window(engine(temporary.path()), cx);
    let (prompt, reply) = Prompt::new(
        1,
        PromptKind::SecondFactor {
            apple_id: "test@example.invalid".into(),
            destination: "trusted device".into(),
            code_length: 6,
            can_request_sms: true,
        },
    );

    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.job_event(JobEvent::Prompt(prompt), window, cx);
            let Some(Dialog::Prompt(dialog)) = &view.dialog else { panic!("prompt") };
            dialog.input.update(cx, |input, cx| input.set_value("12345x", window, cx));
            view.submit_dialog(window, cx);
            assert!(view.dialog_error.is_some());

            let Some(Dialog::Prompt(dialog)) = &view.dialog else { panic!("prompt") };
            dialog.input.update(cx, |input, cx| input.set_value("732841", window, cx));
            assert!(!format!("{view:?}").contains("732841"));
            view.submit_dialog(window, cx);
            assert!(view.dialog.is_none());
        })
    });

    assert_eq!(reply.await.expect("reply"), PromptReply::Text { value: "732841".into(), remember: false });
}
