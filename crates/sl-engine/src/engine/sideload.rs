//! Jobs that need engine state: Apple ID provisioning and device installation.

use super::devices::{self, device_error};
use super::provision::{self, DeviceTarget, ProvisionRequest, Provisioned};
use super::{Inner, acquire, files};
use crate::error::{EngineError, Result};
use crate::job::{JobContext, PromptKind, PromptReply, Stage};
use crate::pipeline::{self, IdentityPlan, Inspected, SigningPlan};
use crate::types::{AppSummary, Installation, JobOutcome, JobSpec, SigningMode, Target};
use chrono::Utc;
use futures::FutureExt;
use futures::future::BoxFuture;
use sl_apple::portal::Platform;
use sl_bundle::{ArchiveKind, ArchiveLimits, BundleArchive, Control, OutputLayout, PackOptions};
use sl_device::install::{self, InstallRequest};
use sl_device::{Backend, Delegate, DeviceWait, FilePackage, Package, PackageReader, Phase, Question};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// Recovered ZipStream compression level (`isign.archive.ZIP_COMPRESSION = 1`).
const STREAM_COMPRESSION: u32 = 1;

/// Streamed ZIP bytes buffered before a channel send, and the channel depth (backpressure).
const STREAM_BLOCK: usize = 256 * 1024;
const STREAM_DEPTH: usize = 8;

pub(super) async fn run(inner: Arc<Inner>, context: JobContext, mut spec: JobSpec) -> Result<JobOutcome> {
    if !acquire::is_remote(&spec.source) {
        return dispatch(inner, context, spec).await;
    }

    // A downloaded source is removed after the job; tracked installations keep their own copy.
    let source = spec.source.to_string_lossy().into_owned();
    let downloaded = acquire::fetch(&inner, &context, &source).await?;

    spec.source = downloaded.clone();
    let outcome = dispatch(inner, context, spec).await;
    let _ = std::fs::remove_file(&downloaded);

    outcome
}

async fn dispatch(inner: Arc<Inner>, context: JobContext, spec: JobSpec) -> Result<JobOutcome> {
    match (&spec.signing, &spec.target) {
        (_, Target::Device { .. }) => install_job(inner, context, spec).await,
        (SigningMode::AppleId { .. }, Target::ExportIpa { .. }) => apple_id_export(inner, context, spec).await,
        _ => pipeline::run_export(context, spec).await,
    }
}

async fn apple_id_export(inner: Arc<Inner>, context: JobContext, spec: JobSpec) -> Result<JobOutcome> {
    pipeline::validate_options(&spec)?;

    let entitlements = spec.options.entitlements.as_deref().map(pipeline::load_entitlements).transpose()?;
    let inspected = pipeline::inspect_job(&context, &spec).await?;

    let provisioned = provision::provision(inner, context.clone(), provision_request(&spec, &inspected, None)?).await?;
    let path = pipeline::output_path(&context, &spec, &inspected.summary).await?;
    let plan = identity_plan(provisioned, entitlements, None);

    context.checkpoint()?;

    let summary = inspected.summary;

    tokio::task::spawn_blocking(move || pipeline::export(context, spec, summary, path, plan))
        .await
        .map_err(|error| EngineError::Other(format!("export worker failed: {error}")))?
}

fn provision_request(spec: &JobSpec, inspected: &Inspected, device: Option<DeviceTarget>) -> Result<ProvisionRequest> {
    let SigningMode::AppleId { apple_id } = &spec.signing else {
        return Err(EngineError::Other("provisioning requires Apple ID signing".into()));
    };

    let summary = &inspected.summary;

    Ok(ProvisionRequest {
        apple_id: apple_id.clone(),
        policy: spec.options.bundle_id.clone(),
        original_bundle_id: inspected.original_bundle_id.clone(),
        current_bundle_id: summary.bundle_id.clone(),
        app_name: summary.name.clone(),
        extensions: summary.extensions.iter().map(|extension| extension.bundle_id.clone()).collect(),
        device,
        tvos_for_apple_tv: spec.options.tvos_for_apple_tv,
        provision_extensions: spec.options.provision_extensions,
    })
}

