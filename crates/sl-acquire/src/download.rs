//! Resumable IPA downloads (recovered `RemoteFile.downloadImpl`/`tryDownload`).
//!
//! Each attempt resumes from the destination's length with `Range: bytes=<n>-`. A failed
//! attempt that copied bytes continues at once with the delay reset to 1 s; one that copied
//! nothing waits twice the previous delay and gives up (deleting the file) when that wait would
//! reach 20 s. HTML responses and non-ZIP starts fail without retrying. After the body, the
//! MD5/SHA-1 digest is checked and the IPA enriched; either failure restarts from zero, at most
//! three times. A server that ignores `Range` on a resume restarts the file.

use crate::enrich;
use crate::flip::{self, FlippedFile};
use crate::link::{Digest, Enrichment};
use crate::{Error, Result};
use md5::Md5;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, RANGE, USER_AGENT};
use reqwest::{Client, StatusCode};
use sha1::{Digest as _, Sha1};
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const FIRST_DELAY: Duration = Duration::from_secs(1);
const MAXIMUM_DELAY: Duration = Duration::from_secs(20);

/// Complete downloads rejected by the digest or enrichment before giving up. The recovered loop
/// retries these without a bound; a permanently wrong file would never finish.
const FULL_RETRIES: u32 = 3;
const HTML_EXCERPT: usize = 256;
const ZIP_MAGIC: [u8; 4] = *b"PK\x03\x04";

/// Progress and log sink for a download.
pub trait Observer: Send + Sync {
    fn progress(&self, done: u64, total: u64);
    fn log(&self, message: &str);
}

#[derive(Debug)]
pub struct Request<'a> {
    pub url: &'a str,
    pub digest: Option<&'a Digest>,
    pub enrichment: &'a Enrichment,
    pub destination: &'a Path,
    /// Store the file XOR-0xAA flipped, as the recovered cache does.
    pub flipped: bool,
}

#[derive(Debug, Clone)]
pub struct Downloader {
    client: Client,
    user_agent: String,
    first_delay: Duration,
    maximum_delay: Duration,
}

/// Why an attempt ended, and whether the retry loop may continue.
struct Failure {
    error: Error,
    copied: u64,
    retryable: bool,
    /// The whole file was fetched and rejected (digest or enrichment).
    rejected: bool,
}

impl Failure {
    fn retry(error: Error, copied: u64) -> Self {
        Self { error, copied, retryable: true, rejected: false }
    }

    fn rejected(error: Error, copied: u64) -> Self {
        Self { error, copied, retryable: true, rejected: true }
    }

    fn fatal(error: Error) -> Self {
        Self { error, copied: 0, retryable: false, rejected: false }
    }
}

