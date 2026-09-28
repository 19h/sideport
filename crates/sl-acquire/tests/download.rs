//! Downloads against a local HTTP server: resume, Range handling, HTML and non-ZIP rejection,
//! digests, bounded retries, enrichment and flipped storage.

use sha1::{Digest as _, Sha1};
use sl_acquire::download::read_plain;
use sl_acquire::{Digest, Downloader, Enrichment, Error, Observer, Request};
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request as Received, Respond, ResponseTemplate};

#[derive(Default)]
struct Recorder {
    logs: Mutex<Vec<String>>,
    progress: Mutex<Vec<(u64, u64)>>,
}

impl Observer for Recorder {
    fn progress(&self, done: u64, total: u64) {
        self.progress.lock().expect("progress").push((done, total));
    }

    fn log(&self, message: &str) {
        self.logs.lock().expect("logs").push(message.into());
    }
}

impl Recorder {
    fn logged(&self, text: &str) -> bool {
        self.logs.lock().expect("logs").iter().any(|line| line.contains(text))
    }
}

/// A small real IPA archive.
fn ipa() -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut bytes);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.add_directory("Payload/Game.app/", options).expect("directory");
    zip.start_file("Payload/Game.app/Info.plist", options).expect("entry");
    zip.write_all(&[7; 5000]).expect("data");
    zip.start_file("Payload/Game.app/Game", options).expect("entry");
    zip.write_all(b"binary").expect("data");
    zip.finish().expect("finish");

    bytes.into_inner()
}

fn downloader() -> Downloader {
    Downloader::new("sideport-test/1")
        .expect("client")
        .with_backoff(Duration::from_millis(10), Duration::from_millis(100))
}

/// Serves `body`, honoring `Range: bytes=N-` with 206 unless `ignore_range`.
struct Ranged {
    body: Vec<u8>,
    ignore_range: bool,
}

impl Respond for Ranged {
    fn respond(&self, request: &Received) -> ResponseTemplate {
        let start = request
            .headers
            .get("range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.trim_end_matches('-').parse::<usize>().ok());

        match start {
            Some(start) if !self.ignore_range => ResponseTemplate::new(206)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(self.body[start..].to_vec()),
            _ => ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(self.body.clone()),
        }
    }
}

async fn serve(server: &MockServer, route: &str, respond: impl Respond + 'static) {
    Mock::given(method("GET")).and(path(route)).respond_with(respond).mount(server).await;
}

fn request<'a>(
    url: &'a str,
    destination: &'a std::path::Path,
    digest: Option<&'a Digest>,
    enrichment: &'a Enrichment,
    flipped: bool,
) -> Request<'a> {
    Request { url, digest, enrichment, destination, flipped }
}

#[tokio::test]
async fn a_clean_download_is_stored_flipped_with_progress_and_user_agent() {
    let server = MockServer::start().await;
    let body = ipa();
    Mock::given(method("GET"))
        .and(path("/app.ipa"))
        .and(header("user-agent", "sideport-test/1"))
        .respond_with(Ranged { body: body.clone(), ignore_range: false })
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("tempdir");
    let destination = directory.path().join("sideloadly-1.ipa");
    let url = format!("{}/app.ipa", server.uri());
    let recorder = Recorder::default();
    let none = Enrichment::default();

    downloader()
        .download(request(&url, &destination, None, &none, true), &recorder, &CancellationToken::new())
        .await
        .expect("download");

    assert_eq!(read_plain(&destination, true).expect("plain"), body);
    assert_ne!(std::fs::read(&destination).expect("stored")[..4], *b"PK\x03\x04", "stored flipped");
    assert_eq!(recorder.progress.lock().expect("progress").last(), Some(&(body.len() as u64, body.len() as u64)));
}

#[tokio::test]
async fn a_partial_file_resumes_with_range_and_a_range_ignoring_server_restarts() {
    let server = MockServer::start().await;
    let body = ipa();
    serve(&server, "/ranged.ipa", Ranged { body: body.clone(), ignore_range: false }).await;
    serve(&server, "/whole.ipa", Ranged { body: body.clone(), ignore_range: true }).await;

    let directory = tempfile::tempdir().expect("tempdir");
    let none = Enrichment::default();

    for (route, expect_range) in [("/ranged.ipa", true), ("/whole.ipa", false)] {
        let destination = directory.path().join(route.trim_start_matches('/'));
        std::fs::write(&destination, &body[..1000]).expect("partial download");

        let url = format!("{}{route}", server.uri());
        downloader()
            .download(request(&url, &destination, None, &none, false), &Recorder::default(), &CancellationToken::new())
            .await
            .expect("resume");

        assert_eq!(std::fs::read(&destination).expect("file"), body, "{route}");

        let requests = server.received_requests().await.expect("requests");
        let ranged =
            requests.iter().any(|request| request.url.path() == route && request.headers.get("range").is_some());
        assert_eq!(ranged, expect_range || route == "/whole.ipa", "{route}");
    }

    let requests = server.received_requests().await.expect("requests");
    assert!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/ranged.ipa")
            .all(|request| request.headers.get("range").is_some_and(|range| range == "bytes=1000-"))
    );
}

