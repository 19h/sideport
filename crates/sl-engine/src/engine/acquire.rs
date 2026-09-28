//! Remote job sources: `sideloadly:` links and HTTP(S) IPA URLs (recovered `urischeme`).
//!
//! Downloads land flipped (XOR 0xAA) in `<data dir>/downloads/sideloadly-<uuid>.ipa`, as the
//! recovered client stores `sideloadly-*.ipa`; the bundle reader accepts flipped archives.

use super::Inner;
use crate::error::{EngineError, Result};
use crate::job::{JobContext, Stage};
use sl_acquire::{Downloader, Link, Observer, Request};
use std::path::{Path, PathBuf};

/// Whether a job source names a remote file rather than a local path.
pub(super) fn is_remote(source: &Path) -> bool {
    let text = source.to_string_lossy();

    text.starts_with("sideloadly:") || text.starts_with("https://") || text.starts_with("http://")
}

fn link(text: &str) -> Result<Link> {
    if text.starts_with("sideloadly:") {
        return Link::parse(text).map_err(|error| EngineError::InvalidApp(error.to_string()));
    }

    Link::parse(&format!("sideloadly:{text}")).map_err(|error| EngineError::InvalidApp(error.to_string()))
}

struct Progress<'a>(&'a JobContext);

impl Observer for Progress<'_> {
    fn progress(&self, done: u64, total: u64) {
        self.0.progress(done, total);
    }

    fn log(&self, message: &str) {
        self.0.info(message);
    }
}

/// Download a remote source into the downloads directory and return the local path.
pub(super) async fn fetch(inner: &Inner, context: &JobContext, source: &str) -> Result<PathBuf> {
    let link = link(source)?;

    let Link::Download { url, name, digest, enrichment } = &link else {
        return Err(EngineError::Unsupported(
            "App Store links need a Store session and kbsync, which are not implemented".into(),
        ));
    };

    let directory = inner.data_dir.join("downloads");
    std::fs::create_dir_all(&directory).map_err(|error| EngineError::Storage(error.to_string()))?;

    let destination = directory.join(format!("sideloadly-{}.ipa", uuid::Uuid::new_v4()));
    let display = name.clone().unwrap_or_else(|| link.file_name());

    context.stage(Stage::Preparing);
    context.info(format!("Downloading {display}"));

    let user_agent = format!("sideport/{}", env!("CARGO_PKG_VERSION"));
    let downloader = Downloader::new(&user_agent).map_err(|error| EngineError::Network(error.to_string()))?;
    let request = Request { url, digest: digest.as_ref(), enrichment, destination: &destination, flipped: true };

    let downloaded = downloader.download(request, &Progress(context), &context.cancellation_token()).await;

    match downloaded {
        Ok(()) => Ok(destination),

        Err(error) => {
            let _ = std::fs::remove_file(&destination);

            Err(match error {
                sl_acquire::Error::Cancelled => EngineError::Cancelled,
                sl_acquire::Error::Local(message) => EngineError::Storage(message),
                sl_acquire::Error::Html(_) | sl_acquire::Error::NotIpa => EngineError::InvalidApp(error.to_string()),
                other => EngineError::Network(other.to_string()),
            })
        }
    }
}
