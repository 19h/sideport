//! Job handles, events and interactive prompts. CONTRACT: front-ends consume these.
//!
//! A job runs on the engine's runtime and reports through an unbounded event channel. Everything here
//! is executor-agnostic (`async-channel`, `futures::channel::oneshot`), so a gpui view can await it.

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

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Producer side used by job implementations.
#[derive(Debug, Clone)]
pub struct JobContext {
    events: async_channel::Sender<JobEvent>,
    cancel: CancellationToken,
    next_prompt: std::sync::Arc<std::sync::atomic::AtomicU64>,
    progress: Arc<parking_lot::Mutex<ProgressState>>,
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
        let ctx =
            Self { events: tx, cancel: cancel.clone(), next_prompt: Default::default(), progress: Default::default() };
        (ctx, rx, cancel)
    }

    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        let message = message.into();
        match level {
            LogLevel::Debug => tracing::debug!("{message}"),
            LogLevel::Info => tracing::info!("{message}"),
            LogLevel::Warn => tracing::warn!("{message}"),
            LogLevel::Error => tracing::error!("{message}"),
        }
        let _ = self.events.try_send(JobEvent::Log { level, message });
    }

    pub fn info(&self, message: impl Into<String>) {
        self.log(LogLevel::Info, message);
    }

    pub fn warn(&self, message: impl Into<String>) {
        self.log(LogLevel::Warn, message);
    }

    pub fn stage(&self, stage: Stage) {
        *self.progress.lock() = ProgressState::default();
        let _ = self.events.try_send(JobEvent::Stage(stage));
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
            let _ = self.events.try_send(JobEvent::Progress { done, total });
            state.last_sent = Some((now, done));
        }
    }

    pub fn fact(&self, fact: Fact) {
        let _ = self.events.try_send(JobEvent::Fact(fact));
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
        self.events.send(JobEvent::Prompt(prompt)).await.map_err(|_| EngineError::Cancelled)?;
        tokio::select! {
            reply = rx => match reply {
                Ok(PromptReply::Cancel) | Err(_) => Err(EngineError::Cancelled),
                Ok(reply) => Ok(reply),
            },
            _ = self.cancel.cancelled() => Err(EngineError::Cancelled),
        }
    }
}
