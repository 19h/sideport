//! `idevice` implementations of the installation traits and device utilities.

use crate::error::{DeviceError, Result};
use crate::framing::PlistStream;
use crate::install::{Connector, InstallStatus, Session, Staging};
use crate::mux::Mux;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use idevice::afc::errors::AfcError;
use idevice::afc::opcode::AfcFopenMode;
use idevice::afc::{AfcClient, file::OwnedFileDescriptor};
use idevice::installation_proxy::InstallationProxyClient;
use idevice::lockdown::LockdownClient;
use idevice::misagent::MisagentClient;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::{IdeviceError, IdeviceService};
use plist::{Dictionary, Value};

/// Connects through the system usbmuxd.
#[derive(Debug, Clone)]
pub struct IdeviceConnector {
    mux: Mux,
}

impl IdeviceConnector {
    pub fn new(mux: Mux) -> Self {
        Self { mux }
    }

    pub fn mux(&self) -> &Mux {
        &self.mux
    }
}

impl Connector for IdeviceConnector {
    fn connect<'a>(&'a self, udid: &'a str, prefer_network: bool) -> BoxFuture<'a, Result<Box<dyn Session>>> {
        async move {
            let provider = self.mux.provider(udid, prefer_network).await?;

            let afc = AfcClient::connect(&provider).await?;
            let proxy = InstallationProxyClient::connect(&provider).await?;
            let socket = proxy.idevice.get_socket().ok_or(DeviceError::NotPaired)?;

            let session: Box<dyn Session> =
                Box::new(IdeviceSession { staging: AfcStaging::new(afc), proxy: PlistStream::new(socket) });

            Ok(session)
        }
        .boxed()
    }

    fn is_attached<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<bool>> {
        self.mux.is_attached(udid).boxed()
    }
}

struct IdeviceSession {
    staging: AfcStaging,
    proxy: PlistStream,
}

impl Session for IdeviceSession {
    fn staging(&mut self) -> &mut dyn Staging {
        &mut self.staging
    }

    fn install<'a>(
        &'a mut self,
        package: &'a str,
        bundle_id: &'a str,
        status: &'a (dyn Fn(InstallStatus) + Send + Sync),
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            let mut options = Dictionary::new();
            options.insert("CFBundleIdentifier".into(), bundle_id.into());

            let mut command = Dictionary::new();
            command.insert("Command".into(), "Install".into());
            command.insert("PackagePath".into(), package.into());
            command.insert("ClientOptions".into(), Value::Dictionary(options));

            self.proxy.send(&command).await?;

            loop {
                let message = self.proxy.receive().await?;

                if let Some(error) = install_error(&message) {
                    return Err(error);
                }

                let state = message.get("Status").and_then(Value::as_string).map(str::to_owned);
                let percent = message.get("PercentComplete").and_then(Value::as_unsigned_integer);

                if let Some(state) = state {
                    let complete = state == "Complete";
                    let percent = if complete { Some(100) } else { percent };

                    status(InstallStatus { status: state, percent });

                    if complete {
                        return Ok(());
                    }
                }
            }
        }
        .boxed()
    }
}

/// `Error`, `ErrorDescription` and `ErrorDetail` of an installation-proxy message.
pub fn install_error(message: &Dictionary) -> Option<DeviceError> {
    let name = message.get("Error")?;

    let name = match name {
        Value::String(name) => name.clone(),
        Value::Integer(code) => code.to_string(),
        _ => "UnknownError".into(),
    };

    let description = message.get("ErrorDescription").and_then(Value::as_string).map(str::to_owned);
    let detail = message.get("ErrorDetail").and_then(Value::as_unsigned_integer);

    Some(DeviceError::Install { name, description, detail })
}

/// AFC staging with one open append handle at a time.
struct AfcStaging {
    state: Option<AfcState>,
}

enum AfcState {
    Client(AfcClient),
    File(OwnedFileDescriptor),
}

impl AfcStaging {
    fn new(client: AfcClient) -> Self {
        Self { state: Some(AfcState::Client(client)) }
    }

    fn client(&mut self) -> Result<&mut AfcClient> {
        match &mut self.state {
            Some(AfcState::Client(client)) => Ok(client),
            _ => Err(DeviceError::Protocol("an AFC file is still open".into())),
        }
    }
}

fn not_found(error: &IdeviceError) -> bool {
    matches!(error, IdeviceError::Afc(AfcError::ObjectNotFound))
}

impl Staging for AfcStaging {
    fn file_size<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<Option<u64>>> {
        async move {
            match self.client()?.get_file_info(path).await {
                Ok(info) => Ok(Some(info.size as u64)),
                Err(error) if not_found(&error) => Ok(None),
                Err(error) => Err(error.into()),
            }
        }
        .boxed()
    }

