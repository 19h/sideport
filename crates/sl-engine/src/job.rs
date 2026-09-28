//! Job handles, events and interactive prompts. CONTRACT: front-ends consume these.
//!
//! A job runs on the engine's runtime and reports through an event channel. Everything here is
//! executor-agnostic (`async-channel`, `futures::channel::oneshot`), so a gpui view can await it.
//!
//! Log lines and progress are lossy once [`EVENT_BACKLOG`] events wait unread: a stalled front end
//! then loses them (reported by one warning when it catches up, with the latest progress) instead
//! of growing memory without bound. Stages, facts and prompts are always delivered; a job emits a
//! bounded number of them.

use crate::error::EngineError;
use crate::types::TeamSummary;
use chrono::{DateTime, Utc};
use futures::channel::oneshot;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Coarse phase of a job, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    Preparing,
    Authenticating,
    Provisioning,
    Patching,
    Signing,
    Packaging,
    Uploading,
    Installing,
    Done,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Preparing => "Preparing",
            Stage::Authenticating => "Signing in",
            Stage::Provisioning => "Provisioning",
            Stage::Patching => "Patching",
            Stage::Signing => "Signing",
            Stage::Packaging => "Packaging",
            Stage::Uploading => "Uploading",
            Stage::Installing => "Installing",
            Stage::Done => "Done",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Structured facts discovered while a job runs (shown as badges/details, not just log text).
#[derive(Debug, Clone, PartialEq)]
pub enum Fact {
    /// Final bundle identifier that will be installed.
    BundleId(String),
    Team(TeamSummary),
    /// Free teams: app IDs still available and when the oldest one frees up.
    AppIdQuota {
        remaining: u32,
        next_release: Option<DateTime<Utc>>,
    },
    /// Provisioning profile validity.
    ProfileExpiry {
        expires: DateTime<Utc>,
        ttl_days: Option<u64>,
    },
    /// How Apple will list this machine in the account's device list (from anisette).
    AnisetteDevice(String),
    /// The main executable is encrypted; the installed app will not launch.
    EncryptedBinary,
}

#[derive(Debug)]
pub enum JobEvent {
    Log {
        level: LogLevel,
        message: String,
    },
    Stage(Stage),
    /// Progress within the current stage (`total == 0` = indeterminate).
    Progress {
        done: u64,
        total: u64,
    },
    Fact(Fact),
    Prompt(Prompt),
    /// The job stopped waiting for the answer to prompt `id` (it continued on its own, for
    /// example because the device returned, or it was cancelled). Close that question.
    PromptWithdrawn {
        id: u64,
    },
}

/// Choice offered by [`PromptKind::ChooseTeam`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamChoice {
    pub team: TeamSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    /// Apple ID password (reply: `Text`; `remember` suggests the keychain checkbox default).
    Password { apple_id: String, remember: bool },
    /// Two-factor code (reply: `Text`). `destination` describes where the code went.
    SecondFactor { apple_id: String, destination: String, code_length: usize, can_request_sms: bool },
    /// Pick a team (reply: `Choice(index)`).
    ChooseTeam { apple_id: String, teams: Vec<TeamChoice> },
    /// Yes/no question (reply: `Confirmed`).
    Confirm { title: String, message: String, confirm_label: String, destructive: bool },
    /// Where to save an exported IPA (reply: `Path`).
    SaveFile { suggested_name: String },
    /// The device vanished mid-install. Reply `Confirmed(true)` to retry now; the engine also completes
    /// the prompt itself (with `DeviceReturned`) if the device reappears.
    WaitForDevice { udid: String, device_name: String, reason: String },
}

#[derive(Clone, PartialEq, Eq)]
pub enum PromptReply {
    Text {
        value: String,
        remember: bool,
    },
    Choice(usize),
    Confirmed(bool),
    Path(PathBuf),
    /// Request an SMS code instead (second-factor prompt only).
    RequestSms,
    DeviceReturned,
    Cancel,
}

