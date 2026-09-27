//! The [`Engine`] facade. CONTRACT: method signatures are consumed by `sl-cli` and `sl-app`.
//!
//! All async methods are executor-agnostic: work is spawned onto the engine's own tokio runtime and the
//! returned future only awaits a oneshot, so gpui (or any executor) can drive it.

mod auth;
mod files;
mod portal;
mod provision;
mod sideload;
mod state;

use crate::demo::Demo;
use crate::error::{EngineError, Result};
use crate::job::{JobContext, JobHandle};
use crate::secrets::SecretStore;
use crate::store::Store;
use crate::types::*;
use futures::channel::oneshot;
use parking_lot::{Mutex, RwLock};
use std::collections::BTreeMap;
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
    /// Override the GSA origin for controlled service fixtures.
    pub auth_origin: Option<String>,
    /// Override the developer-services origin for controlled service fixtures.
    pub portal_origin: Option<String>,
    /// Trust anchors and signer names for downloaded profiles (default: Apple Root CA policy).
    pub profile_trust: Option<sl_codesign::ProfileTrust>,
}

/// Notifications from the background refresh scheduler.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshEvent {
    Started { installation_id: i64, app_name: String },
    Succeeded { installation_id: i64, app_name: String, expires: Option<chrono::DateTime<chrono::Utc>> },
    Failed { installation_id: i64, app_name: String, error: String },
}

#[derive(Debug)]
struct Inner {
    runtime: Arc<RuntimeOwner>,
    data_dir: PathBuf,
    settings: RwLock<Settings>,
    auth_origin: String,
    portal_origin: String,
    store: Store,
    secrets: Box<dyn SecretStore>,
    profile_trust: sl_codesign::ProfileTrust,
    /// Serializes signing-key creation within this process; the store serializes processes.
    key_lock: Mutex<()>,
    accounts: Mutex<BTreeMap<String, LiveAccount>>,
    demo: Option<Demo>,
    next_job: AtomicU64,
    /// Owned by the [`Engine`] handles; jobs and background tasks publish through this weak
    /// reference, so dropping the last handle closes every subscription channel.
    subscribers: std::sync::Weak<Subscribers>,
}

#[derive(Debug, Default)]
struct Subscribers {
    devices: Mutex<Vec<async_channel::Sender<Vec<DeviceInfo>>>>,
    refresh: Mutex<Vec<async_channel::Sender<RefreshEvent>>>,
}

impl Inner {
    #[allow(dead_code)]
    fn publish_devices(&self, devices: &[DeviceInfo]) {
        if let Some(subscribers) = self.subscribers.upgrade() {
            subscribers.devices.lock().retain(|sender| sender.try_send(devices.to_vec()).is_ok());
        }
    }

    #[allow(dead_code)]
    fn publish_refresh(&self, event: &RefreshEvent) {
        if let Some(subscribers) = self.subscribers.upgrade() {
            subscribers.refresh.lock().retain(|sender| sender.try_send(event.clone()).is_ok());
        }
    }
}

#[derive(Debug)]
struct LiveAccount {
    summary: AccountSummary,
    /// `None` when the stored session is missing; jobs must sign in again.
    session: Option<Arc<sl_apple::auth::AuthSession>>,
}

#[derive(Debug)]
struct RuntimeOwner {
    runtime: Option<tokio::runtime::Runtime>,
    handle: tokio::runtime::Handle,
}

impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Cheaply cloneable engine handle.
#[derive(Debug, Clone)]
pub struct Engine {
    inner: Arc<Inner>,
    subscribers: Arc<Subscribers>,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name("sideport-engine")
            .enable_all()
            .build()
            .map_err(|e| EngineError::Other(format!("failed to start runtime: {e}")))?;
        let handle = runtime.handle().clone();
        let runtime = Arc::new(RuntimeOwner { runtime: Some(runtime), handle });

        let data_dir = match config.data_dir {
            Some(d) => d,
            None => dirs::data_dir()
                .ok_or_else(|| EngineError::Storage("no data directory on this platform".into()))?
                .join("Sideport"),
        };
        let demo = config.demo.then(Demo::new);
        let settings = crate::settings::load(&data_dir)?;
        let store = Store::open(&data_dir)?;
        let secrets = crate::secrets::open(&data_dir, config.file_secrets)?;
        let accounts =
            if demo.is_some() { BTreeMap::new() } else { state::restore_accounts(&store, secrets.as_ref())? };
        let profile_trust = match config.profile_trust {
            Some(trust) => trust,
            None => sl_codesign::ProfileTrust::apple().map_err(|error| EngineError::Signing(error.to_string()))?,
        };
        let auth_origin = config.auth_origin.unwrap_or_else(|| "https://gsa.apple.com".into());
        sl_apple::auth::AuthClient::with_origin(&auth_origin).map_err(|error| EngineError::Auth(error.to_string()))?;
        let portal_origin =
            config.portal_origin.unwrap_or_else(|| "https://developerservices2.apple.com/services/QH65B2/".into());
        sl_apple::portal::PortalClient::with_origin(&portal_origin)
            .map_err(|error| EngineError::Auth(error.to_string()))?;