    fn remove<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>> {
        async move {
            match self.client()?.remove(path).await {
                Ok(()) => Ok(()),
                Err(error) if not_found(&error) => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
        .boxed()
    }

    fn remove_all<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>> {
        async move {
            match self.client()?.remove_all(path).await {
                Ok(()) => Ok(()),
                Err(error) if not_found(&error) => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
        .boxed()
    }

    fn make_dirs<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>> {
        async move {
            let mut prefix = String::new();

            for component in path.split('/').filter(|component| !component.is_empty()) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }

                prefix.push_str(component);

                if self.file_size(&prefix).await?.is_none() {
                    self.client()?.mk_dir(prefix.clone()).await?;
                }
            }

            Ok(())
        }
        .boxed()
    }

    fn open_append<'a>(&'a mut self, path: &'a str) -> BoxFuture<'a, Result<()>> {
        async move {
            let Some(AfcState::Client(client)) = self.state.take() else {
                return Err(DeviceError::Protocol("an AFC file is already open".into()));
            };

            let file = client.open_owned(path, AfcFopenMode::Append).await?;
            self.state = Some(AfcState::File(file));

            Ok(())
        }
        .boxed()
    }

    fn write<'a>(&'a mut self, data: &'a [u8]) -> BoxFuture<'a, Result<()>> {
        async move {
            match &mut self.state {
                Some(AfcState::File(file)) => file.write_entire(data).await.map_err(DeviceError::from),
                _ => Err(DeviceError::Protocol("no AFC file is open".into())),
            }
        }
        .boxed()
    }

    fn close(&mut self) -> BoxFuture<'_, Result<()>> {
        async move {
            match self.state.take() {
                Some(AfcState::File(file)) => {
                    let client = file.close().await?;
                    self.state = Some(AfcState::Client(client));

                    Ok(())
                }
                other => {
                    self.state = other;

                    Ok(())
                }
            }
        }
        .boxed()
    }
}

/// Lockdown values shown for a device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceValues {
    pub name: String,
    pub product_type: String,
    pub product_version: String,
    pub build_version: String,
    pub device_class: String,
}

/// Read identity values over a paired lockdown session.
pub async fn device_values(provider: &UsbmuxdProvider) -> Result<DeviceValues> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    let pairing = provider.get_pairing_file().await.map_err(|_| DeviceError::NotPaired)?;
    lockdown.start_session(&pairing).await?;

    let mut value = async |key: &str| -> Result<String> {
        let value = lockdown.get_value(Some(key), None).await?;

        Ok(value.as_string().unwrap_or_default().to_owned())
    };

    Ok(DeviceValues {
        name: value("DeviceName").await?,
        product_type: value("ProductType").await?,
        product_version: value("ProductVersion").await?,
        build_version: value("BuildVersion").await.unwrap_or_default(),
        device_class: value("DeviceClass").await?,
    })
}

/// A user-installed application reported by the installation proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    pub bundle_id: String,
    pub name: String,
    pub version: Option<String>,
    /// Signed with a development or ad-hoc profile (`ProfileValidated` or `SignerIdentity`).
    pub developer: bool,
}

pub async fn installed_apps(provider: &UsbmuxdProvider) -> Result<Vec<InstalledApp>> {
    let mut proxy = InstallationProxyClient::connect(provider).await?;
    let apps = proxy.get_apps(Some("User"), None).await?;

    let mut listed: Vec<_> = apps
        .into_iter()
        .filter_map(|(bundle_id, info)| {
            let info = info.into_dictionary()?;
            let text = |key: &str| info.get(key).and_then(Value::as_string).map(str::to_owned);

            let developer = sideloaded(&info);

            Some(InstalledApp {
                name: text("CFBundleDisplayName").or_else(|| text("CFBundleName")).unwrap_or_else(|| bundle_id.clone()),
                version: text("CFBundleShortVersionString").or_else(|| text("CFBundleVersion")),
                bundle_id,
                developer,
            })
        })
        .collect();

    listed.sort_by_key(|app| app.name.to_lowercase());

    Ok(listed)
}

/// App Store apps are signed by `Apple iPhone OS Application Signing`; every other signer
/// (development, ad-hoc, enterprise) marks a sideloaded app. Observed on a device on
/// 2026-09-28: store apps carried that signer and no `ProfileValidated`; development-signed
/// apps carried their developer signer and `ProfileValidated = true`.
pub fn sideloaded(info: &Dictionary) -> bool {
    const APP_STORE_SIGNER: &str = "Apple iPhone OS Application Signing";

    let signer = info.get("SignerIdentity").and_then(Value::as_string);
    let validated = info.get("ProfileValidated").and_then(Value::as_boolean).unwrap_or(false);

    validated || signer != Some(APP_STORE_SIGNER)
}