fn identity_plan(
    provisioned: Provisioned,
    entitlements: Option<plist::Dictionary>,
    device_udid: Option<String>,
) -> SigningPlan {
    SigningPlan::Identity(Box::new(IdentityPlan {
        identity: provisioned.identity,
        profile: provisioned.profile,
        extension_profiles: provisioned.extension_profiles,
        bundle_id: provisioned.bundle_id,
        record_original_id: provisioned.record_original_id,
        device_udid,
        platform: match provisioned.platform {
            Platform::Ios => "iOS",
            Platform::Tvos => "tvOS",
        },
        entitlements,
    }))
}

/// What the install job learned about the signed app, for the outcome and the refresh record.
struct Signed {
    bundle_id: String,
    team_id: Option<String>,
    expires: Option<chrono::DateTime<Utc>>,
}

async fn install_job(inner: Arc<Inner>, context: JobContext, spec: JobSpec) -> Result<JobOutcome> {
    let Target::Device { udid, prefer_network } = spec.target.clone() else {
        return Err(EngineError::Other("device installation requires a device target".into()));
    };

    if spec.signing == SigningMode::Unsigned {
        return Err(EngineError::Unsupported("unsigned bundles cannot be installed; export them instead".into()));
    }

    pipeline::validate_options(&spec)?;

    let backend = inner.devices.backend()?;
    let entitlements = spec.options.entitlements.as_deref().map(pipeline::load_entitlements).transpose()?;
    let inspected = pipeline::inspect_job(&context, &spec).await?;

    let values = devices::values(&inner, &udid).await?;
    context.info(format!("Installing to {} ({} {})", values.name, values.device_class, values.product_version));

    if values.device_class == "AppleTV" && !inspected.summary.device_family.contains(&3) {
        context.warn("The app does not declare Apple TV support (UIDeviceFamily 3).");
    }

    let device = DeviceTarget {
        udid: udid.clone(),
        name: Some(values.name.clone()).filter(|name| !name.is_empty()),
        device_class: Some(values.device_class.clone()),
        os_version: Some(values.product_version.clone()),
    };

    let (plan, signed) = match &spec.signing {
        SigningMode::AppleId { .. } => {
            let request = provision_request(&spec, &inspected, Some(device))?;
            let provisioned = provision::provision(inner.clone(), context.clone(), request).await?;

            let signed = Signed {
                bundle_id: provisioned.bundle_id.clone(),
                team_id: Some(provisioned.team.team_id.clone()),
                expires: Some(provisioned.profile.expiration_date),
            };

            (identity_plan(provisioned, entitlements, Some(udid.clone())), signed)
        }

        SigningMode::AdHoc => {
            context.info("Will sign in ad-hoc mode");

            (
                SigningPlan::AdHoc,
                Signed { bundle_id: inspected.summary.bundle_id.clone(), team_id: None, expires: None },
            )
        }

        SigningMode::Original | SigningMode::Unsigned => (
            SigningPlan::Original,
            Signed { bundle_id: inspected.summary.bundle_id.clone(), team_id: None, expires: None },
        ),
    };

    context.checkpoint()?;

    let staging = inner.data_dir.join("tmp");
    std::fs::create_dir_all(&staging).map_err(|error| EngineError::Storage(error.to_string()))?;

    let prepared = prepare_package(&context, &spec, plan, &staging).await?;
    let bundle_id = prepared.bundle_id.clone().unwrap_or(signed.bundle_id.clone());

    let chunk =
        spec.options.upload_chunk_mib.map_or(install::DEFAULT_CHUNK, |mib| mib.clamp(1, 64) as usize * 1024 * 1024);
    let request = InstallRequest {
        udid: &udid,
        device_name: Some(values.name.as_str()).filter(|name| !name.is_empty()),
        prefer_network,
        bundle_id: &bundle_id,
        chunk,
    };

    let delegate = JobDelegate { context: context.clone(), backend: backend.clone(), phase: AtomicU8::new(0) };
    let connector: &dyn sl_device::Connector = backend.as_ref();

    let installed = install::install(connector, prepared.package.as_ref(), request, &delegate).await;
    drop(prepared);

    installed.map_err(device_error)?;

    context.stage(Stage::Done);
    context.info("Done.");

    let installation_id = match (&spec.signing, spec.options.track_for_refresh) {
        (SigningMode::AppleId { apple_id }, true) => {
            let record = RecordedInstall { apple_id, udid: &udid, device_name: &values.name, bundle_id: &bundle_id };

            Some(record_installation(&inner, &spec, &inspected.summary, &signed, record)?)
        }
        _ => None,
    };

    Ok(JobOutcome { bundle_id, exported_to: None, expires: signed.expires, installation_id })
}

