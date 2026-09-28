//! Device discovery, snapshots and device utilities over an [`sl_device::Backend`].

use super::Inner;
use crate::error::{EngineError, Result};
use crate::types::{Connection, DeviceApp, DeviceInfo, DeviceProfile};
use futures::StreamExt;
use parking_lot::Mutex;
use sl_device::{Backend, DeviceError, DeviceValues, Link};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long a pairing request waits for "Trust This Computer?".
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);

/// A device layer for [`crate::EngineConfig`]; the default is the system usbmuxd.
#[derive(Clone)]
pub struct DeviceBackend(pub Arc<dyn Backend>);

impl fmt::Debug for DeviceBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("DeviceBackend").finish_non_exhaustive()
    }
}

pub(crate) struct Devices {
    backend: Option<Arc<dyn Backend>>,
    unavailable: Option<String>,
    values: Mutex<BTreeMap<String, DeviceValues>>,
    snapshot: Mutex<Option<Vec<DeviceInfo>>>,
    watching: AtomicBool,
}

impl fmt::Debug for Devices {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Devices").field("available", &self.backend.is_some()).finish_non_exhaustive()
    }
}

impl Devices {
    pub(crate) fn new(configured: Option<DeviceBackend>) -> Self {
        let (backend, unavailable) = match configured {
            Some(DeviceBackend(backend)) => (Some(backend), None),
            None => match sl_device::IdeviceBackend::system() {
                Ok(backend) => (Some(Arc::new(backend) as Arc<dyn Backend>), None),
                Err(error) => (None, Some(error.to_string())),
            },
        };

        Self {
            backend,
            unavailable,
            values: Mutex::new(BTreeMap::new()),
            snapshot: Mutex::new(None),
            watching: AtomicBool::new(false),
        }
    }

    pub(crate) fn backend(&self) -> Result<Arc<dyn Backend>> {
        self.backend.clone().ok_or_else(|| {
            EngineError::DeviceUnavailable(self.unavailable.clone().unwrap_or_else(|| "no device service".into()))
        })
    }

    pub(crate) fn snapshot(&self) -> Option<Vec<DeviceInfo>> {
        self.snapshot.lock().clone()
    }
}

/// Current devices with lockdown values; devices that refuse a session are listed unpaired.
pub(crate) async fn list(inner: &Inner) -> Result<Vec<DeviceInfo>> {
    let backend = inner.devices.backend()?;
    let attached = backend.attached().await.map_err(device_error)?;

    let mut devices = Vec::with_capacity(attached.len());

    for device in attached {
        let cached = inner.devices.values.lock().get(&device.udid).cloned();

        let values = match cached {
            Some(values) => Some(values),
            None => match backend.values(&device.udid).await {
                Ok(values) => {
                    inner.devices.values.lock().insert(device.udid.clone(), values.clone());

                    Some(values)
                }
                Err(_) => None,
            },
        };

        let connections = device
            .links
            .iter()
            .map(|link| match link {
                Link::Usb => Connection::Usb,
                Link::Network(_) => Connection::Network,
            })
            .fold(Vec::new(), |mut connections, connection| {
                if !connections.contains(&connection) {
                    connections.push(connection);
                }

                connections
            });

        let paired = values.is_some();
        let values = values.unwrap_or_default();

        devices.push(DeviceInfo {
            model_name: sl_device::models::marketing_name(&values.product_type).map(str::to_owned),
            udid: device.udid,
            name: values.name,
            product_type: values.product_type,
            os_version: values.product_version,
            device_class: values.device_class,
            connections,
            paired,
        });
    }

    if let Some(mac) = inner.mac() {
        devices.push(DeviceInfo {
            udid: mac.udid.clone(),
            name: mac.name.clone(),
            product_type: mac.model.clone(),
            model_name: Some("This Mac".into()),
            os_version: mac.os_version.clone(),
            device_class: "Mac".into(),
            connections: Vec::new(),
            paired: true,
        });
    }

    *inner.devices.snapshot.lock() = Some(devices.clone());

    Ok(devices)
}

/// Lockdown values for one device, refreshed from the device.
pub(crate) async fn values(inner: &Inner, udid: &str) -> Result<DeviceValues> {
    let backend = inner.devices.backend()?;
    let values = backend.values(udid).await.map_err(device_error)?;

    inner.devices.values.lock().insert(udid.into(), values.clone());

    Ok(values)
}