pub async fn uninstall(provider: &UsbmuxdProvider, bundle_id: &str) -> Result<()> {
    let mut proxy = InstallationProxyClient::connect(provider).await?;

    proxy.uninstall(bundle_id, None).await.map_err(DeviceError::from)
}

/// Raw CMS profiles installed on the device (misagent `CopyAll`).
pub async fn profiles(provider: &UsbmuxdProvider) -> Result<Vec<Vec<u8>>> {
    let mut misagent = MisagentClient::connect(provider).await?;

    misagent.copy_all().await.map_err(DeviceError::from)
}

pub async fn remove_profile(provider: &UsbmuxdProvider, uuid: &str) -> Result<()> {
    let mut misagent = MisagentClient::connect(provider).await?;

    misagent.remove(uuid).await.map_err(DeviceError::from)
}

pub async fn install_profile(provider: &UsbmuxdProvider, profile: Vec<u8>) -> Result<()> {
    let mut misagent = MisagentClient::connect(provider).await?;

    misagent.install(profile).await.map_err(DeviceError::from)
}

/// Where an installed app lives, for the JIT launch/attach (recovered `GetPathForBundleId` +
/// `GetWorkingDirAndExeNameForBundleId`, via the installation proxy `Lookup`).
pub async fn app_launch(provider: &UsbmuxdProvider, bundle_id: &str) -> Result<crate::jit::AppLaunch> {
    let mut proxy = InstallationProxyClient::connect(provider).await?;
    let apps = proxy.get_apps(Some("Any"), Some(vec![bundle_id.to_owned()])).await?;

    let info = apps
        .get(bundle_id)
        .and_then(Value::as_dictionary)
        .ok_or_else(|| DeviceError::Protocol(format!("app {bundle_id} is not installed")))?;

    let text = |key: &str| info.get(key).and_then(Value::as_string).map(str::to_owned);

    let path = text("Path").ok_or_else(|| DeviceError::Protocol("app has no bundle path".into()))?;
    let executable = text("CFBundleExecutable").ok_or_else(|| DeviceError::Protocol("app has no executable".into()))?;

    Ok(crate::jit::AppLaunch { path, container: text("Container"), executable })
}

/// One heartbeat round trip (`Marco`/`Polo`); the returned interval proves the device is
/// reachable. Network and tvOS devices need a running heartbeat to keep services open.
pub async fn heartbeat(provider: &UsbmuxdProvider) -> Result<u64> {
    let mut client = idevice::heartbeat::HeartbeatClient::connect(provider).await?;

    let interval = client.get_marco(15).await?;
    client.send_polo().await?;

    Ok(interval)
}

/// Observe device notifications (recovered `notification_proxy`), e.g.
/// `com.apple.mobile.application_installed`, delivered until the stream is dropped.
pub async fn observe(provider: &UsbmuxdProvider, names: &[String]) -> Result<LineStream> {
    let mut client = idevice::notification_proxy::NotificationProxyClient::connect(provider).await?;

    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    client.observe_notifications(&borrowed).await?;

    let stream = client.into_stream().map(|item| item.map_err(DeviceError::from));

    Ok(Box::pin(stream))
}

/// Unpair from the device and drop the usbmuxd record (recovered `RepairPairing` first half).
pub async fn unpair(mux: &Mux, udid: &str) -> Result<()> {
    let provider = mux.provider(udid, false).await?;
    let pairing = provider.get_pairing_file().await.map_err(|_| DeviceError::NotPaired)?;
    let host_id = pairing.host_id.clone();

    let mut lockdown = LockdownClient::connect(&provider).await?;
    lockdown.unpair(host_id).await?;

    let mut connection = mux_connection(mux).await?;
    let _ = connection.delete_pair_record(udid).await;

    Ok(())
}

/// debugserver over `idevice`'s debug proxy (recovered `getDebugServer`: the TLS
/// `DVTSecureSocketProxy` service first, then plain `com.apple.debugserver`).
pub struct IdeviceDebugger {
    proxy: idevice::debug_proxy::DebugProxyClient<Box<dyn idevice::ReadWrite>>,
}

impl std::fmt::Debug for IdeviceDebugger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("IdeviceDebugger").finish_non_exhaustive()
    }
}

