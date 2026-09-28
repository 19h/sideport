//! Requests from other processes through the real engine IPC server.

use super::*;
use sl_engine::ipc::IpcEvent;

#[gpui::test]
async fn a_second_launch_raises_the_window_and_opens_its_file(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let source = temporary.path().join("Test.app");
    common::synthetic_bundle(&source, "com.example.test", "Test", "APPL");

    let engine = engine(temporary.path());
    let server = engine.serve_ipc(Some(0)).expect("serve");
    let client = engine.ipc_client(Some(server.port())).expect("client");
    let (view, mut cx) = window(engine, cx);

    cx.update(|window, cx| view.update(cx, |view, cx| view.listen_for_ipc(window, cx)));

    let file = source.to_string_lossy().into_owned();
    let raised = std::thread::spawn(move || client.raise(Some(&file)));
    assert!(raised.join().expect("client").expect("raise"), "the running app answers success");

    until(&mut cx, &view, "the handed-over app", |view| view.app.as_ref().is_some_and(|app| app.name == "Test")).await;

    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.ipc_event(IpcEvent::Enqueued { installation_id: 7 }, window, cx))
    });
    assert!(cx.read(|cx| view.read(cx).logs.iter().any(|(_, message)| message.contains("installation 7"))));
}
