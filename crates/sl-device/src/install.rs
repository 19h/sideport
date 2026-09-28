//! Resumable staging upload and the recovered installation retry policy.
//!
//! Recovered `Impactor.install_app`/`install_app_do` (docs/DEVICE.md):
//!
//! 1. Connect lockdown, AFC and installation proxy. The package path is
//!    `PublicStaging/<bundle id>`; the first attempt removes a previous upload.
//! 2. Remove `StagingDirectory` (errors ignored), create `PublicStaging`.
//! 3. Resume: a staged file larger than the package is removed and the attempt retried; an
//!    equal one is kept; a smaller one is appended to from its current size.
//! 4. Verify the staged size, then install with `{CFBundleIdentifier}`.
//! 5. Retry automatically four times for interrupted connections, size problems, extraction
//!    failures and failures that leave the staged package; wait up to 180 s for a vanished
//!    device, then ask; ask for pairing states; ask before continuing after the budget.
//!
//! The operations are traits so the policy runs against the `idevice` backend and fakes.

use crate::error::{DeviceError, Recovery, Result};
use futures::FutureExt;
use futures::future::BoxFuture;
use std::path::Path;
use std::time::Duration;

/// Recovered AFC upload chunk (`UPLOAD_CHUNK_SIZE_DFLT`), 1 MiB.
pub const DEFAULT_CHUNK: usize = 1024 * 1024;

/// Recovered automatic-retry budget before asking to continue.
pub const AUTOMATIC_RETRIES: u32 = 4;

/// Recovered wait for a vanished device before asking the user.
pub const DEVICE_WAIT: Duration = Duration::from_secs(180);

pub const STAGING_ROOT: &str = "PublicStaging";
pub const LEGACY_STAGING: &str = "StagingDirectory";

/// AFC operations used by staging. `open_append` opens one file for subsequent writes.
pub trait Staging: Send {
    /// Size of a file, or `None` when it does not exist.
    fn file_size<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<Option<u64>>>;
    /// Remove a file; a missing file is not an error.
    fn remove<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>>;
    fn remove_all<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>>;
    fn make_dirs<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>>;
    fn open_append<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>>;
    fn write<'a>(&'a mut self, data: &'a [u8]) -> BoxFuture<'a, Result<()>>;
    fn close(&mut self) -> BoxFuture<'_, Result<()>>;
}

/// Installation-proxy status reported while installing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallStatus {
    pub status: String,
    pub percent: Option<u64>,
}

/// One connection to the device for one attempt.
pub trait Session: Send {
    fn staging(&mut self) -> &mut dyn Staging;

    /// Install `package` (relative to the AFC root), reporting status until `Complete`.
    fn install<'a>(
        &'a mut self,
        package: &'a str,
        bundle_id: &'a str,
        status: &'a (dyn Fn(InstallStatus) + Send + Sync),
    ) -> BoxFuture<'a, Result<()>>;
}

/// Opens sessions and observes attachment.
pub trait Connector: Send + Sync {
    fn connect<'a>(&'a self, udid: &'a str, prefer_network: bool) -> BoxFuture<'a, Result<Box<dyn Session>>>;

    fn is_attached<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<bool>>;
}

/// Bytes to upload. The recovered client uploads a temporary IPA or regenerates a
/// deterministic ZIP stream; both can be read again from any offset.
pub trait Package: Send + Sync {
    /// The exact size when known before reading (files); `None` for generated streams.
    fn size(&self) -> Option<u64>;

    /// A progress estimate for streams whose exact size is not known in advance.
    fn estimate(&self) -> Option<u64> {
        self.size()
    }

    /// A reader positioned at `offset`. A stream that ends before `offset` reports
    /// [`DeviceError::StagedTooLarge`], as the recovered `FileTooBig`.
    fn reader(&self, offset: u64) -> BoxFuture<'_, Result<Box<dyn PackageReader>>>;
}

pub trait PackageReader: Send {
    /// Read up to `buffer.len()` bytes; 0 means end of data.
    fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> BoxFuture<'a, Result<usize>>;
}

