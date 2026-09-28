//! Every engine question through the rendered dialog: its controls, keys and replies.

use super::*;
use futures::channel::oneshot;
use sl_engine::{Connection, DeviceInfo, TeamChoice, TeamKind};

/// Deliver a job question to the view as the job event loop does.
fn ask(view: &Entity<Sideport>, cx: &mut VisualTestContext, kind: PromptKind) -> oneshot::Receiver<PromptReply> {
    let (prompt, reply) = Prompt::new(1, kind);

    cx.update(|window, cx| view.update(cx, |view, cx| view.job_event(JobEvent::Prompt(prompt), window, cx)));
    draw(cx);

    reply
}

fn event(view: &Entity<Sideport>, cx: &mut VisualTestContext, event: JobEvent) {
    cx.update(|window, cx| view.update(cx, |view, cx| view.job_event(event, window, cx)));
}

fn second_factor(can_request_sms: bool) -> PromptKind {
    PromptKind::SecondFactor {
        apple_id: "fixture@example.test".into(),
        destination: "your trusted devices".into(),
        code_length: 6,
        can_request_sms,
    }
}

fn confirm(destructive: bool) -> PromptKind {
    PromptKind::Confirm {
        title: "Certificate limit reached".into(),
        message: "Revoke the oldest certificate?".into(),
        confirm_label: if destructive { "Revoke" } else { "Continue" }.into(),
        destructive,
    }
}

fn wait_for_device() -> PromptKind {
    PromptKind::WaitForDevice {
        udid: UDID.into(),
        device_name: UDID.into(),
        reason: "Waiting for the device to re-appear.".into(),
    }
}

fn has_dialog(view: &Entity<Sideport>, cx: &mut VisualTestContext) -> bool {
    cx.read(|cx| view.read(cx).dialog.is_some())
}

#[gpui::test]
async fn second_factor_prompts_offer_a_text_message_code_only_when_available(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (view, mut cx) = window(engine(temporary.path()), cx);

    let reply = ask(&view, &mut cx, second_factor(false));
    assert!(!rendered(&mut cx, "prompt-request-sms"), "no text-message button without SMS support");

    cx.simulate_input("048213");
    cx.simulate_keystrokes("enter");

    assert!(!has_dialog(&view, &mut cx));
    assert_eq!(reply.await.expect("reply"), PromptReply::Text { value: "048213".into(), remember: false });

    let reply = ask(&view, &mut cx, second_factor(true));
    click(&mut cx, "prompt-request-sms");

    assert!(!has_dialog(&view, &mut cx));
    assert_eq!(reply.await.expect("reply"), PromptReply::RequestSms);
}

#[gpui::test]
async fn password_prompts_are_masked_and_return_the_remember_choice(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (view, mut cx) = window(engine(temporary.path()), cx);

    let kind = PromptKind::Password { apple_id: "fixture@example.test".into(), remember: false };
    let reply = ask(&view, &mut cx, kind);

    let masked = cx.read(|cx| matches!(&view.read(cx).dialog, Some(Dialog::Prompt(dialog)) if dialog.masked));
    assert!(masked);

    cx.simulate_keystrokes("enter");
    assert_eq!(cx.read(|cx| view.read(cx).dialog_error.clone()), Some("Enter your password.".into()));

    cx.simulate_input("correct horse");
    click(&mut cx, "prompt-remember");

    let debug = cx.read(|cx| format!("{:?}", view.read(cx)));
    assert!(!debug.contains("correct horse"), "{debug}");

    cx.simulate_keystrokes("enter");

    assert_eq!(reply.await.expect("reply"), PromptReply::Text { value: "correct horse".into(), remember: true });
}

#[gpui::test]
async fn team_prompts_answer_with_the_clicked_team(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (view, mut cx) = window(engine(temporary.path()), cx);

    let team = |team_id: &str, kind| TeamChoice {
        team: sl_engine::TeamSummary { team_id: team_id.into(), name: format!("Team {team_id}"), kind },
    };
    let teams = vec![team("FREE000001", TeamKind::Free), team("PAID000002", TeamKind::Organization)];
    let reply = ask(&view, &mut cx, PromptKind::ChooseTeam { apple_id: "fixture@example.test".into(), teams });

    assert!(!rendered(&mut cx, "dialog-submit"), "a team is chosen from the list");
    cx.simulate_keystrokes("enter");
    assert!(has_dialog(&view, &mut cx));

    click(&mut cx, "prompt-team:1");

    assert!(!has_dialog(&view, &mut cx));
    assert_eq!(reply.await.expect("reply"), PromptReply::Choice(1));
}

