//! Read-only probe of the system usbmuxd and attached devices. It lists devices, reads
//! identity values over lockdown and queries AFC staging; it writes nothing to the device.
//! Run with `cargo test -p sl-device --test probe -- --ignored --nocapture`.

use sl_device::install::{Connector, STAGING_ROOT};
use sl_device::{IdeviceConnector, Mux};

#[tokio::test]
#[ignore = "requires usbmuxd and a paired device"]
async fn attached_devices_answer_lockdown_and_afc_queries() {
    let mux = Mux::system().expect("usbmuxd address");
    let attached = mux.attached().await.expect("usbmuxd device list");

    println!("attached: {attached:?}");

    for device in attached {
        let provider = mux.provider(&device.udid, false).await.expect("provider");

        match sl_device::backend::device_values(&provider).await {
            Ok(values) => {
                println!("{}: {values:?} ({:?})", device.udid, sl_device::models::marketing_name(&values.product_type))
            }
            Err(error) => println!("{}: lockdown values unavailable: {error}", device.udid),
        }

        let connector = IdeviceConnector::new(mux.clone());

        match connector.connect(&device.udid, false).await {
            Ok(mut session) => {
                let staged = session.staging().file_size(STAGING_ROOT).await;
                println!("{}: {STAGING_ROOT} present: {:?}", device.udid, staged.map(|size| size.is_some()));
            }
            Err(error) => println!("{}: session unavailable: {error}", device.udid),
        }

        // Read-only: ask the image mounter whether a developer image is mounted (no upload/mount).
        match sl_device::mounter::IdeviceMounter::connect(&mux, &device.udid).await {
            Ok(mut mounter) => {
                use sl_device::mounter::ImageMounting;

                println!("{}: developer image mounted: {:?}", device.udid, mounter.mounted().await);
            }
            Err(error) => println!("{}: image mounter unavailable: {error}", device.udid),
        }
    }
}

/// Histogram of signer identities and profile validation over user apps (no app names).
#[tokio::test]
#[ignore = "requires usbmuxd and a paired device"]
async fn user_app_signing_attributes() {
    use idevice::IdeviceService;
    use idevice::installation_proxy::InstallationProxyClient;

    let mux = Mux::system().expect("usbmuxd address");

    for device in mux.attached().await.expect("devices") {
        let provider = mux.provider(&device.udid, false).await.expect("provider");
        let mut proxy = InstallationProxyClient::connect(&provider).await.expect("installation proxy");
        let apps = proxy.get_apps(Some("User"), None).await.expect("apps");

        let mut histogram = std::collections::BTreeMap::<String, usize>::new();

        for info in apps.values() {
            let info = info.as_dictionary().expect("app info");
            let signer = info.get("SignerIdentity").and_then(plist::Value::as_string).unwrap_or("<none>");
            let validated = info.get("ProfileValidated").and_then(plist::Value::as_boolean);
            let receipt = info.contains_key("ApplicationDSID") || info.contains_key("iTunesMetadata");

            *histogram
                .entry(format!("signer={signer} validated={validated:?} store-metadata={receipt}"))
                .or_default() += 1;
        }

        println!("{histogram:#?}");
    }
}