/// A package ready for upload plus the final bundle identifier when it was re-read.
struct Prepared {
    package: Box<dyn Package>,
    bundle_id: Option<String>,
    /// Temporary IPA removed when the install finishes.
    _temporary: Option<tempfile::TempPath>,
}

async fn prepare_package(context: &JobContext, spec: &JobSpec, plan: SigningPlan, staging: &Path) -> Result<Prepared> {
    // A flipped (XOR 0xAA) or zipped-app input is repacked; a plain IPA is uploaded as-is.
    let original_ipa = matches!(plan, SigningPlan::Original)
        && spec.source.is_file()
        && starts_with_zip_header(&spec.source)
        && sl_bundle::inspect(&spec.source, ArchiveLimits::default(), Control::default())
            .is_ok_and(|inspection| inspection.kind == ArchiveKind::Ipa);

    if original_ipa {
        context.info("No metadata changed, will just install");

        let package = FilePackage::open(&spec.source).map_err(device_error)?;

        return Ok(Prepared { package: Box::new(package), bundle_id: None, _temporary: None });
    }

    let context = context.clone();
    let spec = spec.clone();
    let staging = staging.to_owned();

    tokio::task::spawn_blocking(move || prepare_blocking(&context, &spec, &plan, &staging))
        .await
        .map_err(|error| EngineError::Other(format!("signing worker failed: {error}")))?
}

fn starts_with_zip_header(path: &Path) -> bool {
    use std::io::Read;

    let mut header = [0; 4];

    std::fs::File::open(path).and_then(|mut file| file.read_exact(&mut header)).is_ok() && header == *b"PK\x03\x04"
}

fn prepare_blocking(context: &JobContext, spec: &JobSpec, plan: &SigningPlan, staging: &Path) -> Result<Prepared> {
    let cancelled = || context.is_cancelled();
    let progress = |progress: sl_bundle::Progress| context.progress(progress.completed, progress.total);
    let control = Control { is_cancelled: Some(&cancelled), on_progress: Some(&progress) };

    let mut archive =
        BundleArchive::unpack(&spec.source, ArchiveLimits::default(), control).map_err(pipeline::bundle_error)?;

    if !matches!(plan, SigningPlan::Original) {
        context.info("Signing...");
        pipeline::prepare(&mut archive, context, &spec.options, plan, control)?;
    }

    let bundle_id =
        archive.bundle().map_err(pipeline::bundle_error)?.identifier().map_err(pipeline::bundle_error)?.to_owned();
    context.fact(crate::job::Fact::BundleId(bundle_id.clone()));

    if spec.options.stream_upload {
        let estimate = stream_estimate(&archive.bundle_path());
        let package = StreamPackage { archive: Arc::new(archive), estimate, context: context.clone() };

        return Ok(Prepared { package: Box::new(package), bundle_id: Some(bundle_id), _temporary: None });
    }

    context.stage(Stage::Packaging);

    let temporary = tempfile::Builder::new()
        .prefix("sideport-")
        .suffix(".ipa")
        .tempfile_in(staging)
        .map_err(|error| EngineError::Storage(error.to_string()))?
        .into_temp_path();

    archive.save(&temporary, OutputLayout::Ipa, PackOptions::default(), control).map_err(pipeline::bundle_error)?;

    let package = FilePackage::open(&temporary).map_err(device_error)?;

    Ok(Prepared { package: Box::new(package), bundle_id: Some(bundle_id), _temporary: Some(temporary) })
}

/// Uncompressed payload plus per-entry ZIP overhead: an upper estimate for stream progress.
fn stream_estimate(root: &Path) -> u64 {
    walkdir_sizes(root).into_iter().map(|(name, size)| size + 2 * name as u64 + 128).sum::<u64>() + 22
}