impl std::fmt::Debug for PromptReply {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text { remember, .. } => {
                formatter.debug_struct("Text").field("value", &"[redacted]").field("remember", remember).finish()
            }
            Self::Choice(index) => formatter.debug_tuple("Choice").field(index).finish(),
            Self::Confirmed(confirmed) => formatter.debug_tuple("Confirmed").field(confirmed).finish(),
            Self::Path(path) => formatter.debug_tuple("Path").field(path).finish(),
            Self::RequestSms => formatter.write_str("RequestSms"),
            Self::DeviceReturned => formatter.write_str("DeviceReturned"),
            Self::Cancel => formatter.write_str("Cancel"),
        }
    }
}

/// An interactive question. Answer exactly once; dropping it counts as [`PromptReply::Cancel`].
#[derive(Debug)]
pub struct Prompt {
    pub id: u64,
    pub kind: PromptKind,
    reply: Option<oneshot::Sender<PromptReply>>,
}

impl Prompt {
    /// Create a prompt and the receiver the job awaits.
    pub fn new(id: u64, kind: PromptKind) -> (Self, oneshot::Receiver<PromptReply>) {
        let (tx, rx) = oneshot::channel();
        (Self { id, kind, reply: Some(tx) }, rx)
    }

    pub fn answer(mut self, reply: PromptReply) {
        if let Some(tx) = self.reply.take() {
            // The job may have been cancelled meanwhile; nothing to do then.
            let _ = tx.send(reply);
        }
    }

    pub fn cancel(self) {
        self.answer(PromptReply::Cancel);
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        if let Some(tx) = self.reply.take() {
            let _ = tx.send(PromptReply::Cancel);
        }
    }
}

/// Handle to a running job.
#[derive(Debug)]
pub struct JobHandle<T> {
    pub id: u64,
    events: async_channel::Receiver<JobEvent>,
    result: oneshot::Receiver<Result<T, EngineError>>,
    cancel: CancellationToken,
}

impl<T> JobHandle<T> {
    pub(crate) fn new(
        id: u64,
        events: async_channel::Receiver<JobEvent>,
        result: oneshot::Receiver<Result<T, EngineError>>,
        cancel: CancellationToken,
    ) -> Self {
        Self { id, events, result, cancel }
    }

    /// Event stream; ends when the job finishes.
    pub fn events(&self) -> async_channel::Receiver<JobEvent> {
        self.events.clone()
    }

    /// Request cancellation (pending prompts are cancelled, the job stops at the next checkpoint).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Wait for the final result.
    pub async fn result(mut self) -> Result<T, EngineError> {
        (&mut self.result).await.unwrap_or(Err(EngineError::Cancelled))
    }
}

impl<T> Drop for JobHandle<T> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Unread events after which log lines and progress are dropped.
pub const EVENT_BACKLOG: usize = 4096;

/// Producer side used by job implementations.
#[derive(Debug, Clone)]
pub struct JobContext {
    events: async_channel::Sender<JobEvent>,
    cancel: CancellationToken,
    next_prompt: std::sync::Arc<std::sync::atomic::AtomicU64>,
    progress: Arc<parking_lot::Mutex<ProgressState>>,
    backlog: Arc<parking_lot::Mutex<Backlog>>,
}

/// Lossy events dropped while the receiver was behind.
#[derive(Debug, Default)]
struct Backlog {
    dropped_logs: u64,
    pending_progress: Option<(u64, u64)>,
}

#[derive(Debug, Default)]
struct ProgressState {
    maximum: u64,
    total: u64,
    last_sent: Option<(Instant, u64)>,
}

