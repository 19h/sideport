use super::{AnisetteProvider, AuthClient, cancellable};
use crate::srp::{SessionData, XCODE_APP};
use crate::transport::{base_headers, insert_header};
use crate::wire::{self, SecretValue, dictionary};
use crate::{Error, Result};
use base64::Engine;
use futures::future::BoxFuture;
use plist::Value;
use reqwest::Method;
use reqwest::header::HeaderMap;
use serde_json::Value as Json;
use std::fmt;
use tokio_util::sync::CancellationToken;
use zeroize::{Zeroize, Zeroizing};

pub const MAX_CODE_ATTEMPTS: usize = 5;
pub const MAX_SMS_REQUESTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactorPrompt {
    pub destination: String,
    pub code_length: usize,
    pub can_request_sms: bool,
    pub incorrect_code: bool,
    pub attempt: usize,
}

pub enum FactorReply {
    Code(Zeroizing<String>),
    RequestSms,
    Cancel,
}

impl fmt::Debug for FactorReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Code(_) => "FactorReply::Code([redacted])",
            Self::RequestSms => "FactorReply::RequestSms",
            Self::Cancel => "FactorReply::Cancel",
        })
    }
}

pub trait FactorDelegate: Send + Sync {
    fn ask<'a>(&'a self, prompt: FactorPrompt) -> BoxFuture<'a, Result<FactorReply>>;
}