#[tokio::test]
async fn html_and_non_zip_responses_fail_without_retrying() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html>sign in</html>", "text/html; charset=utf-8"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/text.ipa"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"plain text, not a zip".to_vec()))
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("tempdir");
    let none = Enrichment::default();

    let html_url = format!("{}/login", server.uri());
    let recorder = Recorder::default();
    let html = downloader()
        .download(
            request(&html_url, &directory.path().join("a.ipa"), None, &none, false),
            &recorder,
            &CancellationToken::new(),
        )
        .await;

    assert!(matches!(&html, Err(Error::Html(message)) if message.contains("<html>sign in</html>")), "{html:?}");

    let text_url = format!("{}/text.ipa", server.uri());
    let text = downloader()
        .download(
            request(&text_url, &directory.path().join("b.ipa"), None, &none, false),
            &Recorder::default(),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(text, Err(Error::NotIpa));
}

/// Serves `first` once, then `second`.
struct Sequence {
    first: Vec<u8>,
    second: Vec<u8>,
    served: Mutex<usize>,
}

impl Respond for Sequence {
    fn respond(&self, _: &Received) -> ResponseTemplate {
        let mut served = self.served.lock().expect("count");
        *served += 1;

        let body = if *served == 1 { self.first.clone() } else { self.second.clone() };

        ResponseTemplate::new(200).set_body_bytes(body)
    }
}

#[tokio::test]
async fn digest_mismatches_download_again_and_give_up_after_three_rejections() {
    let server = MockServer::start().await;
    let body = ipa();
    let mut corrupt = body.clone();
    let last = corrupt.len() - 30;
    corrupt[last] ^= 1;

    serve(&server, "/flaky.ipa", Sequence { first: corrupt.clone(), second: body.clone(), served: Mutex::new(0) })
        .await;
    serve(&server, "/broken.ipa", Ranged { body: corrupt, ignore_range: true }).await;

    let digest = Digest::Sha1(Sha1::digest(&body).into());
    let directory = tempfile::tempdir().expect("tempdir");
    let none = Enrichment::default();

    let flaky = directory.path().join("flaky.ipa");
    let url = format!("{}/flaky.ipa", server.uri());
    let recorder = Recorder::default();
    downloader()
        .download(request(&url, &flaky, Some(&digest), &none, false), &recorder, &CancellationToken::new())
        .await
        .expect("second copy");

    assert_eq!(std::fs::read(&flaky).expect("file"), body);
    assert!(recorder.logged("Hash mismatch, will re-download"));

    let broken = directory.path().join("broken.ipa");
    let url = format!("{}/broken.ipa", server.uri());
    let result = downloader()
        .download(request(&url, &broken, Some(&digest), &none, false), &Recorder::default(), &CancellationToken::new())
        .await;

    assert!(matches!(result, Err(Error::Hash(_))));
    assert!(!broken.exists());

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.iter().filter(|request| request.url.path() == "/broken.ipa").count(), 3);
}

#[tokio::test]
async fn failures_that_copy_nothing_back_off_then_delete_the_file() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/missing.ipa")).respond_with(ResponseTemplate::new(503)).mount(&server).await;

    let directory = tempfile::tempdir().expect("tempdir");
    let destination = directory.path().join("missing.ipa");
    let url = format!("{}/missing.ipa", server.uri());
    let recorder = Recorder::default();
    let none = Enrichment::default();

    let result = downloader()
        .download(request(&url, &destination, None, &none, false), &recorder, &CancellationToken::new())
        .await;

    assert!(matches!(result, Err(Error::Network(_))));
    assert!(recorder.logged("bailing out"));
    assert!(!destination.exists());

    // 10 ms first delay, 100 ms ceiling: waits of 20, 40 and 80 ms, then 160 ms exceeds it.
    assert_eq!(server.received_requests().await.expect("requests").len(), 4);
}

#[tokio::test]
async fn enrichment_adds_metadata_sinf_and_artwork_to_the_archive() {
    let server = MockServer::start().await;
    serve(&server, "/store.ipa", Ranged { body: ipa(), ignore_range: false }).await;
    Mock::given(method("GET"))
        .and(path("/artwork.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artwork bytes".to_vec()))
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("tempdir");
    let destination = directory.path().join("store.ipa");
    let url = format!("{}/store.ipa", server.uri());
    let enrichment = Enrichment {
        metadata: Some("<plist version=\"1.0\"><dict/></plist>".into()),
        sinfs: Some("c2luZiBieXRlcw==".into()),
        artwork: Some(format!("{}/artwork.png", server.uri())),
    };

    downloader()
        .download(request(&url, &destination, None, &enrichment, true), &Recorder::default(), &CancellationToken::new())
        .await
        .expect("enriched");

    let plain = read_plain(&destination, true).expect("plain");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(plain)).expect("archive");

    let mut read = |name: &str| {
        let mut entry = archive.by_name(name).unwrap_or_else(|_| panic!("{name}"));
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).expect("entry");

        bytes
    };

    assert_eq!(read("iTunesMetadata.plist"), b"<plist version=\"1.0\"><dict/></plist>");
    assert_eq!(read("Payload/Game.app/SC_Info/Game.sinf"), b"sinf bytes");
    assert_eq!(read("iTunesArtwork"), b"artwork bytes");
    assert_eq!(read("Payload/Game.app/Game"), b"binary");
}

#[tokio::test]
async fn cancellation_stops_a_download() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow.ipa"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(ipa()).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("tempdir");
    let destination = directory.path().join("slow.ipa");
    let url = format!("{}/slow.ipa", server.uri());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let none = Enrichment::default();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });

    let started = std::time::Instant::now();
    let result =
        downloader().download(request(&url, &destination, None, &none, false), &Recorder::default(), &cancel).await;

    assert_eq!(result, Err(Error::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(3));
}
