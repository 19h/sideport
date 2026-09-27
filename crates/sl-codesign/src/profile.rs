//! Provisioning profile decoding (CMS-wrapped XML plist). CONTRACT.
//!
//! Decoding and field validation are separate from trust: [`ProvisioningProfile::validate_for`]
//! checks decoded fields, while [`ProvisioningProfile::verify_trust`] checks the CMS signature,
//! certificate chain and signer policy.

use crate::trust::{self, ProfileTrust};
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use cms::{content_info::ContentInfo, signed_data::SignedData};
use const_oid::ObjectIdentifier;
use der::Decode;
use der::asn1::OctetString;
use std::io::Cursor;

const CMS_SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
const CMS_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");

const APPLICATION_IDENTIFIER: &str = "application-identifier";
const TEAM_IDENTIFIER: &str = "com.apple.developer.team-identifier";

/// A decoded .mobileprovision.
#[derive(Debug, Clone)]
pub struct ProvisioningProfile {
    /// Original CMS bytes (written verbatim to embedded.mobileprovision).
    pub raw: Vec<u8>,
    pub name: String,
    pub uuid: String,
    pub team_identifiers: Vec<String>,
    /// App ID prefixes, which can differ from team identifiers for older profiles.
    pub application_identifier_prefixes: Vec<String>,
    pub app_id_name: Option<String>,
    pub entitlements: plist::Dictionary,
    pub creation_date: DateTime<Utc>,
    pub expiration_date: DateTime<Utc>,
    pub time_to_live_days: Option<u64>,
    /// LocalProvision — true for free (personal team) profiles.
    pub local_provision: bool,
    /// Platform names such as iOS, tvOS or xrOS; empty when the profile omits the field.
    pub platforms: Vec<String>,
    /// ProvisionsAllDevices — in-house profiles are not limited to ProvisionedDevices.
    pub provisions_all_devices: bool,
    pub provisioned_devices: Vec<String>,
    /// DER certificates listed in DeveloperCertificates.
    pub developer_certificates: Vec<Vec<u8>>,
}