impl JobContext {
    /// Create a context plus the pieces needed for a [`JobHandle`].
    pub fn channel() -> (Self, async_channel::Receiver<JobEvent>, CancellationToken) {
        let (tx, rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let ctx = Self {
            events: tx,
            cancel: cancel.clone(),
            next_prompt: Default::default(),
            progress: Default::default(),
            backlog: Default::default(),
        };

        (ctx, rx, cancel)
    }

    /// Deliver an event that must not be lost, after reporting what was dropped before it.
    fn deliver(&self, event: JobEvent) {
        self.flush_backlog();

        let _ = self.events.try_send(event);
    }

    /// Deliver a log line or progress update unless the receiver is [`EVENT_BACKLOG`] behind.
    fn deliver_lossy(&self, event: JobEvent) {
        let mut backlog = self.backlog.lock();

        if self.events.len() >= EVENT_BACKLOG {
            match event {
                JobEvent::Progress { done, total } => backlog.pending_progress = Some((done, total)),
                _ => backlog.dropped_logs += 1,
            }

            return;
        }

        Self::flush(&self.events, &mut backlog);

        if matches!(event, JobEvent::Progress { .. }) {
            backlog.pending_progress = None;
        }

        let _ = self.events.try_send(event);
    }

    fn flush_backlog(&self) {
        Self::flush(&self.events, &mut self.backlog.lock());
    }

    fn flush(events: &async_channel::Sender<JobEvent>, backlog: &mut Backlog) {
        if backlog.dropped_logs > 0 {
            let count = std::mem::take(&mut backlog.dropped_logs);
            let noun = if count == 1 { "message was" } else { "messages were" };
            let message = format!("{count} log {noun} dropped because the display fell behind.");

            let _ = events.try_send(JobEvent::Log { level: LogLevel::Warn, message });
        }

        if let Some((done, total)) = backlog.pending_progress.take() {
            let _ = events.try_send(JobEvent::Progress { done, total });
        }
    }

    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        let message = message.into();
        match level {
            LogLevel::Debug => tracing::debug!("{message}"),
            LogLevel::Info => tracing::info!("{message}"),
            LogLevel::Warn => tracing::warn!("{message}"),
            LogLevel::Error => tracing::error!("{message}"),
        }
        self.deliver_lossy(JobEvent::Log { level, message });
    }

    pub fn info(&self, message: impl Into<String>) {
        self.log(LogLevel::Info, message);
    }

    pub fn warn(&self, message: impl Into<String>) {
        self.log(LogLevel::Warn, message);
    }

    pub fn stage(&self, stage: Stage) {
        *self.progress.lock() = ProgressState::default();
        self.backlog.lock().pending_progress = None;
        self.deliver(JobEvent::Stage(stage));
    }

    pub fn progress(&self, done: u64, total: u64) {
        let mut state = self.progress.lock();

        if state.total != total {
            *state = ProgressState { total, ..ProgressState::default() };
        }

        state.maximum = state.maximum.max(done);
        let now = Instant::now();
        let terminal = total != 0 && state.maximum >= total;
        let should_send = state.last_sent.is_none_or(|(last, value)| {
            value != state.maximum && (terminal || now.duration_since(last) >= Duration::from_millis(100))
        });

        if should_send {
            let done = state.maximum;
            self.deliver_lossy(JobEvent::Progress { done, total });
            state.last_sent = Some((now, done));
        }
    }

