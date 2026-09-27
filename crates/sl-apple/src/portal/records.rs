use crate::wire;
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use plist::{Dictionary, Value};
use std::collections::BTreeSet;
use std::time::SystemTime;

const MAX_RECORDS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamKind {
    Free,
    Individual,
    Organization,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRecord {
    pub team_id: String,
    pub name: String,
    pub kind: TeamKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub device_number: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIdRecord {
    pub app_id_id: String,
    pub identifier: String,
    pub name: String,
    pub expiration: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateRecord {
    pub serial_number: String,
    pub machine_name: Option<String>,
    pub expiration: Option<DateTime<Utc>>,
    pub content_der: Option<Vec<u8>>,
}

pub(super) fn teams(response: &Value) -> Result<Vec<TeamRecord>> {
    let teams = rows(response, "teams", |value| {
        let fields = wire::dict(value)?;
        let team_id = checked_string(fields, "teamId", 128)?;
        let name = checked_string(fields, "name", 256)?;
        let portal_type = checked_string(fields, "type", 128)?;
        let memberships = match fields.get("memberships") {
            Some(Value::Array(memberships)) => memberships.as_slice(),
            None => &[],
            _ => return Err(Error::Invalid("team memberships")),
        };
        let free = if memberships.len() == 1 {
            let membership = wire::dict(&memberships[0])?;
            checked_string(membership, "name", 256)?.to_lowercase().contains("free")
        } else {
            false
        };
        let kind = match portal_type.as_str() {
            "Company/Organization" => TeamKind::Organization,
            _ if free => TeamKind::Free,
            "Individual" => TeamKind::Individual,
            _ => TeamKind::Other(portal_type),
        };

        Ok(TeamRecord { team_id, name, kind })
    })?;

    unique_records(teams, |team| &team.team_id, "duplicate team ID")
}

pub(super) fn devices(response: &Value) -> Result<Vec<DeviceRecord>> {
    let devices = rows(response, "devices", |value| {
        let fields = wire::dict(value)?;

        Ok(DeviceRecord {
            device_number: checked_string(fields, "deviceNumber", 128)?,
            name: checked_string(fields, "name", 256)?,
        })
    })?;

    unique_records(devices, |device| &device.device_number, "duplicate device number")
}

pub(super) fn app_ids(response: &Value) -> Result<Vec<AppIdRecord>> {
    let app_ids = rows(response, "appIds", app_id)?;

    unique_records(app_ids, |app_id| &app_id.app_id_id, "duplicate app ID")
}

pub(super) fn app_id(value: &Value) -> Result<AppIdRecord> {
    let fields = wire::dict(value)?;

    Ok(AppIdRecord {
        app_id_id: checked_string(fields, "appIdId", 128)?,
        identifier: checked_string(fields, "identifier", 255)?,
        name: checked_string(fields, "name", 255)?,
        expiration: optional_date(fields, "expirationDate")?,
    })
}

pub(super) fn certificates(response: &Value) -> Result<Vec<CertificateRecord>> {
    let certificates = rows(response, "certificates", |value| {
        let fields = wire::dict(value)?;
        let content_der = match fields.get("certContent") {
            Some(Value::Data(data)) => Some(data.clone()),
            None => None,
            _ => return Err(Error::Invalid("certContent")),
        };

        Ok(CertificateRecord {
            serial_number: checked_string(fields, "serialNumber", 128)?,
            machine_name: optional_string(fields, "machineName", 256)?,
            expiration: optional_date(fields, "expirationDate")?,
            content_der,
        })
    })?;

    unique_records(certificates, |certificate| &certificate.serial_number, "duplicate certificate serial")
}

fn unique_records<T>(records: Vec<T>, key: impl Fn(&T) -> &str, error: &'static str) -> Result<Vec<T>> {
    let mut seen = BTreeSet::new();

    for record in &records {
        if !seen.insert(key(record)) {
            return Err(Error::Invalid(error));
        }
    }

    Ok(records)
}

fn rows<T>(response: &Value, key: &'static str, parse: impl Fn(&Value) -> Result<T>) -> Result<Vec<T>> {
    let fields = wire::dict(response)?;
    let values = fields.get(key).and_then(Value::as_array).ok_or(Error::Invalid(key))?;

    if values.len() > MAX_RECORDS {
        return Err(Error::Invalid("portal record count"));
    }

    values.iter().map(parse).collect()
}

fn checked_string(fields: &Dictionary, key: &'static str, maximum: usize) -> Result<String> {
    let value = wire::string(fields, key)?;

    if value.len() > maximum || value.contains(['\r', '\n']) {
        return Err(Error::Invalid(key));
    }

    Ok(value.into())
}

fn optional_string(fields: &Dictionary, key: &'static str, maximum: usize) -> Result<Option<String>> {
    match fields.get(key) {
        Some(_) => checked_string(fields, key, maximum).map(Some),
        None => Ok(None),
    }
}

fn optional_date(fields: &Dictionary, key: &'static str) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = fields.get(key) else {
        return Ok(None);
    };
    let date = match value {
        Value::Date(date) => DateTime::<Utc>::from(SystemTime::from(*date)),
        Value::String(date) if date.eq_ignore_ascii_case("never") => return Ok(None),
        Value::String(date) => DateTime::parse_from_rfc3339(date).map_err(|_| Error::Invalid(key))?.with_timezone(&Utc),
        _ => return Err(Error::Invalid(key)),
    };

    Ok(Some(date))
}
