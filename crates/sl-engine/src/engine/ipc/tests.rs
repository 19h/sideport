use super::*;
use crate::engine::{DeviceBackend, Engine, EngineConfig, MacTargetSetting};
use crate::ipc::{IpcClient, PollReply};
use crate::types::{AppOptions, Installation, JobSpec, SigningMode, Target};
use std::io::{Read, Write};

fn engine(directory: &std::path::Path) -> Engine {
    // A fake device layer without the recorded device: a queued refresh fails without hardware.
    let device = sl_testkit::FakeDevice::iphone("00000000-0000000000000000");

    let config = EngineConfig {
        data_dir: Some(directory.into()),
        disable_scheduler: true,
        file_secrets: true,
        device_backend: Some(DeviceBackend(device.backend())),
        mac_target: MacTargetSetting::Disabled,
        ..EngineConfig::default()
    };

    Engine::new(config).expect("engine")
}

/// Send a raw request and return the status and body.
fn raw(port: u16, request: &str) -> (u16, String) {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    stream.write_all(request.as_bytes()).expect("request");

    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("reply");

    let status = reply.split(' ').nth(1).and_then(|code| code.parse().ok()).expect("status");
    let body = reply.split_once("\r\n\r\n").map(|(_, body)| body.to_owned()).unwrap_or_default();

    (status, body)
}

fn get(port: u16, target: &str, token: Option<&str>) -> (u16, String) {
    let token = token.map(|token| format!("{TOKEN_HEADER}: {token}\r\n")).unwrap_or_default();

    raw(port, &format!("GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\n{token}Connection: close\r\n\r\n"))
}

fn installation() -> Installation {
    Installation {
        id: 0,
        app_name: "App".into(),
        bundle_id: "com.example.app.TEAM123456".into(),
        original_bundle_id: "com.example.app".into(),
        version: Some("1".into()),
        device_udid: "00008030-001A2D0C0E38802E".into(),
        device_name: "Phone".into(),
        apple_id: "fixture@example.test".into(),
        team_id: "TEAM123456".into(),
        installed_at: Utc::now(),
        expires_at: Some(Utc::now()),
        auto_refresh: true,
        last_error: None,
        consecutive_failures: 0,
        icon_png: None,
        spec: JobSpec {
            source: "/nonexistent/App.ipa".into(),
            target: Target::Device { udid: "00008030-001A2D0C0E38802E".into(), prefer_network: false },
            signing: SigningMode::AdHoc,
            options: AppOptions::default(),
        },
    }
}

#[test]
fn clients_raise_restart_and_enqueue_through_the_recovered_routes() {
    let directory = tempfile::tempdir().expect("tempdir");
    let engine = engine(directory.path());
    let events = engine.subscribe_ipc();

    let server = engine.serve_ipc(Some(0)).expect("serve");
    let client = IpcClient::new(directory.path(), server.port()).expect("client");

    assert!(matches!(engine.serve_ipc(Some(server.port())), Err(EngineError::Ipc(_))), "one server per port");

    assert!(client.raise(Some("/Users/fixture/App.ipa")).expect("raise"));
    assert!(client.raise(None).expect("raise"));
    client.restart(Some("update")).expect("restart");

    let received: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    assert_eq!(
        received,
        [
            IpcEvent::Raise { file: Some("/Users/fixture/App.ipa".into()) },
            IpcEvent::Raise { file: None },
            IpcEvent::Restart { message: Some("update".into()) },
        ]
    );

    let token = crate::ipc::token(directory.path(), false).expect("token");
    let port = server.port();

    assert_eq!(get(port, "/enqueue?id=abc", Some(&token)), (200, "ERROR 0 expected integer".into()));
    assert_eq!(get(port, "/enqueue", Some(&token)), (200, "ERROR 0 unexpected EOF".into()));
    assert_eq!(get(port, "/enqueue?id=77", Some(&token)), (500, "Task enqueue failOK".into()));
    assert!(client.enqueue(77).is_err());

    let id = engine.inner.store.record_installation(&installation()).expect("installation");
    client.enqueue(id).expect("enqueue");
    assert_eq!(events.try_recv().ok(), Some(IpcEvent::Enqueued { installation_id: id }));

    // The queued refresh runs in the serving process; the recorded device is absent.
    let failed = (0..100).find_map(|_| {
        std::thread::sleep(Duration::from_millis(50));

        let stored = engine.inner.store.installation(id).expect("read").expect("installation");
        (stored.consecutive_failures > 0).then_some(stored)
    });
    assert!(failed.is_some(), "the queued refresh ran and recorded its failure");
}

