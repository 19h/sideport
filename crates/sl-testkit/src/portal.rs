//! A stateful developer-portal fake for the recovered QH65B2 actions.
//!
//! It issues development certificates for submitted CSRs, enforces a certificate limit with
//! result code 7460, records every request, and returns CMS-signed team profiles that list the
//! registered devices and issued certificates. It models the recovered client contract only;
//! Apple's current service behavior is unknown.

use crate::pki::{DevelopmentAuthority, ProfileChain};
use crate::profile::ProfileFields;
use chrono::{DateTime, Duration, Utc};
use der::{Decode, DecodePem, Encode};
use parking_lot::{Mutex, MutexGuard};
use plist::{Dictionary, Value};
use std::sync::Arc;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
use x509_cert::request::CertReq;

pub const CERTIFICATE_LIMIT: i64 = 7460;
pub const SESSION_EXPIRED: i64 = 1100;

#[derive(Debug, Clone)]
pub struct PortalCertificate {
    pub serial: String,
    pub machine_name: String,
    pub machine_id: Option<String>,
    pub expiration: DateTime<Utc>,
    pub der: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PortalAppId {
    pub app_id_id: String,
    pub identifier: String,
    pub name: String,
    pub expiration: Option<DateTime<Utc>>,
}

/// One received request: action path and plist fields.
#[derive(Debug, Clone)]
pub struct PortalRequest {
    pub action: String,
    pub fields: Dictionary,
}

impl PortalRequest {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).and_then(Value::as_string)
    }
}

#[derive(Debug)]
pub struct PortalState {
    pub team_id: String,
    pub team_name: String,
    pub free: bool,
    pub devices: Vec<(String, String)>,
    pub certificates: Vec<PortalCertificate>,
    pub app_ids: Vec<PortalAppId>,
    pub certificate_limit: usize,
    pub app_id_limit: Option<usize>,
    /// Answer this many upcoming portal requests with the session-expired code.
    pub expire_sessions: usize,
    /// Profile validity in days.
    pub profile_days: i64,
    pub requests: Vec<PortalRequest>,
    next_serial: u32,
    next_app_id: u32,
}

impl PortalState {
    pub fn actions(&self) -> Vec<&str> {
        self.requests.iter().map(|request| request.action.as_str()).collect()
    }

    pub fn count(&self, action: &str) -> usize {
        self.requests.iter().filter(|request| request.action == action).count()
    }
}

#[derive(Debug, Clone)]
pub struct FakePortal {
    state: Arc<Mutex<PortalState>>,
}

impl FakePortal {
    pub fn new(team_id: &str, free: bool) -> Self {
        let state = PortalState {
            team_id: team_id.into(),
            team_name: if free { "Fixture (Personal Team)".into() } else { "Fixture Team".into() },
            free,
            devices: Vec::new(),
            certificates: Vec::new(),
            app_ids: Vec::new(),
            certificate_limit: 2,
            app_id_limit: free.then_some(10),
            expire_sessions: 0,
            profile_days: 7,
            requests: Vec::new(),
            next_serial: 0x100,
            next_app_id: 1,
        };

        Self { state: Arc::new(Mutex::new(state)) }
    }

