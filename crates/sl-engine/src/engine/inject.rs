//! Resolve injection sources to local files before the offline bundle pipeline runs.
//!
//! `sl-bundle` stays offline and takes local files. The engine turns each requested injection into
//! one or more local paths: a `.deb` (local, downloaded, or a resolved special) is unpacked with
//! [`sl_bundle::extract_deb`]; an `http(s)://` item is downloaded into a cache; a special
//! (`///special/substrate|substitute|spoofer`, §7.4) is resolved through [`sl_acquire`]; a plain
//! local file passes through unchanged.

use super::Inner;
use crate::error::{EngineError, Result};
use crate::job::JobContext;
use crate::types::LibraryInjection;
use sl_acquire::SpecialResolver;
use sl_bundle::{Control, DebPackage};
use std::fs;
use std::path::{Path, PathBuf};

/// Local injections plus the temporary trees and downloads that back them. Dropping this removes
/// the extracted trees and cached downloads, so callers hold it until preparation has copied them.
pub(super) struct Resolved {
    pub injections: Vec<LibraryInjection>,
    _packages: Vec<DebPackage>,
    _downloads: Vec<CachedDownload>,
}

/// A downloaded artifact removed from the cache when the job finishes.
struct CachedDownload(PathBuf);

impl Drop for CachedDownload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Resolve every requested injection into local files feedable to `sl-bundle`.
pub(super) async fn resolve(inner: &Inner, context: &JobContext, requested: &[LibraryInjection]) -> Result<Resolved> {
    if requested.is_empty() {
        return Ok(Resolved { injections: Vec::new(), _packages: Vec::new(), _downloads: Vec::new() });
    }

    let resolver = SpecialResolver::new(&user_agent(), inner.special_sources.clone())
        .map_err(|error| EngineError::Network(error.to_string()))?;
    let cache = inner.data_dir.join("injection-cache");

    let mut injections = Vec::new();
    let mut packages = Vec::new();
    let mut downloads = Vec::new();

    for item in requested {
        let text = item.source.to_string_lossy().into_owned();

        let local = if let Some(name) = text.strip_prefix("///special/") {
            let url = resolver.resolve(name).await.map_err(acquire_error)?;

            fetch(&resolver, context, &cache, &url, &mut downloads).await?
        } else if text.starts_with("http://") || text.starts_with("https://") {
            fetch(&resolver, context, &cache, &text, &mut downloads).await?
        } else {
            item.source.clone()
        };

        add_source(&local, item.name.clone(), &mut injections, &mut packages).await?;
    }

    Ok(Resolved { injections, _packages: packages, _downloads: downloads })
}

/// Download `url` into the cache and record it for cleanup, returning the local path.
async fn fetch(
    resolver: &SpecialResolver,
    context: &JobContext,
    cache: &Path,
    url: &str,
    downloads: &mut Vec<CachedDownload>,
) -> Result<PathBuf> {
    fs::create_dir_all(cache).map_err(|error| EngineError::Storage(error.to_string()))?;

    let stem = url.rsplit('/').find(|part| !part.is_empty()).unwrap_or("download");
    let safe: String =
        stem.chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') { character } else { '_' }
            })
            .collect();
    let destination = cache.join(format!("{}-{safe}", uuid::Uuid::new_v4()));

    context.info(format!("Downloading {url}"));
    resolver.download(url, &destination, &context.cancellation_token()).await.map_err(acquire_error)?;

    downloads.push(CachedDownload(destination.clone()));

    Ok(destination)
}

/// Add one resolved local path: a `.deb` is unpacked into many injections, anything else passes
/// through. Deb unpacking runs on a blocking worker because it decompresses and writes files.
async fn add_source(
    local: &Path,
    name: Option<String>,
    injections: &mut Vec<LibraryInjection>,
    packages: &mut Vec<DebPackage>,
) -> Result<()> {
    if !is_deb(local) {
        injections.push(LibraryInjection { source: local.to_owned(), name });

        return Ok(());
    }

    let path = local.to_owned();
    let package = tokio::task::spawn_blocking(move || sl_bundle::extract_deb(&path, Control::default()))
        .await
        .map_err(|error| EngineError::Other(format!("deb worker failed: {error}")))?
        .map_err(|error| EngineError::InvalidApp(error.to_string()))?;

    for source in package.injections() {
        injections.push(LibraryInjection { source: source.clone(), name: None });
    }

    packages.push(package);

    Ok(())
}

/// A `.deb` by extension or by the `ar` archive magic (`!<arch>\n`), so downloaded specials that
/// carry no `.deb` suffix are still recognized.
fn is_deb(path: &Path) -> bool {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("deb"))
    {
        return true;
    }

    let mut magic = [0u8; 8];

    fs::File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut magic).map(|()| magic))
        .is_ok_and(|magic| &magic == b"!<arch>\n")
}

fn user_agent() -> String {
    format!("sideport/{}", env!("CARGO_PKG_VERSION"))
}

fn acquire_error(error: sl_acquire::Error) -> EngineError {
    match error {
        sl_acquire::Error::Cancelled => EngineError::Cancelled,
        sl_acquire::Error::Local(message) => EngineError::Storage(message),
        other => EngineError::Network(other.to_string()),
    }
}
