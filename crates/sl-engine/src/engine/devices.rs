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

/// Developer-disk-image endpoints for [`crate::EngineConfig`]; `None` uses the recovered mirrors.
#[derive(Debug, Clone, Default)]
pub struct DdiConfig {
    pub github_api: Option<String>,
    pub raw_content: Option<String>,
    pub tss_endpoint: Option<String>,
}

impl DdiConfig {
    fn catalog(&self) -> sl_device::Catalog {
        let mut catalog = sl_device::Catalog::default();

        if let Some(api) = &self.github_api {
            catalog.github_api = api.clone();
        }

        if let Some(raw) = &self.raw_content {
            catalog.raw_content = raw.clone();
        }

        catalog
    }

    fn tss(&self) -> sl_device::TssClient {
        match &self.tss_endpoint {
            Some(endpoint) => sl_device::TssClient::new(endpoint.clone()),
            None => sl_device::TssClient::default(),
        }
    }
}

/// Outcome of a Developer Disk Image mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DdiMount {
    /// A developer image was already mounted (nothing was downloaded or uploaded).
    pub already_mounted: bool,
    /// The personalized (iOS 17+) flow was used.
    pub personalized: bool,
}

/// Download (or reuse the cache) and mount the Developer Disk Image matching the device version.
pub(crate) async fn mount_developer_image(
    inner: &Inner,
    context: &crate::job::JobContext,
    udid: &str,
) -> Result<DdiMount> {
    let backend = inner.devices.backend()?;
    let cancel = context.cancellation_token();

    context.stage(crate::Stage::Preparing);

    let values = backend.values(udid).await.map_err(device_error)?;
    let personalized = sl_device::ddi::is_personalized(&values.product_version);

    // A mounted image needs no download. The service connection is not kept open while
    // downloading; the mount below opens a new one.
    let already_mounted = {
        let mut mounter = backend.image_mounter(udid).await.map_err(device_error)?;
        mounter.mounted().await.map_err(device_error)?.is_some()
    };

    if already_mounted {
        context.info("A developer image is already mounted");

        return Ok(DdiMount { already_mounted: true, personalized });
    }

    let store = sl_device::Store::new(inner.data_dir.join("developer-disk-images"));
    let catalog = inner.ddi.catalog();

    context.info(format!("Resolving a Developer Disk Image for iOS {}", values.product_version));

    let plan = if personalized {
        store.personalized(&catalog, &cancel).await
    } else {
        store.legacy(&catalog, &values.product_version, &cancel).await
    }
    .map_err(device_error)?;

    context.stage(crate::Stage::Installing);

    let mut mounter = backend.image_mounter(udid).await.map_err(device_error)?;
    let tss = inner.ddi.tss();
    let log = |message: &str| context.info(message.to_owned());

    let outcome = sl_device::ddi::mount(mounter.as_mut(), &tss, &plan, &log).await.map_err(device_error)?;

    Ok(DdiMount { already_mounted: matches!(outcome, sl_device::Outcome::AlreadyMounted(_)), personalized })
}

/// Enable JIT for an installed bundle: mount the developer image, then launch or attach it
/// through debugserver and detach so it keeps running (recovered `StartJIT`).
pub(crate) async fn enable_jit(
    inner: &Inner,
    context: &crate::job::JobContext,
    udid: &str,
    bundle_id: &str,
    launch: bool,
) -> Result<()> {
    mount_developer_image(inner, context, udid).await?;

    let backend = inner.devices.backend()?;
    let app = backend.app_launch(udid, bundle_id).await.map_err(device_error)?;

    context.info(format!("Enabling JIT for {bundle_id}"));

    let mut debugger = backend.debugserver(udid).await.map_err(device_error)?;

    sl_device::jit::enable_jit(debugger.as_mut(), &app, launch).await.map_err(device_error)
}

/// Repair pairing: unpair, then pair again (the device shows "Trust This Computer?"). Emits
/// progress the GUI can drive (recovered `RepairPairing`).
pub(crate) async fn repair_pairing(inner: &Inner, context: &crate::job::JobContext, udid: &str) -> Result<()> {
    let backend = inner.devices.backend()?;

    context.info("Removing the existing pairing");
    backend.unpair(udid).await.map_err(device_error)?;
    inner.devices.values.lock().remove(udid);

    context.info("Reconnect the device if needed, then confirm \"Trust This Computer?\"");

    let paired = tokio::time::timeout(PAIRING_TIMEOUT, backend.pair(udid)).await.map_err(|_| {
        EngineError::Device("the device did not answer \"Trust This Computer?\" within two minutes".into())
    })?;

    paired.map_err(device_error)?;

    context.info("Pairing repaired");

    Ok(())
}

/// One heartbeat round trip; the interval proves the device (network or tvOS included) is
/// reachable and keeps its services open.
pub(crate) async fn heartbeat(inner: &Inner, udid: &str) -> Result<u64> {
    inner.devices.backend()?.heartbeat(udid).await.map_err(device_error)
}

/// Forward device notifications as job log events until the job is cancelled.
pub(crate) async fn notifications(
    inner: &Inner,
    context: &crate::job::JobContext,
    udid: &str,
    names: Vec<String>,
) -> Result<()> {
    let mut stream = inner.devices.backend()?.observe(udid, &names).await.map_err(device_error)?;
    let cancellation = context.cancellation_token();

    loop {
        let notification = tokio::select! {
            notification = stream.next() => notification,
            () = cancellation.cancelled() => return Ok(()),
        };

        match notification {
            Some(notification) => context.info(notification.map_err(device_error)?),
            None => return Ok(()),
        }
    }
}

pub(crate) fn device_error(error: DeviceError) -> EngineError {
    match error {
        DeviceError::Cancelled => EngineError::Cancelled,
        DeviceError::NotConnected(udid) => EngineError::DeviceUnavailable(format!("device {udid} is not connected")),
        DeviceError::MuxUnavailable(message) => EngineError::DeviceUnavailable(message),
        error @ (DeviceError::Install { .. } | DeviceError::InstallRetry(_)) => EngineError::Install(error.to_string()),
        error @ DeviceError::Unsupported(_) => EngineError::Unsupported(error.to_string()),
        error @ DeviceError::Remote(_) => EngineError::Network(error.to_string()),
        error => EngineError::Device(error.to_string()),
    }
}
