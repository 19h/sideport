//! The [`Engine`] facade. CONTRACT: method signatures are consumed by `sl-cli` and `sl-app`.
//!
//! All async methods are executor-agnostic: work is spawned onto the engine's own tokio runtime and the
//! returned future only awaits a oneshot, so gpui (or any executor) can drive it.

mod acquire;
mod anisette;
mod auth;
mod devices;
mod files;
mod ipc;
mod mac;
mod portal;
mod provision;
mod refresh;
mod sideload;
mod state;

use crate::demo::Demo;
use crate::error::{EngineError, Result};
use crate::job::{JobContext, JobHandle};
use crate::secrets::SecretStore;
use crate::store::Store;
use crate::types::*;
pub use anisette::MachineAnisette;
pub use devices::DeviceBackend;
use futures::channel::oneshot;
pub use ipc::IpcServer;
pub use mac::{MacTarget, MacTargetSetting};
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
    /// Device layer (default: the system usbmuxd).
    pub device_backend: Option<DeviceBackend>,
    /// Local anisette source (default: AOSKit on macOS, none elsewhere).
    pub machine_anisette: Option<MachineAnisette>,
    /// This Mac as an install target (default: detected with the system device layer).
    pub mac_target: MacTargetSetting,
    /// Login-item directory for autostart (default: the platform's LaunchAgents/autostart).
    pub autostart_dir: Option<PathBuf>,
    /// Private-service endpoints and feature-token key (default: none; nothing is contacted).
    pub services: Option<sl_services::ServiceConfig>,
}

/// Refresh notifications kept for a receiver that is not reading.
const REFRESH_BACKLOG: usize = 256;

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
    /// Settings as last read or written, with the identity of the file they came from.
    settings: RwLock<(Settings, crate::settings::Revision)>,
    auth_origin: String,
    portal_origin: String,
    store: Store,
    secrets: Box<dyn SecretStore>,
    profile_trust: sl_codesign::ProfileTrust,
    devices: devices::Devices,
    machine: Option<Arc<dyn sl_apple::anisette::MachineSource>>,
    mac_setting: MacTargetSetting,
    autostart_dir: Option<PathBuf>,
    mac: std::sync::OnceLock<Option<MacTarget>>,
    /// Private-service client (updates, feature tokens); `None` when nothing is configured.
    services: Option<sl_services::Services>,
    /// Serializes signing-key creation within this process; the store serializes processes.
    key_lock: Mutex<()>,
    accounts: Mutex<BTreeMap<String, LiveAccount>>,
    demo: Option<Demo>,
    next_job: AtomicU64,
    /// Messages for other processes' `/poll` and the sign-in waiting for `/tokens`.
    ipc: ipc::Mailbox,
    /// Owned by the [`Engine`] handles; jobs and background tasks publish through this weak
    /// reference, so dropping the last handle closes every subscription channel.
    subscribers: std::sync::Weak<Subscribers>,
}

#[derive(Debug, Default)]
struct Subscribers {
    devices: Mutex<Vec<async_channel::Sender<Vec<DeviceInfo>>>>,
    refresh: Mutex<Vec<async_channel::Sender<RefreshEvent>>>,
    ipc: Mutex<Vec<async_channel::Sender<crate::ipc::IpcEvent>>>,
}

impl Inner {
    /// Current settings, reloaded when another process saved them. A file that cannot be read
    /// keeps the last good settings.
    fn settings(&self) -> Settings {
        let revision = crate::settings::revision(&self.data_dir);

        {
            let cached = self.settings.read();

            if cached.1 == revision {
                return cached.0.clone();
            }
        }

        let mut cached = self.settings.write();

        if let Ok(loaded) = crate::settings::load(&self.data_dir) {
            *cached = loaded;
        }

        cached.0.clone()
    }

    /// Change the settings as stored now. The state database's writer lock excludes other
    /// processes' settings changes meanwhile.
    fn modify_settings(&self, change: impl FnOnce(&mut Settings) -> Result<()>) -> Result<Settings> {
        let mut cached = self.settings.write();

        self.store.exclusive(|| {
            let mut settings = match crate::settings::load(&self.data_dir) {
                Ok((settings, _)) => settings,
                Err(_) => cached.0.clone(),
            };

            change(&mut settings)?;

            let revision = crate::settings::save(&self.data_dir, &settings)?;
            *cached = (settings.clone(), revision);

            Ok(settings)
        })
    }

    /// This Mac as an install target, detected once.
    fn mac(&self) -> Option<&MacTarget> {
        self.mac
            .get_or_init(|| match &self.mac_setting {
                MacTargetSetting::Detect => mac::detect(),
                MacTargetSetting::Disabled => None,
                MacTargetSetting::Fixed(target) => Some(target.clone()),
            })
            .as_ref()
    }

