//! The [`Engine`] facade. CONTRACT: method signatures are consumed by `sl-cli` and `sl-app`.
//!
//! All async methods are executor-agnostic: work is spawned onto the engine's own tokio runtime and the
//! returned future only awaits a oneshot, so gpui (or any executor) can drive it.

use crate::demo::Demo;
use crate::error::{EngineError, Result};
use crate::job::{JobContext, JobHandle};
use crate::types::*;
use futures::channel::oneshot;
use parking_lot::RwLock;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// How to construct an [`Engine`].
#[derive(Debug, Clone, Default)]
pub struct EngineConfig {
    /// Override the data directory (default: `<platform data dir>/Sideport`).
    pub data_dir: Option<PathBuf>,
    /// Simulated devices/accounts/jobs, for UI development, screenshots and demos.
    pub demo: bool,
    /// Store secrets in a 0600 file instead of the OS keychain (tests, headless Linux).
    pub file_secrets: bool,
    /// Do not start the background refresh scheduler (CLI one-shots, tests).
    pub disable_scheduler: bool,
}

/// Notifications from the background refresh scheduler.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshEvent {
    Started {
        installation_id: i64,
        app_name: String,
    },
    Succeeded {
        installation_id: i64,
        app_name: String,
        expires: Option<chrono::DateTime<chrono::Utc>>,
    },
    Failed {
        installation_id: i64,
        app_name: String,
        error: String,
    },
}

#[derive(Debug)]
struct Inner {
    runtime: tokio::runtime::Runtime,
    data_dir: PathBuf,
    settings: RwLock<Settings>,
    demo: Option<Demo>,
    next_job: AtomicU64,
}

