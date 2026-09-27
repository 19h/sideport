//! Provisioning profile decoding (CMS-wrapped XML plist). CONTRACT.

use crate::{Error, Result};
use chrono::{DateTime, Utc};
use cms::{content_info::ContentInfo, signed_data::SignedData};
use const_oid::ObjectIdentifier;
use der::Decode;
use der::asn1::OctetString;
use std::io::Cursor;

const CMS_SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
const CMS_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");

/// A decoded .mobileprovision.
#[derive(Debug, Clone)]
pub struct ProvisioningProfile {
    /// Original CMS bytes (written verbatim to embedded.mobileprovision).
    pub raw: Vec<u8>,
    pub name: String,
    pub uuid: String,
    pub team_identifiers: Vec<String>,
    pub app_id_name: Option<String>,
    pub entitlements: plist::Dictionary,
    pub creation_date: DateTime<Utc>,
    pub expiration_date: DateTime<Utc>,
    pub time_to_live_days: Option<u64>,
    /// LocalProvision — true for free (personal team) profiles.
    pub local_provision: bool,
    pub provisioned_devices: Vec<String>,
    /// DER certificates listed in DeveloperCertificates.
    pub developer_certificates: Vec<Vec<u8>>,
}

impl ProvisioningProfile {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let payload = Self::payload(raw)?;
        let value = plist::Value::from_reader(Cursor::new(payload))?;
        let fields = value.as_dictionary().ok_or_else(|| profile_error("payload is not a dictionary"))?;

        let creation_date = date_field(fields, "CreationDate")?;
        let expiration_date = date_field(fields, "ExpirationDate")?;

        if expiration_date <= creation_date {
            return Err(profile_error("expiration must follow creation"));
        }

        let team_identifiers = string_array(fields, "TeamIdentifier", true)?;

        if team_identifiers.is_empty() {
            return Err(profile_error("empty TeamIdentifier"));
        }

        let entitlements = required_field(fields, "Entitlements", plist::Value::as_dictionary)?.clone();
        let certificates = required_field(fields, "DeveloperCertificates", plist::Value::as_array)?;

        let developer_certificates = certificates
            .iter()
            .map(|value| {
                value.as_data().map(<[u8]>::to_vec).ok_or_else(|| profile_error("non-data DeveloperCertificates entry"))
            })
            .collect::<Result<Vec<_>>>()?;

        let time_to_live_days = optional_field(fields, "TimeToLive", plist::Value::as_unsigned_integer)?;
        let local_provision = optional_field(fields, "LocalProvision", plist::Value::as_boolean)?.unwrap_or(false);
        let app_id_name = optional_field(fields, "AppIDName", plist::Value::as_string)?.map(str::to_owned);

        Ok(Self {
            raw: raw.to_vec(),
            name: required_field(fields, "Name", nonempty_string)?.to_owned(),
            uuid: required_field(fields, "UUID", nonempty_string)?.to_owned(),
            team_identifiers,
            app_id_name,
            entitlements,
            creation_date,
            expiration_date,
            time_to_live_days,
            local_provision,
            provisioned_devices: string_array(fields, "ProvisionedDevices", false)?,
            developer_certificates,
        })
    }

    /// application-identifier with the team prefix removed (may end in *).
    pub fn bundle_id(&self) -> Option<&str> {
        let identifier = self.entitlements.get("application-identifier")?.as_string()?;

        identifier.split_once('.').map(|(_, bundle)| bundle)
    }

    /// Decode the raw plist payload of the CMS envelope. This is structural decoding,
    /// not verification of the CMS signature or Apple's trust chain.
    pub fn payload(raw: &[u8]) -> Result<Vec<u8>> {
        let envelope = ContentInfo::from_der(raw).map_err(|error| profile_error(error.to_string()))?;

        if envelope.content_type != CMS_SIGNED_DATA {
            return Err(profile_error("CMS content type is not SignedData"));
        }

        let signed: SignedData = envelope.content.decode_as().map_err(|error| profile_error(error.to_string()))?;

        if signed.encap_content_info.econtent_type != CMS_DATA {
            return Err(profile_error("encapsulated CMS content type is not data"));
        }

        let content =
            signed.encap_content_info.econtent.ok_or_else(|| profile_error("detached CMS has no profile payload"))?;
        let octets: OctetString = content.decode_as().map_err(|error| profile_error(error.to_string()))?;

        Ok(octets.as_bytes().to_vec())
    }
}

fn required_field<'a, T>(
    fields: &'a plist::Dictionary,
    key: &str,
    decode: impl FnOnce(&'a plist::Value) -> Option<T>,
) -> Result<T> {
    fields.get(key).and_then(decode).ok_or_else(|| profile_error(format!("missing or invalid {key}")))
}

fn optional_field<'a, T>(
    fields: &'a plist::Dictionary,
    key: &str,
    decode: impl FnOnce(&'a plist::Value) -> Option<T>,
) -> Result<Option<T>> {
    let Some(value) = fields.get(key) else {
        return Ok(None);
    };

    decode(value).map(Some).ok_or_else(|| profile_error(format!("invalid {key}")))
}

fn nonempty_string(value: &plist::Value) -> Option<&str> {
    value.as_string().filter(|text| !text.is_empty())
}

fn date_field(fields: &plist::Dictionary, key: &str) -> Result<DateTime<Utc>> {
    let date = required_field(fields, key, plist::Value::as_date)?;

    Ok(DateTime::<Utc>::from(std::time::SystemTime::from(date)))
}

fn string_array(fields: &plist::Dictionary, key: &str, required: bool) -> Result<Vec<String>> {
    let Some(value) = fields.get(key) else {
        return if required { Err(profile_error(format!("missing {key}"))) } else { Ok(Vec::new()) };
    };

    let values = value.as_array().ok_or_else(|| profile_error(format!("invalid {key}")))?;

    values
        .iter()
        .map(|value| {
            nonempty_string(value).map(str::to_owned).ok_or_else(|| profile_error(format!("invalid {key} entry")))
        })
        .collect()
}

fn profile_error(message: impl Into<String>) -> Error {
    Error::Profile(message.into())
}