impl IdeviceDebugger {
    pub async fn connect(mux: &Mux, udid: &str) -> Result<Self> {
        let provider = mux.provider(udid, false).await?;

        let socket = match crate::service::open::<crate::service::SecureDebugServer>(&provider).await {
            Ok(socket) => socket,
            Err(_) => crate::service::open::<crate::service::DebugServer>(&provider).await?,
        };

        Ok(Self { proxy: idevice::debug_proxy::DebugProxyClient::new(socket) })
    }
}

impl crate::jit::Debugger for IdeviceDebugger {
    fn command<'a>(&'a mut self, name: &'a str, argv: &'a [String]) -> BoxFuture<'a, Result<Option<String>>> {
        async move {
            let command = idevice::debug_proxy::DebugserverCommand::new(name.to_owned(), argv.to_vec());
            let response = self.proxy.send_command(command).await?;

            crate::jit::interpret(name, response)
        }
        .boxed()
    }

    fn set_argv<'a>(&'a mut self, argv: &'a [String]) -> BoxFuture<'a, Result<String>> {
        async move { self.proxy.set_argv(argv.to_vec()).await.map_err(DeviceError::from) }.boxed()
    }
}

/// Everything the engine needs from a device layer: installation sessions plus discovery and
/// utilities. [`IdeviceBackend`] uses the system usbmuxd; tests provide fakes.
pub trait Backend: Connector {
    fn attached(&self) -> BoxFuture<'_, Result<Vec<crate::mux::Attached>>>;

    fn values<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<DeviceValues>>;

    fn watch(&self) -> BoxFuture<'_, Result<crate::mux::EventStream>>;

    fn apps<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<Vec<InstalledApp>>>;

    fn uninstall<'a>(&'a self, udid: &'a str, bundle_id: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Raw CMS profiles installed on the device.
    fn profiles<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<Vec<Vec<u8>>>>;

    fn remove_profile<'a>(&'a self, udid: &'a str, uuid: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Pair and store the record with usbmuxd; the device shows "Trust This Computer?".
    fn pair<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Unpair (drops the device and usbmuxd records) for pairing repair.
    fn unpair<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Lines from the device's syslog relay until the stream is dropped.
    fn syslog<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<LineStream>>;

    /// A connected image mounter for Developer Disk Image operations.
    fn image_mounter<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<Box<dyn crate::mounter::ImageMounting>>>;

    /// A connected debugserver for JIT.
    fn debugserver<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<Box<dyn crate::jit::Debugger>>>;

    /// Where an installed app lives, for the JIT launch/attach.
    fn app_launch<'a>(&'a self, udid: &'a str, bundle_id: &'a str) -> BoxFuture<'a, Result<crate::jit::AppLaunch>>;

    /// One heartbeat round trip; the interval proves the device is reachable.
    fn heartbeat<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<u64>>;

    /// Observe device notifications until the stream is dropped.
    fn observe<'a>(&'a self, udid: &'a str, names: &'a [String]) -> BoxFuture<'a, Result<LineStream>>;
}

/// Device log lines.
pub type LineStream = std::pin::Pin<Box<dyn futures::Stream<Item = Result<String>> + Send>>;

/// The system usbmuxd with `idevice` services.
#[derive(Debug, Clone)]
pub struct IdeviceBackend {
    connector: IdeviceConnector,
}

impl IdeviceBackend {
    pub fn system() -> Result<Self> {
        Ok(Self { connector: IdeviceConnector::new(Mux::system()?) })
    }

    fn mux(&self) -> &Mux {
        self.connector.mux()
    }
}

impl Connector for IdeviceBackend {
    fn connect<'a>(&'a self, udid: &'a str, prefer_network: bool) -> BoxFuture<'a, Result<Box<dyn Session>>> {
        self.connector.connect(udid, prefer_network)
    }

    fn is_attached<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<bool>> {
        self.connector.is_attached(udid)
    }
}

