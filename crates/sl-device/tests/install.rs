//! The recovered upload/installation policy against a fault-injecting fake device.
//! These fixtures establish resume, retry and prompt behavior; physical-device acceptance is
//! outside their scope.

use futures::FutureExt;
use futures::executor::block_on;
use futures::future::BoxFuture;
use sl_device::install::{self, InstallStatus, Session, Staging};
use sl_device::{
    Connector, Delegate, DeviceError, DeviceWait, InstallRequest, Package, PackageReader, Phase, Question,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const UDID: &str = "00008030-001A2D0C0E38802E";
const BUNDLE: &str = "com.example.app";
const CHUNK: usize = 1000;

#[derive(Debug, Clone)]
enum Fault {
    /// The next connection attempt fails.
    Connect(DeviceError),
    /// After `bytes` more bytes are stored, the write fails (the partial chunk is kept).
    WriteBreaks { bytes: usize, error: DeviceError },
    /// The next write stores fewer bytes than sent without reporting an error.
    WriteDrops(usize),
    /// The next installation fails; `keep_staged` leaves the package in place.
    Install { error: DeviceError, keep_staged: bool },
}

#[derive(Debug, Default)]
struct State {
    files: BTreeMap<String, Vec<u8>>,
    directories: Vec<String>,
    faults: VecDeque<Fault>,
    open: Option<String>,
    attached: bool,
    /// Attach again after this many `is_attached` polls.
    returns_after: Option<usize>,
    polls: usize,
    operations: Vec<String>,
    installs: usize,
}

#[derive(Clone, Default)]
struct FakeDevice {
    state: Arc<Mutex<State>>,
}

impl FakeDevice {
    fn new(faults: impl IntoIterator<Item = Fault>) -> Self {
        let state = State { faults: faults.into_iter().collect(), attached: true, ..State::default() };

        Self { state: Arc::new(Mutex::new(state)) }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("fake device state")
    }

    fn staged(&self) -> Option<Vec<u8>> {
        self.state().files.get(&install::package_path(BUNDLE)).cloned()
    }
}

impl Connector for FakeDevice {
    fn connect<'a>(&'a self, _: &'a str, _: bool) -> BoxFuture<'a, sl_device::Result<Box<dyn Session>>> {
        async move {
            let mut state = self.state();
            state.operations.push("connect".into());

            if let Some(Fault::Connect(_)) = state.faults.front() {
                let Some(Fault::Connect(error)) = state.faults.pop_front() else { unreachable!() };

                if matches!(error, DeviceError::NotConnected(_)) {
                    state.attached = false;
                }

                return Err(error);
            }

            let session: Box<dyn Session> = Box::new(FakeSession { device: self.clone() });

            Ok(session)
        }
        .boxed()
    }

    fn is_attached<'a>(&'a self, _: &'a str) -> BoxFuture<'a, sl_device::Result<bool>> {
        async move {
            let mut state = self.state();
            state.polls += 1;

            if state.returns_after.is_some_and(|polls| state.polls >= polls) {
                state.attached = true;
            }

            Ok(state.attached)
        }
        .boxed()
    }
}

struct FakeSession {
    device: FakeDevice,
}

impl Staging for FakeSession {
    fn file_size<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, sl_device::Result<Option<u64>>> {
        let size = self.device.state().files.get(path).map(|bytes| bytes.len() as u64);

        async move { Ok(size) }.boxed()
    }

    fn remove<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, sl_device::Result<()>> {
        let mut state = self.device.state();
        state.operations.push(format!("remove {path}"));
        state.files.remove(path);

        async { Ok(()) }.boxed()
    }

    fn remove_all<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, sl_device::Result<()>> {
        let mut state = self.device.state();
        state.operations.push(format!("remove_all {path}"));
        state.files.retain(|name, _| !name.starts_with(path));

        async { Ok(()) }.boxed()
    }

    fn make_dirs<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, sl_device::Result<()>> {
        let mut state = self.device.state();
        state.operations.push(format!("make_dirs {path}"));
        state.directories.push(path.into());

        async { Ok(()) }.boxed()
    }

    fn open_append<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, sl_device::Result<()>> {
        let mut state = self.device.state();
        state.operations.push(format!("append {path}"));
        state.files.entry(path.into()).or_default();
        state.open = Some(path.into());

        async { Ok(()) }.boxed()
    }

    fn write<'a>(&'a mut self, data: &'a [u8]) -> BoxFuture<'a, sl_device::Result<()>> {
        let result = (|| {
            let mut state = self.device.state();
            let path = state.open.clone().ok_or(DeviceError::Protocol("no open file".into()))?;

            let fault = match state.faults.front() {
                Some(Fault::WriteBreaks { .. } | Fault::WriteDrops(_)) => state.faults.pop_front(),
                _ => None,
            };

            match fault {
                Some(Fault::WriteBreaks { bytes, error }) if bytes < data.len() => {
                    state.files.get_mut(&path).expect("open file").extend_from_slice(&data[..bytes]);
                    state.open = None;

                    Err(error)
                }
                Some(Fault::WriteBreaks { bytes, error }) => {
                    state.faults.push_front(Fault::WriteBreaks { bytes: bytes - data.len(), error });
                    state.files.get_mut(&path).expect("open file").extend_from_slice(data);

                    Ok(())
                }
                Some(Fault::WriteDrops(dropped)) => {
                    let kept = data.len().saturating_sub(dropped);
                    state.files.get_mut(&path).expect("open file").extend_from_slice(&data[..kept]);

                    Ok(())
                }
                _ => {
                    state.files.get_mut(&path).expect("open file").extend_from_slice(data);

                    Ok(())
                }
            }
        })();

        async move { result }.boxed()
    }

    fn close(&mut self) -> BoxFuture<'_, sl_device::Result<()>> {
        self.device.state().open = None;

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
        _bundle_id: &'a str,
        status: &'a (dyn Fn(InstallStatus) + Send + Sync),
    ) -> BoxFuture<'a, sl_device::Result<()>> {
        async move {
            let fault = {
                let mut state = self.device.state();
                state.installs += 1;
                state.operations.push(format!("install {package}"));

                match state.faults.front() {
                    Some(Fault::Install { .. }) => state.faults.pop_front(),
                    _ => None,
                }
            };

            if let Some(Fault::Install { error, keep_staged }) = fault {
                if !keep_staged {
                    self.device.state().files.remove(package);
                }

                return Err(error);
            }

            for (state, percent) in [("CreatingStagingDirectory", 5), ("InstallingApplication", 60), ("Complete", 100)]
            {
                status(InstallStatus { status: state.into(), percent: Some(percent) });
            }

            self.device.state().files.remove(package);

            Ok(())
        }
        .boxed()
    }
}

