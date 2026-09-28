use super::{Error, Manifest, UpdateEndpoints, UpdatePlan, UpdateStatus, Updater, parse_manifest, swap};
use crate::testsupport::{gzip, patch_all_diff};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn old_and_new() -> (Vec<u8>, Vec<u8>) {
    let old: Vec<u8> = (0u8..=250).cycle().take(2048).collect();
    let mut new = old.clone();
    new.iter_mut().step_by(5).for_each(|byte| *byte = byte.wrapping_add(9));
    new.extend_from_slice(b"newer build payload");

    (old, new)
}

fn manifest_json(version: &str, bytes: &[u8]) -> String {
    let sha = STANDARD.encode(Sha256::digest(bytes));

    format!(r#"{{"version":"{version}","sha256":"{sha}"}}"#)
}

fn endpoints(server: &MockServer) -> UpdateEndpoints {
    UpdateEndpoints {
        info_base: format!("{}/i/", server.uri()),
        diff_base: format!("{}/d/", server.uri()),
        binary_base: format!("{}/b/", server.uri()),
        command: "exe".into(),
        platform: "darwin-arm64".into(),
    }
}

async fn mount_info(server: &MockServer, version: &str, target: &[u8]) {
    Mock::given(method("GET"))
        .and(path("/i/exe/darwin-arm64.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(manifest_json(version, target)))
        .mount(server)
        .await;
}

#[test]
fn a_manifest_needs_a_version_and_a_32_byte_sha256() {
    let good = manifest_json("2.0", b"payload");
    let manifest = parse_manifest(good.as_bytes()).expect("manifest");
    assert_eq!(manifest.version, "2.0");
    assert_eq!(manifest.sha256, <[u8; 32]>::from(Sha256::digest(b"payload")));

    assert!(matches!(parse_manifest(br#"{"sha256":""}"#).expect_err("error expected"), Error::Manifest));
    assert!(matches!(
        parse_manifest(br#"{"version":"2.0","sha256":"!!"}"#).expect_err("error expected"),
        Error::Manifest
    ));
    let short = format!(r#"{{"version":"2.0","sha256":"{}"}}"#, STANDARD.encode([0u8; 16]));
    assert!(matches!(parse_manifest(short.as_bytes()).expect_err("error expected"), Error::Manifest));
}

#[tokio::test]
async fn check_reports_available_and_up_to_date() {
    let (_, new) = old_and_new();
    let server = MockServer::start().await;
    mount_info(&server, "2.0", &new).await;

    let updater = Updater::new(endpoints(&server), "sideport/test").expect("updater");

    match updater.check("1.0").await.expect("check") {
        UpdateStatus::Available { manifest } => {
            assert_eq!(manifest.version, "2.0");
            assert_eq!(manifest.sha256, <[u8; 32]>::from(Sha256::digest(&new)));
        }
        other => panic!("expected an available update, got {other:?}"),
    }

    assert_eq!(updater.check("2.0").await.expect("check"), UpdateStatus::UpToDate { version: "2.0".into() });
}

#[tokio::test]
async fn stage_prefers_the_patch_and_verifies_the_checksum() {
    let (old, new) = old_and_new();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/d/exe/1.0/2.0/darwin-arm64"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(patch_all_diff(&old, &new)))
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("temp dir");
    let current = directory.path().join("component");
    let staged = directory.path().join("component.staged");
    std::fs::write(&current, &old).expect("write current");

    let manifest = Manifest { version: "2.0".into(), sha256: Sha256::digest(&new).into() };
    let updater = Updater::new(endpoints(&server), "sideport/test").expect("updater");
    let plan = updater.stage("1.0", &current, &manifest, &staged, &CancellationToken::new()).await.expect("stage");

    assert_eq!(plan, UpdatePlan::Patched);
    assert_eq!(std::fs::read(&staged).expect("staged"), new);
    assert_eq!(std::fs::read(&current).expect("current"), old, "the current binary is not modified");
}

#[tokio::test]
async fn stage_falls_back_to_the_full_binary_when_the_patch_is_unavailable() {
    let (old, new) = old_and_new();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/d/exe/1.0/2.0/darwin-arm64"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/b/exe/2.0/darwin-arm64.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(gzip(&new)))
        .expect(1)
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("temp dir");
    let current = directory.path().join("component");
    let staged = directory.path().join("component.staged");
    std::fs::write(&current, &old).expect("write current");

    let manifest = Manifest { version: "2.0".into(), sha256: Sha256::digest(&new).into() };
    let updater = Updater::new(endpoints(&server), "sideport/test").expect("updater");
    let plan = updater.stage("1.0", &current, &manifest, &staged, &CancellationToken::new()).await.expect("stage");

    assert_eq!(plan, UpdatePlan::FullDownload);
    assert_eq!(std::fs::read(&staged).expect("staged"), new);
}

#[tokio::test]
async fn stage_rejects_a_full_binary_that_fails_the_checksum() {
    let (old, new) = old_and_new();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/d/exe/1.0/2.0/darwin-arm64"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/b/exe/2.0/darwin-arm64.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(gzip(b"a different binary")))
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("temp dir");
    let current = directory.path().join("component");
    let staged = directory.path().join("component.staged");
    std::fs::write(&current, &old).expect("write current");

    let manifest = Manifest { version: "2.0".into(), sha256: Sha256::digest(&new).into() };
    let updater = Updater::new(endpoints(&server), "sideport/test").expect("updater");
    let error =
        updater.stage("1.0", &current, &manifest, &staged, &CancellationToken::new()).await.expect_err("mismatch");

    assert!(matches!(error, Error::Checksum));
    assert!(!staged.exists(), "no staged file is left after a checksum failure");
}

#[tokio::test]
async fn stage_stops_immediately_when_already_cancelled() {
    let (old, new) = old_and_new();
    let server = MockServer::start().await;

    let directory = tempfile::tempdir().expect("temp dir");
    let current = directory.path().join("component");
    let staged = directory.path().join("component.staged");
    std::fs::write(&current, &old).expect("write current");

    let manifest = Manifest { version: "2.0".into(), sha256: Sha256::digest(&new).into() };
    let updater = Updater::new(endpoints(&server), "sideport/test").expect("updater");
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = updater.stage("1.0", &current, &manifest, &staged, &cancel).await.expect_err("cancelled");
    assert!(matches!(error, Error::Cancelled));
}

#[test]
fn install_replaces_the_target_keeps_a_backup_and_restores() {
    let directory = tempfile::tempdir().expect("temp dir");
    let target = directory.path().join("component");
    let staged = directory.path().join("component.staged");
    std::fs::write(&target, b"version one").expect("write target");
    std::fs::write(&staged, b"version two").expect("write staged");

    let backup = swap::backup(&target).expect("backup");
    assert_eq!(std::fs::read(&backup).expect("backup"), b"version one");

    swap::install(&target, &staged).expect("install");
    assert_eq!(std::fs::read(&target).expect("target"), b"version two");
    assert!(!staged.exists(), "the staged file is moved into place");
    assert!(!directory.path().join(".component.old").exists(), "the .old file is removed on success");

    swap::restore_from_backup(&target, &backup).expect("restore");
    assert_eq!(std::fs::read(&target).expect("restored"), b"version one");
}
