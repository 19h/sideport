use super::*;
use std::io::Write;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn values() -> BTreeMap<String, String> {
    [
        ("X-Apple-I-MD", "fixture-otp"),
        ("X-Apple-I-MD-M", "fixture-machine-token"),
        ("X-Mme-Device-Id", "fixture-device"),
        ("X-MMe-Client-Info", "<Mac14,1> <macOS;15.0;24A335> <com.apple.AuthKit/1>"),
        ("X-Apple-Locale", "en_US"),
        ("X-Apple-I-TimeZone", "UTC"),
        ("X-Apple-I-SRL-NO", "fixture-serial"),
    ]
    .into_iter()
    .map(|(name, value)| (name.into(), value.into()))
    .collect()
}

fn time(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("fixture timestamp")
}

#[test]
fn validated_headers_preserve_extensions_and_redact_debug() {
    let mut values = values();
    values.insert("X-Apple-I-MD-RINFO".into(), "17106176".into());
    let mut headers = AnisetteHeaders::new(values).expect("headers");
    headers.set_time(time(1_800_000_001));

    assert_eq!(headers.get("x-apple-i-md-rinfo"), Some("17106176"));
    assert_eq!(headers.get(CLIENT_TIME), Some("2027-01-15T08:00:01Z"));
    assert_eq!(headers.description(), "Mac14,1 with serial number fixture-serial running macOS 15.0 24A335");
    assert!(!format!("{headers:?}").contains("fixture-machine-token"));
    assert_eq!(login_hash("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}

#[test]
fn malformed_and_duplicate_headers_are_rejected() {
    let mut missing = values();
    missing.remove("X-Apple-I-MD");

    let mut duplicate = values();
    duplicate.insert("x-apple-i-md".into(), "other".into());

    let mut injected = values();
    injected.insert("X-Apple-I-MD".into(), "token\r\nAuthorization: leaked".into());

    let mut unrelated = values();
    unrelated.insert("Authorization".into(), "Bearer secret".into());

    let mut oversized = values();
    oversized.insert("X-Apple-Extra".into(), "a".repeat(4097));

    for values in [missing, duplicate, injected, unrelated, oversized] {
        assert!(AnisetteHeaders::new(values).is_err());
    }

    let duplicate_json = br#"{"X-Apple-I-MD":"first","X-Apple-I-MD":"second"}"#;
    assert!(serde_json::from_slice::<WireHeaders>(duplicate_json).is_err());
    assert!(serde_json::from_slice::<WireHeaders>(br#"{"X-Apple-I-MD":42}"#).is_err());
}

#[test]
fn cache_reuse_stops_at_bucket_guard_expiry_and_clock_changes() {
    let instant = Instant::now();
    let entry = CacheEntry {
        login_hash: "hash".into(),
        received: instant,
        client_time: time(1_800_000_010),
        headers: AnisetteHeaders::new(values()).expect("headers"),
    };

    assert!(reusable(&entry, time(1_800_000_011), instant + Duration::from_secs(1)));
    assert!(reusable(&entry, time(1_800_000_026), instant + Duration::from_secs(16)));

    for (seconds, elapsed) in [(1_800_000_027, 17), (1_800_000_030, 20), (1_800_000_009, 1), (1_800_000_011, 30)] {
        assert!(!reusable(&entry, time(seconds), instant + Duration::from_secs(elapsed)));
    }

    assert!(!reusable(&entry, time(1_800_000_011), instant - Duration::from_secs(1)));
}

#[tokio::test]
async fn remote_query_refresh_and_cache_are_exercised_by_http() {
    let server = MockServer::start().await;
    let hash = login_hash("fixture@example.test");
    Mock::given(method("GET"))
        .and(path("/anisette"))
        .and(query_param("u", hash.clone()))
        .and(query_param("key", "configured-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(values()))
        .expect(2)
        .mount(&server)
        .await;

    let provider =
        RemoteAnisette::new(&format!("{}/anisette?key=configured-key&u=obsolete", server.uri())).expect("provider");
    let instant = Instant::now();
    let first = provider.fetch(&hash, || (time(1_800_000_010), instant)).await.expect("first fetch");
    let reused =
        provider.fetch(&hash, || (time(1_800_000_011), instant + Duration::from_secs(1))).await.expect("cache");
    assert_eq!(first.get("X-Apple-I-MD"), reused.get("X-Apple-I-MD"));
    assert_eq!(reused.get(CLIENT_TIME), Some("2027-01-15T08:00:11Z"));

    provider.fetch(&hash, || (time(1_800_000_027), instant + Duration::from_secs(17))).await.expect("guard refresh");
    assert!(!format!("{provider:?}").contains("configured-key"));

    let requests = server.received_requests().await.expect("request log");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].url.query_pairs().filter(|(name, _)| name == "u").count(), 1);
}

#[tokio::test]
async fn concurrent_clones_share_refresh_and_user_change_invalidates_cache() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(values()))
        .expect(2)
        .mount(&server)
        .await;

    let provider = RemoteAnisette::new(&server.uri()).expect("provider");
    let instant = Instant::now();
    let requests = (0..16).map(|_| {
        let provider = provider.clone();

        async move { provider.fetch("first-user", || (time(1_800_000_010), instant)).await }
    });

    for result in futures::future::join_all(requests).await {
        result.expect("shared fetch");
    }

    provider.fetch("second-user", || (time(1_800_000_011), instant)).await.expect("new user");
}

#[tokio::test]
async fn invalid_json_response_status_size_and_redirect_do_not_populate_cache() {
    for (response, expected) in [
        (ResponseTemplate::new(200).set_body_string("secret-invalid-json"), "anisette JSON"),
        (ResponseTemplate::new(503).set_body_string("secret-service-detail"), "HTTP 503"),
        (ResponseTemplate::new(200).set_body_string("a".repeat(MAX_RESPONSE_BYTES + 1)), "exceeds"),
        (ResponseTemplate::new(302).insert_header("Location", "http://127.0.0.1:1/secret-redirect"), "HTTP 302"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(response).expect(2).mount(&server).await;
        let provider = RemoteAnisette::new(&format!("{}?key=secret-query", server.uri())).expect("provider");

        for _ in 0..2 {
            let error = provider.headers(None).await.expect_err("failed response");
            let message = format!("{error} {error:?}");

            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("secret-"), "{message}");
        }
    }
}

#[test]
fn unsupported_urls_and_embedded_credentials_are_rejected() {
    for endpoint in
        ["file:///tmp/anisette", "not a URL", "https://user:password@example.test", "https://example.test/#secret"]
    {
        assert!(RemoteAnisette::new(endpoint).is_err());
    }
}

#[tokio::test]
async fn compressed_response_limit_applies_after_decompression() {
    let mut compressor = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    compressor.write_all(&vec![b'a'; MAX_RESPONSE_BYTES + 1]).expect("compressed fixture");
    let compressed = compressor.finish().expect("gzip fixture");
    assert!(compressed.len() < MAX_RESPONSE_BYTES);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Encoding", "gzip").set_body_bytes(compressed))
        .expect(1)
        .mount(&server)
        .await;

    let provider = RemoteAnisette::new(&server.uri()).expect("provider");
    assert!(matches!(provider.headers(None).await, Err(Error::ResponseTooLarge(MAX_RESPONSE_BYTES))));
}

#[tokio::test]
async fn cancelled_fetch_releases_lock_and_does_not_cache_partial_response() {
    let server = MockServer::start().await;
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = attempts.clone();
    Mock::given(method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            let template = ResponseTemplate::new(200).set_body_json(values());

            if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                template.set_delay(Duration::from_millis(500))
            } else {
                template
            }
        })
        .expect(2)
        .mount(&server)
        .await;

    let provider = RemoteAnisette::new(&server.uri()).expect("provider");
    let cancelled_provider = provider.clone();
    let task = tokio::spawn(async move { cancelled_provider.headers(None).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while attempts.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first request started");

    task.abort();
    assert!(task.await.expect_err("aborted request").is_cancelled());
    let headers = tokio::time::timeout(Duration::from_secs(2), provider.headers(None))
        .await
        .expect("lock released")
        .expect("retry");

    assert_eq!(headers.get("X-Apple-I-MD"), Some("fixture-otp"));
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
}
