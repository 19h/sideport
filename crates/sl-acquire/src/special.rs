//! Remote and "special" injection sources, reconstructed from the Go
//! `slpy.(*injector)` (`decompiled/go/sideloadly_slpy.c`) and `SIDELOADLY_DECONSTRUCTED.md` §7.4.
//!
//! An inject item may be an `http(s)://` URL or one of three specials that the recovered client
//! resolves against public package indexes:
//!
//!   * `///special/substrate`  → `http://apt.saurik.com/debs/mobilesubstrate_<v>_iphoneos-arm.deb`,
//!     with `<v>` scraped from `https://cydia.saurik.com/package/mobilesubstrate/`
//!     (default `0.9.6301`);
//!   * `///special/substitute` → the newest `com.ex.substitute_*.deb` under
//!     `https://apt.bingner.com/debs/1443.00/` (default `2.1.0`);
//!   * `///special/spoofer`    → the `filename` field of `https://sideloadly.io/spoofer.json`,
//!     joined onto `https://sideloadly.io/`.
//!
//! Every endpoint is configurable through [`SpecialSources`] so tests point it at a wiremock
//! server; the real hosts are never contacted in tests.

use crate::{Error, Result};
use reqwest::Client;
use reqwest::header::USER_AGENT;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Bound on a downloaded injection artifact (`.deb` or tweak file).
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// The special item names, without the `///special/` prefix.
pub const SUBSTRATE: &str = "substrate";
pub const SUBSTITUTE: &str = "substitute";
pub const SPOOFER: &str = "spoofer";

/// Configurable endpoints for the three specials. [`Default`] holds the recovered public hosts.
#[derive(Debug, Clone)]
pub struct SpecialSources {
    /// Cydia package page scraped for the latest MobileSubstrate version.
    pub substrate_index: String,
    /// `.deb` URL template; `{version}` is replaced by the scraped or default version.
    pub substrate_deb_template: String,
    /// Version assumed when the index cannot be parsed.
    pub substrate_default_version: String,
    /// Directory listing scanned for `com.ex.substitute_*.deb`; must end with `/`.
    pub substitute_index: String,
    /// `.deb` filename assumed when the listing yields no candidate.
    pub substitute_default_deb: String,
    /// JSON document naming the current spoofer file.
    pub spoofer_json: String,
    /// Base joined onto the spoofer `filename`; must end with `/`.
    pub spoofer_base: String,
}

impl Default for SpecialSources {
    fn default() -> Self {
        Self {
            substrate_index: "https://cydia.saurik.com/package/mobilesubstrate/".into(),
            substrate_deb_template: "http://apt.saurik.com/debs/mobilesubstrate_{version}_iphoneos-arm.deb".into(),
            substrate_default_version: "0.9.6301".into(),
            substitute_index: "https://apt.bingner.com/debs/1443.00/".into(),
            substitute_default_deb: "com.ex.substitute_2.1.0_iphoneos-arm.deb".into(),
            spoofer_json: "https://sideloadly.io/spoofer.json".into(),
            spoofer_base: "https://sideloadly.io/".into(),
        }
    }
}

/// Owns an HTTP client and the configured [`SpecialSources`], so callers need no `reqwest` types.
#[derive(Debug, Clone)]
pub struct SpecialResolver {
    client: Client,
    user_agent: String,
    sources: SpecialSources,
}

impl SpecialResolver {
    pub fn new(user_agent: &str, sources: SpecialSources) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|error| Error::Network(error.to_string()))?;

        Ok(Self { client, user_agent: user_agent.into(), sources })
    }

    /// Resolve `///special/<name>` (the `name` alone) to its concrete download URL.
    pub async fn resolve(&self, name: &str) -> Result<String> {
        self.sources.resolve(&self.client, &self.user_agent, name).await
    }

    /// Download `url` (a `.deb` or tweak) into `destination`.
    pub async fn download(&self, url: &str, destination: &Path, cancel: &CancellationToken) -> Result<()> {
        download(&self.client, &self.user_agent, url, destination, cancel).await
    }
}

impl SpecialSources {
    /// Resolve one special (`substrate`/`substitute`/`spoofer`) to a concrete download URL.
    pub async fn resolve(&self, client: &Client, user_agent: &str, name: &str) -> Result<String> {
        match name {
            SUBSTRATE => self.resolve_substrate(client, user_agent).await,
            SUBSTITUTE => self.resolve_substitute(client, user_agent).await,
            SPOOFER => self.resolve_spoofer(client, user_agent).await,
            other => Err(Error::Link(format!("Wrong special lib {other}"))),
        }
    }