/// A regular file whose size is fixed when opened; a changed size fails the upload.
#[derive(Debug)]
pub struct FilePackage {
    path: std::path::PathBuf,
    size: u64,
}

impl FilePackage {
    pub fn open(path: &Path) -> Result<Self> {
        let size = std::fs::metadata(path).map_err(local_error)?.len();

        Ok(Self { path: path.to_owned(), size })
    }
}

impl Package for FilePackage {
    fn size(&self) -> Option<u64> {
        Some(self.size)
    }

    fn reader(&self, offset: u64) -> BoxFuture<'_, Result<Box<dyn PackageReader>>> {
        async move {
            use tokio::io::AsyncSeekExt;

            let mut file = tokio::fs::File::open(&self.path).await.map_err(local_error)?;
            let length = file.metadata().await.map_err(local_error)?.len();

            if length != self.size {
                return Err(DeviceError::Local("the package changed size during installation".into()));
            }

            file.seek(std::io::SeekFrom::Start(offset)).await.map_err(local_error)?;

            let reader: Box<dyn PackageReader> = Box::new(FileReader { file });

            Ok(reader)
        }
        .boxed()
    }
}

struct FileReader {
    file: tokio::fs::File,
}

impl PackageReader for FileReader {
    fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> BoxFuture<'a, Result<usize>> {
        async move {
            use tokio::io::AsyncReadExt;

            self.file.read(buffer).await.map_err(local_error)
        }
        .boxed()
    }
}

fn local_error(error: std::io::Error) -> DeviceError {
    DeviceError::Local(error.to_string())
}

/// Phases reported to the delegate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Uploading,
    Installing,
}

/// Why the installer is waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// Retry after the user fixes the stated condition.
    Retry { error: String, reason: String },
    /// Continue after the automatic-retry budget is spent.
    ContinueRetrying { error: String },
    /// The installation proxy reported an error; retry?
    InstallIssue { error: String },
}

/// Outcome of waiting for a vanished device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceWait {
    Returned,
    RetryNow,
    GiveUp,
}

/// Front-end hooks for one installation.
pub trait Delegate: Send + Sync {
    fn log(&self, message: &str);
    fn progress(&self, phase: Phase, done: u64, total: u64);
    fn is_cancelled(&self) -> bool;
    /// `true` retries; `false` stops.
    fn ask<'a>(&'a self, question: Question) -> BoxFuture<'a, bool>;
    /// Ask the user to reconnect the device. Implementations complete early with
    /// [`DeviceWait::Returned`] when the device reappears.
    fn await_device<'a>(&'a self, udid: &'a str, message: String) -> BoxFuture<'a, DeviceWait>;
    /// Sleep between polls; tests replace real time.
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;
}

#[derive(Debug, Clone)]
pub struct InstallRequest<'a> {
    pub udid: &'a str,
    pub device_name: Option<&'a str>,
    pub prefer_network: bool,
    pub bundle_id: &'a str,
    pub chunk: usize,
}

/// Staging path for a bundle identifier.
pub fn package_path(bundle_id: &str) -> String {
    format!("{STAGING_ROOT}/{bundle_id}")
}