    /// Send a device snapshot to subscribers, replacing one they have not received yet; `false`
    /// once every engine handle is gone.
    fn publish_devices(&self, devices: &[DeviceInfo]) -> bool {
        let Some(subscribers) = self.subscribers.upgrade() else {
            return false;
        };

        subscribers.devices.lock().retain(|sender| sender.force_send(devices.to_vec()).is_ok());

        true
    }

    /// Send a refresh notification; a receiver more than [`REFRESH_BACKLOG`] behind loses the
    /// oldest ones.
    fn publish_refresh(&self, event: &RefreshEvent) {
        if let Some(subscribers) = self.subscribers.upgrade() {
            subscribers.refresh.lock().retain(|sender| sender.force_send(event.clone()).is_ok());
        }
    }

    fn publish_ipc(&self, event: &crate::ipc::IpcEvent) {
        if let Some(subscribers) = self.subscribers.upgrade() {
            subscribers.ipc.lock().retain(|sender| sender.force_send(event.clone()).is_ok());
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

        let services = match config.services {
            Some(config) => {
                Some(sl_services::Services::new(config).map_err(|error| EngineError::Other(error.to_string()))?)
            }
            None => None,
        };

        let subscribers = Arc::new(Subscribers::default());
        let scheduler = !config.disable_scheduler && !config.demo;

        let engine = Self {
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
                mac_setting: match (&config.mac_target, &config.device_backend) {
                    (MacTargetSetting::Detect, Some(_)) => MacTargetSetting::Disabled,
                    (setting, _) => setting.clone(),
                },
                mac: std::sync::OnceLock::new(),
                services,
                autostart_dir: config.autostart_dir.or_else(crate::autostart::default_directory),
                devices: devices::Devices::new(config.device_backend),
                machine: config
                    .machine_anisette
                    .map(|MachineAnisette(source)| source)
                    .or_else(anisette::default_machine),
                key_lock: Mutex::new(()),
                accounts: Mutex::new(accounts),
                demo,
                next_job: AtomicU64::new(1),
                ipc: ipc::Mailbox::new(),
                subscribers: Arc::downgrade(&subscribers),
            }),
        };

        if scheduler {
            refresh::start(&engine.inner);
        }