impl Downloader {
    /// `user_agent` replaces the recovered `sideloadly/<version>@darwin`.
    pub fn new(user_agent: &str) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(20))
            .build()
            .map_err(|error| Error::Network(error.to_string()))?;

        Ok(Self { client, user_agent: user_agent.into(), first_delay: FIRST_DELAY, maximum_delay: MAXIMUM_DELAY })
    }

    /// Replace the recovered 1 s first delay and 20 s ceiling (tests use short delays).
    pub fn with_backoff(mut self, first: Duration, maximum: Duration) -> Self {
        self.first_delay = first;
        self.maximum_delay = maximum;

        self
    }

    pub async fn download(
        &self,
        request: Request<'_>,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut delay = self.first_delay;
        let mut rejected = 0;

        loop {
            let failure = match self.attempt(&request, observer, cancel).await {
                Ok(()) => return Ok(()),
                Err(failure) => failure,
            };

            observer.log(&format!("Download error: {}", failure.error));

            if !failure.retryable {
                return Err(failure.error);
            }

            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }

            if failure.rejected {
                rejected += 1;

                if rejected >= FULL_RETRIES {
                    let _ = std::fs::remove_file(request.destination);

                    return Err(failure.error);
                }
            }

            if failure.copied > 0 {
                delay = self.first_delay;
            } else {
                let wait = delay * 2;

                if wait >= self.maximum_delay {
                    observer.log("Nothing copied and delay is exceeded, bailing out");
                    let _ = std::fs::remove_file(request.destination);

                    return Err(failure.error);
                }

                observer.log(&format!("Nothing downloaded this time, will retry in {}s", wait.as_secs()));

                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = cancel.cancelled() => return Err(Error::Cancelled),
                }

                delay = wait;
            }

            observer.log("Continuing download...");
        }
    }

    async fn attempt(
        &self,
        request: &Request<'_>,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> Result<(), Failure> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(request.destination)
            .map_err(|error| Failure::fatal(Error::Local(error.to_string())))?;
        let mut file = FlippedFile::new(file, request.flipped);

        let mut position = file.len().map_err(|error| Failure::fatal(Error::Local(error.to_string())))?;
        observer.log(&format!("Downloading {} to {}", request.url, request.destination.display()));

        let mut builder = self.client.get(request.url).header(USER_AGENT, &self.user_agent);

        if position > 0 {
            builder = builder.header(RANGE, format!("bytes={position}-"));
        }

        let response = tokio::select! {
            response = builder.send() => response,
            () = cancel.cancelled() => return Err(Failure::fatal(Error::Cancelled)),
        };

        let response = response.map_err(|error| {
            Failure::retry(Error::Network(format!("Failed to download {}: {error}", request.url)), 0)
        })?;
        let status = response.status();

        if status != StatusCode::OK && status != StatusCode::PARTIAL_CONTENT {
            return Err(Failure::retry(
                Error::Network(format!("Failed to download {}: HTTP {status}", request.url)),
                0,
            ));
        }

        // A server that ignores Range sends the whole file; restart instead of appending it.
        if position > 0 && status == StatusCode::OK {
            file.truncate().map_err(|error| Failure::fatal(Error::Local(error.to_string())))?;
            position = 0;
        }

        let content_type =
            response.headers().get(CONTENT_TYPE).and_then(|value| value.to_str().ok()).unwrap_or_default();

        if content_type.to_ascii_lowercase().starts_with("text/html") {
            let final_url = response.url().to_string();
            let body = response.bytes().await.unwrap_or_default();
            let excerpt = String::from_utf8_lossy(&body[..body.len().min(HTML_EXCERPT)]).into_owned();

            return Err(Failure::fatal(Error::Html(format!(
                "This is an HTML, not IPA file! URL: {final_url}. {excerpt}"
            ))));
        }

        let length = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
            .unwrap_or(0);
        observer.progress(position, position + length);

        let copied = self.copy(response, &mut file, position, position + length, observer, cancel).await?;

        if let Some(digest) = request.digest
            && let Err(error) = verify(&mut file, digest)
        {
            file.truncate().map_err(|error| Failure::fatal(Error::Local(error.to_string())))?;

            return Err(Failure::rejected(error, copied));
        }

        if !request.enrichment.is_empty() {
            let enriched = enrich::enrich(&self.client, request.destination, request.flipped, request.enrichment).await;

            if let Err(error) = enriched {
                file.truncate().map_err(|error| Failure::fatal(Error::Local(error.to_string())))?;

                let error = Error::Enrich(format!("Enrich failed: {error}; will re-download and retry"));

                return Err(Failure::rejected(error, copied));
            }
        }

        Ok(())
    }

    async fn copy(
        &self,
        mut response: reqwest::Response,
        file: &mut FlippedFile,
        start: u64,
        total: u64,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> Result<u64, Failure> {
        let mut copied = 0u64;
        let mut header = Vec::with_capacity(4);

        file.seek_end().map_err(|error| Failure::fatal(Error::Local(error.to_string())))?;

        loop {
            let chunk = tokio::select! {
                chunk = response.chunk() => chunk,
                () = cancel.cancelled() => return Err(Failure::retry(Error::Cancelled, copied)),
            };

            let chunk = chunk.map_err(|error| Failure::retry(Error::Network(error.to_string()), copied))?;

            let Some(chunk) = chunk else {
                break;
            };

            // Recovered ZipValidator: a fresh download must start with a local file header.
            if start == 0 && header.len() < 4 {
                let needed = (4 - header.len()).min(chunk.len());
                header.extend_from_slice(&chunk[..needed]);

                if header.len() == 4 && header != ZIP_MAGIC {
                    return Err(Failure::fatal(Error::NotIpa));
                }
            }

            file.write_all(&chunk).map_err(|error| Failure::retry(Error::Local(error.to_string()), copied))?;
            copied += chunk.len() as u64;

            observer.progress(start + copied, total.max(start + copied));
        }

        file.flush().map_err(|error| Failure::retry(Error::Local(error.to_string()), copied))?;

        if start == 0 && header.len() < 4 {
            return Err(Failure::fatal(Error::NotIpa));
        }

        Ok(copied)
    }
}

fn verify(file: &mut FlippedFile, digest: &Digest) -> Result<()> {
    file.rewind().map_err(|error| Error::Local(error.to_string()))?;

    let mut buffer = vec![0; 128 * 1024];
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();

    loop {
        let read = file.read(&mut buffer).map_err(|error| Error::Local(format!("Failed to compute hash: {error}")))?;

        if read == 0 {
            break;
        }

        md5.update(&buffer[..read]);
        sha1.update(&buffer[..read]);
    }

    let (expected, actual) = match digest {
        Digest::Md5(expected) => (expected.to_vec(), md5.finalize().to_vec()),
        Digest::Sha1(expected) => (expected.to_vec(), sha1.finalize().to_vec()),
    };

    if expected != actual {
        return Err(Error::Hash(format!(
            "Hash mismatch, will re-download. Expected hash {}, got hash {}",
            hex::encode(expected),
            hex::encode(actual)
        )));
    }

    Ok(())
}

/// Read a whole downloaded file, unflipping when needed (for callers and tests).
pub fn read_plain(path: &Path, flipped: bool) -> Result<Vec<u8>> {
    let mut file = File::open(path).map_err(|error| Error::Local(error.to_string()))?;
    let mut bytes = Vec::new();

    file.read_to_end(&mut bytes).map_err(|error| Error::Local(error.to_string()))?;

    if flipped {
        flip::xor(&mut bytes);
    }

    Ok(bytes)
}