/// Start the attach/detach watcher once. It republishes the device list after every event and
/// reconnects to usbmuxd after failures. It stops when the last engine handle is dropped.
pub(crate) fn watch(inner: &Arc<Inner>) {
    if inner.devices.backend.is_none() || inner.devices.watching.swap(true, Ordering::SeqCst) {
        return;
    }

    let weak = Arc::downgrade(inner);
    let handle = inner.runtime.handle.clone();

    handle.spawn(async move {
        loop {
            let Some(inner) = weak.upgrade() else {
                return;
            };

            let Ok(backend) = inner.devices.backend() else {
                return;
            };

            let stream = backend.watch().await;
            drop(inner);

            if let Ok(mut events) = stream {
                if !publish(&weak).await {
                    return;
                }

                while let Some(event) = events.next().await {
                    if event.is_err() || !publish(&weak).await {
                        break;
                    }
                }
            }

            if !publish(&weak).await {
                return;
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

/// Refresh the snapshot and send it to subscribers; `false` once nobody can receive it.
async fn publish(weak: &std::sync::Weak<Inner>) -> bool {
    let Some(inner) = weak.upgrade() else {
        return false;
    };

    let devices = list(&inner).await.unwrap_or_default();

    inner.publish_devices(&devices)
}

pub(crate) async fn apps(inner: &Inner, udid: &str) -> Result<Vec<DeviceApp>> {
    let apps = inner.devices.backend()?.apps(udid).await.map_err(device_error)?;

    Ok(apps
        .into_iter()
        .map(|app| DeviceApp {
            bundle_id: app.bundle_id,
            name: app.name,
            version: app.version,
            is_developer_app: app.developer,
        })
        .collect())
}

pub(crate) async fn profiles(inner: &Inner, udid: &str) -> Result<Vec<DeviceProfile>> {
    let raw = inner.devices.backend()?.profiles(udid).await.map_err(device_error)?;

    let mut profiles: Vec<_> = raw
        .iter()
        .filter_map(|bytes| sl_codesign::ProvisioningProfile::parse(bytes).ok())
        .map(|profile| DeviceProfile {
            app_id: profile.bundle_id().map(str::to_owned),
            team_id: profile.team_identifiers.first().cloned(),
            expires: Some(profile.expiration_date),
            is_free: profile.local_provision,
            uuid: profile.uuid,
            name: profile.name,
        })
        .collect();

    profiles.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(profiles)
}

pub(crate) async fn uninstall(inner: &Inner, udid: &str, bundle_id: &str) -> Result<()> {
    inner.devices.backend()?.uninstall(udid, bundle_id).await.map_err(device_error)
}

pub(crate) async fn remove_profile(inner: &Inner, udid: &str, uuid: &str) -> Result<()> {
    inner.devices.backend()?.remove_profile(udid, uuid).await.map_err(device_error)
}

pub(crate) async fn pair(inner: &Inner, udid: &str) -> Result<()> {
    let backend = inner.devices.backend()?;

    let paired = tokio::time::timeout(PAIRING_TIMEOUT, backend.pair(udid)).await.map_err(|_| {
        EngineError::Device("the device did not answer \"Trust This Computer?\" within two minutes".into())
    })?;

    paired.map_err(device_error)?;
    inner.devices.values.lock().remove(udid);

    Ok(())
}

/// Forward syslog lines as job log events until the job is cancelled. Lines not containing
/// `filter` (case-insensitive) are skipped.
pub(crate) async fn syslog(
    inner: &Inner,
    context: &crate::job::JobContext,
    udid: &str,
    filter: Option<&str>,
) -> Result<()> {
    let mut lines = inner.devices.backend()?.syslog(udid).await.map_err(device_error)?;
    let filter = filter.map(str::to_lowercase).filter(|filter| !filter.is_empty());
    let cancellation = context.cancellation_token();

    loop {
        let line = tokio::select! {
            line = lines.next() => line,
            () = cancellation.cancelled() => return Ok(()),
        };

        let Some(line) = line else {
            return Ok(());
        };

        let line = line.map_err(device_error)?;

        if filter.as_ref().is_none_or(|filter| line.to_lowercase().contains(filter)) {
            context.info(line);
        }
    }
}

pub(crate) fn device_error(error: DeviceError) -> EngineError {
    match error {
        DeviceError::Cancelled => EngineError::Cancelled,
        DeviceError::NotConnected(udid) => EngineError::DeviceUnavailable(format!("device {udid} is not connected")),
        DeviceError::MuxUnavailable(message) => EngineError::DeviceUnavailable(message),
        error @ (DeviceError::Install { .. } | DeviceError::InstallRetry(_)) => EngineError::Install(error.to_string()),
        error => EngineError::Device(error.to_string()),
    }
}
