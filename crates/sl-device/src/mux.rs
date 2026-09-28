//! usbmuxd discovery and connection providers.
//!
//! usbmuxd lists each transport separately, so one device can appear once over USB and once
//! over the network. The recovered client connects with `IDEVICE_LOOKUP_USBMUX | NETWORK`,
//! adding `PREFER_NETWORK` for its `-wifi` device entries.

use crate::error::{DeviceError, Result};
use futures::{Stream, StreamExt};
use idevice::provider::UsbmuxdProvider;
use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice, UsbmuxdListenEvent};
use std::net::IpAddr;
use std::pin::Pin;

/// Lockdown label; the recovered client uses `sideloadly`.
pub const LABEL: &str = "sideport";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Link {
    Usb,
    Network(IpAddr),
}

/// A device and every transport usbmuxd currently offers for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub udid: String,
    pub links: Vec<Link>,
}

/// Attach and detach notifications. Detach carries the usbmuxd device ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MuxEvent {
    Attached { udid: String, device_id: u32, link: Link },
    Detached { device_id: u32 },
}

/// Attach/detach notifications as a stream.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<MuxEvent>> + Send>>;

#[derive(Debug, Clone)]
pub struct Mux {
    address: UsbmuxdAddr,
}

impl Mux {
    /// The system usbmuxd, or `USBMUXD_SOCKET_ADDRESS` when set.
    pub fn system() -> Result<Self> {
        let address = UsbmuxdAddr::from_env_var().map_err(|error| DeviceError::MuxUnavailable(error.to_string()))?;

        Ok(Self { address })
    }

    pub fn with_address(address: UsbmuxdAddr) -> Self {
        Self { address }
    }

    pub(crate) async fn connection(&self) -> Result<UsbmuxdConnection> {
        self.address.connect(0).await.map_err(|error| DeviceError::MuxUnavailable(error.to_string()))
    }

    pub async fn devices(&self) -> Result<Vec<UsbmuxdDevice>> {
        let mut connection = self.connection().await?;

        connection.get_devices().await.map_err(DeviceError::from)
    }

    /// Devices grouped by UDID, USB first.
    pub async fn attached(&self) -> Result<Vec<Attached>> {
        Ok(group(self.devices().await?))
    }

    pub async fn is_attached(&self, udid: &str) -> Result<bool> {
        Ok(self.devices().await?.iter().any(|device| device.udid == udid))
    }

    /// A provider for `udid`: USB unless `prefer_network` and a network link exists.
    pub async fn provider(&self, udid: &str, prefer_network: bool) -> Result<UsbmuxdProvider> {
        let devices = self.devices().await?;
        let mut candidates: Vec<_> = devices.into_iter().filter(|device| device.udid == udid).collect();

        candidates.sort_by_key(|device| {
            let network = matches!(device.connection_type, Connection::Network(_));

            network != prefer_network
        });

        let device = candidates.into_iter().next().ok_or_else(|| DeviceError::NotConnected(udid.into()))?;

        Ok(device.to_provider(self.address.clone(), LABEL))
    }

    /// Attach/detach events for as long as the returned stream is polled.
    pub async fn watch(&self) -> Result<EventStream> {
        // idevice's listen stream is not Send, so it runs on a dedicated thread with its own
        // current-thread runtime. The thread stops once the receiver is dropped.
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        let address = self.address.clone();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| DeviceError::Local(error.to_string()))?;

        std::thread::Builder::new()
            .name("sideport-usbmuxd-listen".into())
            .spawn(move || runtime.block_on(listen(address, sender)))
            .map_err(|error| DeviceError::Local(error.to_string()))?;

        Ok(Box::pin(receiver))
    }
}

async fn listen(address: UsbmuxdAddr, sender: futures::channel::mpsc::UnboundedSender<Result<MuxEvent>>) {
    let mut connection = match address.connect(0).await {
        Ok(connection) => connection,
        Err(error) => {
            let _ = sender.unbounded_send(Err(DeviceError::MuxUnavailable(error.to_string())));

            return;
        }
    };

    let mut events = match connection.listen().await {
        Ok(events) => events,
        Err(error) => {
            let _ = sender.unbounded_send(Err(DeviceError::from(error)));

            return;
        }
    };

    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(1), events.next()).await;

        let event = match next {
            Err(_) if sender.is_closed() => return,
            Err(_) => continue,
            Ok(None) => return,
            Ok(Some(event)) => event,
        };

        let event = event.map_err(DeviceError::from).map(|event| match event {
            UsbmuxdListenEvent::Connected(device) => MuxEvent::Attached {
                link: link(&device.connection_type),
                udid: device.udid,
                device_id: device.device_id,
            },
            UsbmuxdListenEvent::Disconnected(device_id) => MuxEvent::Detached { device_id },
        });

        let failed = event.is_err();

        if sender.unbounded_send(event).is_err() || failed {
            return;
        }
    }
}

fn link(connection: &Connection) -> Link {
    match connection {
        Connection::Network(address) => Link::Network(*address),
        Connection::Usb | Connection::Unknown(_) => Link::Usb,
    }
}

/// Group usbmuxd entries by UDID, keeping first-seen order and listing USB before network.
pub fn group(devices: Vec<UsbmuxdDevice>) -> Vec<Attached> {
    let mut grouped: Vec<Attached> = Vec::new();

    for device in devices {
        let link = link(&device.connection_type);

        match grouped.iter_mut().find(|attached| attached.udid == device.udid) {
            Some(attached) if !attached.links.contains(&link) => attached.links.push(link),
            Some(_) => {}
            None => grouped.push(Attached { udid: device.udid, links: vec![link] }),
        }
    }

    for attached in &mut grouped {
        attached.links.sort_by_key(|link| matches!(link, Link::Network(_)));
    }

    grouped
}