        Ok(engine)
    }

    pub fn is_demo(&self) -> bool {
        self.inner.demo.is_some()
    }

    pub fn data_dir(&self) -> PathBuf {
        self.inner.data_dir.clone()
    }

    // --------------------------------------------------------------------------------------------
    // Settings

    /// Current settings; another process's saved changes are picked up.
    pub fn settings(&self) -> Settings {
        self.inner.settings()
    }

    /// Replace every setting. Prefer [`Engine::modify_settings`], which keeps fields another
    /// process changed meanwhile.
    pub fn update_settings(&self, settings: Settings) -> Result<()> {
        self.inner.modify_settings(|current| {
            *current = settings;

            Ok(())
        })?;

        Ok(())
    }

    /// Apply `change` to the stored settings as they are now, under a lock shared with other
    /// processes, and save them. Nothing is saved when `change` fails. Returns the saved settings.
    pub fn modify_settings(&self, change: impl FnOnce(&mut Settings) -> Result<()>) -> Result<Settings> {
        self.inner.modify_settings(change)
    }

    /// Whether the refresh scheduler starts at login.
    pub fn autostart(&self) -> bool {
        self.inner.demo.is_none() && self.inner.autostart_dir.as_deref().is_some_and(crate::autostart::is_enabled)
    }

    /// Start `<program> daemon` at login (LaunchAgent on macOS, XDG autostart on Linux).
    pub fn set_autostart(&self, enabled: bool, program: &std::path::Path) -> Result<()> {
        if self.inner.demo.is_some() {
            return Err(EngineError::Unsupported("the demo backend does not install login items".into()));
        }

        let directory = self
            .inner
            .autostart_dir
            .as_deref()
            .ok_or_else(|| EngineError::Unsupported("autostart is not supported on this platform".into()))?;

        crate::autostart::set(directory, program, Some(&self.inner.data_dir), enabled)
    }

    /// Fetch anisette with the given setting and describe the machine Apple will see.
    pub fn test_anisette(&self, setting: AnisetteSetting) -> impl Future<Output = Result<String>> + use<> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.run(async move {
            if let Some(demo) = demo {
                return demo.test_anisette(setting).await;
            }

            // A remote check sends an empty `u` parameter, as the recovered client does.
            if let AnisetteSetting::Remote { url } = &setting {
                let remote = sl_apple::anisette::RemoteAnisette::new(url)
                    .map_err(|error| EngineError::Anisette(error.to_string()))?;
                let headers = remote.headers(None).await.map_err(|error| EngineError::Anisette(error.to_string()))?;

                return Ok(headers.description());
            }

            let provider = anisette::provider(&inner, &setting)?;

            anisette::describe(provider.as_ref(), "").await
        })
    }

    // --------------------------------------------------------------------------------------------
    // Private services (updates and feature tokens)

    /// What private services are configured and which features are currently unlocked. With no
    /// [`EngineConfig::services`] set, everything reports unconfigured and no features are granted.
    pub fn services_status(&self) -> sl_services::ServicesStatus {
        match &self.inner.services {
            Some(services) => services.status(),
            None => sl_services::ServicesStatus {
                updates_configured: false,
                token_verifier_configured: false,
                feature_state: sl_services::FeatureState::default(),
            },
        }
    }

    /// Check for an application update. Returns [`sl_services::UpdateStatus::NotConfigured`] when no
    /// update endpoints are configured; nothing is contacted in that case.
    pub fn check_update(&self) -> impl Future<Output = Result<sl_services::UpdateStatus>> + use<> {
        let inner = self.inner.clone();

        self.run(async move {
            match &inner.services {
                Some(services) => {
                    services.check_update().await.map_err(|error| EngineError::Network(error.to_string()))
                }
                None => Ok(sl_services::UpdateStatus::NotConfigured),
            }
        })
    }

    /// Wait for the browser to return a feature token to the local IPC `/tokens` route (the app
    /// must serve IPC), then verify it into feature state. The front end opens the configured
    /// sign-in page; a later wait replaces this one.
    pub fn receive_feature_token(&self) -> impl Future<Output = Result<sl_services::FeatureState>> + use<> {
        let token = self.await_sign_in_token();
        let engine = self.clone();

        async move { engine.apply_feature_token(token.await?) }
    }

    /// Validate a feature token received by the local IPC `/tokens` route into feature state. The
    /// IPC server owns the HTTP listener; this is the services-side step it calls with the token.
    pub fn apply_feature_token(&self, token: String) -> Result<sl_services::FeatureState> {
        let services = self
            .inner
            .services
            .as_ref()
            .ok_or_else(|| EngineError::Unsupported("private services are not configured".into()))?;

        services.apply_token(&token, chrono::Utc::now()).map_err(|error| EngineError::Other(error.to_string()))
    }

    // --------------------------------------------------------------------------------------------
    // Devices

    /// Current device list, then a fresh snapshot whenever it changes. Only the latest snapshot
    /// is kept for a slow receiver.
    pub fn subscribe_devices(&self) -> async_channel::Receiver<Vec<DeviceInfo>> {
        let (tx, rx) = async_channel::bounded(1);

        if let Some(demo) = &self.inner.demo {
            let _ = tx.force_send(demo.devices());

            return rx;
        }

        // Register before the watcher starts and read the cached snapshot under the subscriber
        // lock: a concurrent publication then either precedes the read or reaches this sender.
        {
            let mut subscribers = self.subscribers.devices.lock();

            if let Some(snapshot) = self.inner.devices.snapshot() {
                let _ = tx.force_send(snapshot);
            }

            subscribers.retain(|sender| !sender.is_closed());
            subscribers.push(tx);
        }

        devices::watch(&self.inner);

        rx
    }

    pub fn devices(&self) -> impl Future<Output = Result<Vec<DeviceInfo>>> + use<> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.run(async move {
            match demo {
                Some(demo) => Ok(demo.devices()),
                None => devices::list(&inner).await,
            }
        })
    }

    pub fn device_apps(&self, udid: String) -> impl Future<Output = Result<Vec<DeviceApp>>> + use<> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.run(async move {
            match demo {
                Some(demo) => Ok(demo.device_apps(&udid)),
                None => devices::apps(&inner, &udid).await,
            }
        })
    }

    pub fn uninstall_app(&self, udid: String, bundle_id: String) -> impl Future<Output = Result<()>> + use<> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.run(async move {
            match demo {
                Some(_) => Ok(()),
                None => devices::uninstall(&inner, &udid, &bundle_id).await,
            }
        })
    }

    pub fn device_profiles(&self, udid: String) -> impl Future<Output = Result<Vec<DeviceProfile>>> + use<> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.run(async move {
            match demo {
                Some(demo) => Ok(demo.device_profiles(&udid)),
                None => devices::profiles(&inner, &udid).await,
            }
        })
    }

    pub fn remove_profile(&self, udid: String, uuid: String) -> impl Future<Output = Result<()>> + use<> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.run(async move {
            match demo {
                Some(_) => Ok(()),
                None => devices::remove_profile(&inner, &udid, &uuid).await,
            }
        })
    }

    /// Stream the device syslog as job log events until the job is cancelled; `filter` keeps
    /// only lines containing it (case-insensitive).
    pub fn syslog(&self, udid: String, filter: Option<String>) -> JobHandle<()> {
        let inner = self.inner.clone();

        self.job(move |context| async move { devices::syslog(&inner, &context, &udid, filter.as_deref()).await })
    }

    /// Start pairing (shows the "Trust This Computer?" dialog on the device).
    pub fn pair_device(&self, udid: String) -> impl Future<Output = Result<()>> + use<> {
        let demo = self.inner.demo.clone();
        let inner = self.inner.clone();

        self.run(async move {
            match demo {
                Some(_) => Ok(()),
                None => devices::pair(&inner, &udid).await,
            }
        })
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

    /// Choose the team jobs use for an account without asking; `None` asks again at the next
    /// job. The team must be one of the account's known teams.
    pub fn set_default_team(&self, apple_id: &str, team_id: Option<String>) -> Result<()> {
        if let Some(demo) = &self.inner.demo {
            return demo.set_default_team(apple_id, team_id);
        }

        let summary = {
            let mut accounts = self.inner.accounts.lock();
            let account =
                accounts.get_mut(apple_id).ok_or_else(|| EngineError::Auth(format!("{apple_id} is not signed in")))?;

            if let Some(team_id) = &team_id
                && !account.summary.teams.iter().any(|team| &team.team_id == team_id)
            {
                return Err(EngineError::Other(format!("{apple_id} has no team {team_id}")));
            }

            account.summary.default_team = team_id;
            account.summary.clone()
        };

        state::save_summary(&self.inner, &summary)
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

    /// Download a `sideloadly:` link or HTTP(S) IPA URL into the downloads directory.
    pub fn download(&self, source: String) -> JobHandle<PathBuf> {
        let inner = self.inner.clone();

        self.job(move |context| async move { acquire::fetch(&inner, &context, &source).await })
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
        let inner = self.inner.clone();

        self.job(move |ctx| async move {
            match demo {
                Some(d) => d.refresh(ctx, installation_id).await,
                None => refresh::refresh(inner, ctx, installation_id).await,
            }
        })
    }

    /// Run one scheduler pass now: queue due installations on reachable devices and refresh
    /// them without interaction. Returns the number refreshed successfully.
    pub fn refresh_due(&self) -> impl Future<Output = Result<usize>> + use<> {
        let demo = self.inner.demo.is_some();
        let inner = self.inner.clone();

        self.run(async move { if demo { Ok(0) } else { refresh::tick(&inner).await } })
    }

    /// Background refresh notifications.
    pub fn subscribe_refresh(&self) -> async_channel::Receiver<RefreshEvent> {
        let (tx, rx) = async_channel::bounded(REFRESH_BACKLOG);
        let mut subscribers = self.subscribers.refresh.lock();
        subscribers.retain(|sender| !sender.is_closed());
        subscribers.push(tx);

        rx
    }

    // --------------------------------------------------------------------------------------------
    // Local IPC

    /// Serve local IPC for this data directory on `port` (default [`crate::ipc::DEFAULT_PORT`];
    /// 0 picks a free port). Fails when the port is taken, typically by a running Sideport, which
    /// [`Engine::ipc_client`] can then reach. The server stops when the handle is dropped.
    pub fn serve_ipc(&self, port: Option<u16>) -> Result<IpcServer> {
        ipc::serve(&self.inner, port.unwrap_or(crate::ipc::DEFAULT_PORT))
    }

    /// A client for the process serving this data directory's IPC.
    pub fn ipc_client(&self, port: Option<u16>) -> Result<crate::ipc::IpcClient> {
        crate::ipc::IpcClient::new(&self.inner.data_dir, port.unwrap_or(crate::ipc::DEFAULT_PORT))
    }

    /// Requests other processes made of this one through its IPC server.
    pub fn subscribe_ipc(&self) -> async_channel::Receiver<crate::ipc::IpcEvent> {
        let (tx, rx) = async_channel::bounded(16);
        let mut subscribers = self.subscribers.ipc.lock();
        subscribers.retain(|sender| !sender.is_closed());
        subscribers.push(tx);

        rx
    }

    /// Leave a message for a process polling this one; the ten newest are kept.
    pub fn leave_message(&self, message: impl Into<String>) {
        self.inner.ipc.leave(message.into());
    }

    /// Wait for the browser to return a sign-in token to `/tokens`. A later call replaces this
    /// waiter, which then resolves to [`EngineError::Cancelled`].
    pub fn await_sign_in_token(&self) -> impl Future<Output = Result<String>> + use<> {
        let token = self.inner.ipc.expect_sign_in();

        async move { token.await.map_err(|_| EngineError::Cancelled) }
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