        let subscribers = Arc::new(Subscribers::default());

        Ok(Self {
            subscribers: subscribers.clone(),
            inner: Arc::new(Inner {
                runtime,
                data_dir,
                settings: RwLock::new(settings),
                auth_origin,
                portal_origin,
                store,
                secrets,
                profile_trust,
                key_lock: Mutex::new(()),
                accounts: Mutex::new(accounts),
                demo,
                next_job: AtomicU64::new(1),
                subscribers: Arc::downgrade(&subscribers),
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
        let mut current = self.inner.settings.write();
        crate::settings::save(&self.inner.data_dir, &settings)?;
        *current = settings;

        Ok(())
    }

    /// Fetch anisette with the given setting and describe the machine Apple will see.
    pub fn test_anisette(&self, setting: AnisetteSetting) -> impl Future<Output = Result<String>> + use<> {
        let demo = self.inner.demo.clone();

        self.run(async move {
            if let Some(demo) = demo {
                return demo.test_anisette(setting).await;
            }

            match setting {
                AnisetteSetting::Remote { url } => {
                    let provider = sl_apple::anisette::RemoteAnisette::new(&url)
                        .map_err(|error| EngineError::Anisette(error.to_string()))?;
                    let headers =
                        provider.headers(None).await.map_err(|error| EngineError::Anisette(error.to_string()))?;

                    Ok(headers.description())
                }
                AnisetteSetting::Local => {
                    Err(EngineError::Unsupported("local anisette bridge is not implemented yet".into()))
                }
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

        let mut subscribers = self.subscribers.devices.lock();
        subscribers.retain(|sender| !sender.is_closed());
        subscribers.push(tx);

        rx
    }

    pub fn devices(&self) -> impl Future<Output = Result<Vec<DeviceInfo>>> + use<> {
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|d| d.devices()).ok_or_else(not_yet) })
    }

    pub fn device_apps(&self, udid: String) -> impl Future<Output = Result<Vec<DeviceApp>>> + use<> {
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|d| d.device_apps(&udid)).ok_or_else(not_yet) })
    }

