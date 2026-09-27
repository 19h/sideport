use crate::{Error, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Response};
use std::future::Future;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

pub(crate) fn checkpoint(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
}

pub(crate) async fn cancellable<T>(
    cancellation: &CancellationToken,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    checkpoint(cancellation)?;

    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(Error::Cancelled),
        result = future => {
            checkpoint(cancellation)?;

            result
        }
    }
}

pub(crate) fn base_headers(content_type: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    insert_header(&mut headers, "User-Agent", "Xcode")?;
    insert_header(&mut headers, "X-Xcode-Version", "11.2 (11B52)")?;
    insert_header(&mut headers, "Accept-Language", "en-us")?;
    insert_header(&mut headers, "Accept", "text/x-xml-plist")?;
    insert_header(&mut headers, "Content-Type", content_type)?;

    Ok(headers)
}

pub(crate) fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Invalid("request header name"))?;
    let mut value = HeaderValue::from_str(value).map_err(|_| Error::Invalid("request header value"))?;
    value.set_sensitive(true);
    headers.insert(name, value);

    Ok(())
}

pub(crate) fn client() -> Result<Client> {
    client_with_cookies(false)
}

pub(crate) fn client_with_cookies(cookies: bool) -> Result<Client> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .cookie_store(cookies)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("Sideport/", env!("CARGO_PKG_VERSION")))
        .build()?;

    Ok(client)
}

pub(crate) async fn read_bounded(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    if !response.status().is_success() {
        return Err(Error::HttpStatus(response.status().as_u16()));
    }

    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(Error::ResponseTooLarge(limit));
    }

    let mut body = Zeroizing::new(Vec::new());

    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > limit - body.len() {
            return Err(Error::ResponseTooLarge(limit));
        }

        body.extend_from_slice(&chunk);
    }

    Ok(std::mem::take(&mut *body))
}