    pub fn fact(&self, fact: Fact) {
        self.deliver(JobEvent::Fact(fact));
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Fail with [`EngineError::Cancelled`] if cancellation was requested.
    pub fn checkpoint(&self) -> Result<(), EngineError> {
        if self.is_cancelled() { Err(EngineError::Cancelled) } else { Ok(()) }
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Ask the front-end a question and wait for the answer (or cancellation).
    pub async fn ask(&self, kind: PromptKind) -> Result<PromptReply, EngineError> {
        let id = self.next_prompt.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (prompt, rx) = Prompt::new(id, kind);

        self.flush_backlog();
        self.events.send(JobEvent::Prompt(prompt)).await.map_err(|_| EngineError::Cancelled)?;

        // Withdraw the question unless an answer arrives, including when this future is dropped.
        let mut withdrawal = Withdrawal { context: self, id, answered: false };

        tokio::select! {
            reply = rx => {
                withdrawal.answered = true;

                match reply {
                    Ok(PromptReply::Cancel) | Err(_) => Err(EngineError::Cancelled),
                    Ok(reply) => Ok(reply),
                }
            },
            _ = self.cancel.cancelled() => Err(EngineError::Cancelled),
        }
    }
}

/// Announces [`JobEvent::PromptWithdrawn`] when a question is abandoned unanswered.
struct Withdrawal<'a> {
    context: &'a JobContext,
    id: u64,
    answered: bool,
}

impl Drop for Withdrawal<'_> {
    fn drop(&mut self) {
        if !self.answered {
            self.context.deliver(JobEvent::PromptWithdrawn { id: self.id });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    fn drain(events: &async_channel::Receiver<JobEvent>) -> Vec<JobEvent> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    #[test]
    fn a_stalled_receiver_bounds_logs_and_progress_but_keeps_stages_and_facts() {
        let (context, events, _cancel) = JobContext::channel();
        let lines = EVENT_BACKLOG + 5000;

        context.stage(Stage::Uploading);

        for line in 0..lines {
            context.info(format!("line {line}"));
        }

        context.fact(Fact::EncryptedBinary);
        context.progress(7, 10);
        context.stage(Stage::Installing);

        // The backlog, the drop report, the fact and the second stage.
        assert_eq!(events.len(), EVENT_BACKLOG + 3);

        let first = drain(&events);
        let dropped = lines - (EVENT_BACKLOG - 1);

        assert!(matches!(first.first(), Some(JobEvent::Stage(Stage::Uploading))));
        assert!(matches!(first.last(), Some(JobEvent::Stage(Stage::Installing))));
        assert!(first.iter().any(|event| matches!(event, JobEvent::Fact(Fact::EncryptedBinary))));

        let report = first.iter().find_map(|event| match event {
            JobEvent::Log { level: LogLevel::Warn, message } => Some(message.clone()),
            _ => None,
        });
        assert_eq!(report, Some(format!("{dropped} log messages were dropped because the display fell behind.")));
        assert!(
            !first.iter().any(|event| matches!(event, JobEvent::Progress { .. })),
            "progress of a finished stage is not replayed"
        );

        context.info("after");
        assert!(matches!(drain(&events).as_slice(), [JobEvent::Log { message, .. }] if message == "after"));
    }

    #[test]
    fn the_latest_dropped_progress_is_delivered_when_the_receiver_catches_up() {
        let (context, events, _cancel) = JobContext::channel();

        for line in 0..EVENT_BACKLOG {
            context.info(format!("line {line}"));
        }

        context.progress(10, 10);
        drain(&events);
        context.info("after");

        let delivered = drain(&events);
        assert!(matches!(delivered.as_slice(), [JobEvent::Progress { done: 10, total: 10 }, JobEvent::Log { .. }]));
    }

    #[test]
    fn a_question_the_job_stops_waiting_for_is_withdrawn() {
        let (context, events, cancel) = JobContext::channel();

        let mut question = Box::pin(context.ask(PromptKind::SaveFile { suggested_name: "App.ipa".into() }));
        assert!(question.as_mut().now_or_never().is_none());

        let Ok(JobEvent::Prompt(prompt)) = events.try_recv() else { panic!("prompt") };
        drop(question);

        assert!(matches!(events.try_recv(), Ok(JobEvent::PromptWithdrawn { id }) if id == prompt.id));

        let answered = context.ask(PromptKind::SaveFile { suggested_name: "App.ipa".into() });
        let responder = async {
            let Ok(JobEvent::Prompt(prompt)) = events.recv().await else { panic!("prompt") };
            prompt.answer(PromptReply::Path("out.ipa".into()));
        };

        let (reply, ()) = futures::executor::block_on(futures::future::join(answered, responder));
        assert_eq!(reply.expect("reply"), PromptReply::Path("out.ipa".into()));
        assert!(events.try_recv().is_err(), "an answered question is not withdrawn");

        cancel.cancel();
        let cancelled = futures::executor::block_on(context.ask(PromptKind::SaveFile { suggested_name: "A".into() }));
        assert!(matches!(cancelled, Err(EngineError::Cancelled)));
        assert!(matches!(events.try_recv(), Ok(JobEvent::Prompt(_))));
        assert!(matches!(events.try_recv(), Ok(JobEvent::PromptWithdrawn { .. })));
    }
}
