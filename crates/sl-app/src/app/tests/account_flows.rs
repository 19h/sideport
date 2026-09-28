//! Accounts and Apple ID signing through the demo engine and the rendered controls.

use super::*;

const DEMO_IPHONE: &str = "00008120-001A2B3C4D5E6F70";

/// Sign in through the Accounts form: password prompt, then a verification code.
async fn sign_in(cx: &mut VisualTestContext, view: &Entity<Sideport>, apple_id: &str, password: &str) {
    click(cx, "nav:Accounts");

    let input = cx.read(|cx| view.read(cx).accounts.apple_id.clone());
    set_input(cx, &input, apple_id);
    click(cx, "sign-in");

    until(cx, view, "password prompt", |view| matches!(prompt_kind(view), Some(PromptKind::Password { .. }))).await;
    draw(cx);
    cx.simulate_input(password);
    click(cx, "prompt-remember");
    cx.simulate_keystrokes("enter");

    until(cx, view, "code prompt", |view| matches!(prompt_kind(view), Some(PromptKind::SecondFactor { .. }))).await;
}

async fn enter_code(cx: &mut VisualTestContext, view: &Entity<Sideport>, code: &str) {
    draw(cx);
    cx.simulate_input(code);
    cx.simulate_keystrokes("enter");

    until(cx, view, "signed in", |view| !view.busy).await;
}

#[gpui::test]
async fn demo_sign_in_answers_password_and_code_prompts_and_lists_the_account(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (view, mut cx) = window(demo_engine(temporary.path()), cx);

    click(&mut cx, "nav:Accounts");
    assert!(rendered(&mut cx, "account:jane@example.com"), "the demo account is listed");

    let password_field = cx.read(|cx| view.read(cx).accounts.password.clone());
    set_input(&mut cx, &password_field, "typed-secret");
    assert!(!cx.read(|cx| format!("{:?}", view.read(cx))).contains("typed-secret"));
    set_input(&mut cx, &password_field, "");

    sign_in(&mut cx, &view, "dev@org.example", "hunter2-secret").await;

    let first = cx.read(|cx| match &view.read(cx).dialog {
        Some(Dialog::Prompt(dialog)) => dialog.prompt.id,
        _ => panic!("code prompt"),
    });
    click(&mut cx, "prompt-request-sms");
    until(&mut cx, &view, "a second code prompt", |view| match &view.dialog {
        Some(Dialog::Prompt(dialog)) => dialog.prompt.id != first,
        _ => false,
    })
    .await;
    assert!(cx.read(|cx| view.read(cx).logs.iter().any(|(_, message)| message.starts_with("Sent a code"))));

    draw(&mut cx);
    cx.simulate_input("123456");

    let debug = cx.read(|cx| format!("{:?}", view.read(cx)));
    assert!(!debug.contains("hunter2-secret") && !debug.contains("123456"), "{debug}");

    cx.simulate_keystrokes("enter");
    until(&mut cx, &view, "signed in", |view| !view.busy).await;

    cx.read(|cx| {
        let view = view.read(cx);
        let account = view.accounts.list.iter().find(|account| account.apple_id == "dev@org.example");
        let account = account.expect("new account");

        assert_eq!(view.error, None);
        assert_eq!(view.status, "Signed in as dev@org.example");
        assert_eq!(view.accounts.list.len(), 2);
        assert_eq!(account.teams.len(), 2);
        assert!(account.has_session && account.remembers_password);
        assert_eq!(view.draft.apple_id.as_deref(), Some("dev@org.example"), "the new account signs");
        assert_eq!(view.accounts.password.read(cx).value().as_ref(), "");
    });
    assert!(rendered(&mut cx, "account:dev@org.example"));

    click(&mut cx, "certificates:jane@example.com");
    until(&mut cx, &view, "certificates", |view| view.accounts.certificates.is_some()).await;
    assert!(rendered(&mut cx, "revoke:5A1D2C3B4E5F6071"));

    click(&mut cx, "revoke:5A1D2C3B4E5F6071");
    assert!(rendered(&mut cx, "dialog-submit-danger"), "revocation asks with destructive styling");
    click(&mut cx, "dialog-submit-danger");
    until(&mut cx, &view, "revocation", |view| !view.busy).await;

    cx.read(|cx| {
        let (_, certificates) = view.read(cx).accounts.certificates.clone().expect("certificates");

        assert_eq!(certificates.len(), 1);
        assert!(!certificates[0].is_ours);
    });

    click(&mut cx, "app-ids:jane@example.com");
    until(&mut cx, &view, "App IDs", |view| view.accounts.app_ids.is_some()).await;
    assert_eq!(cx.read(|cx| view.read(cx).accounts.app_ids.as_ref().map(|(_, ids)| ids.len())), Some(1));

    click(&mut cx, "sign-out:jane@example.com");
    until(&mut cx, &view, "signed out", |view| view.accounts.list.len() == 1).await;
    assert!(cx.read(|cx| view.read(cx).accounts.list.iter().all(|account| account.apple_id != "jane@example.com")));

    let sessions = temporary.path().join("sessions.json");
    fs::write(&sessions, b"{}").expect("sessions");
    cx.update(|_, cx| view.update(cx, |view, _| view.accounts.sessions_path = Some(sessions)));

    click(&mut cx, "import-sessions");
    until(&mut cx, &view, "import result", |view| !view.accounts.importing).await;

    let error = cx.read(|cx| view.read(cx).error.clone()).expect("the demo backend has no session storage");
    assert!(error.contains("demo"), "{error}");
}

