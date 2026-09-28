//! A fake device layer implementing [`sl_device::Backend`].
//!
//! It keeps AFC staging in memory, records each installed package's bytes, and can interrupt
//! one upload after a byte count to exercise resume. It models the recovered installation
//! contract, not installd's validation.

use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use parking_lot::{Mutex, MutexGuard};
use sl_device::install::{InstallStatus, Session, Staging};
use sl_device::{
    Attached, Backend, Connector, DeviceError, DeviceValues, EventStream, InstalledApp, LineStream, Link, Result,
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// One installed package: its path, requested bundle identifier and staged bytes.
#[derive(Debug, Clone)]
pub struct InstalledPackage {
    pub bundle_id: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct DeviceState {
    pub values: DeviceValues,
    pub attached: bool,
    pub files: BTreeMap<String, Vec<u8>>,
    pub installed: Vec<InstalledPackage>,
    pub apps: Vec<InstalledApp>,
    pub profiles: Vec<Vec<u8>>,
    pub removed_profiles: Vec<String>,
    /// Lines served by the syslog relay; the stream then stays open.
    pub syslog: Vec<String>,
    pub uninstalled: Vec<String>,
    pub paired: bool,
    /// Interrupt the next upload after this many bytes (the partial data stays staged).
    pub interrupt_after: Option<usize>,
    pub connections: usize,
    open: Option<String>,
    written: usize,
}

#[derive(Debug, Clone)]
pub struct FakeDevice {
    pub udid: String,
    state: Arc<Mutex<DeviceState>>,
}

impl FakeDevice {
    pub fn new(udid: &str, name: &str, device_class: &str, product_type: &str, version: &str) -> Self {
        let values = DeviceValues {
            name: name.into(),
            product_type: product_type.into(),
            product_version: version.into(),
            build_version: "FIXTURE".into(),
            device_class: device_class.into(),
        };

        let state = DeviceState { values, attached: true, paired: true, ..DeviceState::default() };

        Self { udid: udid.into(), state: Arc::new(Mutex::new(state)) }
    }

    pub fn iphone(udid: &str) -> Self {
        Self::new(udid, "Fixture iPhone", "iPhone", "iPhone15,2", "17.5")
    }

    pub fn state(&self) -> MutexGuard<'_, DeviceState> {
        self.state.lock()
    }

    pub fn backend(&self) -> Arc<dyn Backend> {
        Arc::new(self.clone())
    }

    fn check(&self, udid: &str) -> Result<()> {
        let state = self.state.lock();

        if udid != self.udid || !state.attached {
            return Err(DeviceError::NotConnected(udid.into()));
        }

        if !state.paired {
            return Err(DeviceError::NotPaired);
        }

        Ok(())
    }
}

impl Connector for FakeDevice {
    fn connect<'a>(&'a self, udid: &'a str, _: bool) -> BoxFuture<'a, Result<Box<dyn Session>>> {
        async move {
            self.check(udid)?;
            self.state.lock().connections += 1;

            let session: Box<dyn Session> = Box::new(FakeSession { device: self.clone() });

            Ok(session)
        }
        .boxed()
    }

    fn is_attached<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<bool>> {
        let attached = udid == self.udid && self.state.lock().attached;

        async move { Ok(attached) }.boxed()
    }
}

impl Backend for FakeDevice {
    fn attached(&self) -> BoxFuture<'_, Result<Vec<Attached>>> {
        let attached = self.state.lock().attached;
        let devices =
            if attached { vec![Attached { udid: self.udid.clone(), links: vec![Link::Usb] }] } else { Vec::new() };

        async move { Ok(devices) }.boxed()
    }

    fn values<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<DeviceValues>> {
        async move {
            self.check(udid)?;

            Ok(self.state.lock().values.clone())
        }
        .boxed()
    }

    fn watch(&self) -> BoxFuture<'_, Result<EventStream>> {
        async { Ok(Box::pin(futures::stream::pending()) as EventStream) }.boxed()
    }

    fn apps<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<Vec<InstalledApp>>> {
        async move {
            self.check(udid)?;

            Ok(self.state.lock().apps.clone())
        }
        .boxed()
    }

    fn uninstall<'a>(&'a self, udid: &'a str, bundle_id: &'a str) -> BoxFuture<'a, Result<()>> {
        async move {
            self.check(udid)?;

            let mut state = self.state.lock();
            state.apps.retain(|app| app.bundle_id != bundle_id);
            state.uninstalled.push(bundle_id.into());

            Ok(())
        }
        .boxed()
    }

    fn profiles<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<Vec<Vec<u8>>>> {
        async move {
            self.check(udid)?;

            Ok(self.state.lock().profiles.clone())
        }
        .boxed()
    }

    fn remove_profile<'a>(&'a self, udid: &'a str, uuid: &'a str) -> BoxFuture<'a, Result<()>> {
        async move {
            self.check(udid)?;
            self.state.lock().removed_profiles.push(uuid.into());

            Ok(())
        }
        .boxed()
    }

    fn syslog<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<LineStream>> {
        async move {
            self.check(udid)?;

            let lines = self.state.lock().syslog.clone();
            let stream = futures::stream::iter(lines.into_iter().map(Ok)).chain(futures::stream::pending());

            Ok(Box::pin(stream) as LineStream)
        }
        .boxed()
    }

    fn pair<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<()>> {
        async move {
            if udid != self.udid {
                return Err(DeviceError::NotConnected(udid.into()));
            }

            self.state.lock().paired = true;

            Ok(())
        }
        .boxed()
    }
}