    pub fn state(&self) -> MutexGuard<'_, PortalState> {
        self.state.lock()
    }

    /// The portal origin to configure on a client for `server`.
    pub fn origin(server: &MockServer) -> String {
        format!("{}/services/QH65B2/", server.uri())
    }

    /// Mount the portal and a remote-anisette endpoint at `/anisette`.
    pub async fn mount(&self, server: &MockServer) {
        let headers = serde_json::json!({
            "X-Apple-I-MD": "fixture-otp",
            "X-Apple-I-MD-M": "fixture-machine-token",
            "X-Mme-Device-Id": "fixture-device",
            "X-MMe-Client-Info": "<MacBookPro18,3> <macOS;14.0;23A344> <com.apple.AuthKit/1 (com.apple.dt.Xcode/3594.4.19)>",
            "X-Apple-Locale": "en_US",
            "X-Apple-I-TimeZone": "UTC",
        });

        Mock::given(method("GET"))
            .and(path("/anisette"))
            .respond_with(ResponseTemplate::new(200).set_body_json(headers))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex(r"^/services/QH65B2/.+\.action$"))
            .respond_with(self.clone())
            .mount(server)
            .await;
    }

    /// Add a certificate for another machine's key.
    pub fn add_foreign_certificate(&self, machine_name: &str, expiration: DateTime<Utc>) -> String {
        let key = sl_codesign::generate_signing_key().expect("foreign key");
        let public_key = crate::pki::subject_public_key(&key);

        let mut state = self.state.lock();
        let serial = state.next_serial;
        state.next_serial += 1;

        let team_id = state.team_id.clone();
        let certificate = DevelopmentAuthority::shared().issue_development(public_key, &team_id, serial);
        let serial = format!("{serial:X}");

        state.certificates.push(PortalCertificate {
            serial: serial.clone(),
            machine_name: machine_name.into(),
            machine_id: None,
            expiration,
            der: certificate.to_der().expect("certificate DER"),
        });

        serial
    }

    pub fn add_app_id(&self, identifier: &str, expiration: Option<DateTime<Utc>>) -> String {
        let mut state = self.state.lock();
        let app_id_id = format!("APPID{:04}", state.next_app_id);
        state.next_app_id += 1;

        state.app_ids.push(PortalAppId {
            app_id_id: app_id_id.clone(),
            identifier: identifier.into(),
            name: identifier.into(),
            expiration,
        });

        app_id_id
    }

    fn handle(&self, action: &str, fields: &Dictionary) -> Result<Dictionary, i64> {
        let mut state = self.state.lock();

        if state.expire_sessions > 0 {
            state.expire_sessions -= 1;

            return Err(SESSION_EXPIRED);
        }

        let text = |name: &str| fields.get(name).and_then(Value::as_string).unwrap_or_default().to_owned();

        match action {
            "listTeams" => {
                let mut team = Dictionary::new();
                team.insert("teamId".into(), state.team_id.clone().into());
                team.insert("name".into(), state.team_name.clone().into());
                team.insert("type".into(), "Individual".into());

                let membership_name =
                    if state.free { "Xcode Free Provisioning Program" } else { "Apple Developer Program" };
                let mut membership = Dictionary::new();
                membership.insert("name".into(), membership_name.into());
                team.insert("memberships".into(), Value::Array(vec![Value::Dictionary(membership)]));

                Ok(response([("teams", Value::Array(vec![Value::Dictionary(team)]))]))
            }

            "ios/listDevices" => {
                let devices = state
                    .devices
                    .iter()
                    .map(|(number, name)| {
                        record([("deviceNumber", number.clone().into()), ("name", name.clone().into())])
                    })
                    .collect();

                Ok(response([("devices", Value::Array(devices))]))
            }

            "ios/addDevice" => {
                let number = text("deviceNumber");
                let name = text("name");
                state.devices.push((number.clone(), name.clone()));

                Ok(response([("device", record([("deviceNumber", number.into()), ("name", name.into())]))]))
            }

            "ios/listAllDevelopmentCerts" => {
                let certificates = state.certificates.iter().map(certificate_record).collect();

                Ok(response([("certificates", Value::Array(certificates))]))
            }

            "ios/submitDevelopmentCSR" => {
                if state.certificates.len() >= state.certificate_limit {
                    return Err(CERTIFICATE_LIMIT);
                }

                let request = CertReq::from_pem(text("csrContent")).map_err(|_| 9999)?;
                let public_key = request.info.public_key;

                let serial = state.next_serial;
                state.next_serial += 1;

                let team_id = state.team_id.clone();
                let certificate = DevelopmentAuthority::shared().issue_development(public_key, &team_id, serial);
                let serial = format!("{serial:X}");

                state.certificates.push(PortalCertificate {
                    serial: serial.clone(),
                    machine_name: text("machineName"),
                    machine_id: Some(text("machineId")),
                    expiration: Utc::now() + Duration::days(365),
                    der: certificate.to_der().map_err(|_| 9999)?,
                });

                Ok(response([("certRequest", record([("serialNum", serial.into())]))]))
            }

            "ios/revokeDevelopmentCert" => {
                let serial = text("serialNumber");
                let before = state.certificates.len();
                state.certificates.retain(|certificate| certificate.serial != serial);

                if state.certificates.len() == before { Err(9998) } else { Ok(response([])) }
            }

            "ios/listAppIds" => {
                let app_ids = state.app_ids.iter().map(app_id_record).collect();

                Ok(response([("appIds", Value::Array(app_ids))]))
            }

            "ios/addAppId" => {
                if state.app_id_limit.is_some_and(|limit| state.app_ids.len() >= limit) {
                    return Err(9401);
                }

                let app_id_id = format!("APPID{:04}", state.next_app_id);
                state.next_app_id += 1;

                let expiration = state.free.then(|| Utc::now() + Duration::days(7));
                let app_id = PortalAppId { app_id_id, identifier: text("identifier"), name: text("name"), expiration };
                let encoded = app_id_record(&app_id);
                state.app_ids.push(app_id);

                Ok(response([("appId", encoded)]))
            }

            "ios/downloadTeamProvisioningProfile" => {
                let app_id_id = text("appIdId");
                let app_id = state.app_ids.iter().find(|app_id| app_id.app_id_id == app_id_id).ok_or(35)?;

                let now = Utc::now();
                let platform = if text("subPlatform") == "tvOS" { "tvOS" } else { "iOS" };
                let profile = ProfileFields {
                    team_id: state.team_id.clone(),
                    identifier: app_id.identifier.clone(),
                    name: app_id.name.clone(),
                    uuid: format!("PROFILE-{app_id_id}"),
                    created: now - Duration::hours(1),
                    expires: now + Duration::days(state.profile_days),
                    free: state.free,
                    platform,
                    devices: state.devices.iter().map(|(number, _)| number.clone()).collect(),
                    certificates: state.certificates.iter().map(|certificate| certificate.der.clone()).collect(),
                };

                let encoded = profile.signed(ProfileChain::shared());
                let profile = record([("encodedProfile", Value::Data(encoded))]);

                Ok(response([("provisioningProfile", profile)]))
            }

            _ => Err(9000),
        }
    }
}