/// Upload and install `package`, applying the recovered retry policy.
pub async fn install(
    connector: &dyn Connector,
    package: &dyn Package,
    request: InstallRequest<'_>,
    delegate: &dyn Delegate,
) -> Result<()> {
    let reason = if request.prefer_network {
        "the device and computer are on the same network and the device screen is on"
    } else {
        "the cable is connected firmly"
    };

    let mut attempts = 0u32;
    let mut automatic = AUTOMATIC_RETRIES;
    let mut ask = true;
    let mut last_error: Option<DeviceError> = None;

    loop {
        if delegate.is_cancelled() {
            return Err(DeviceError::Cancelled);
        }

        if let Some(error) = &last_error {
            let message = error.to_string();

            let proceed = if ask {
                delegate.ask(Question::Retry { error: message, reason: reason.into() }).await
            } else if automatic > 0 {
                delegate.log("Retrying automatically");
                automatic -= 1;
                delegate.sleep(Duration::from_millis(500)).await;

                true
            } else {
                automatic = AUTOMATIC_RETRIES;

                delegate.ask(Question::ContinueRetrying { error: message }).await
            };

            if !proceed {
                return Err(last_error.take().unwrap_or(DeviceError::Cancelled));
            }

            delegate.log(&format!("Retrying, attempt {}...", attempts + 1));
        }

        let outcome = attempt(connector, package, &request, attempts > 0, delegate).await;

        let error = match outcome {
            Ok(()) => return Ok(()),
            Err(DeviceError::Cancelled) => return Err(DeviceError::Cancelled),
            Err(error) => error,
        };

        attempts += 1;
        delegate.log(&format!("FAILED: {error}"));

        match error.recovery() {
            Recovery::Automatic => ask = false,

            Recovery::AskUser => ask = true,

            Recovery::AwaitDevice => {
                if !wait_for_device(connector, &request, &error, reason, delegate).await? {
                    return Err(error);
                }

                automatic = AUTOMATIC_RETRIES;
                ask = false;
            }

            Recovery::Fatal => return Err(error),
        }

        last_error = Some(error);
    }
}

/// Recovered device wait: retry at once if the device is attached again, else poll for up to
/// 180 s, then ask. The recovered loop kept polling after the device returned; this stops.
async fn wait_for_device(
    connector: &dyn Connector,
    request: &InstallRequest<'_>,
    error: &DeviceError,
    reason: &str,
    delegate: &dyn Delegate,
) -> Result<bool> {
    if connector.is_attached(request.udid).await.unwrap_or(false) {
        delegate.log("Device disappeared but is now available, will retry/resume");

        return Ok(true);
    }

    let name = request.device_name.unwrap_or(request.udid);
    delegate.log(&format!("Waiting for the device {name} to re-appear (will wait for at most 3 minutes)"));

    let polls = DEVICE_WAIT.as_secs();

    for _ in 0..polls {
        if delegate.is_cancelled() {
            return Err(DeviceError::Cancelled);
        }

        if connector.is_attached(request.udid).await.unwrap_or(false) {
            delegate.log("Device disappeared but is now available, will retry/resume");

            return Ok(true);
        }

        delegate.sleep(Duration::from_secs(1)).await;
    }

    delegate.log("Device disappeared. Waiting for the device to appear via Wi-Fi or USB to continue sideloading.");

    let message = format!(
        "Installation fail: {error}.\nWaiting for the device to re-appear.\n\nOr please make sure that {reason} and then retry."
    );

    match delegate.await_device(request.udid, message).await {
        DeviceWait::Returned | DeviceWait::RetryNow => Ok(true),
        DeviceWait::GiveUp => Err(DeviceError::Cancelled),
    }
}