impl Backend for IdeviceBackend {
    fn attached(&self) -> BoxFuture<'_, Result<Vec<crate::mux::Attached>>> {
        self.mux().attached().boxed()
    }

    fn values<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<DeviceValues>> {
        async move { device_values(&self.mux().provider(udid, false).await?).await }.boxed()
    }

    fn watch(&self) -> BoxFuture<'_, Result<crate::mux::EventStream>> {
        self.mux().watch().boxed()
    }

    fn apps<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<Vec<InstalledApp>>> {
        async move { installed_apps(&self.mux().provider(udid, false).await?).await }.boxed()
    }

    fn uninstall<'a>(&'a self, udid: &'a str, bundle_id: &'a str) -> BoxFuture<'a, Result<()>> {
        async move { uninstall(&self.mux().provider(udid, false).await?, bundle_id).await }.boxed()
    }

    fn profiles<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<Vec<Vec<u8>>>> {
        async move { profiles(&self.mux().provider(udid, false).await?).await }.boxed()
    }

    fn remove_profile<'a>(&'a self, udid: &'a str, uuid: &'a str) -> BoxFuture<'a, Result<()>> {
        async move { remove_profile(&self.mux().provider(udid, false).await?, uuid).await }.boxed()
    }

    fn pair<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<()>> {
        async move { pair(self.mux(), udid).await }.boxed()
    }

    fn unpair<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<()>> {
        async move { unpair(self.mux(), udid).await }.boxed()
    }

    fn syslog<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<LineStream>> {
        async move { syslog(&self.mux().provider(udid, false).await?).await }.boxed()
    }

    fn image_mounter<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<Box<dyn crate::mounter::ImageMounting>>> {
        async move {
            let mounter = crate::mounter::IdeviceMounter::connect(self.mux(), udid).await?;
            let mounter: Box<dyn crate::mounter::ImageMounting> = Box::new(mounter);

            Ok(mounter)
        }
        .boxed()
    }

    fn debugserver<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<Box<dyn crate::jit::Debugger>>> {
        async move {
            let debugger = IdeviceDebugger::connect(self.mux(), udid).await?;
            let debugger: Box<dyn crate::jit::Debugger> = Box::new(debugger);

            Ok(debugger)
        }
        .boxed()
    }

    fn app_launch<'a>(&'a self, udid: &'a str, bundle_id: &'a str) -> BoxFuture<'a, Result<crate::jit::AppLaunch>> {
        async move { app_launch(&self.mux().provider(udid, false).await?, bundle_id).await }.boxed()
    }

    fn heartbeat<'a>(&'a self, udid: &'a str) -> BoxFuture<'a, Result<u64>> {
        async move { heartbeat(&self.mux().provider(udid, false).await?).await }.boxed()
    }

    fn observe<'a>(&'a self, udid: &'a str, names: &'a [String]) -> BoxFuture<'a, Result<LineStream>> {
        async move { observe(&self.mux().provider(udid, false).await?, names).await }.boxed()
    }
}

/// The syslog relay (`com.apple.syslog_relay`): NUL/newline-delimited lines.
pub async fn syslog(provider: &UsbmuxdProvider) -> Result<LineStream> {
    let client = idevice::syslog_relay::SyslogRelayClient::connect(provider).await?;

    let lines = futures::stream::unfold(Some(client), |state| async move {
        let mut client = state?;

        match client.next().await {
            Ok(line) => Some((Ok(line.trim_end_matches(['\0', '\n']).to_owned()), Some(client))),
            Err(error) => Some((Err(DeviceError::from(error)), None)),
        }
    });

    Ok(Box::pin(lines))
}

/// Lockdown pairing (the device asks "Trust This Computer?"), then store the record with
/// usbmuxd so every later connection finds it. The host identifier is a new UUID and the host
/// name the computer name, as libimobiledevice's `idevicepair` does.
async fn pair(mux: &Mux, udid: &str) -> Result<()> {
    let provider = mux.provider(udid, false).await?;
    let mut lockdown = LockdownClient::connect(&provider).await?;

    let mut connection = mux_connection(mux).await?;
    let buid = connection.get_buid().await?;

    let host_id = uuid_v4();
    let host_name = gethostname::gethostname().to_string_lossy().into_owned();
    let record = lockdown.pair(host_id, buid, Some(&host_name)).await?;
    let serialized = record.serialize()?;

    let mut connection = mux_connection(mux).await?;
    connection.save_pair_record(udid, serialized).await.map_err(DeviceError::from)
}

async fn mux_connection(mux: &Mux) -> Result<idevice::usbmuxd::UsbmuxdConnection> {
    mux.connection().await
}

fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string().to_uppercase()
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_app_store_signed_apps_are_not_sideloaded() {
        let app = |signer: Option<&str>, validated: Option<bool>| {
            let mut info = plist::Dictionary::new();

            if let Some(signer) = signer {
                info.insert("SignerIdentity".into(), signer.into());
            }

            if let Some(validated) = validated {
                info.insert("ProfileValidated".into(), validated.into());
            }

            super::sideloaded(&info)
        };

        assert!(!app(Some("Apple iPhone OS Application Signing"), None));
        assert!(app(Some("Apple Development: Fixture (TEAM123456)"), Some(true)));
        assert!(app(Some("iPhone Developer: Fixture (TEAM123456)"), None));
        assert!(app(None, None), "an unsigned or ad-hoc app is not from the App Store");
    }
}
