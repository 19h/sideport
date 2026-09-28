//! Validated machine-provisioning headers, the recovered remote GET protocol and local anisette.

mod local;

pub use local::{LocalAnisette, MachineSource, MachineValues, machine_headers};

use crate::{Error, Result, transport};
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Client, Url};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use zeroize::{Zeroize, Zeroizing};

pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const CLIENT_TIME: &str = "X-Apple-I-Client-Time";
const REQUIRED_HEADERS: [&str; 5] =
    ["X-Apple-I-MD", "X-Apple-I-MD-M", "X-Mme-Device-Id", "X-MMe-Client-Info", "X-Apple-Locale"];
const BLOCKED_CLIENT: &str = "com.apple.dt.Xcode/";
const AUTHKIT_CLIENT: &str = "com.apple.akd/1.0";

#[derive(Clone)]
pub struct AnisetteHeaders {
    values: BTreeMap<String, String>,
}

impl fmt::Debug for AnisetteHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("AnisetteHeaders").field("count", &self.values.len()).finish_non_exhaustive()
    }
}

impl Drop for AnisetteHeaders {
    fn drop(&mut self) {
        self.values.values_mut().for_each(Zeroize::zeroize);
    }
}

impl AnisetteHeaders {
    pub fn new(values: BTreeMap<String, String>) -> Result<Self> {
        let mut headers = Self { values };

        if headers.values.len() > 128 {
            return Err(Error::Invalid("anisette header count"));
        }

        let mut names = BTreeSet::new();

        for (name, value) in &headers.values {
            let folded = name.to_ascii_lowercase();

            if name.len() > 256 || value.len() > 4096 || !names.insert(folded.clone()) {
                return Err(Error::Invalid("anisette header size or duplicate"));
            }

            if !folded.starts_with("x-apple-") && !folded.starts_with("x-mme-") && folded != "loc" {
                return Err(Error::Invalid("unexpected anisette header"));
            }

            HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Invalid("anisette header name"))?;
            HeaderValue::from_str(value).map_err(|_| Error::Invalid("anisette header value"))?;
        }

        for name in REQUIRED_HEADERS {
            if headers.get(name).is_none_or(str::is_empty) {
                return Err(Error::Invalid("missing anisette header"));
            }
        }

        for (name, value) in &mut headers.values {
            if name.eq_ignore_ascii_case("X-MMe-Client-Info") {
                if let Some(normalized) = normalize_client_info(value) {
                    value.zeroize();
                    *value = normalized;
                }
            }
        }

        Ok(headers)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn description(&self) -> String {
        let client_info = self.get("X-MMe-Client-Info").unwrap_or_default();
        let mut groups = client_info.trim_matches(['<', '>']).split("> <");
        let model = groups.next().filter(|model| !model.is_empty()).unwrap_or("unknown device");
        let system = groups.next().unwrap_or("unknown system").replace(';', " ");
        let serial = self.get("X-Apple-I-SRL-NO").filter(|serial| !serial.is_empty()).unwrap_or("unknown");

        format!("{model} with serial number {serial} running {system}")
    }

    fn set_time(&mut self, time: DateTime<Utc>) {
        self.values.retain(|name, _| !name.eq_ignore_ascii_case(CLIENT_TIME));
        self.values.insert(CLIENT_TIME.into(), time.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }
}

fn normalize_client_info(value: &str) -> Option<String> {
    let (prefix, remainder) = value.split_once(BLOCKED_CLIENT)?;
    let version_length = remainder.bytes().take_while(|byte| byte.is_ascii_digit() || *byte == b'.').count();

    if version_length == 0 || !remainder.as_bytes()[..version_length].iter().any(u8::is_ascii_digit) {
        return None;
    }

    let suffix = &remainder[version_length..];

    Some(format!("{prefix}{AUTHKIT_CLIENT}{suffix}"))
}

pub fn login_hash(username: &str) -> String {
    hex::encode(Sha256::digest(username.as_bytes()))
}

#[derive(Clone)]
pub struct RemoteAnisette {
    inner: Arc<RemoteInner>,
}

impl fmt::Debug for RemoteAnisette {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RemoteAnisette").finish_non_exhaustive()
    }
}

