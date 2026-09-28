//! Developer Disk Image catalog, download/cache and mount flow against wiremock and a fake
//! image mounter. These fixtures establish the recovered resolution, caching and mount ordering;
//! they never contact GitHub or Apple, and physical mounting is outside their scope.

use futures::FutureExt;
use futures::future::BoxFuture;
use sl_device::ddi::{self, Catalog, Outcome, Plan, Store};
use sl_device::mounter::{DEVELOPER, ImageMounting, Mounted, PERSONALIZED};
use sl_device::{DeviceError, Result, TssClient};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn catalog(server: &MockServer) -> Catalog {
    Catalog::with_endpoints(server.uri(), server.uri())
}

/// A fake image mounter recording mount calls, optionally already-mounted or holding a manifest.
#[derive(Default)]
struct FakeMounter {
    mounted: Option<Mounted>,
    device_manifest: Option<Vec<u8>>,
    nonce: Vec<u8>,
    identifiers: plist::Dictionary,
    calls: Mutex<Vec<String>>,
}

impl FakeMounter {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }
}

impl ImageMounting for FakeMounter {
    fn mounted(&mut self) -> BoxFuture<'_, Result<Option<Mounted>>> {
        let mounted = self.mounted;

        async move { Ok(mounted) }.boxed()
    }

    fn mount<'a>(
        &'a mut self,
        image_type: &'a str,
        image: &'a [u8],
        _signature: Vec<u8>,
        trust_cache: Option<Vec<u8>>,
        _info: Option<plist::Value>,
    ) -> BoxFuture<'a, Result<()>> {
        self.calls.lock().expect("calls").push(format!(
            "mount {image_type} {} tc={}",
            image.len(),
            trust_cache.is_some()
        ));

        async move { Ok(()) }.boxed()
    }

    fn device_manifest<'a>(&'a mut self, _image: &'a [u8]) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        self.calls.lock().expect("calls").push("device_manifest".into());
        let manifest = self.device_manifest.clone();

        async move { Ok(manifest) }.boxed()
    }

    fn personalization_identifiers(&mut self) -> BoxFuture<'_, Result<plist::Dictionary>> {
        self.calls.lock().expect("calls").push("identifiers".into());
        let identifiers = self.identifiers.clone();

        async move { Ok(identifiers) }.boxed()
    }

    fn nonce(&mut self) -> BoxFuture<'_, Result<Vec<u8>>> {
        self.calls.lock().expect("calls").push("nonce".into());
        let nonce = self.nonce.clone();

        async move { Ok(nonce) }.boxed()
    }

    fn reconnect(&mut self) -> BoxFuture<'_, Result<()>> {
        self.calls.lock().expect("calls").push("reconnect".into());

        async move { Ok(()) }.boxed()
    }
}

fn silent() -> impl Fn(&str) + Send + Sync {
    |_: &str| {}
}

#[tokio::test]
async fn legacy_image_downloads_from_the_raw_mirror_and_caches() {
    let server = MockServer::start().await;

    // No GitHub release exists; the raw mirror serves the dmg and signature directly.
    Mock::given(method("GET"))
        .and(path("/repos/xushuduo/Xcode-iOS-Developer-Disk-Image/releases/tags/16.5"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/mspvirajpatel/Xcode_Developer_Disk_Images/releases/tags/16.5"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pdso/DeveloperDiskImage/master/16.5/DeveloperDiskImage.dmg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"the-image".to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pdso/DeveloperDiskImage/master/16.5/DeveloperDiskImage.dmg.signature"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"the-signature".to_vec()))
        .mount(&server)
        .await;

    let cache = tempfile::tempdir().expect("tempdir");
    let store = Store::new(cache.path());
    let cancel = CancellationToken::new();

    let plan = store.legacy(&catalog(&server), "16.5", &cancel).await.expect("download");
    assert_eq!(plan, Plan::Legacy { image: b"the-image".to_vec(), signature: b"the-signature".to_vec() });

    // A second call is served from the cache; dropping the server would make a fetch fail.
    drop(server);
    let cached = store.legacy(&catalog(&MockServer::start().await), "16.5", &cancel).await.expect("cache");
    assert_eq!(cached, plan);
}