#[gpui::test]
async fn destructive_confirmations_need_a_click_and_decline_as_false(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (view, mut cx) = window(engine(temporary.path()), cx);

    let reply = ask(&view, &mut cx, confirm(true));
    assert!(rendered(&mut cx, "dialog-submit-danger"));
    assert!(!rendered(&mut cx, "dialog-submit"));

    cx.simulate_keystrokes("enter");
    assert!(has_dialog(&view, &mut cx), "Enter does not confirm a destructive action");

    click(&mut cx, "dialog-cancel");
    assert_eq!(reply.await.expect("reply"), PromptReply::Confirmed(false));

    let reply = ask(&view, &mut cx, confirm(true));
    click(&mut cx, "dialog-submit-danger");
    assert_eq!(reply.await.expect("reply"), PromptReply::Confirmed(true));

    let reply = ask(&view, &mut cx, confirm(false));
    assert!(rendered(&mut cx, "dialog-submit"));
    cx.simulate_keystrokes("enter");
    assert_eq!(reply.await.expect("reply"), PromptReply::Confirmed(true));

    let reply = ask(&view, &mut cx, confirm(false));
    cx.simulate_keystrokes("escape");
    assert_eq!(reply.await.expect("reply"), PromptReply::Confirmed(false));
}

#[gpui::test]
async fn device_questions_retry_or_close_when_the_job_continues(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (view, mut cx) = window(engine(temporary.path()), cx);

    let device = DeviceInfo {
        udid: UDID.into(),
        name: "Fixture iPhone".into(),
        product_type: "iPhone15,2".into(),
        model_name: Some("iPhone 14 Pro".into()),
        os_version: "17.5".into(),
        device_class: "iPhone".into(),
        connections: vec![Connection::Usb],
        paired: true,
    };
    cx.update(|_, cx| view.update(cx, |view, _| view.devices.list = vec![device]));

    let reply = ask(&view, &mut cx, wait_for_device());
    let message = cx.read(|cx| match &view.read(cx).dialog {
        Some(Dialog::Prompt(dialog)) => dialog.message(),
        _ => panic!("device question"),
    });
    assert!(message.starts_with("Fixture iPhone is no longer connected."), "{message}");

    click(&mut cx, "dialog-submit");
    assert_eq!(reply.await.expect("reply"), PromptReply::Confirmed(true));

    let reply = ask(&view, &mut cx, wait_for_device());
    event(&view, &mut cx, JobEvent::Progress { done: 1, total: 4 });

    assert!(!has_dialog(&view, &mut cx), "progress after the device returned closes the question");
    assert_eq!(reply.await.expect("reply"), PromptReply::Cancel, "the abandoned question is released");
    assert!(cx.read(|cx| view.read(cx).logs.iter().any(|(_, message)| message.contains("continuing"))));

    let reply = ask(&view, &mut cx, wait_for_device());
    event(&view, &mut cx, JobEvent::Stage(Stage::Installing));
    assert!(!has_dialog(&view, &mut cx));
    assert_eq!(reply.await.expect("reply"), PromptReply::Cancel);

    let reply = ask(&view, &mut cx, wait_for_device());
    cx.update(|window, cx| view.update(cx, |view, cx| view.finished(window, cx)));
    assert!(!has_dialog(&view, &mut cx), "a finished job closes its question");
    assert_eq!(reply.await.expect("reply"), PromptReply::Cancel);

    let _password =
        ask(&view, &mut cx, PromptKind::Password { apple_id: "fixture@example.test".into(), remember: false });
    event(&view, &mut cx, JobEvent::Progress { done: 1, total: 4 });
    assert!(has_dialog(&view, &mut cx), "other questions stay until the job moves to another stage");
    event(&view, &mut cx, JobEvent::Stage(Stage::Provisioning));
    assert!(!has_dialog(&view, &mut cx));
}
