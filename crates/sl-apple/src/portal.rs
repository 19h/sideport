//! Typed client for the recovered developerservices2 QH65B2 actions.

mod records;

pub use records::{AppIdRecord, CertificateRecord, DeviceRecord, TeamKind, TeamRecord};

use crate::auth::{AnisetteProvider, AuthSession};
use crate::srp::XCODE_APP;
use crate::transport::{self, base_headers, cancellable, insert_header};
use crate::wire::{self, SecretValue, dictionary};
use crate::{Error, Result};
use plist::Value;
use reqwest::{Client, Url};
use std::fmt;
use std::net::IpAddr;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

const CLIENT_ID: &str = "XABBG36SBA";
const PROTOCOL: &str = "QH65B2";
const BASE_PATH: &str = "/services/QH65B2/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Ios,
    Tvos,
}

impl Platform {
    fn field(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Tvos => "tvos",
        }
    }
}

#[derive(Clone, Copy)]
pub struct PortalAccess<'a> {
    session: &'a AuthSession,
    provider: &'a dyn AnisetteProvider,
    cancellation: &'a CancellationToken,
}

impl<'a> PortalAccess<'a> {
    pub fn new(
        session: &'a AuthSession,
        provider: &'a dyn AnisetteProvider,
        cancellation: &'a CancellationToken,
    ) -> Self {
        Self { session, provider, cancellation }
    }
}

impl fmt::Debug for PortalAccess<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PortalAccess").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy)]
enum Action {
    ListTeams,
    ListDevices,
    AddDevice,
    ListAppIds,
    AddAppId,
    ListCertificates,
    SubmitCsr,
    RevokeCertificate,
    DownloadProfile,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::ListTeams => "listTeams",
            Self::ListDevices => "listDevices",
            Self::AddDevice => "addDevice",
            Self::ListAppIds => "listAppIds",
            Self::AddAppId => "addAppId",
            Self::ListCertificates => "listAllDevelopmentCerts",
            Self::SubmitCsr => "submitDevelopmentCSR",
            Self::RevokeCertificate => "revokeDevelopmentCert",
            Self::DownloadProfile => "downloadTeamProvisioningProfile",
        }
    }

    fn system(self) -> bool {
        !matches!(self, Self::ListTeams)
    }

    fn path(self) -> String {
        let prefix = if self.system() { "ios/" } else { "" };

        format!("{prefix}{}.action", self.name())
    }
}

#[derive(Clone)]
pub struct PortalClient {
    client: Client,
    origin: Url,
}

impl fmt::Debug for PortalClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PortalClient").finish_non_exhaustive()
    }
}

impl PortalClient {
    pub fn new() -> Result<Self> {
        Self::with_origin("https://developerservices2.apple.com/services/QH65B2/")
    }

    /// HTTP is accepted only for literal loopback fixtures.
    pub fn with_origin(origin: &str) -> Result<Self> {
        let origin = Url::parse(origin).map_err(|_| Error::Invalid("portal origin"))?;
        let host = origin.host_str().unwrap_or_default().trim_matches(['[', ']']);
        let loopback = host.parse::<IpAddr>().is_ok_and(|address| address.is_loopback());
        let permitted_scheme = origin.scheme() == "https" || origin.scheme() == "http" && loopback;
        let clean_origin = origin.path() == BASE_PATH
            && origin.query().is_none()
            && origin.fragment().is_none()
            && origin.username().is_empty()
            && origin.password().is_none();

        if host.is_empty() || !permitted_scheme || !clean_origin {
            return Err(Error::Invalid("portal origin"));
        }

        Ok(Self { client: transport::client_with_cookies(true)?, origin })
    }

    pub async fn list_teams(&self, access: PortalAccess<'_>) -> Result<Vec<TeamRecord>> {
        let response = self.request(Action::ListTeams, None, None, dictionary([]), access).await?;

        records::teams(&response.0)
    }