struct Bytes {
    data: Vec<u8>,
    /// A generated stream: size unknown until read.
    stream: bool,
}

impl Package for Bytes {
    fn size(&self) -> Option<u64> {
        (!self.stream).then_some(self.data.len() as u64)
    }

    fn estimate(&self) -> Option<u64> {
        Some(self.data.len() as u64 + 100)
    }

    fn reader(&self, offset: u64) -> BoxFuture<'_, sl_device::Result<Box<dyn PackageReader>>> {
        async move {
            if offset > self.data.len() as u64 {
                return Err(DeviceError::StagedTooLarge { staged: offset, expected: self.data.len() as u64 });
            }

            let reader: Box<dyn PackageReader> =
                Box::new(Cursor { data: self.data[offset as usize..].to_vec(), position: 0 });

            Ok(reader)
        }
        .boxed()
    }
}

struct Cursor {
    data: Vec<u8>,
    position: usize,
}

impl PackageReader for Cursor {
    fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> BoxFuture<'a, sl_device::Result<usize>> {
        // Short reads exercise the chunk-filling loop.
        let count = buffer.len().min(self.data.len() - self.position).min(333);
        buffer[..count].copy_from_slice(&self.data[self.position..self.position + count]);
        self.position += count;

        async move { Ok(count) }.boxed()
    }
}

#[derive(Default)]
struct Recorder {
    answers: Mutex<VecDeque<bool>>,
    wait: Mutex<VecDeque<DeviceWait>>,
    questions: Mutex<Vec<Question>>,
    logs: Mutex<Vec<String>>,
    progress: Mutex<Vec<(Phase, u64, u64)>>,
    cancel_after_upload_bytes: Option<u64>,
    cancelled: Mutex<bool>,
    uploaded_copy: Mutex<Option<FakeDevice>>,
}

impl Recorder {
    fn answering(answers: impl IntoIterator<Item = bool>) -> Self {
        Self { answers: Mutex::new(answers.into_iter().collect()), ..Self::default() }
    }

