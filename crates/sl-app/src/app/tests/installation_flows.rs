//! Tracked installations through the demo engine and the rendered controls.

use super::*;
use sl_engine::RefreshEvent;

fn installation(view: &Entity<Sideport>, cx: &mut VisualTestContext, id: i64) -> Option<sl_engine::Installation> {
    cx.read(|cx| view.read(cx).installations.list.iter().find(|installation| installation.id == id).cloned())
}

#[gpui::test]
async fn installations_toggle_refresh_now_and_forget_after_confirmation(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let engine = demo_engine(temporary.path());
    let (view, mut cx) = window(engine.clone(), cx);

    click(&mut cx, "nav:Installations");
    assert!(rendered(&mut cx, "installation:1") && rendered(&mut cx, "installation:2"));
    assert!(rendered(&mut cx, "expiry:1"));
    assert_eq!(installation(&view, &mut cx, 2).map(|item| item.consecutive_failures), Some(1));

    click(&mut cx, "auto-refresh:2");
    let stored = engine.installations().expect("installations");
    assert!(stored.iter().any(|item| item.id == 2 && item.auto_refresh), "the toggle is saved");
    assert_eq!(installation(&view, &mut cx, 2).map(|item| item.auto_refresh), Some(true));

    click(&mut cx, "refresh:2");
    assert!(cx.read(|cx| view.read(cx).busy));
    until(&mut cx, &view, "refresh", |view| !view.busy).await;

    let refreshed = installation(&view, &mut cx, 2).expect("installation");
    assert_eq!(cx.read(|cx| view.read(cx).status.clone()), "Refreshed Trailhead");
    assert_eq!((refreshed.last_error, refreshed.consecutive_failures), (None, 0));
    assert!(refreshed.expires_at.is_some_and(|expires| expires > chrono::Utc::now() + chrono::Duration::days(6)));

    click(&mut cx, "forget:1");
    assert!(rendered(&mut cx, "dialog-submit-danger"));
    cx.simulate_keystrokes("enter");
    assert!(installation(&view, &mut cx, 1).is_some(), "Enter does not confirm forgetting");

    click(&mut cx, "dialog-submit-danger");
    assert!(installation(&view, &mut cx, 1).is_none());
    assert_eq!(engine.installations().expect("installations").len(), 1);

    let failed =
        RefreshEvent::Failed { installation_id: 2, app_name: "Trailhead".into(), error: "the device is locked".into() };
    cx.update(|_, cx| view.update(cx, |view, cx| view.refresh_event(failed, cx)));
    assert!(rendered(&mut cx, "refresh-notice"));
    assert_eq!(
        cx.read(|cx| view.read(cx).installations.notice.clone()).as_deref(),
        Some("Refreshing Trailhead failed: the device is locked")
    );
}