impl Respond for FakePortal {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let action = request.url.path().trim_start_matches("/services/QH65B2/").trim_end_matches(".action").to_owned();
        let fields =
            Value::from_reader_xml(request.body.as_slice()).ok().and_then(Value::into_dictionary).unwrap_or_default();

        self.state.lock().requests.push(PortalRequest { action: action.clone(), fields: fields.clone() });

        let body = match self.handle(&action, &fields) {
            Ok(body) => body,
            Err(code) => {
                let mut body = Dictionary::new();
                body.insert("resultCode".into(), Value::Integer(code.into()));
                body.insert("resultString".into(), "fixture service error".into());

                body
            }
        };

        let mut bytes = Vec::new();
        Value::Dictionary(body).to_writer_xml(&mut bytes).expect("portal fixture");

        ResponseTemplate::new(200).set_body_bytes(bytes)
    }
}

fn response<const N: usize>(fields: [(&str, Value); N]) -> Dictionary {
    let mut body = Dictionary::new();
    body.insert("resultCode".into(), Value::Integer(0.into()));

    for (name, value) in fields {
        body.insert(name.into(), value);
    }

    body
}

fn record<const N: usize>(fields: [(&str, Value); N]) -> Value {
    let mut record = Dictionary::new();

    for (name, value) in fields {
        record.insert(name.into(), value);
    }

    Value::Dictionary(record)
}

fn date(time: DateTime<Utc>) -> Value {
    Value::Date(std::time::SystemTime::from(time).into())
}

fn certificate_record(certificate: &PortalCertificate) -> Value {
    record([
        ("serialNumber", certificate.serial.clone().into()),
        ("machineName", certificate.machine_name.clone().into()),
        ("expirationDate", date(certificate.expiration)),
        ("certContent", Value::Data(certificate.der.clone())),
    ])
}

fn app_id_record(app_id: &PortalAppId) -> Value {
    let mut fields = Dictionary::new();
    fields.insert("appIdId".into(), app_id.app_id_id.clone().into());
    fields.insert("identifier".into(), app_id.identifier.clone().into());
    fields.insert("name".into(), app_id.name.clone().into());

    if let Some(expiration) = app_id.expiration {
        fields.insert("expirationDate".into(), date(expiration));
    }

    Value::Dictionary(fields)
}

/// Decode a certificate from DER for assertions.
pub fn certificate(der: &[u8]) -> x509_cert::Certificate {
    x509_cert::Certificate::from_der(der).expect("certificate DER")
}