/// Inputs that must agree with a profile before it is embedded in a signed bundle.
#[derive(Debug, Clone, Copy)]
pub struct ProfileTarget<'a> {
    pub team_id: &'a str,
    pub bundle_id: &'a str,
    pub certificate_der: &'a [u8],
    /// Device that must be provisioned by the profile; `None` skips the device check.
    pub device_udid: Option<&'a str>,
    /// Required `Platform` entry, such as iOS or tvOS; `None` skips the platform check.
    pub platform: Option<&'a str>,
    pub now: DateTime<Utc>,
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
        let all_devices = optional_field(fields, "ProvisionsAllDevices", plist::Value::as_boolean)?.unwrap_or(false);
        let app_id_name = optional_field(fields, "AppIDName", plist::Value::as_string)?.map(str::to_owned);

        Ok(Self {
            raw: raw.to_vec(),
            name: required_field(fields, "Name", nonempty_string)?.to_owned(),
            uuid: required_field(fields, "UUID", nonempty_string)?.to_owned(),
            team_identifiers,
            application_identifier_prefixes: string_array(fields, "ApplicationIdentifierPrefix", false)?,
            app_id_name,
            entitlements,
            creation_date,
            expiration_date,
            time_to_live_days,
            local_provision,
            platforms: string_array(fields, "Platform", false)?,
            provisions_all_devices: all_devices,
            provisioned_devices: string_array(fields, "ProvisionedDevices", false)?,
            developer_certificates,
        })
    }

    /// application-identifier with the team prefix removed (may end in *).
    pub fn bundle_id(&self) -> Option<&str> {
        let identifier = self.entitlements.get(APPLICATION_IDENTIFIER)?.as_string()?;

        identifier.split_once('.').map(|(_, bundle)| bundle)
    }

    /// Check the profile's dates, team, App ID, platform, certificate and optional target device.
    /// This validates the decoded fields, not the CMS signature or Apple's trust chain.
    pub fn validate_for(&self, target: ProfileTarget<'_>) -> Result<()> {
        if target.now < self.creation_date || target.now >= self.expiration_date {
            return Err(profile_error("profile is not valid at the requested time"));
        }

        if target.team_id.is_empty() || !self.team_identifiers.iter().any(|team| team == target.team_id) {
            return Err(profile_error("profile team does not match the selected team"));
        }

        if let Some(team) = self.entitlements.get(TEAM_IDENTIFIER)
            && team.as_string() != Some(target.team_id)
        {
            return Err(profile_error("profile team entitlement does not match the selected team"));
        }

        let (prefix, pattern) = self.application_identifier()?;

        if !self.prefix_is_associated(prefix, target.team_id) {
            return Err(profile_error("profile App ID prefix is not associated with the profile"));
        }

        if !bundle_pattern_matches(pattern, target.bundle_id) {
            return Err(profile_error("profile App ID does not cover the bundle identifier"));
        }

        if let Some(platform) = target.platform
            && !self.platforms.is_empty()
            && !self.platforms.iter().any(|candidate| candidate == platform)
        {
            return Err(profile_error("profile does not support the target platform"));
        }

        if target.certificate_der.is_empty()
            || !self.developer_certificates.iter().any(|certificate| certificate == target.certificate_der)
        {
            return Err(profile_error("signing certificate is absent from the profile"));
        }

        if let Some(udid) = target.device_udid {
            self.check_device(udid)?;
        }

        Ok(())
    }

    /// Verify the CMS signature over the raw profile, the signer chain to a configured anchor and
    /// the signer naming policy. Certificates must be valid at the signed profile's CreationDate.
    /// This establishes who signed `raw`; it does not check the decoded fields for a target.
    pub fn verify_trust(&self, trust: &ProfileTrust) -> Result<()> {
        let signed = trust::SignedProfile::decode(&self.raw)?;
        signed.verify_signature()?;

        let signed_at = Self::parse(&self.raw)?.creation_date;

        signed.verify_chain(trust, signed_at)
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

    fn application_identifier(&self) -> Result<(&str, &str)> {
        let app_id = self
            .entitlements
            .get(APPLICATION_IDENTIFIER)
            .and_then(plist::Value::as_string)
            .ok_or_else(|| profile_error("missing application-identifier entitlement"))?;

        let (prefix, pattern) =
            app_id.split_once('.').ok_or_else(|| profile_error("invalid application-identifier entitlement"))?;

        if prefix.is_empty() || pattern.is_empty() {
            return Err(profile_error("invalid application-identifier entitlement"));
        }

        Ok((prefix, pattern))
    }

    /// A listed ApplicationIdentifierPrefix may be a legacy prefix that differs from the Team ID
    /// (TN2318). Without that list, only the modern association prefix == selected Team ID is proven.
    fn prefix_is_associated(&self, prefix: &str, team_id: &str) -> bool {
        if self.application_identifier_prefixes.is_empty() {
            return prefix == team_id;
        }

        self.application_identifier_prefixes.iter().any(|candidate| candidate == prefix)
    }

    fn check_device(&self, udid: &str) -> Result<()> {
        let target = normalize_udid(udid).ok_or_else(|| profile_error("target device UDID is malformed"))?;

        if self.provisions_all_devices {
            return Ok(());
        }

        let included = self.provisioned_devices.iter().any(|device| normalize_udid(device).as_ref() == Some(&target));

        if !included {
            return Err(profile_error("target device is absent from the profile"));
        }

        Ok(())
    }
}

/// Explicit App IDs compare exactly. A wildcard App ID ends with one asterisk that matches a
/// nonempty suffix, so `com.example.*` covers `com.example.app` but not `com.example`.
fn bundle_pattern_matches(pattern: &str, bundle_id: &str) -> bool {
    if bundle_id.is_empty() || bundle_id.contains('*') {
        return false;
    }

    let Some(stem) = pattern.strip_suffix('*') else {
        return pattern == bundle_id;
    };

    !stem.contains('*') && bundle_id.len() > stem.len() && bundle_id.starts_with(stem)
}

/// Matches the recovered `Impactor._format_udid`: casefold and remove hyphens. Inspected profiles
/// list 40-digit legacy UDIDs in lowercase and 24-digit UDIDs as uppercase 8-16 digit groups.
fn normalize_udid(udid: &str) -> Option<String> {
    let normalized: String =
        udid.chars().filter(|character| *character != '-').map(|character| character.to_ascii_lowercase()).collect();

    let hexadecimal = !normalized.is_empty() && normalized.chars().all(|character| character.is_ascii_hexdigit());

    hexadecimal.then_some(normalized)
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
