//! Local probes of this Mac. They print header names and value lengths only; OTPs, serial numbers
//! and identifiers are never printed. Run with
//! `cargo test -p sl-macos --test probe -- --ignored --nocapture`.

#![cfg(target_os = "macos")]

use sl_apple::auth::AnisetteProvider;
use std::sync::Arc;

#[tokio::test]
#[ignore = "calls AOSKit on this Mac"]
async fn aoskit_produces_recovered_anisette_headers() {
    let provider = sl_apple::anisette::LocalAnisette::new(Arc::new(sl_macos::AosKitSource));
    let headers = AnisetteProvider::headers(&provider, "").await.expect("local anisette");

    for (name, value) in headers.iter() {
        println!("{name}: {} bytes", value.len());
    }

    let second = AnisetteProvider::headers(&provider, "").await.expect("second request");
    assert_eq!(headers.get("X-Mme-Device-Id"), second.get("X-Mme-Device-Id"), "machine identity is stable");
}

#[test]
#[ignore = "queries System Information on this Mac"]
fn this_mac_reports_its_model_version_and_provisioning_udid() {
    let model = sl_macos::hardware_model().expect("hw.model");
    let (version, build) = sl_macos::os_version().expect("SystemVersion.plist");
    let udid = sl_macos::provisioning_udid();

    println!(
        "model {model}, macOS {version} ({build}), provisioning UDID: {} characters",
        udid.map_or(0, |udid| udid.len())
    );
}

#[test]
#[ignore = "calls AOSKit on this Mac"]
fn aoskit_header_names_only() {
    println!("{:?}", sl_macos::aoskit_header_names());
}