#[gpui::test]
async fn demo_apple_id_install_chooses_a_team_and_reports_job_facts(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let source = temporary.path().join("Field Guide.ipa");
    fs::write(&source, b"demo input").expect("source");
    let (view, mut cx) = window(demo_engine(temporary.path()), cx);

    sign_in(&mut cx, &view, "dev@org.example", "hunter2").await;
    enter_code(&mut cx, &view, "123456").await;

    cx.update(|window, cx| view.update(cx, |view, cx| view.load_path(source.clone(), window, cx)));
    until(&mut cx, &view, "inspection", |view| view.app.is_some() && !view.busy).await;
    assert_eq!(cx.read(|cx| view.read(cx).section), Section::App, "opening an app shows the editor");

    click(&mut cx, "mode:Apple ID");
    click(&mut cx, "signing-account:dev@org.example");
    click(&mut cx, "destination:Install on device");
    until(&mut cx, &view, "devices", |view| view.devices.listed).await;
    click(&mut cx, &format!("target-device:{DEMO_IPHONE}"));

    cx.read(|cx| {
        let view = view.read(cx);

        assert_eq!(view.primary_label(), "Install");
        assert_eq!(view.action_blocker(), None);
        assert_eq!(view.devices.selected.as_deref(), Some(DEMO_IPHONE));
    });

    click(&mut cx, "primary-action");
    until(&mut cx, &view, "team prompt", |view| matches!(prompt_kind(view), Some(PromptKind::ChooseTeam { .. }))).await;
    click(&mut cx, "prompt-team:1");
    until(&mut cx, &view, "installation", |view| !view.busy).await;

    cx.read(|cx| {
        let view = view.read(cx);
        let facts = &view.facts;
        let outcome = view.outcome.as_ref().expect("outcome");

        assert_eq!(view.error, None);
        assert_eq!(view.status, "Installed");
        assert_eq!(facts.team.as_ref().map(|team| team.team_id.as_str()), Some("Z9Y8X7W6V5"));
        assert_eq!(facts.bundle_id.as_deref(), Some("com.example.fieldguide"));
        assert_eq!(facts.quota.map(|quota| quota.remaining), Some(8));
        assert_eq!(facts.ttl_days, Some(7));
        assert_eq!(facts.expires, outcome.expires);
        assert_eq!(view.accounts.quota.get("dev@org.example").map(|quota| quota.remaining), Some(8));

        let installation = outcome.installation_id.expect("tracked installation");
        assert!(view.installations.list.iter().any(|item| item.id == installation && item.app_name == "Field Guide"));
    });

    for fact in ["fact:team", "fact:bundle-id", "fact:quota", "fact:expiry"] {
        assert!(rendered(&mut cx, fact), "{fact} is shown beside the progress");
    }
}

