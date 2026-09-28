//! App Store enrichment of a downloaded IPA (recovered `ipa.EnrichIpa`).
//!
//! Every existing entry is copied unchanged, then `iTunesMetadata.plist` (the link's metadata
//! text), `Payload/<app>.app/SC_Info/<app>.sinf` (base64-decoded SINF, named after the app
//! directory) and `iTunesArtwork` (fetched from the artwork URL) are added.

use crate::flip::FlippedFile;
use crate::link::Enrichment;
use crate::{Error, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::Client;
use std::io::{Seek, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;

/// Artwork responses larger than this are rejected.
const MAX_ARTWORK_BYTES: usize = 16 * 1024 * 1024;

pub(crate) async fn enrich(client: &Client, path: &Path, flipped: bool, enrichment: &Enrichment) -> Result<()> {
    let artwork = match &enrichment.artwork {
        Some(url) => Some(fetch_artwork(client, url).await?),
        None => None,
    };

    let sinf = match &enrichment.sinfs {
        Some(encoded) => {
            Some(STANDARD.decode(encoded).map_err(|error| Error::Enrich(format!("invalid SINF: {error}")))?)
        }
        None => None,
    };

    let metadata = enrichment.metadata.clone().unwrap_or_default();
    let path = path.to_owned();

    tokio::task::spawn_blocking(move || {
        rewrite(&path, flipped, metadata.as_bytes(), sinf.as_deref(), artwork.as_deref())
    })
    .await
    .map_err(|error| Error::Enrich(error.to_string()))?
}

async fn fetch_artwork(client: &Client, url: &str) -> Result<Vec<u8>> {
    let response =
        client.get(url).send().await.map_err(|error| Error::Enrich(format!("Artwork fetch failed: {error}")))?;

    if !response.status().is_success() {
        return Err(Error::Enrich(format!("Artwork download failed: HTTP {}", response.status())));
    }

    let bytes = response.bytes().await.map_err(|error| Error::Enrich(format!("Artwork download failed: {error}")))?;

    if bytes.len() > MAX_ARTWORK_BYTES {
        return Err(Error::Enrich("artwork exceeds 16 MiB".into()));
    }

    Ok(bytes.to_vec())
}

fn rewrite(path: &Path, flipped: bool, metadata: &[u8], sinf: Option<&[u8]>, artwork: Option<&[u8]>) -> Result<()> {
    let source = std::fs::File::open(path).map_err(local)?;
    let mut archive = zip::ZipArchive::new(FlippedFile::new(source, flipped))
        .map_err(|error| Error::Enrich(format!("Not a good partial IPA: {error}")))?;

    let directory = path.parent().unwrap_or(Path::new("."));
    let output = tempfile::NamedTempFile::new_in(directory).map_err(local)?;
    let writer = FlippedFile::new(output.reopen().map_err(local)?, flipped);
    let mut zip = zip::ZipWriter::new(writer);

    let mut app = None;

    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index).map_err(zip_error)?;
        let name = entry.name().to_owned();

        if app.is_none()
            && let Some(directory) = app_directory(&name)
        {
            app = Some(directory);
        }

        zip.raw_copy_file(entry).map_err(zip_error)?;
    }

    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("iTunesMetadata.plist", options).map_err(zip_error)?;
    zip.write_all(metadata).map_err(local)?;

    if let Some(sinf) = sinf {
        let app = app.ok_or_else(|| Error::Enrich("no Payload/*.app directory".into()))?;

        zip.start_file(format!("Payload/{app}.app/SC_Info/{app}.sinf"), options).map_err(zip_error)?;
        zip.write_all(sinf).map_err(local)?;
    }

    if let Some(artwork) = artwork {
        zip.start_file("iTunesArtwork", options).map_err(zip_error)?;
        zip.write_all(artwork).map_err(local)?;
    }

    let mut writer = zip.finish().map_err(zip_error)?;
    writer.flush().map_err(local)?;
    writer.rewind().map_err(local)?;

    output.persist(path).map_err(|error| local(error.error))?;

    Ok(())
}

/// `Payload/<name>.app/…` → `<name>` (the recovered name for the SINF file and directory).
fn app_directory(name: &str) -> Option<String> {
    let rest = name.strip_prefix("Payload/")?;
    let directory = rest.split('/').next()?;

    directory.strip_suffix(".app").filter(|stem| !stem.is_empty()).map(str::to_owned)
}

fn zip_error(error: zip::result::ZipError) -> Error {
    Error::Enrich(error.to_string())
}

fn local(error: std::io::Error) -> Error {
    Error::Local(error.to_string())
}