    pub async fn list_devices(
        &self,
        team_id: &str,
        platform: Platform,
        access: PortalAccess<'_>,
    ) -> Result<Vec<DeviceRecord>> {
        let response = self.request(Action::ListDevices, Some(team_id), Some(platform), dictionary([]), access).await?;

        records::devices(&response.0)
    }

    pub async fn add_device(
        &self,
        team_id: &str,
        platform: Platform,
        device_number: &str,
        name: &str,
        access: PortalAccess<'_>,
    ) -> Result<()> {
        checked_text(device_number, "device number", 128)?;
        checked_text(name, "device name", 256)?;

        let fields =
            dictionary([("deviceNumber", Value::String(device_number.into())), ("name", Value::String(name.into()))]);
        self.request(Action::AddDevice, Some(team_id), Some(platform), fields, access).await?;

        Ok(())
    }

    pub async fn list_app_ids(
        &self,
        team_id: &str,
        platform: Platform,
        access: PortalAccess<'_>,
    ) -> Result<Vec<AppIdRecord>> {
        let response = self.request(Action::ListAppIds, Some(team_id), Some(platform), dictionary([]), access).await?;

        records::app_ids(&response.0)
    }

    pub async fn add_app_id(
        &self,
        team_id: &str,
        platform: Platform,
        identifier: &str,
        name: &str,
        access: PortalAccess<'_>,
    ) -> Result<AppIdRecord> {
        checked_text(identifier, "app identifier", 255)?;
        checked_text(name, "app name", 255)?;

        let fields =
            dictionary([("identifier", Value::String(identifier.into())), ("name", Value::String(name.into()))]);
        let response = self.request(Action::AddAppId, Some(team_id), Some(platform), fields, access).await?;

        records::app_id(wire::dict(&response.0)?.get("appId").ok_or(Error::Invalid("appId"))?)
    }

    pub async fn list_certificates(
        &self,
        team_id: &str,
        platform: Platform,
        access: PortalAccess<'_>,
    ) -> Result<Vec<CertificateRecord>> {
        let response =
            self.request(Action::ListCertificates, Some(team_id), Some(platform), dictionary([]), access).await?;

        records::certificates(&response.0)
    }

    pub async fn submit_development_csr(
        &self,
        team_id: &str,
        platform: Platform,
        csr_pem: &str,
        machine_id: Uuid,
        machine_name: &str,
        access: PortalAccess<'_>,
    ) -> Result<String> {
        checked_csr(csr_pem)?;
        checked_text(machine_name, "machine name", 256)?;

        let fields = dictionary([
            ("csrContent", Value::String(csr_pem.into())),
            ("machineId", Value::String(machine_id.to_string())),
            ("machineName", Value::String(machine_name.into())),
        ]);
        let response = self.request(Action::SubmitCsr, Some(team_id), Some(platform), fields, access).await?;
        let response = wire::dict(&response.0)?;
        let certificate = wire::dict(response.get("certRequest").ok_or(Error::Invalid("certRequest"))?)?;

        Ok(wire::string(certificate, "serialNum")?.into())
    }

    pub async fn revoke_development_certificate(
        &self,
        team_id: &str,
        platform: Platform,
        serial_number: &str,
        access: PortalAccess<'_>,
    ) -> Result<()> {
        checked_text(serial_number, "certificate serial", 128)?;

        let fields = dictionary([("serialNumber", Value::String(serial_number.into()))]);
        self.request(Action::RevokeCertificate, Some(team_id), Some(platform), fields, access).await?;

        Ok(())
    }