#[gpui::test]
async fn apple_id_signing_without_an_account_explains_the_next_step(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let source = temporary.path().join("Test.app");
    common::synthetic_bundle(&source, "com.example.test", "Test", "APPL");
    let (view, mut cx) = window(engine(temporary.path()), cx);

    cx.update(|window, cx| view.update(cx, |view, cx| view.load_path(source.clone(), window, cx)));
    until(&mut cx, &view, "inspection", |view| view.app.is_some() && !view.busy).await;

    click(&mut cx, "mode:Apple ID");
    assert!(rendered(&mut cx, "action-guidance"));
    assert!(rendered(&mut cx, "open-accounts"));
    assert!(cx.read(|cx| view.read(cx).action_blocker()).is_some_and(|reason| reason.contains("Accounts")));

    click(&mut cx, "mode:Unsigned");
    click(&mut cx, "destination:Install on device");
    until(&mut cx, &view, "devices", |view| view.devices.listed).await;
    assert!(cx.read(|cx| view.read(cx).action_blocker()).is_some_and(|reason| reason.contains("only be exported")));

    click(&mut cx, "mode:Apple ID");
    click(&mut cx, "open-accounts");
    assert_eq!(cx.read(|cx| view.read(cx).section), Section::Accounts);
}

#[gpui::test]
async fn an_account_with_several_teams_signs_with_a_default_team_or_asks_at_the_next_job(cx: &mut TestAppContext) {
    const ORG: &str = "dev@org.example";

    let temporary = tempfile::tempdir().expect("tempdir");
    let engine = demo_engine(temporary.path());
    let (view, mut cx) = window(engine.clone(), cx);

    let default_team = |engine: &Engine| {
        let accounts = engine.accounts().expect("accounts");

        accounts.into_iter().find(|account| account.apple_id == ORG).and_then(|account| account.default_team)
    };

    sign_in(&mut cx, &view, ORG, "hunter2").await;
    enter_code(&mut cx, &view, "123456").await;
    assert_eq!(default_team(&engine).as_deref(), Some("A1B2C3D4E5"));

    click(&mut cx, &format!("default-team:{ORG}:Z9Y8X7W6V5"));
    assert_eq!(default_team(&engine).as_deref(), Some("Z9Y8X7W6V5"));
    assert_eq!(cx.read(|cx| view.read(cx).error.clone()), None);

    let listed = cx.read(|cx| view.read(cx).accounts.list.iter().find(|account| account.apple_id == ORG).cloned());
    assert_eq!(listed.and_then(|account| account.default_team).as_deref(), Some("Z9Y8X7W6V5"), "the card follows");

    click(&mut cx, &format!("default-team:{ORG}:ask"));
    assert_eq!(default_team(&engine), None, "the next job asks for the team again");
    assert!(cx.read(|cx| view.read(cx).status.contains("asks which team")));

    assert!(!rendered(&mut cx, "default-team:jane@example.com:ask"), "a single team needs no choice");

    // A choice the engine refuses (the account was signed out meanwhile) is reported.
    futures::executor::block_on(engine.logout(ORG.into())).expect("sign out elsewhere");
    click(&mut cx, &format!("default-team:{ORG}:Z9Y8X7W6V5"));

    let error = cx.read(|cx| view.read(cx).error.clone()).expect("refused choice");
    assert!(error.contains("not signed in"), "{error}");
    assert!(rendered(&mut cx, "error"));
}