fn walkdir_sizes(root: &Path) -> Vec<(usize, u64)> {
    let mut sizes = Vec::new();
    let mut pending = vec![root.to_owned()];

    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };

            let name = entry.file_name().len();

            if metadata.is_dir() {
                pending.push(entry.path());
                sizes.push((name, 0));
            } else {
                sizes.push((name, metadata.len()));
            }
        }
    }

    sizes
}

/// The prepared archive packed on demand into a bounded channel (recovered `ZipStream`). The
/// packer is deterministic, so a resumed upload regenerates the stream and skips the prefix.
struct StreamPackage {
    archive: Arc<BundleArchive>,
    estimate: u64,
    context: JobContext,
}

impl Package for StreamPackage {
    fn size(&self) -> Option<u64> {
        None
    }

    fn estimate(&self) -> Option<u64> {
        Some(self.estimate)
    }

    fn reader(&self, offset: u64) -> BoxFuture<'_, sl_device::Result<Box<dyn PackageReader>>> {
        let archive = self.archive.clone();
        let context = self.context.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(STREAM_DEPTH);

        let producer = tokio::task::spawn_blocking(move || {
            let cancelled = || context.is_cancelled();
            let control = Control { is_cancelled: Some(&cancelled), on_progress: None };
            let mut writer = ChannelWriter { sender, skip: offset, block: Vec::with_capacity(STREAM_BLOCK) };
            let options = PackOptions { compression_level: STREAM_COMPRESSION, force_zip64: false };

            let size = archive
                .write_to(&mut writer, OutputLayout::Ipa, options, control)
                .map_err(|error| error.to_string())?;
            writer.finish().map_err(|error| error.to_string())?;

            Ok(size)
        });

        async move {
            let reader: Box<dyn PackageReader> =
                Box::new(StreamReader { receiver, producer: Some(producer), pending: Vec::new(), position: 0, offset });

            Ok(reader)
        }
        .boxed()
    }
}

/// Discards the first `skip` bytes, then sends blocks; a closed receiver stops the packer.
struct ChannelWriter {
    sender: tokio::sync::mpsc::Sender<Vec<u8>>,
    skip: u64,
    block: Vec<u8>,
}

impl ChannelWriter {
    fn send_block(&mut self) -> std::io::Result<()> {
        let block = std::mem::replace(&mut self.block, Vec::with_capacity(STREAM_BLOCK));

        self.sender.blocking_send(block).map_err(|_| std::io::Error::other("the upload stopped reading the stream"))
    }

    fn finish(&mut self) -> std::io::Result<()> {
        if self.block.is_empty() { Ok(()) } else { self.send_block() }
    }
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let skipped = usize::try_from(self.skip).map_or(bytes.len(), |skip| skip.min(bytes.len()));
        self.skip -= skipped as u64;

        self.block.extend_from_slice(&bytes[skipped..]);

        if self.block.len() >= STREAM_BLOCK {
            self.send_block()?;
        }

        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct StreamReader {
    receiver: tokio::sync::mpsc::Receiver<Vec<u8>>,
    producer: Option<tokio::task::JoinHandle<std::result::Result<u64, String>>>,
    pending: Vec<u8>,
    position: usize,
    offset: u64,
}

impl PackageReader for StreamReader {
    fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> BoxFuture<'a, sl_device::Result<usize>> {
        async move {
            while self.position == self.pending.len() {
                match self.receiver.recv().await {
                    Some(block) => {
                        self.pending = block;
                        self.position = 0;
                    }
                    None => return self.finish().await,
                }
            }

            let count = buffer.len().min(self.pending.len() - self.position);
            buffer[..count].copy_from_slice(&self.pending[self.position..self.position + count]);
            self.position += count;

            Ok(count)
        }
        .boxed()
    }
}

impl StreamReader {
    /// End of stream: surface packer errors, and a stream shorter than the staged prefix.
    async fn finish(&mut self) -> sl_device::Result<usize> {
        let Some(producer) = self.producer.take() else {
            return Ok(0);
        };

        let produced = producer.await.map_err(|error| sl_device::DeviceError::Local(error.to_string()))?;
        let produced = produced.map_err(|error| {
            if error.contains("cancelled") {
                sl_device::DeviceError::Cancelled
            } else {
                sl_device::DeviceError::Local(error)
            }
        })?;

        if produced < self.offset {
            return Err(sl_device::DeviceError::StagedTooLarge { staged: self.offset, expected: produced });
        }

        Ok(0)
    }
}