#[test]
fn requests_without_the_token_a_loopback_host_or_get_are_refused() {
    let directory = tempfile::tempdir().expect("tempdir");
    let engine = engine(directory.path());
    let server = engine.serve_ipc(Some(0)).expect("serve");
    let port = server.port();
    let token = crate::ipc::token(directory.path(), false).expect("token");

    assert_eq!(get(port, "/raise", None).0, 403, "no token");
    assert_eq!(get(port, "/raise", Some("wrong")).0, 403, "wrong token");
    assert_eq!(get(port, "/raise", Some(&token)), (200, "success".into()));
    assert_eq!(get(port, "/missing", Some(&token)).0, 404);

    let rebinding = format!("GET /raise HTTP/1.1\r\nHost: attacker.example:{port}\r\n{TOKEN_HEADER}: {token}\r\n\r\n");
    assert_eq!(raw(port, &rebinding).0, 403, "a foreign Host is refused");

    let preflight =
        format!("OPTIONS /raise HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: https://attacker.example\r\n\r\n");
    assert_eq!(raw(port, &preflight).0, 405, "preflights are refused");

    let post = format!("POST /enqueue?id=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{TOKEN_HEADER}: {token}\r\n\r\n");
    assert_eq!(raw(port, &post).0, 405);
}

#[test]
fn polls_receive_left_messages_version_mismatches_and_bye() {
    let directory = tempfile::tempdir().expect("tempdir");
    let engine = engine(directory.path());
    let server = engine.serve_ipc(Some(0)).expect("serve");
    let client = IpcClient::new(directory.path(), server.port()).expect("client");
    let token = crate::ipc::token(directory.path(), false).expect("token");

    engine.leave_message("refreshed");
    assert_eq!(client.poll().expect("poll"), PollReply::Message("refreshed".into()));

    for message in 0..12 {
        engine.leave_message(format!("message {message}"));
    }
    assert_eq!(client.poll().expect("poll"), PollReply::Message("message 2".into()), "ten newest are kept");

    assert_eq!(get(server.port(), "/poll?v=0.0.1", Some(&token)), (200, "Version mismatch".into()));

    while engine.inner.ipc.receiver.try_recv().is_ok() {}

    let waiting = std::thread::spawn(move || client.poll());
    std::thread::sleep(Duration::from_millis(300));

    drop(server);
    assert_eq!(waiting.join().expect("poller").expect("poll"), PollReply::Bye);
}

#[test]
fn sign_in_tokens_reach_only_a_waiting_sign_in() {
    let directory = tempfile::tempdir().expect("tempdir");
    let engine = engine(directory.path());
    let server = engine.serve_ipc(Some(0)).expect("serve");
    let port = server.port();

    let (status, body) = get(port, "/tokens?user_token=early", None);
    assert_eq!((status, body.as_str()), (500, SIGN_IN_ERROR), "nothing waits for a token");

    let replaced = engine.await_sign_in_token();
    let waiting = engine.await_sign_in_token();
    assert!(matches!(futures::executor::block_on(replaced), Err(EngineError::Cancelled)));

    assert_eq!(get(port, "/tokens", None).0, 500, "an empty token does not complete the sign-in");

    let (status, body) = get(port, "/tokens?user_token=abc%2B123", None);
    assert_eq!((status, body.as_str()), (200, SIGNED_IN_PAGE));
    assert_eq!(futures::executor::block_on(waiting).expect("token"), "abc+123");

    assert_eq!(get(port, "/tokens?user_token=again", None).0, 500, "the waiter is consumed");
}