    pub fn uninstall_app(&self, udid: String, bundle_id: String) -> impl Future<Output = Result<()>> + use<> {
        let _ = (udid, bundle_id);
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|_| ()).ok_or_else(not_yet) })
    }

    pub fn device_profiles(&self, udid: String) -> impl Future<Output = Result<Vec<DeviceProfile>>> + use<> {
        let demo = self.inner.demo.clone();
        self.run(async move { demo.map(|d| d.device_profiles(&udid)).ok_or_else(not_yet) })
    }

    pub fn remove_profile(&self, udid: String, uuid: String) -> impl Future<Output = Result<()>> + use<> {
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
        if let Some(demo) = &self.inner.demo {
            return Ok(demo.accounts());
        }

        let accounts = self.inner.accounts.lock();
        let summaries = accounts
            .values()
            .map(|account| {
                let mut summary = account.summary.clone();
                summary.has_session = account.session.is_some();

                summary
            })
            .collect();

        Ok(summaries)
    }

    /// Sign in (prompts for the password when `password` is `None`, and for 2FA codes as needed).
    pub fn login(&self, apple_id: String, password: Option<String>, remember: bool) -> JobHandle<AccountSummary> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.job(move |ctx| async move {
            match demo {
                Some(d) => d.login(ctx, apple_id, password, remember).await,
                None => auth::login(inner, ctx, apple_id, password, remember).await,
            }
        })
    }

    pub fn logout(&self, apple_id: String) -> impl Future<Output = Result<()>> + use<> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.run(async move {
            if let Some(demo) = demo {
                demo.logout(&apple_id);

                return Ok(());
            }

            inner.accounts.lock().remove(&apple_id);
            state::forget_account(&inner, &apple_id)
        })
    }

    /// Default location of the recovered Sideloadly `sessions.json`, if the platform has one.
    pub fn recovered_sessions_path() -> Option<PathBuf> {
        state::recovered_sessions_path()
    }

    /// Import GrandSlam sessions from a recovered Sideloadly `sessions.json`.
    pub fn import_sessions(&self, path: PathBuf) -> Result<SessionImport> {
        if self.inner.demo.is_some() {
            return Err(EngineError::Unsupported("the demo backend has no session storage".into()));
        }

        state::import_recovered_sessions(&self.inner, &path)
    }

    pub fn certificates(&self, apple_id: String) -> JobHandle<Vec<CertificateSummary>> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.job(move |context| async move {
            match demo {
                Some(demo) => Ok(demo.certificates(&apple_id)),
                None => portal::certificates(inner, context, apple_id).await,
            }
        })
    }

    pub fn revoke_certificate(&self, apple_id: String, serial: String) -> JobHandle<()> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.job(move |context| async move {
            match demo {
                Some(_) => Ok(()),
                None => portal::revoke_certificate(inner, context, apple_id, serial).await,
            }
        })
    }

    pub fn app_ids(&self, apple_id: String) -> JobHandle<Vec<AppIdSummary>> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.job(move |context| async move {
            match demo {
                Some(demo) => Ok(demo.app_ids(&apple_id)),
                None => portal::app_ids(inner, context, apple_id).await,
            }
        })
    }

    /// Devices registered with the account's selected team.
    pub fn registered_devices(&self, apple_id: String) -> JobHandle<Vec<RegisteredDevice>> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.job(move |context| async move {
            match demo {
                Some(demo) => Ok(demo.registered_devices()),
                None => portal::registered_devices(inner, context, apple_id).await,
            }
        })
    }

    // --------------------------------------------------------------------------------------------
    // Apps and jobs

    /// Read metadata and the icon of an `.ipa` without extracting it.
    pub fn inspect(&self, path: PathBuf) -> impl Future<Output = Result<AppSummary>> + use<> {
        let demo = self.inner.demo.clone();
        self.run(async move {
            match demo {
                Some(d) => Ok(d.inspect(path)),
                None => tokio::task::spawn_blocking(move || crate::pipeline::inspect(path, None))
                    .await
                    .map_err(|error| EngineError::Other(format!("inspection worker failed: {error}")))?,
            }
        })
    }

    /// Cancellable metadata inspection for interactive front ends.
    pub fn inspect_job(&self, path: PathBuf) -> JobHandle<AppSummary> {
        let demo = self.inner.demo.clone();

        self.job(move |context| async move {
            context.checkpoint()?;
            context.stage(crate::Stage::Preparing);

            match demo {
                Some(demo) => Ok(demo.inspect(path)),
                None => tokio::task::spawn_blocking(move || crate::pipeline::inspect(path, Some(&context)))
                    .await
                    .map_err(|error| EngineError::Other(format!("inspection worker failed: {error}")))?,
            }
        })
    }

    /// Run a sideload/export job.
    pub fn start(&self, spec: JobSpec) -> JobHandle<JobOutcome> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.job(move |ctx| async move {
            match demo {
                Some(d) => d.run_job(ctx, spec).await,
                None => sideload::run(inner, ctx, spec).await,
            }
        })
    }

    // --------------------------------------------------------------------------------------------
    // Installations

    pub fn installations(&self) -> Result<Vec<Installation>> {
        match &self.inner.demo {
            Some(demo) => Ok(demo.installations()),
            None => self.inner.store.installations(),
        }
    }

    pub fn set_auto_refresh(&self, installation_id: i64, enabled: bool) -> Result<()> {
        if let Some(demo) = &self.inner.demo {
            demo.set_auto_refresh(installation_id, enabled);

            return Ok(());
        }

        let mut installation = self
            .inner
            .store
            .installation(installation_id)?
            .ok_or_else(|| EngineError::Storage(format!("installation {installation_id} does not exist")))?;

        installation.auto_refresh = enabled;
        self.inner.store.update_installation(&installation)
    }

    /// Forget a tracked installation. Its stored IPA copy is deleted once no other installation
    /// refers to it; the app on the device is not touched.
    pub fn forget_installation(&self, installation_id: i64) -> Result<()> {
        if let Some(demo) = &self.inner.demo {
            demo.forget(installation_id);

            return Ok(());
        }

        let installation = self.inner.store.installation(installation_id)?;

        if !self.inner.store.delete_installation(installation_id)? {
            return Err(EngineError::Storage(format!("installation {installation_id} does not exist")));
        }

        if let Some(installation) = installation {
            files::release(&self.inner, &installation.spec.source)?;
        }

        Ok(())
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
        let mut subscribers = self.subscribers.refresh.lock();
        subscribers.retain(|sender| !sender.is_closed());
        subscribers.push(tx);

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
        let runtime = self.inner.runtime.clone();

        self.inner.runtime.handle.spawn(async move {
            let _runtime_lifetime = runtime;
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
        let runtime = self.inner.runtime.clone();

        self.inner.runtime.handle.spawn(async move {
            let _runtime_lifetime = runtime;
            // Workers cooperate with cancellation. Await their termination so a cancelled
            // result cannot race a still-running worker that owns an output transaction.
            let result = f(ctx).await;
            let _ = tx.send(result);
        });

        JobHandle::new(id, events, rx, cancel)
    }
}

fn not_yet() -> EngineError {
    EngineError::Unsupported("engine backend not implemented yet".into())
}
