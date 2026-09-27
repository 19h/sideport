use crate::{Error, Result};
use reqwest::{Client, Response};
use std::time::Duration;
use zeroize::Zeroizing;

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
