//! Download sources typed into the App section or opened through the URL scheme.

use super::*;
use sl_bundle::{OutputLayout, PackOptions};

/// Serve a packed IPA of a synthetic app; the runtime must outlive the server.
fn served_ipa(root: &Path) -> (tokio::runtime::Runtime, wiremock::MockServer) {
    let bundle = root.join("Remote.app");
    common::synthetic_bundle(&bundle, "com.example.remote", "Remote", "APPL");

    let archive = BundleArchive::unpack(&bundle, ArchiveLimits::default(), Control::default()).expect("unpack");
    let ipa = root.join("remote.ipa");
    archive.save(&ipa, OutputLayout::Ipa, PackOptions::default(), Control::default()).expect("pack");

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let server = runtime.block_on(wiremock::MockServer::start());
    let response = wiremock::ResponseTemplate::new(200).set_body_bytes(fs::read(&ipa).expect("IPA"));
    runtime
        .block_on(wiremock::Mock::given(wiremock::matchers::path("/remote.ipa")).respond_with(response).mount(&server));

    let page = wiremock::ResponseTemplate::new(200).set_body_raw("<html></html>", "text/html");
    runtime.block_on(wiremock::Mock::given(wiremock::matchers::path("/page")).respond_with(page).mount(&server));

    (runtime, server)
}

#[gpui::test]
async fn links_typed_or_opened_by_the_system_download_and_open_the_app(cx: &mut TestAppContext) {
    let temporary = tempfile::tempdir().expect("tempdir");
    let (_runtime, server) = served_ipa(temporary.path());
    let (view, mut cx) = window(engine(temporary.path()), cx);

    assert!(rendered(&mut cx, "download-link"), "the empty App section offers links");

    let link = format!("sideloadly:?dn=Remote.ipa&xs={}/remote.ipa", server.uri());
    let input = cx.read(|cx| view.read(cx).fields.link.clone());
    set_input(&mut cx, &input, &link);
    click(&mut cx, "download-link");

    until(&mut cx, &view, "the downloaded app", |view| view.app.as_ref().is_some_and(|app| app.name == "Remote")).await;

    let (sender, batches) = async_channel::unbounded();
    cx.update(|window, cx| view.update(cx, |view, cx| view.listen_for_urls(batches, window, cx)));

    let bundle = temporary.path().join("Remote.app");
    let file_url = url::Url::from_file_path(&bundle).expect("file URL").to_string();
    sender.try_send(vec![file_url]).expect("open file");

    until(&mut cx, &view, "the opened file", |view| view.app.as_ref().is_some_and(|app| app.path == bundle)).await;

    // A web page instead of an IPA is refused, as the recovered client does.
    sender.try_send(vec![format!("{}/page", server.uri())]).expect("open link");
    until(&mut cx, &view, "the refused download", |view| !view.busy && view.error.is_some()).await;
}
