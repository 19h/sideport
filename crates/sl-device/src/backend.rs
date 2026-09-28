//! `idevice` implementations of the installation traits and device utilities.

use crate::error::{DeviceError, Result};
use crate::install::{Connector, InstallStatus, Session, Staging};
use crate::mux::Mux;
use futures::FutureExt;
use futures::future::BoxFuture;
use idevice::afc::errors::AfcError;
use idevice::afc::opcode::AfcFopenMode;
use idevice::afc::{AfcClient, file::OwnedFileDescriptor};
use idevice::installation_proxy::InstallationProxyClient;
use idevice::lockdown::LockdownClient;
use idevice::misagent::MisagentClient;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::{IdeviceError, IdeviceService, ReadWrite};
use plist::{Dictionary, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Largest installation-proxy message accepted from a device.
const MAX_MESSAGE_BYTES: u32 = 16 * 1024 * 1024;

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

/// Length-prefixed plist messages (32-bit big-endian length, then the plist) over a service
/// socket, as lockdown services frame them.
struct PlistStream {
    socket: Box<dyn ReadWrite>,
}

impl PlistStream {
    fn new(socket: Box<dyn ReadWrite>) -> Self {
        Self { socket }
    }

    async fn send(&mut self, message: &Dictionary) -> Result<()> {
        let mut body = Vec::new();
        Value::Dictionary(message.clone())
            .to_writer_xml(&mut body)
            .map_err(|error| DeviceError::Protocol(error.to_string()))?;

        let length = u32::try_from(body.len()).map_err(|_| DeviceError::Protocol("message too large".into()))?;

        self.socket.write_all(&length.to_be_bytes()).await.map_err(io_error)?;
        self.socket.write_all(&body).await.map_err(io_error)?;
        self.socket.flush().await.map_err(io_error)
    }

    async fn receive(&mut self) -> Result<Dictionary> {
        let mut header = [0; 4];
        self.socket.read_exact(&mut header).await.map_err(io_error)?;

        let length = u32::from_be_bytes(header);

        if length > MAX_MESSAGE_BYTES {
            return Err(DeviceError::Protocol(format!("device message of {length} bytes exceeds 16 MiB")));
        }

        let mut body = vec![0; length as usize];
        self.socket.read_exact(&mut body).await.map_err(io_error)?;

        let value =
            Value::from_reader(std::io::Cursor::new(body)).map_err(|error| DeviceError::Protocol(error.to_string()))?;

        value.into_dictionary().ok_or_else(|| DeviceError::Protocol("device message is not a dictionary".into()))
    }
}

fn io_error(error: std::io::Error) -> DeviceError {
    DeviceError::Interrupted(error.to_string())
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
}

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