    pub async fn download_profile(
        &self,
        team_id: &str,
        platform: Platform,
        app_id_id: &str,
        access: PortalAccess<'_>,
    ) -> Result<Zeroizing<Vec<u8>>> {
        checked_text(app_id_id, "app ID identifier", 128)?;

        let fields = dictionary([("appIdId", Value::String(app_id_id.into()))]);
        let response = self.request(Action::DownloadProfile, Some(team_id), Some(platform), fields, access).await?;
        let response = wire::dict(&response.0)?;
        let profile = wire::dict(response.get("provisioningProfile").ok_or(Error::Invalid("provisioningProfile"))?)?;

        Ok(Zeroizing::new(wire::data(profile, "encodedProfile")?.to_vec()))
    }

    async fn request(
        &self,
        action: Action,
        team_id: Option<&str>,
        platform: Option<Platform>,
        fields: Value,
        access: PortalAccess<'_>,
    ) -> Result<SecretValue> {
        let mut request = wire::dict(&fields)?.clone();
        request.insert("clientId".into(), Value::String(CLIENT_ID.into()));
        request.insert("protocolVersion".into(), Value::String(PROTOCOL.into()));
        request.insert("requestId".into(), Value::String(Uuid::new_v4().to_string()));
        request.insert("userLocale".into(), Value::String("en_US".into()));

        if action.system() {
            let team_id = team_id.ok_or(Error::Invalid("teamId"))?;
            let platform = platform.ok_or(Error::Invalid("platform"))?;
            checked_text(team_id, "teamId", 128)?;

            request.insert("teamId".into(), Value::String(team_id.into()));
            request.insert("DTDK_Platform".into(), Value::String(platform.field().into()));

            if platform == Platform::Tvos {
                request.insert("subPlatform".into(), Value::String("tvOS".into()));
            }
        }

        let body = SecretValue(Value::Dictionary(request));
        let mut body = wire::encode(&body.0)?;
        let anisette = cancellable(access.cancellation, access.provider.headers(access.session.username())).await?;
        let mut headers = base_headers("text/x-xml-plist")?;

        for (name, value) in anisette.iter() {
            insert_header(&mut headers, name, value)?;
        }

        insert_header(
            &mut headers,
            "X-Apple-I-Locale",
            anisette.get("X-Apple-Locale").ok_or(Error::Invalid("locale"))?,
        )?;
        insert_header(&mut headers, "X-Apple-I-Identity-Id", access.session.dsid())?;
        insert_header(&mut headers, "X-Apple-GS-Token", access.session.token())?;
        insert_header(&mut headers, "X-Apple-App-Info", XCODE_APP)?;

        let mut endpoint = self.origin.join(&action.path()).map_err(|_| Error::Invalid("portal endpoint"))?;
        endpoint.query_pairs_mut().append_pair("clientId", CLIENT_ID);
        let request = self.client.post(endpoint).headers(headers).body(std::mem::take(&mut *body));
        let bytes = cancellable(access.cancellation, async {
            let response = request.send().await?;

            transport::read_bounded(response, wire::MAX_BODY_BYTES).await
        })
        .await?;
        let bytes = Zeroizing::new(bytes);
        let response = wire::decode(&bytes, false)?;
        let dictionary = wire::dict(&response.0)?;
        let code = wire::integer(dictionary, "resultCode")?;

        if code != 0 {
            return Err(Error::Service { operation: action.name(), code });
        }

        Ok(response)
    }
}

/// `csrContent` is the PEM text itself, including its line breaks (recovered `public_bytes(PEM)`).
fn checked_csr(value: &str) -> Result<()> {
    let framed = value.trim_start().starts_with("-----BEGIN CERTIFICATE REQUEST-----")
        && value.trim_end().ends_with("-----END CERTIFICATE REQUEST-----");

    if !framed || value.len() > 65_536 || value.contains('\0') {
        return Err(Error::Invalid("CSR"));
    }

    Ok(())
}

fn checked_text(value: &str, name: &'static str, maximum: usize) -> Result<()> {
    if value.is_empty() || value.len() > maximum || value.contains(['\0', '\r', '\n']) {
        return Err(Error::Invalid(name));
    }

    Ok(())
}

#[cfg(test)]
mod tests;
