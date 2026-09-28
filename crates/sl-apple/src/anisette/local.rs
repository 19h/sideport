//! Local ("private") anisette from this machine's own provisioning.
//!
//! Recovered `kbsync.privateAnisetter`: `[AOSUtilities retrieveOTPHeadersForDSID:@"-2"]`
//! supplies `X-Apple-MD`/`X-Apple-MD-M`, renamed to `X-Apple-I-MD`/`X-Apple-I-MD-M`;
//! `machineSerialNumber` → `X-Apple-I-SRL-NO`; `machineUDID` → `X-Mme-Device-Id`;
//! `NSLocale.languageCode` → `X-Apple-Locale`; `NSTimeZone.abbreviation` →
//! `X-Apple-I-TimeZone`; the current time → `X-Apple-I-Client-Time`. The Go host adds
//! `X-Apple-I-MD-LU = upper(hex(sha256(device id)))`, `X-Apple-I-MD-RINFO = 17106176` and
//! `X-MMe-Client-Info = <model> <macOS;version;build> <com.apple.AuthKit/1 (com.apple.akd/1.0)>`.

use super::AnisetteHeaders;
use crate::Result;
use crate::auth::AnisetteProvider;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use futures::future::BoxFuture;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use zeroize::Zeroizing;

const ROUTING_INFO: &str = "17106176";
const CLIENT_SUFFIX: &str = "<com.apple.AuthKit/1 (com.apple.akd/1.0)>";

/// Values read from the local machine for one anisette request.
pub struct MachineValues {
    /// AOSKit `X-Apple-MD` (one-time password).
    pub otp: Zeroizing<String>,
    /// AOSKit `X-Apple-MD-M` (machine token).
    pub machine_token: Zeroizing<String>,
    pub serial: String,
    /// AOSKit `machineUDID`.
    pub device_id: String,
    pub locale: String,
    pub time_zone: String,
    /// `hw.model`, e.g. `Mac16,5`.
    pub hardware_model: String,
    pub os_version: String,
    pub os_build: String,
}

impl fmt::Debug for MachineValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MachineValues")
            .field("hardware_model", &self.hardware_model)
            .field("os_version", &self.os_version)
            .finish_non_exhaustive()
    }
}

/// A platform source of [`MachineValues`], such as the macOS AOSKit bridge.
pub trait MachineSource: fmt::Debug + Send + Sync {
    fn values(&self) -> BoxFuture<'_, Result<MachineValues>>;

    fn headers(&self) -> BoxFuture<'_, Result<AnisetteHeaders>> {
        async {
            let values = self.values().await?;

            machine_headers(&values, Utc::now())
        }
        .boxed()
    }
}

/// Assemble validated headers in the recovered layout.
pub fn machine_headers(values: &MachineValues, now: DateTime<Utc>) -> Result<AnisetteHeaders> {
    let device_digest = Sha256::digest(values.device_id.as_bytes());
    let local_user = hex::encode_upper(device_digest);

    let client_info =
        format!("<{}> <macOS;{};{}> {CLIENT_SUFFIX}", values.hardware_model, values.os_version, values.os_build);

    let mut headers = BTreeMap::new();

    headers.insert("X-Apple-I-MD".into(), values.otp.to_string());
    headers.insert("X-Apple-I-MD-M".into(), values.machine_token.to_string());
    headers.insert("X-Apple-I-SRL-NO".into(), values.serial.clone());
    headers.insert("X-Mme-Device-Id".into(), values.device_id.clone());
    headers.insert("X-Apple-Locale".into(), values.locale.clone());
    headers.insert("X-Apple-I-TimeZone".into(), values.time_zone.clone());
    headers.insert("X-Apple-I-Client-Time".into(), now.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    headers.insert("X-Apple-I-MD-LU".into(), local_user);
    headers.insert("X-Apple-I-MD-RINFO".into(), ROUTING_INFO.into());
    headers.insert("X-MMe-Client-Info".into(), client_info);

    AnisetteHeaders::new(headers)
}

/// Anisette from a [`MachineSource`]; every request asks the machine for a fresh OTP.
#[derive(Debug, Clone)]
pub struct LocalAnisette {
    source: Arc<dyn MachineSource>,
}

impl LocalAnisette {
    pub fn new(source: Arc<dyn MachineSource>) -> Self {
        Self { source }
    }

    pub async fn headers(&self) -> Result<AnisetteHeaders> {
        self.source.headers().await
    }
}

impl AnisetteProvider for LocalAnisette {
    fn headers<'a>(&'a self, _username: &'a str) -> BoxFuture<'a, Result<AnisetteHeaders>> {
        LocalAnisette::headers(self).boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    fn values() -> MachineValues {
        MachineValues {
            otp: Zeroizing::new("otp-value".into()),
            machine_token: Zeroizing::new("machine-token".into()),
            serial: "C02FIXTURE".into(),
            device_id: "11111111-2222-3333-4444-555555555555".into(),
            locale: "en".into(),
            time_zone: "CEST".into(),
            hardware_model: "Mac16,5".into(),
            os_version: "27.2".into(),
            os_build: "26B5091g".into(),
        }
    }

    #[test]
    fn machine_values_map_to_the_recovered_header_layout() {
        let now = "2026-09-28T12:34:56Z".parse().expect("time");
        let headers = machine_headers(&values(), now).expect("headers");

        assert_eq!(headers.get("X-Apple-I-MD"), Some("otp-value"));
        assert_eq!(headers.get("X-Apple-I-MD-M"), Some("machine-token"));
        assert_eq!(headers.get("X-Apple-I-SRL-NO"), Some("C02FIXTURE"));
        assert_eq!(headers.get("X-Apple-I-Client-Time"), Some("2026-09-28T12:34:56Z"));
        assert_eq!(headers.get("X-Apple-I-MD-RINFO"), Some("17106176"));
        assert_eq!(
            headers.get("X-MMe-Client-Info"),
            Some("<Mac16,5> <macOS;27.2;26B5091g> <com.apple.AuthKit/1 (com.apple.akd/1.0)>")
        );

        let expected = hex::encode_upper(Sha256::digest(b"11111111-2222-3333-4444-555555555555"));
        assert_eq!(headers.get("X-Apple-I-MD-LU"), Some(expected.as_str()));
        assert_eq!(headers.description(), "Mac16,5 with serial number C02FIXTURE running macOS 27.2 26B5091g");
    }

    #[derive(Debug)]
    struct Fixed(bool);

    impl MachineSource for Fixed {
        fn values(&self) -> BoxFuture<'_, Result<MachineValues>> {
            let mut values = values();

            if !self.0 {
                values.otp = Zeroizing::new(String::new());
            }

            async move { Ok(values) }.boxed()
        }
    }

    #[tokio::test]
    async fn empty_machine_values_are_rejected() {
        LocalAnisette::new(Arc::new(Fixed(true))).headers().await.expect("valid values");

        let error = LocalAnisette::new(Arc::new(Fixed(false))).headers().await.expect_err("empty OTP");
        assert!(matches!(error, Error::Invalid(_)));
    }
}