struct FakeSession {
    device: FakeDevice,
}

impl Staging for FakeSession {
    fn file_size<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<Option<u64>>> {
        let size = self.device.state.lock().files.get(path).map(|bytes| bytes.len() as u64);

        async move { Ok(size) }.boxed()
    }

    fn remove<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>> {
        self.device.state.lock().files.remove(path);

        async { Ok(()) }.boxed()
    }

    fn remove_all<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>> {
        self.device.state.lock().files.retain(|name, _| !name.starts_with(path));

        async { Ok(()) }.boxed()
    }

    fn make_dirs<'a>(&'a mut self, _: &'a str) -> BoxFuture<'a, Result<()>> {
        async { Ok(()) }.boxed()
    }

    fn open_append<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>> {
        let mut state = self.device.state.lock();
        state.files.entry(path.into()).or_default();
        state.open = Some(path.into());

        async { Ok(()) }.boxed()
    }

    fn write<'a>(&'a mut self, data: &'a [u8]) -> BoxFuture<'a, Result<()>> {
        let result = (|| {
            let mut state = self.device.state.lock();
            let path = state.open.clone().ok_or(DeviceError::Protocol("no open file".into()))?;

            if let Some(limit) = state.interrupt_after {
                let room = limit.saturating_sub(state.written);

                if room < data.len() {
                    state.files.get_mut(&path).expect("open file").extend_from_slice(&data[..room]);
                    state.interrupt_after = None;
                    state.written = 0;
                    state.open = None;

                    return Err(DeviceError::Interrupted("fixture interruption".into()));
                }
            }

            state.written += data.len();
            state.files.get_mut(&path).expect("open file").extend_from_slice(data);

            Ok(())
        })();

        async move { result }.boxed()
    }

    fn close(&mut self) -> BoxFuture<'_, Result<()>> {
        self.device.state.lock().open = None;

        async { Ok(()) }.boxed()
    }
}

impl Session for FakeSession {
    fn staging(&mut self) -> &mut dyn Staging {
        self
    }

    fn install<'a>(
        &'a mut self,
        package: &'a str,
        bundle_id: &'a str,
        status: &'a (dyn Fn(InstallStatus) + Send + Sync),
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            let bytes = {
                let mut state = self.device.state.lock();
                state.files.remove(package).ok_or_else(|| DeviceError::Install {
                    name: "PackageNotFound".into(),
                    description: None,
                    detail: None,
                })?
            };

            for (state, percent) in [("ExtractingPackage", 10), ("InstallingApplication", 70), ("Complete", 100)] {
                status(InstallStatus { status: state.into(), percent: Some(percent) });
            }

            let mut state = self.device.state.lock();
            state.installed.push(InstalledPackage { bundle_id: bundle_id.into(), bytes });
            state.apps.retain(|app| app.bundle_id != bundle_id);
            state.apps.push(InstalledApp {
                bundle_id: bundle_id.into(),
                name: bundle_id.into(),
                version: None,
                developer: true,
            });

            Ok(())
        }
        .boxed()
    }
}