struct RemoteInner {
    client: Client,
    endpoint: Url,
    cache: Mutex<Option<CacheEntry>>,
}

struct CacheEntry {
    login_hash: String,
    received: Instant,
    client_time: DateTime<Utc>,
    headers: AnisetteHeaders,
}

impl RemoteAnisette {
    /// Explicitly configured URLs support HTTP and HTTPS anisette services.
    /// Redirects are refused, so credentials/query parameters remain at this endpoint.
    pub fn new(endpoint: &str) -> Result<Self> {
        let endpoint = Url::parse(endpoint).map_err(|_| Error::Invalid("anisette URL"))?;

        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(Error::Invalid("anisette URL"));
        }

        Ok(Self { inner: Arc::new(RemoteInner { client: transport::client()?, endpoint, cache: Mutex::new(None) }) })
    }

    pub async fn headers(&self, username: Option<&str>) -> Result<AnisetteHeaders> {
        let hash = username.map(login_hash).unwrap_or_default();
        self.fetch(&hash, || (Utc::now(), Instant::now())).await
    }

    async fn fetch(&self, hash: &str, clock: impl Fn() -> (DateTime<Utc>, Instant)) -> Result<AnisetteHeaders> {
        // Serialize refreshes across clones. Dropping a request releases the lock.
        let mut cache = self.inner.cache.lock().await;
        let (client_time, received) = clock();

        let cached = cache.as_ref().filter(|entry| entry.login_hash == hash && reusable(entry, client_time, received));

        if let Some(entry) = cached {
            let mut headers = entry.headers.clone();
            headers.set_time(client_time);

            return Ok(headers);
        }

        let mut endpoint = self.inner.endpoint.clone();
        let parameters: Vec<_> = endpoint
            .query_pairs()
            .filter(|(name, _)| name != "u")
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        endpoint.set_query(None);
        endpoint.query_pairs_mut().extend_pairs(parameters).append_pair("u", hash);

        let response = self.inner.client.get(endpoint).send().await?;
        let body = Zeroizing::new(transport::read_bounded(response, MAX_RESPONSE_BYTES).await?);
        let mut values: WireHeaders = serde_json::from_slice(&body).map_err(|_| Error::Invalid("anisette JSON"))?;
        let mut headers = AnisetteHeaders::new(std::mem::take(&mut values.0))?;
        headers.set_time(clock().0);

        *cache = Some(CacheEntry { login_hash: hash.into(), received, client_time, headers: headers.clone() });

        Ok(headers)
    }
}

struct WireHeaders(BTreeMap<String, String>);

impl Drop for WireHeaders {
    fn drop(&mut self) {
        self.0.values_mut().for_each(Zeroize::zeroize);
    }
}

impl<'de> Deserialize<'de> for WireHeaders {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct HeadersVisitor;

        impl<'de> Visitor<'de> for HeadersVisitor {
            type Value = WireHeaders;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a dictionary of distinct anisette header strings")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> std::result::Result<Self::Value, M::Error> {
                let mut headers = WireHeaders(BTreeMap::new());

                while let Some((name, value)) = map.next_entry::<String, String>()? {
                    let mut value = Zeroizing::new(value);

                    if headers.0.len() == 128 || headers.0.contains_key(&name) {
                        return Err(serde::de::Error::custom("excessive or duplicate headers"));
                    }

                    headers.0.insert(name, std::mem::take(&mut *value));
                }

                Ok(headers)
            }
        }

        deserializer.deserialize_map(HeadersVisitor)
    }
}

fn reusable(entry: &CacheEntry, now: DateTime<Utc>, monotonic: Instant) -> bool {
    let age = now.signed_duration_since(entry.client_time);
    let monotonic_age = monotonic.checked_duration_since(entry.received);

    age >= chrono::Duration::zero()
        && age < chrono::Duration::seconds(30)
        && monotonic_age.is_some_and(|age| age < Duration::from_secs(30))
        && now.timestamp().div_euclid(30) == entry.client_time.timestamp().div_euclid(30)
        && now.timestamp().rem_euclid(30) < 27
}

#[cfg(test)]
mod tests;