#[tokio::test]
async fn legacy_resolution_falls_back_to_major_minor() {
    let server = MockServer::start().await;

    for repo in ["xushuduo/Xcode-iOS-Developer-Disk-Image", "mspvirajpatel/Xcode_Developer_Disk_Images"] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo}/releases/tags/16.5.1")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo}/releases/tags/16.5")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
    }

    // The exact version is missing from the raw mirror; the major.minor fallback exists.
    Mock::given(method("GET"))
        .and(path("/pdso/DeveloperDiskImage/master/16.5.1/DeveloperDiskImage.dmg"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pdso/DeveloperDiskImage/master/16.5/DeveloperDiskImage.dmg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"image".to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pdso/DeveloperDiskImage/master/16.5/DeveloperDiskImage.dmg.signature"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"sig".to_vec()))
        .mount(&server)
        .await;

    let cache = tempfile::tempdir().expect("tempdir");
    let store = Store::new(cache.path());

    let plan = store.legacy(&catalog(&server), "16.5.1", &CancellationToken::new()).await.expect("fallback");
    assert_eq!(plan, Plan::Legacy { image: b"image".to_vec(), signature: b"sig".to_vec() });
}

#[tokio::test]
async fn legacy_mount_uploads_the_developer_image_when_none_is_mounted() {
    let mut mounter = FakeMounter::default();
    let plan = Plan::Legacy { image: vec![1, 2, 3], signature: vec![4, 5] };

    let outcome = ddi::mount(&mut mounter, &TssClient::default(), &plan, &silent()).await.expect("mount");

    assert_eq!(outcome, Outcome::Mounted);
    assert_eq!(mounter.calls(), vec![format!("mount {DEVELOPER} 3 tc=false")]);
}

#[tokio::test]
async fn an_already_mounted_image_short_circuits() {
    let mut mounter = FakeMounter { mounted: Some(Mounted::Developer), ..FakeMounter::default() };
    let plan = Plan::Legacy { image: vec![0; 10], signature: vec![1] };

    let outcome = ddi::mount(&mut mounter, &TssClient::default(), &plan, &silent()).await.expect("mount");

    assert_eq!(outcome, Outcome::AlreadyMounted(Mounted::Developer));
    assert!(mounter.calls().is_empty(), "nothing is uploaded when an image is already mounted");
}

#[tokio::test]
async fn personalized_mount_uses_the_device_manifest_without_tss() {
    let mut mounter = FakeMounter { device_manifest: Some(vec![9, 9, 9]), ..FakeMounter::default() };
    let plan = Plan::Personalized { image: vec![1; 5], trust_cache: vec![7], build_manifest: vec![] };

    // The TSS endpoint points nowhere; a device-held manifest must avoid contacting it.
    let tss = TssClient::new("http://127.0.0.1:0/never");
    let outcome = ddi::mount(&mut mounter, &tss, &plan, &silent()).await.expect("mount");

    assert_eq!(outcome, Outcome::Mounted);
    assert_eq!(mounter.calls(), vec!["device_manifest".to_string(), format!("mount {PERSONALIZED} 5 tc=true")]);
}

#[tokio::test]
async fn personalized_mount_fetches_a_tss_ticket_when_the_device_has_no_manifest() {
    let server = MockServer::start().await;

    // A minimal TSS success carrying an ApImg4Ticket.
    let mut ticket = plist::Dictionary::new();
    ticket.insert("ApImg4Ticket".into(), plist::Value::Data(vec![0xAA, 0xBB]));
    let mut xml = Vec::new();
    plist::Value::Dictionary(ticket).to_writer_xml(&mut xml).expect("xml");
    let encoded = xml.iter().map(|byte| format!("%{byte:02X}")).collect::<String>();

    Mock::given(method("POST"))
        .and(path("/TSS/controller"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("STATUS=0&MESSAGE=SUCCESS&REQUEST_STRING={encoded}")),
        )
        .mount(&server)
        .await;

    let mut identifiers = plist::Dictionary::new();
    identifiers.insert("BoardId".into(), plist::Value::from(0x08u64));
    identifiers.insert("ChipID".into(), plist::Value::from(0x8030u64));
    identifiers.insert("UniqueChipID".into(), plist::Value::from(0x1122u64));

    let mut mounter = FakeMounter { device_manifest: None, identifiers, nonce: vec![1, 2], ..FakeMounter::default() };
    let plan = Plan::Personalized { image: vec![2; 4], trust_cache: vec![3], build_manifest: build_manifest() };

    let tss = TssClient::new(format!("{}/TSS/controller?action=2", server.uri()));
    let outcome = ddi::mount(&mut mounter, &tss, &plan, &silent()).await.expect("mount");

    assert_eq!(outcome, Outcome::Mounted);
    assert_eq!(
        mounter.calls(),
        vec![
            "device_manifest".to_string(),
            "reconnect".into(),
            "identifiers".into(),
            "nonce".into(),
            format!("mount {PERSONALIZED} 4 tc=true"),
        ]
    );
}

#[tokio::test]
async fn a_cancelled_download_stops() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(404)).mount(&server).await;

    let cache = tempfile::tempdir().expect("tempdir");
    let store = Store::new(cache.path());
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = store.legacy(&catalog(&server), "16.5", &cancel).await;
    assert!(matches!(error, Err(DeviceError::Cancelled)), "{error:?}");
}

/// A minimal DDI build manifest with one trusted, ruled component.
fn build_manifest() -> Vec<u8> {
    let rule = {
        let mut conditions = plist::Dictionary::new();
        conditions.insert("ApRawProductionMode".into(), true.into());
        let mut actions = plist::Dictionary::new();
        actions.insert("EPRO".into(), true.into());
        let mut rule = plist::Dictionary::new();
        rule.insert("Conditions".into(), plist::Value::Dictionary(conditions));
        rule.insert("Actions".into(), plist::Value::Dictionary(actions));
        plist::Value::Dictionary(rule)
    };

    let mut info = plist::Dictionary::new();
    info.insert("RestoreRequestRules".into(), plist::Value::Array(vec![rule]));

    let mut trust_cache = plist::Dictionary::new();
    trust_cache.insert("Trusted".into(), true.into());
    trust_cache.insert("Digest".into(), plist::Value::Data(vec![1, 2, 3]));
    trust_cache.insert("Info".into(), plist::Value::Dictionary(info));

    let mut manifest = plist::Dictionary::new();
    manifest.insert("LoadableTrustCache".into(), plist::Value::Dictionary(trust_cache));

    let mut identity = plist::Dictionary::new();
    identity.insert("ApBoardID".into(), "0x08".into());
    identity.insert("ApChipID".into(), "0x8030".into());
    identity.insert("Manifest".into(), plist::Value::Dictionary(manifest));

    let mut root = plist::Dictionary::new();
    root.insert("BuildIdentities".into(), plist::Value::Array(vec![plist::Value::Dictionary(identity)]));

    let mut bytes = Vec::new();
    plist::Value::Dictionary(root).to_writer_xml(&mut bytes).expect("xml");

    bytes
}