    async fn resolve_substrate(&self, client: &Client, user_agent: &str) -> Result<String> {
        let version = match fetch_text(client, user_agent, &self.substrate_index).await {
            Ok(html) => latest_substrate_version(&html).unwrap_or_else(|| self.substrate_default_version.clone()),
            Err(_) => self.substrate_default_version.clone(),
        };

        Ok(self.substrate_deb_template.replace("{version}", &version))
    }

    async fn resolve_substitute(&self, client: &Client, user_agent: &str) -> Result<String> {
        let filename = match fetch_text(client, user_agent, &self.substitute_index).await {
            Ok(html) => latest_substitute_deb(&html).unwrap_or_else(|| self.substitute_default_deb.clone()),
            Err(_) => self.substitute_default_deb.clone(),
        };

        Ok(format!("{}{filename}", self.substitute_index))
    }

    async fn resolve_spoofer(&self, client: &Client, user_agent: &str) -> Result<String> {
        let body = fetch_text(client, user_agent, &self.spoofer_json)
            .await
            .map_err(|error| Error::Network(format!("Could not obtain Spoofer: {error}")))?;

        let document: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| Error::Network(format!("Could not obtain Spoofer: {error}")))?;
        let filename = document
            .get("filename")
            .and_then(serde_json::Value::as_str)
            .filter(|filename| !filename.is_empty())
            .ok_or_else(|| Error::Network("Could not obtain Spoofer: no filename".into()))?;

        Ok(format!("{}{filename}", self.spoofer_base))
    }
}

/// Scrape the MobileSubstrate version from the Cydia package page: the text between the first
/// `latest">` and the next `<`, matching the recovered `genSplit` parse.
fn latest_substrate_version(html: &str) -> Option<String> {
    let after = html.split_once("latest\">")?.1;
    let version = after.split('<').next()?.trim();

    (!version.is_empty()).then(|| version.to_owned())
}

/// Pick the newest `com.ex.substitute_<version>_iphoneos-arm.deb` from a directory listing,
/// comparing versions component-by-component as the recovered scanner does.
fn latest_substitute_deb(html: &str) -> Option<String> {
    let mut best: Option<(Vec<u64>, String)> = None;

    for candidate in html.split("com.ex.substitute_").skip(1) {
        let filename = format!("com.ex.substitute_{}", stop_at_quote(candidate));

        if !filename.ends_with(".deb") {
            continue;
        }

        let version = candidate.split('_').next().unwrap_or_default();
        let numbers = version_numbers(version);

        if numbers.iter().any(|&number| number > 0) && best.as_ref().is_none_or(|(top, _)| numbers > *top) {
            best = Some((numbers, filename));
        }
    }

    best.map(|(_, filename)| filename)
}

/// Truncate an anchor body at the first `"` or `<`, so `2.1.0_iphoneos-arm.deb">…` yields the name.
fn stop_at_quote(text: &str) -> &str {
    let end = text.find(['"', '<', '>']).unwrap_or(text.len());

    &text[..end]
}

/// Parse a dotted/dashed version into comparable numeric components; non-numeric parts count as 0.
fn version_numbers(version: &str) -> Vec<u64> {
    version.split(['.', '-']).map(|part| part.parse::<u64>().unwrap_or(0)).collect()
}

async fn fetch_text(client: &Client, user_agent: &str, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .header(USER_AGENT, user_agent)
        .send()
        .await
        .map_err(|error| Error::Network(format!("Failed to fetch {url}: {error}")))?;

    let status = response.status();

    if !status.is_success() {
        return Err(Error::Network(format!("Failed to fetch {url}: HTTP {status}")));
    }

    response.text().await.map_err(|error| Error::Network(format!("Failed to read {url}: {error}")))
}