impl AuthClient {
    pub(super) async fn verify_factor(
        &self,
        username: &str,
        data: SessionData,
        unlock: &str,
        provider: &dyn AnisetteProvider,
        delegate: &dyn FactorDelegate,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let headers = self.factor_headers(username, &data, provider, cancellation).await?;
        let mut using_phone = unlock == "secondaryAuth";
        let mut metadata = session_metadata(&data)?;

        if data.additional_data("canHaveCustodian").is_none() {
            let bootstrap = self.factor_request(Method::GET, "auth", headers.clone())?;
            self.response_body(bootstrap, cancellation).await?;
            let path = if using_phone { "auth/verify/phone" } else { "auth/verify/trusteddevice" };
            let request = self.factor_request(Method::GET, path, headers.clone())?;

            match self.factor_json(request, cancellation).await {
                Ok(response) => metadata = response,
                Err(Error::HttpStatus(401 | 500)) if !using_phone => {
                    metadata = self.sms(&headers, "1", None, cancellation).await?;
                    using_phone = true;
                }
                Err(error) => return Err(error),
            }
        }

        let mut code_attempts = 0;
        let mut sms_requests = 0;
        let mut incorrect_code = false;

        loop {
            if code_attempts == MAX_CODE_ATTEMPTS {
                return Err(Error::RetryLimit("verification code attempts"));
            }

            let description = factor_description(&metadata.0, using_phone)?;
            let prompt = FactorPrompt {
                destination: description.destination,
                code_length: description.length,
                can_request_sms: sms_requests < MAX_SMS_REQUESTS,
                incorrect_code,
                attempt: code_attempts + 1,
            };
            let reply = cancellable(cancellation, delegate.ask(prompt)).await?;

            match reply {
                FactorReply::Cancel => return Err(Error::Cancelled),
                FactorReply::RequestSms => {
                    if sms_requests == MAX_SMS_REQUESTS {
                        return Err(Error::RetryLimit("SMS requests"));
                    }

                    let response = self.sms(&headers, &description.phone_id, None, cancellation).await?;
                    merge_metadata(&mut metadata.0, &response.0)?;
                    using_phone = true;
                    sms_requests += 1;
                }
                FactorReply::Code(code) => {
                    code_attempts += 1;

                    if code.len() != description.length || !code.bytes().all(|byte| byte.is_ascii_digit()) {
                        incorrect_code = true;

                        continue;
                    }

                    let result = if using_phone {
                        self.sms(&headers, &description.phone_id, Some(&code), cancellation).await.map(|_| ())
                    } else {
                        self.validate_device_code(&headers, &code, data.additional_data("idmsdata"), cancellation).await
                    };

                    match result {
                        Ok(()) => return Ok(()),
                        Err(Error::Service { code: -21669, .. }) => incorrect_code = true,
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }

    async fn factor_headers(
        &self,
        username: &str,
        data: &SessionData,
        provider: &dyn AnisetteProvider,
        cancellation: &CancellationToken,
    ) -> Result<HeaderMap> {
        let anisette = cancellable(cancellation, provider.headers(username)).await?;
        let mut headers = base_headers("text/x-xml-plist")?;

        for (name, value) in anisette.iter().filter(|(name, _)| *name != "loc") {
            insert_header(&mut headers, name, value)?;
        }

        let identity = Zeroizing::new(format!("{}:{}", data.dsid(), data.idms_token()));
        let identity = Zeroizing::new(
            identity
                .chars()
                .map(|character| u8::try_from(u32::from(character)))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| Error::Invalid("identity token Latin-1 encoding"))?,
        );
        let encoded = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(&identity));
        insert_header(&mut headers, "X-Apple-App-Info", XCODE_APP)?;
        insert_header(&mut headers, "X-Apple-Client-App-Name", "Xcode")?;
        insert_header(&mut headers, "X-Apple-Identity-Token", &encoded)?;

        Ok(headers)
    }

    async fn sms(
        &self,
        headers: &HeaderMap,
        phone_id: &str,
        code: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<SecretJson> {
        let mut body = dictionary([("serverInfo", dictionary([("phoneNumber.id", Value::String(phone_id.into()))]))]);
        let path = if let Some(code) = code {
            body.as_dictionary_mut()
                .ok_or(Error::Invalid("SMS request dictionary"))?
                .insert("securityCode.code".into(), Value::String(code.into()));

            "auth/verify/phone/securitycode?referrer=/auth/verify/phone/put"
        } else {
            "auth/verify/phone/put"
        };

        let body = SecretValue(body);
        let mut encoded = wire::encode(&body.0)?;
        let mut headers = headers.clone();
        insert_header(&mut headers, "Content-Type", "application/x-plist")?;
        insert_header(&mut headers, "Accept", "application/json")?;
        let request = self.factor_request(Method::POST, path, headers)?.body(std::mem::take(&mut *encoded));

        self.factor_json(request, cancellation).await
    }

    async fn validate_device_code(
        &self,
        headers: &HeaderMap,
        code: &str,
        idms_data: Option<&Value>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let mut headers = headers.clone();
        insert_header(&mut headers, "security-code", code)?;
        let method = if idms_data.is_some() { Method::POST } else { Method::GET };
        let mut request = self.factor_request(method, "grandslam/GsService2/validate", headers)?;

        if let Some(idms_data) = idms_data {
            let body = SecretValue(dictionary([
                ("Header", dictionary([])),
                ("Request", dictionary([("idmsdata", idms_data.clone())])),
            ]));
            let mut encoded = wire::encode(&body.0)?;
            request = request.body(std::mem::take(&mut *encoded));
        }

        let response = self.plist_response(request, cancellation).await?;
        let response = wire::dict(&response.0)?;

        if response.is_empty() {
            return Ok(());
        }

        let code = wire::integer(response, "ec")?;

        if code != 0 {
            return Err(Error::Service { operation: "verification", code });
        }

        Ok(())
    }

    async fn factor_json(
        &self,
        request: reqwest::RequestBuilder,
        cancellation: &CancellationToken,
    ) -> Result<SecretJson> {
        let body = self.response_body(request, cancellation).await?;
        let response = SecretJson(serde_json::from_slice(&body).map_err(|_| Error::Invalid("second-factor JSON"))?);

        if !response.0.is_object() {
            return Err(Error::Invalid("second-factor dictionary"));
        }

        if let Some(errors) = response.0.get("serviceErrors") {
            let errors = errors.as_array().ok_or(Error::Invalid("second-factor service errors"))?;

            if let Some(error) = errors.first() {
                let code = error
                    .get("code")
                    .and_then(|code| code.as_i64().or_else(|| code.as_str()?.parse().ok()))
                    .ok_or(Error::Invalid("second-factor service error code"))?;

                return Err(Error::Service { operation: "verification", code });
            }
        }

        Ok(response)
    }
}

struct SecretJson(Json);

impl Drop for SecretJson {
    fn drop(&mut self) {
        fn wipe(value: &mut Json) {
            match value {
                Json::String(value) => value.zeroize(),
                Json::Array(values) => values.iter_mut().for_each(wipe),
                Json::Object(values) => values.values_mut().for_each(wipe),
                _ => {}
            }
        }

        wipe(&mut self.0);
    }
}

fn session_metadata(data: &SessionData) -> Result<SecretJson> {
    let mut metadata = serde_json::Map::new();

    for name in [
        "phoneNumber",
        "trustedPhoneNumber",
        "phoneNumberVerification",
        "additionalInfo",
        "maskedPhoneNumber",
        "securityCode",
        "otherTrustedDeviceClass",
    ] {
        if let Some(value) = data.additional_data(name) {
            metadata.insert(
                name.into(),
                serde_json::to_value(value).map_err(|_| Error::Invalid("second-factor metadata"))?,
            );
        }
    }

    Ok(SecretJson(Json::Object(metadata)))
}

fn merge_metadata(target: &mut Json, source: &Json) -> Result<()> {
    let target = target.as_object_mut().ok_or(Error::Invalid("second-factor metadata"))?;
    let source = source.as_object().ok_or(Error::Invalid("second-factor metadata"))?;
    target.extend(source.iter().map(|(key, value)| (key.clone(), value.clone())));

    Ok(())
}

struct FactorDescription {
    destination: String,
    length: usize,
    phone_id: String,
}

fn factor_description(metadata: &Json, using_phone: bool) -> Result<FactorDescription> {
    let length = match metadata.get("securityCode") {
        Some(code) => code.get("length").and_then(Json::as_u64).ok_or(Error::Invalid("verification code length"))?,
        None => 6,
    };

    if !(4..=10).contains(&length) {
        return Err(Error::Invalid("verification code length"));
    }

    let phone = metadata
        .get("phoneNumber")
        .or_else(|| metadata.get("trustedPhoneNumber"))
        .or_else(|| metadata.get("phoneNumberVerification")?.get("trustedPhoneNumber"))
        .or_else(|| metadata.get("additionalInfo")?.get("obfuscatedPhoneNumbers")?.as_array()?.first());

    if using_phone && phone.is_none() {
        return Err(Error::Invalid("trusted phone metadata"));
    }

    let phone_id = match phone.and_then(|phone| phone.get("id")) {
        Some(Json::Number(number)) => number.as_u64().ok_or(Error::Invalid("trusted phone identifier"))?.to_string(),
        Some(Json::String(value))
            if !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            value.clone()
        }
        None => "1".into(),
        _ => return Err(Error::Invalid("trusted phone identifier")),
    };
    let description = if using_phone {
        phone
            .and_then(|phone| phone.get("numberWithDialCode").or_else(|| phone.get("obfuscatedNumber")))
            .and_then(Json::as_str)
            .or_else(|| metadata.get("maskedPhoneNumber").and_then(Json::as_str))
            .unwrap_or("your trusted phone")
    } else {
        metadata.get("otherTrustedDeviceClass").and_then(Json::as_str).unwrap_or("your trusted device")
    };

    if description.len() > 256 || description.contains(['\0', '\r', '\n']) {
        return Err(Error::Invalid("verification destination"));
    }

    Ok(FactorDescription { destination: description.into(), length: length as usize, phone_id })
}