/// Bridges installation progress, questions and device waits to the job's events and prompts.
struct JobDelegate {
    context: JobContext,
    backend: Arc<dyn Backend>,
    /// 0 = none yet, 1 = uploading, 2 = installing.
    phase: AtomicU8,
}

impl Delegate for JobDelegate {
    fn log(&self, message: &str) {
        self.context.info(message);
    }

    fn progress(&self, phase: Phase, done: u64, total: u64) {
        let (code, stage) = match phase {
            Phase::Uploading => (1, Stage::Uploading),
            Phase::Installing => (2, Stage::Installing),
        };

        if self.phase.swap(code, Ordering::SeqCst) != code {
            self.context.stage(stage);
        }

        self.context.progress(done, total);
    }

    fn is_cancelled(&self) -> bool {
        self.context.is_cancelled()
    }

    fn ask<'a>(&'a self, question: Question) -> BoxFuture<'a, bool> {
        async move {
            let (title, message, confirm_label) = match question {
                Question::Retry { error, reason } => (
                    "Installation failed",
                    format!("Installation fail: {error}.\nPlease make sure that {reason} and then retry."),
                    "Retry",
                ),
                Question::ContinueRetrying { error } => (
                    "Installation failed",
                    format!("Installation fail: {error}.\nDo you want to continue automatic retries?"),
                    "Continue",
                ),
                Question::InstallIssue { error } => {
                    ("Installation issue", format!("There was an issue during installation: {error}"), "Retry")
                }
            };

            let prompt = PromptKind::Confirm {
                title: title.into(),
                message,
                confirm_label: confirm_label.into(),
                destructive: false,
            };

            matches!(self.context.ask(prompt).await, Ok(PromptReply::Confirmed(true)))
        }
        .boxed()
    }

    fn await_device<'a>(&'a self, udid: &'a str, message: String) -> BoxFuture<'a, DeviceWait> {
        async move {
            let prompt = PromptKind::WaitForDevice { udid: udid.into(), device_name: udid.into(), reason: message };

            let returned = async {
                loop {
                    if self.backend.is_attached(udid).await.unwrap_or(false) {
                        return;
                    }

                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            };

            tokio::select! {
                reply = self.context.ask(prompt) => match reply {
                    Ok(PromptReply::Confirmed(true) | PromptReply::DeviceReturned) => DeviceWait::RetryNow,
                    _ => DeviceWait::GiveUp,
                },
                () = returned => DeviceWait::Returned,
            }
        }
        .boxed()
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        tokio::time::sleep(duration).boxed()
    }
}

struct RecordedInstall<'a> {
    apple_id: &'a str,
    udid: &'a str,
    device_name: &'a str,
    bundle_id: &'a str,
}

/// Keep a content-addressed copy of the input and record the installation for refresh.
fn record_installation(
    inner: &Inner,
    spec: &JobSpec,
    summary: &AppSummary,
    signed: &Signed,
    record: RecordedInstall<'_>,
) -> Result<i64> {
    let stored = files::store(inner, &spec.source)?;

    let mut replay = spec.clone();
    replay.source = stored;
    replay.options.track_for_refresh = true;

    let installation = Installation {
        id: 0,
        app_name: summary.name.clone(),
        bundle_id: record.bundle_id.into(),
        original_bundle_id: summary.bundle_id.clone(),
        version: summary.short_version.clone().or_else(|| summary.version.clone()),
        device_udid: record.udid.into(),
        device_name: record.device_name.into(),
        apple_id: record.apple_id.into(),
        team_id: signed.team_id.clone().unwrap_or_default(),
        installed_at: Utc::now(),
        expires_at: signed.expires,
        auto_refresh: true,
        last_error: None,
        consecutive_failures: 0,
        icon_png: summary.icon_png.clone(),
        spec: replay,
    };

    inner.store.record_installation(&installation)
}

/// For tests: the directory holding stored IPA copies.
#[allow(dead_code)]
pub(super) fn stored_files(inner: &Inner) -> PathBuf {
    inner.data_dir.join("files")
}