async fn attempt(
    connector: &dyn Connector,
    package: &dyn Package,
    request: &InstallRequest<'_>,
    reupload: bool,
    delegate: &dyn Delegate,
) -> Result<()> {
    delegate.log("Connecting...");

    let mut session = connector.connect(request.udid, request.prefer_network).await?;
    let path = package_path(request.bundle_id);

    if !reupload {
        delegate.log("Cleaning up...");
        session.staging().remove(&path).await?;
    }

    let message = if request.prefer_network { "Uploading, please keep device screen ON!" } else { "Uploading..." };
    delegate.log(message);

    let _ = session.staging().remove_all(LEGACY_STAGING).await;
    session.staging().make_dirs(STAGING_ROOT).await?;

    let produced = upload(session.staging(), package, &path, request.chunk, delegate).await?;
    let staged = session.staging().file_size(&path).await?.unwrap_or(0);

    if staged != produced {
        session.staging().remove(&path).await?;

        return Err(DeviceError::SizeMismatch { expected: produced, actual: staged });
    }

    delegate.log("Installing...");
    delegate.progress(Phase::Installing, 0, 100);

    let report = |status: InstallStatus| {
        let percent = status.percent.unwrap_or(0);

        delegate.progress(Phase::Installing, percent, 100);
        delegate.log(&format!(
            "Installing {}%: {}",
            status.percent.map_or("???".into(), |value| value.to_string()),
            status.status
        ));
    };

    let installed = session.install(&path, request.bundle_id, &report).await;

    match installed {
        Ok(()) => {
            delegate.progress(Phase::Installing, 100, 100);

            Ok(())
        }

        Err(DeviceError::Install { name, description, detail }) => {
            let error = DeviceError::Install { name, description, detail };

            if error.is_terminal_install() {
                return Err(error);
            }

            let staged = session.staging().file_size(&path).await.ok().flatten().is_some();

            if staged || error.is_extraction_failure() {
                delegate.log(&format!("Got error: {error}. IPA file exists, retrying"));

                return Err(DeviceError::InstallRetry(error.to_string()));
            }

            if delegate.ask(Question::InstallIssue { error: error.to_string() }).await {
                return Err(DeviceError::InstallRetry(error.to_string()));
            }

            Err(error)
        }

        Err(error) => Err(error),
    }
}

/// Resumable append upload: keep an equal staged file, remove a larger one, append otherwise.
/// Returns the package size (for streams, the number of bytes produced).
pub async fn upload(
    staging: &mut dyn Staging,
    package: &dyn Package,
    path: &str,
    chunk: usize,
    delegate: &dyn Delegate,
) -> Result<u64> {
    let staged = staging.file_size(path).await?.unwrap_or(0);

    if let Some(total) = package.size() {
        if staged > total {
            delegate.log("File exists and is bigger than original! Will remove and retry");
            staging.remove(path).await?;

            return Err(DeviceError::StagedTooLarge { staged, expected: total });
        }

        if staged == total {
            delegate.progress(Phase::Uploading, total, total);

            return Ok(total);
        }
    }

    let mut reader = match package.reader(staged).await {
        Err(DeviceError::StagedTooLarge { staged, expected }) => {
            delegate.log("File exists and is bigger than original! Will remove and retry");
            staging.remove(path).await?;

            return Err(DeviceError::StagedTooLarge { staged, expected });
        }
        reader => reader?,
    };

    delegate.progress(Phase::Uploading, staged, package.estimate().unwrap_or(0));
    staging.open_append(path).await?;

    let written = write_from(staging, reader.as_mut(), package, staged, chunk.max(1), delegate).await;
    let closed = staging.close().await;

    let produced = written?;
    closed?;

    delegate.progress(Phase::Uploading, produced, produced);

    Ok(produced)
}

/// Write the reader's bytes in exact `chunk` blocks (the last one shorter), as the recovered
/// stream upload does.
async fn write_from(
    staging: &mut dyn Staging,
    reader: &mut dyn PackageReader,
    package: &dyn Package,
    mut offset: u64,
    chunk: usize,
    delegate: &dyn Delegate,
) -> Result<u64> {
    let mut buffer = vec![0; chunk];

    loop {
        if delegate.is_cancelled() {
            return Err(DeviceError::Cancelled);
        }

        let mut filled = 0;

        while filled < chunk {
            let read = reader.read(&mut buffer[filled..]).await?;

            if read == 0 {
                break;
            }

            filled += read;
        }

        if filled == 0 {
            break;
        }

        staging.write(&buffer[..filled]).await?;
        offset += filled as u64;

        if package.size().is_some_and(|size| offset > size) {
            return Err(DeviceError::Local("the package grew during installation".into()));
        }

        let estimate = package.estimate().unwrap_or(0).max(offset);
        delegate.progress(Phase::Uploading, offset, estimate);

        if filled < chunk {
            break;
        }
    }

    if package.size().is_some_and(|size| offset != size) {
        return Err(DeviceError::Local("the package ended before its recorded size".into()));
    }

    Ok(offset)
}