    fn questions(&self) -> Vec<Question> {
        self.questions.lock().expect("questions").clone()
    }
}

impl Delegate for Recorder {
    fn log(&self, message: &str) {
        self.logs.lock().expect("logs").push(message.into());
    }

    fn progress(&self, phase: Phase, done: u64, total: u64) {
        self.progress.lock().expect("progress").push((phase, done, total));

        if phase == Phase::Uploading && self.cancel_after_upload_bytes.is_some_and(|limit| done >= limit) {
            *self.cancelled.lock().expect("cancel flag") = true;
        }
    }

    fn is_cancelled(&self) -> bool {
        *self.cancelled.lock().expect("cancel flag")
    }

    fn ask<'a>(&'a self, question: Question) -> BoxFuture<'a, bool> {
        self.questions.lock().expect("questions").push(question);
        let answer = self.answers.lock().expect("answers").pop_front().unwrap_or(false);

        async move { answer }.boxed()
    }

    fn await_device<'a>(&'a self, _: &'a str, _: String) -> BoxFuture<'a, DeviceWait> {
        let outcome = self.wait.lock().expect("wait").pop_front().unwrap_or(DeviceWait::GiveUp);

        if outcome == DeviceWait::RetryNow
            && let Some(device) = self.uploaded_copy.lock().expect("device").as_ref()
        {
            device.state().attached = true;
        }

        async move { outcome }.boxed()
    }

    fn sleep(&self, _: Duration) -> BoxFuture<'static, ()> {
        async {}.boxed()
    }
}

fn package() -> Bytes {
    Bytes { data: (0..4500u32).map(|value| (value * 7 % 251) as u8).collect(), stream: false }
}

fn stream() -> Bytes {
    Bytes { stream: true, ..package() }
}

fn request() -> InstallRequest<'static> {
    InstallRequest { udid: UDID, device_name: Some("Phone"), prefer_network: false, bundle_id: BUNDLE, chunk: CHUNK }
}

fn run(device: &FakeDevice, package: &Bytes, delegate: &Recorder) -> sl_device::Result<()> {
    block_on(install::install(device, package, request(), delegate))
}