/// Cheaply cloneable engine handle.
#[derive(Debug, Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name("sideport-engine")
            .enable_all()
            .build()
            .map_err(|e| EngineError::Other(format!("failed to start runtime: {e}")))?;
        let data_dir = match config.data_dir {
            Some(d) => d,
            None => dirs::data_dir()
                .ok_or_else(|| EngineError::Storage("no data directory on this platform".into()))?
                .join("Sideport"),
        };
        let demo = config.demo.then(Demo::new);
        Ok(Self {
            inner: Arc::new(Inner {
                runtime,
                data_dir,
                settings: RwLock::new(Settings::default()),
                demo,
                next_job: AtomicU64::new(1),
            }),
        })
    }

    pub fn is_demo(&self) -> bool {
        self.inner.demo.is_some()
    }

    pub fn data_dir(&self) -> PathBuf {
        self.inner.data_dir.clone()
    }

    // --------------------------------------------------------------------------------------------
    // Settings

    pub fn settings(&self) -> Settings {
        self.inner.settings.read().clone()
    }

    pub fn update_settings(&self, settings: Settings) -> Result<()> {
        *self.inner.settings.write() = settings;
        Ok(())
    }

    /// Fetch anisette with the given setting and describe the machine Apple will see.
    pub fn test_anisette(
        &self,
        setting: AnisetteSetting,
    ) -> impl Future<Output = Result<String>> + use<> {
        let demo = self.inner.demo.clone();
        self.run(async move {
            match demo {
                Some(d) => d.test_anisette(setting).await,
                None => Err(not_yet()),
            }
        })
    }

    // --------------------------------------------------------------------------------------------
    // Devices

    /// Current device list, then a fresh snapshot whenever it changes.
    pub fn subscribe_devices(&self) -> async_channel::Receiver<Vec<DeviceInfo>> {
        let (tx, rx) = async_channel::unbounded();
        if let Some(d) = &self.inner.demo {
            let _ = tx.try_send(d.devices());
        }
        std::mem::forget(tx); // real implementation keeps the sender in the device watcher
        rx
    }

    pub fn devices(&self) -> impl Future<Output = Result<Vec<DeviceInfo>>> + use<> {
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|d| d.devices()).ok_or_else(not_yet) })
    }

    pub fn device_apps(
        &self,
        udid: String,
    ) -> impl Future<Output = Result<Vec<DeviceApp>>> + use<> {
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|d| d.device_apps(&udid)).ok_or_else(not_yet) })
    }

    pub fn uninstall_app(
        &self,
        udid: String,
        bundle_id: String,
    ) -> impl Future<Output = Result<()>> + use<> {
        let _ = (udid, bundle_id);
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|_| ()).ok_or_else(not_yet) })
    }

    pub fn device_profiles(
        &self,
        udid: String,
    ) -> impl Future<Output = Result<Vec<DeviceProfile>>> + use<> {
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|d| d.device_profiles(&udid)).ok_or_else(not_yet) })
    }

    pub fn remove_profile(
        &self,
        udid: String,
        uuid: String,
    ) -> impl Future<Output = Result<()>> + use<> {
        let _ = (udid, uuid);
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|_| ()).ok_or_else(not_yet) })
    }

    /// Start pairing (shows the "Trust This Computer?" dialog on the device).
    pub fn pair_device(&self, udid: String) -> impl Future<Output = Result<()>> + use<> {
        let _ = udid;
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|_| ()).ok_or_else(not_yet) })
    }

    // --------------------------------------------------------------------------------------------
    // Accounts

    pub fn accounts(&self) -> Result<Vec<AccountSummary>> {
        self.inner
            .demo
            .as_ref()
            .map(|d| d.accounts())
            .ok_or_else(not_yet)
    }

    /// Sign in (prompts for the password when `password` is `None`, and for 2FA codes as needed).
    pub fn login(
        &self,
        apple_id: String,
        password: Option<String>,
        remember: bool,
    ) -> JobHandle<AccountSummary> {
        let demo = self.inner.demo.clone();
        self.job(move |ctx| async move {
            match demo {
                Some(d) => d.login(ctx, apple_id, password, remember).await,
                None => Err(not_yet()),
            }
        })
    }

    pub fn logout(&self, apple_id: String) -> impl Future<Output = Result<()>> + use<> {
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|d| d.logout(&apple_id)).ok_or_else(not_yet) })
    }

    pub fn certificates(&self, apple_id: String) -> JobHandle<Vec<CertificateSummary>> {
        let demo = self.inner.demo.clone();
        self.job(
            move |_ctx| async move { demo.map(|d| d.certificates(&apple_id)).ok_or_else(not_yet) },
        )
    }

    pub fn revoke_certificate(&self, apple_id: String, serial: String) -> JobHandle<()> {
        let _ = (apple_id, serial);
        let demo = self.inner.demo.clone();
        self.job(move |_ctx| async move { demo.map(|_| ()).ok_or_else(not_yet) })
    }

    pub fn app_ids(&self, apple_id: String) -> JobHandle<Vec<AppIdSummary>> {
        let demo = self.inner.demo.clone();
        self.job(move |_ctx| async move { demo.map(|d| d.app_ids(&apple_id)).ok_or_else(not_yet) })
    }

    // --------------------------------------------------------------------------------------------
    // Apps and jobs

    /// Read metadata and the icon of an `.ipa` without extracting it.
    pub fn inspect(&self, path: PathBuf) -> impl Future<Output = Result<AppSummary>> + use<> {
        let demo = self.inner.demo.clone();
        self.run(async move {
            match demo {
                Some(d) => Ok(d.inspect(path)),
                None => Err(not_yet()),
            }
        })
    }

    /// Run a sideload/export job.
    pub fn start(&self, spec: JobSpec) -> JobHandle<JobOutcome> {
        let demo = self.inner.demo.clone();
        self.job(move |ctx| async move {
            match demo {
                Some(d) => d.run_job(ctx, spec).await,
                None => Err(not_yet()),
            }
        })
    }

    // --------------------------------------------------------------------------------------------
    // Installations

    pub fn installations(&self) -> Result<Vec<Installation>> {
        self.inner
            .demo
            .as_ref()
            .map(|d| d.installations())
            .ok_or_else(not_yet)
    }

    pub fn set_auto_refresh(&self, installation_id: i64, enabled: bool) -> Result<()> {
        match &self.inner.demo {
            Some(d) => {
                d.set_auto_refresh(installation_id, enabled);
                Ok(())
            }
            None => Err(not_yet()),
        }
    }

    pub fn forget_installation(&self, installation_id: i64) -> Result<()> {
        match &self.inner.demo {
            Some(d) => {
                d.forget(installation_id);
                Ok(())
            }
            None => Err(not_yet()),
        }
    }

    /// Re-run the stored job of an installation now.
    pub fn refresh(&self, installation_id: i64) -> JobHandle<JobOutcome> {
        let demo = self.inner.demo.clone();
        self.job(move |ctx| async move {
            match demo {
                Some(d) => d.refresh(ctx, installation_id).await,
                None => Err(not_yet()),
            }
        })
    }

    /// Background refresh notifications.
    pub fn subscribe_refresh(&self) -> async_channel::Receiver<RefreshEvent> {
        let (tx, rx) = async_channel::unbounded();
        std::mem::forget(tx);
        rx
    }

    // --------------------------------------------------------------------------------------------
    // Plumbing

    fn run<T, F>(&self, fut: F) -> impl Future<Output = Result<T>> + use<T, F>
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.inner.runtime.spawn(async move {
            let _ = tx.send(fut.await);
        });
        async move { rx.await.unwrap_or(Err(EngineError::Cancelled)) }
    }

    fn job<T, F, Fut>(&self, f: F) -> JobHandle<T>
    where
        T: Send + 'static,
        F: FnOnce(JobContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let id = self.inner.next_job.fetch_add(1, Ordering::Relaxed);
        let (ctx, events, cancel) = JobContext::channel();
        let (tx, rx) = oneshot::channel();
        let token = cancel.clone();
        self.inner.runtime.spawn(async move {
            let result = tokio::select! {
                r = f(ctx) => r,
                _ = token.cancelled() => Err(EngineError::Cancelled),
            };
            let _ = tx.send(result);
        });
        JobHandle::new(id, events, rx, cancel)
    }
}

fn not_yet() -> EngineError {
    EngineError::Unsupported("engine backend not implemented yet".into())
}
