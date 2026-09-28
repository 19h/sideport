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
    }
}