#[test]
fn a_clean_install_stages_the_package_then_installs_with_monotonic_progress() {
    let device = FakeDevice::new([]);
    let package = package();
    let delegate = Recorder::default();

    run(&device, &package, &delegate).expect("install");

    let state = device.state();
    let staging = install::package_path(BUNDLE);

    assert_eq!(
        state.operations,
        [
            "connect".to_owned(),
            format!("remove {staging}"),
            "remove_all StagingDirectory".into(),
            "make_dirs PublicStaging".into(),
            format!("append {staging}"),
            format!("install {staging}"),
        ]
    );
    drop(state);

    let progress = delegate.progress.lock().expect("progress").clone();
    let uploads: Vec<_> =
        progress.iter().filter(|(phase, ..)| *phase == Phase::Uploading).map(|(_, done, _)| *done).collect();

    assert_eq!(uploads.first(), Some(&0));
    assert_eq!(uploads.last(), Some(&(package.data.len() as u64)));
    assert!(uploads.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(progress.last(), Some(&(Phase::Installing, 100, 100)));
    assert!(delegate.questions().is_empty());
}

#[test]
fn an_interrupted_write_resumes_from_the_staged_size_without_gaps_or_duplicates() {
    let device = FakeDevice::new([Fault::WriteBreaks { bytes: 1500, error: DeviceError::Interrupted("cable".into()) }]);
    let package = package();
    let delegate = Recorder::default();

    let staged = Arc::new(Mutex::new(None));
    let observed = staged.clone();
    let probe = device.clone();

    // Capture the staged bytes just before installation consumes them.
    let wrapped = InstallProbe { device: device.clone(), staged: observed };
    block_on(install::install(&wrapped, &package, request(), &delegate)).expect("resumed install");

    let staged = staged.lock().expect("staged").clone().expect("captured package");
    assert_eq!(staged, package.data, "the resumed upload matches the package byte for byte");

    let operations = probe.state().operations.clone();
    let removals = operations.iter().filter(|operation| operation.starts_with("remove PublicStaging")).count();
    assert_eq!(removals, 1, "a retry keeps the partial upload");
    assert!(delegate.questions().is_empty(), "interruptions retry without asking");
    assert!(delegate.logs.lock().expect("logs").iter().any(|log| log == "Retrying automatically"));
}

/// Wraps a fake to capture the staged package at installation time.
struct InstallProbe {
    device: FakeDevice,
    staged: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Connector for InstallProbe {
    fn connect<'a>(&'a self, udid: &'a str, network: bool) -> BoxFuture<'a, sl_device::Result<Box<dyn Session>>> {
        async move {
            let inner = self.device.connect(udid, network).await?;
            let session: Box<dyn Session> =
                Box::new(ProbeSession { inner, device: self.device.clone(), staged: self.staged.clone() });

            Ok(session)
        }
        .boxed()
    }

    fn is_attached<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, sl_device::Result<bool>> {
        self.device.is_attached(udid)
    }
}

struct ProbeSession {
    inner: Box<dyn Session>,
    device: FakeDevice,
    staged: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Session for ProbeSession {
    fn staging(&mut self) -> &mut dyn Staging {
        self.inner.staging()
    }

    fn install<'a>(
        &'a mut self,
        package: &'a str,
        bundle_id: &'a str,
        status: &'a (dyn Fn(InstallStatus) + Send + Sync),
    ) -> BoxFuture<'a, sl_device::Result<()>> {
        *self.staged.lock().expect("staged") = self.device.staged();

        self.inner.install(package, bundle_id, status)
    }
}

#[test]
fn oversized_and_short_staged_files_are_removed_and_uploaded_again() {
    let package = package();

    let oversized = FakeDevice::new([Fault::Connect(DeviceError::Interrupted("first".into()))]);
    oversized.state().files.insert(install::package_path(BUNDLE), vec![0; 5000]);
    let delegate = Recorder::default();

    // The first attempt fails before cleanup, so the oversized file is found on the retry.
    run(&oversized, &package, &delegate).expect("install after oversized staging");
    assert!(delegate.logs.lock().expect("logs").iter().any(|log| log.contains("bigger than original")));

    let short = FakeDevice::new([Fault::WriteDrops(10)]);
    let delegate = Recorder::default();

    run(&short, &package, &delegate).expect("install after size mismatch");
    assert!(delegate.logs.lock().expect("logs").iter().any(|log| log.contains("size mismatch")));
    assert_eq!(short.state().installs, 1);
}

#[test]
fn installation_failures_with_a_staged_package_retry_four_times_then_ask() {
    let failure = || Fault::Install {
        error: DeviceError::Install { name: "APIInternalError".into(), description: None, detail: None },
        keep_staged: true,
    };

    let device = FakeDevice::new((0..6).map(|_| failure()));
    let delegate = Recorder::answering([false]);

    let error = run(&device, &package(), &delegate).expect_err("gives up");

    assert!(matches!(error, DeviceError::InstallRetry(_)), "{error:?}");
    assert_eq!(device.state().installs, 5, "one attempt plus four automatic retries");
    assert!(matches!(delegate.questions().as_slice(), [Question::ContinueRetrying { .. }]));

    let continued = FakeDevice::new((0..5).map(|_| failure()));
    let delegate = Recorder::answering([true]);

    run(&continued, &package(), &delegate).expect("succeeds after continuing");
    assert_eq!(continued.state().installs, 6);
}

#[test]
fn terminal_installation_errors_stop_without_retrying() {
    let error = DeviceError::Install {
        name: "ApplicationVerificationFailed".into(),
        description: Some("Failed to verify code signature".into()),
        detail: Some(0xe8008015),
    };
    let device = FakeDevice::new([Fault::Install { error: error.clone(), keep_staged: true }]);
    let delegate = Recorder::default();

    assert_eq!(run(&device, &package(), &delegate), Err(error));
    assert_eq!(device.state().installs, 1);
    assert!(delegate.questions().is_empty());
}

#[test]
fn installation_issues_without_a_staged_package_ask_before_retrying() {
    let issue = || Fault::Install {
        error: DeviceError::Install { name: "InstallProhibited".into(), description: None, detail: None },
        keep_staged: false,
    };

    let declined = FakeDevice::new([issue()]);
    let delegate = Recorder::answering([false]);
    assert!(matches!(run(&declined, &package(), &delegate), Err(DeviceError::Install { .. })));
    assert!(matches!(delegate.questions().as_slice(), [Question::InstallIssue { .. }]));

    let accepted = FakeDevice::new([issue()]);
    let delegate = Recorder::answering([true]);
    run(&accepted, &package(), &delegate).expect("retry after confirmation");

    let extraction = FakeDevice::new([Fault::Install {
        error: DeviceError::Install {
            name: "APIInternalError".into(),
            description: Some("Could not extract archive".into()),
            detail: None,
        },
        keep_staged: false,
    }]);
    let delegate = Recorder::default();
    run(&extraction, &package(), &delegate).expect("extraction failures retry automatically");
    assert!(delegate.questions().is_empty());
}

#[test]
fn a_vanished_device_is_awaited_and_the_upload_resumes() {
    let package = package();
    let device = FakeDevice::new([
        Fault::WriteBreaks { bytes: 2500, error: DeviceError::Interrupted("unplugged".into()) },
        Fault::Connect(DeviceError::NotConnected(UDID.into())),
    ]);
    device.state().returns_after = Some(30);
    let delegate = Recorder::default();

    run(&device, &package, &delegate).expect("install after reconnect");

    let state = device.state();
    assert!(state.polls >= 30 && state.polls < 180, "{}", state.polls);
    assert_eq!(state.installs, 1);
    drop(state);

    let appends = device.state().operations.iter().filter(|operation| operation.starts_with("append")).count();
    assert_eq!(appends, 2, "the upload resumed instead of restarting");
    assert!(delegate.logs.lock().expect("logs").iter().any(|log| log.contains("will retry/resume")));
}

#[test]
fn a_device_that_does_not_return_leads_to_a_prompt() {
    let lost = FakeDevice::new([Fault::Connect(DeviceError::NotConnected(UDID.into()))]);
    let delegate = Recorder::default();

    assert_eq!(run(&lost, &package(), &delegate), Err(DeviceError::Cancelled));
    assert_eq!(lost.state().polls, 181, "one immediate check and 180 one-second polls");

    let retried = FakeDevice::new([Fault::Connect(DeviceError::NotConnected(UDID.into()))]);
    let delegate = Recorder::default();
    delegate.wait.lock().expect("wait").push_back(DeviceWait::RetryNow);
    *delegate.uploaded_copy.lock().expect("device") = Some(retried.clone());

    run(&retried, &package(), &delegate).expect("retry chosen at the prompt");
    assert_eq!(retried.state().installs, 1);
}

#[test]
fn pairing_states_ask_the_user_with_the_connection_reason() {
    let locked = FakeDevice::new([Fault::Connect(DeviceError::PasswordProtected)]);
    let delegate = Recorder::answering([true]);

    run(&locked, &package(), &delegate).expect("install after unlocking");
    assert!(matches!(
        delegate.questions().as_slice(),
        [Question::Retry { reason, .. }] if reason.contains("cable")
    ));

    let denied = FakeDevice::new([Fault::Connect(DeviceError::UserDeniedPairing)]);
    let delegate = Recorder::answering([false]);
    assert_eq!(run(&denied, &package(), &delegate), Err(DeviceError::UserDeniedPairing));
}

#[test]
fn cancellation_during_upload_stops_before_installation() {
    let device = FakeDevice::new([]);
    let delegate = Recorder { cancel_after_upload_bytes: Some(2000), ..Recorder::default() };

    assert_eq!(run(&device, &package(), &delegate), Err(DeviceError::Cancelled));
    assert_eq!(device.state().installs, 0);

    let staged = device.staged().expect("partial upload stays for a later resume");
    assert_eq!(staged, package().data[..staged.len()]);
}

#[test]
fn generated_streams_resume_by_skipping_the_staged_prefix_in_exact_chunks() {
    let package = stream();
    let device =
        FakeDevice::new([Fault::WriteBreaks { bytes: 2100, error: DeviceError::Interrupted("unplugged".into()) }]);
    let staged = Arc::new(Mutex::new(None));
    let probe = InstallProbe { device: device.clone(), staged: staged.clone() };
    let delegate = Recorder::default();

    block_on(install::install(&probe, &package, request(), &delegate)).expect("stream install");

    assert_eq!(staged.lock().expect("staged").clone().expect("captured"), package.data);

    let oversized = FakeDevice::new([Fault::Connect(DeviceError::Interrupted("first".into()))]);
    oversized.state().files.insert(install::package_path(BUNDLE), vec![0; 9000]);
    let delegate = Recorder::default();

    run(&oversized, &stream(), &delegate).expect("stream shorter than the staged file restarts");
    assert!(delegate.logs.lock().expect("logs").iter().any(|log| log.contains("bigger than original")));
}
