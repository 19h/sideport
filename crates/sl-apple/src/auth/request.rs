use super::{AnisetteProvider, AuthClient, cancellable};
use crate::anisette::AnisetteHeaders;
use crate::transport::{base_headers, insert_header};
use crate::wire::{self, SecretValue, dictionary};
use crate::{Error, Result, transport};
use chrono::Utc;
use plist::{Dictionary, Value};
use reqwest::header::HeaderMap;
use reqwest::{Method, RequestBuilder, Url};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

impl AuthClient {
    pub(super) fn endpoint(&self, path: &str) -> Result<Url> {
        self.origin.join(path).map_err(|_| Error::Invalid("authentication endpoint"))
    }

    pub(super) async fn gsa(
        &self,
        operation: &'static str,
        identity: &str,
        username: &str,
        additions: Value,
        provider: &dyn AnisetteProvider,
        cancellation: &CancellationToken,
    ) -> Result<SecretValue> {
        let additions = SecretValue(additions);
        let anisette = cancellable(cancellation, provider.headers(username)).await?;
        let mut cpd = Dictionary::new();

        for (name, value) in anisette.iter() {
            if !name.eq_ignore_ascii_case("X-Apple-Locale") && name != "loc" {
                let name = ["X-MMe-Client-Info", "X-Apple-I-MD-M", "X-Mme-Device-Id", "X-Apple-I-MD"]
                    .into_iter()
                    .find(|canonical| name.eq_ignore_ascii_case(canonical))
                    .unwrap_or(name);
                cpd.insert(name.into(), Value::String(value.into()));
            }
        }

        cpd.insert("loc".into(), Value::String(required(&anisette, "X-Apple-Locale")?.into()));
        cpd.insert("bootstrap".into(), Value::Boolean(true));

        match operation {
            "init" => {
                cpd.insert("prkgen".into(), Value::Boolean(true));
                cpd.insert("icscrec".into(), Value::Boolean(true));
            }
            "complete" => {
                cpd.insert("ckgen".into(), Value::Boolean(true));
            }
            _ => {}
        }

        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut request = wire::dict(&additions.0)?.clone();
        request.insert("cpd".into(), Value::Dictionary(cpd));
        request.insert("o".into(), Value::String(operation.into()));
        request.insert("u".into(), Value::String(identity.into()));

        for name in ["bootstrap", "prkgen", "pbe", "wpbe", "icscrec"] {
            request.insert(name.into(), Value::Boolean(true));
        }

        request.insert("svct".into(), Value::String("iCloud".into()));
        request.insert("X-Apple-I-Client-Time".into(), Value::String(now));
        request.insert("X-Apple-I-Device-Configuration-Mode".into(), Value::String("0".into()));
        request.insert("AppleIDClientIdentifier".into(), Value::String(required(&anisette, "X-Mme-Device-Id")?.into()));

        let envelope = SecretValue(dictionary([
            ("Header", dictionary([("Version", Value::String("1.0.1".into()))])),
            ("Request", Value::Dictionary(request)),
        ]));
        let mut headers = base_headers("text/x-xml-plist")?;

        for name in ["X-MMe-Client-Info", "X-Apple-I-MD-M", "X-Mme-Device-Id"] {
            insert_header(&mut headers, name, required(&anisette, name)?)?;
        }

        let endpoint = self.endpoint("grandslam/GsService2")?;
        let mut body = wire::encode(&envelope.0)?;
        let request = self.client.post(endpoint).headers(headers).body(std::mem::take(&mut *body));
        let mut response = self.plist_response(request, cancellation).await?;
        let root = response.0.as_dictionary_mut().ok_or(Error::Invalid("GSA response dictionary"))?;
        let response = SecretValue(root.remove("Response").ok_or(Error::Invalid("GSA response envelope"))?);
        let dictionary = wire::dict(&response.0)?;
        let status = wire::dict(dictionary.get("Status").ok_or(Error::Invalid("GSA status"))?)?;
        let code = wire::integer(status, "ec")?;

        if code != 0 {
            return Err(Error::Service { operation, code });
        }

        Ok(response)
    }

    pub(super) async fn plist_response(
        &self,
        request: RequestBuilder,
        cancellation: &CancellationToken,
    ) -> Result<SecretValue> {
        let body = self.response_body(request, cancellation).await?;

        wire::decode(&body, false)
    }

    pub(super) async fn response_body(
        &self,
        request: RequestBuilder,
        cancellation: &CancellationToken,
    ) -> Result<Zeroizing<Vec<u8>>> {
        cancellable(cancellation, async {
            let response = request.send().await?;
            let bytes = transport::read_bounded(response, wire::MAX_BODY_BYTES).await?;

            Ok(Zeroizing::new(bytes))
        })
        .await
    }

    pub(super) fn factor_request(&self, method: Method, path: &str, headers: HeaderMap) -> Result<RequestBuilder> {
        Ok(self.client.request(method, self.endpoint(path)?).headers(headers))
    }
}

pub(super) fn required<'a>(headers: &'a AnisetteHeaders, name: &str) -> Result<&'a str> {
    headers.get(name).ok_or(Error::Invalid("required anisette header"))
}