/// Download an arbitrary injection artifact (a `.deb` or tweak) into `destination`. Unlike
/// [`crate::Downloader`], this makes no assumptions about IPA structure, flipping or hashing.
pub async fn download(
    client: &Client,
    user_agent: &str,
    url: &str,
    destination: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let response = tokio::select! {
        response = client.get(url).header(USER_AGENT, user_agent).send() => response,
        () = cancel.cancelled() => return Err(Error::Cancelled),
    };

    let response = response.map_err(|error| Error::Network(format!("Failed to download {url}: {error}")))?;
    let status = response.status();

    if !status.is_success() {
        return Err(Error::Network(format!("Failed to download {url}: HTTP {status}")));
    }

    let mut response = response;
    let mut file = tokio::fs::File::create(destination)
        .await
        .map_err(|error| Error::Local(format!("{}: {error}", destination.display())))?;
    let mut written = 0u64;

    loop {
        let chunk = tokio::select! {
            chunk = response.chunk() => chunk,
            () = cancel.cancelled() => return Err(Error::Cancelled),
        };

        let Some(chunk) = chunk.map_err(|error| Error::Network(error.to_string()))? else {
            break;
        };

        written += chunk.len() as u64;

        if written > MAX_DOWNLOAD_BYTES {
            return Err(Error::Network(format!("{url}: download exceeds {MAX_DOWNLOAD_BYTES} bytes")));
        }

        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .map_err(|error| Error::Local(format!("{}: {error}", destination.display())))?;
    }

    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|error| Error::Local(format!("{}: {error}", destination.display())))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client() -> Client {
        Client::builder().build().expect("client")
    }

    fn sources(base: &str) -> SpecialSources {
        SpecialSources {
            substrate_index: format!("{base}/package/mobilesubstrate/"),
            substrate_deb_template: format!("{base}/debs/mobilesubstrate_{{version}}_iphoneos-arm.deb"),
            substrate_default_version: "0.9.6301".into(),
            substitute_index: format!("{base}/debs/1443.00/"),
            substitute_default_deb: "com.ex.substitute_2.1.0_iphoneos-arm.deb".into(),
            spoofer_json: format!("{base}/spoofer.json"),
            spoofer_base: format!("{base}/"),
        }
    }

    #[tokio::test]
    async fn substrate_scrapes_the_latest_version_and_falls_back_when_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/package/mobilesubstrate/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<b>latest\">0.9.7000</b> other"))
            .mount(&server)
            .await;

        let url = sources(&server.uri()).resolve(&client(), "sideport-test", SUBSTRATE).await.expect("substrate");

        assert_eq!(url, format!("{}/debs/mobilesubstrate_0.9.7000_iphoneos-arm.deb", server.uri()));
    }

    #[tokio::test]
    async fn substrate_uses_the_default_version_on_a_bad_index() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/package/mobilesubstrate/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let url = sources(&server.uri()).resolve(&client(), "sideport-test", SUBSTRATE).await.expect("substrate");

        assert!(url.ends_with("mobilesubstrate_0.9.6301_iphoneos-arm.deb"));
    }

    #[tokio::test]
    async fn substitute_picks_the_highest_version_from_the_listing() {
        let listing = r#"<a href="com.ex.substitute_2.0.0_iphoneos-arm.deb">old</a>
                         <a href="com.ex.substitute_2.2.1_iphoneos-arm.deb">new</a>
                         <a href="com.ex.substitute_2.1.0_iphoneos-arm.deb">mid</a>"#;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/debs/1443.00/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(listing))
            .mount(&server)
            .await;

        let url = sources(&server.uri()).resolve(&client(), "sideport-test", SUBSTITUTE).await.expect("substitute");

        assert_eq!(url, format!("{}/debs/1443.00/com.ex.substitute_2.2.1_iphoneos-arm.deb", server.uri()));
    }

    #[tokio::test]
    async fn spoofer_reads_the_filename_from_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/spoofer.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"filename":"spoofer-1.2.deb"}"#))
            .mount(&server)
            .await;

        let url = sources(&server.uri()).resolve(&client(), "sideport-test", SPOOFER).await.expect("spoofer");

        assert_eq!(url, format!("{}/spoofer-1.2.deb", server.uri()));
    }

    #[tokio::test]
    async fn an_unknown_special_is_rejected() {
        let error =
            SpecialSources::default().resolve(&client(), "sideport-test", "nope").await.expect_err("expected error");

        assert!(matches!(error, Error::Link(message) if message.contains("Wrong special lib")));
    }

    #[tokio::test]
    async fn download_writes_the_body_to_the_destination() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tweak.deb"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"!<arch>\n".to_vec()))
            .mount(&server)
            .await;

        let directory = tempfile::tempdir().expect("tempdir");
        let destination = directory.path().join("tweak.deb");
        let url = format!("{}/tweak.deb", server.uri());

        download(&client(), "sideport-test", &url, &destination, &CancellationToken::new()).await.expect("download");

        assert_eq!(std::fs::read(&destination).expect("read"), b"!<arch>\n");
    }

    #[test]
    fn version_parsing_prefers_larger_numeric_tuples() {
        assert!(version_numbers("2.10.0") > version_numbers("2.9.9"));
        assert_eq!(latest_substitute_deb("no substitute here"), None);
    }
}
