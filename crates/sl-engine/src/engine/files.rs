//! Content-addressed copies of tracked IPAs in `<data_dir>/files/<sha256>.ipa`.
//!
//! The recovered client caches inputs in `stored_files` so refreshes can re-sign after the
//! original download or selection disappears.

use super::Inner;
use crate::error::{EngineError, Result};
use std::path::Path;

/// Delete a stored copy when no installation refers to it any longer. Other paths are ignored.
pub(super) fn release(inner: &Inner, source: &Path) -> Result<()> {
    let directory = inner.data_dir.join("files");

    if source.parent() != Some(directory.as_path()) {
        return Ok(());
    }

    let Some(digest) = source.file_stem().and_then(|stem| stem.to_str()) else {
        return Ok(());
    };

    if inner.store.stored_file_referenced(digest)? {
        return Ok(());
    }

    match std::fs::remove_file(source) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(EngineError::Storage(error.to_string())),
    }

    inner.store.delete_stored_file(digest)
}

/// Copy `source` to `<data_dir>/files/<sha256>.ipa` unless an identical copy exists, and
/// record it. Directory inputs are packed as an IPA first so a refresh can unpack them again.
pub(super) fn store(inner: &Inner, source: &Path) -> Result<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};

    let directory = inner.data_dir.join("files");
    std::fs::create_dir_all(&directory).map_err(storage_error)?;

    if source.parent() == Some(directory.as_path()) {
        return Ok(source.to_owned());
    }

    let mut temporary = tempfile::NamedTempFile::new_in(&directory).map_err(storage_error)?;

    if source.is_dir() {
        let archive = sl_bundle::BundleArchive::unpack(
            source,
            sl_bundle::ArchiveLimits::default(),
            sl_bundle::Control::default(),
        )
        .map_err(crate::pipeline::bundle_error)?;
        archive
            .write_to(
                temporary.as_file_mut(),
                sl_bundle::OutputLayout::Ipa,
                sl_bundle::PackOptions::default(),
                sl_bundle::Control::default(),
            )
            .map_err(crate::pipeline::bundle_error)?;
    } else {
        let mut input = std::fs::File::open(source).map_err(storage_error)?;
        std::io::copy(&mut input, temporary.as_file_mut()).map_err(storage_error)?;
    }

    temporary.as_file_mut().flush().map_err(storage_error)?;
    temporary.as_file().sync_all().map_err(storage_error)?;

    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(temporary.path()).map_err(storage_error)?;
    let mut buffer = vec![0; 128 * 1024];

    loop {
        let read = file.read(&mut buffer).map_err(storage_error)?;

        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);
    }

    let digest: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    let destination = directory.join(format!("{digest}.ipa"));
    let size = std::fs::metadata(temporary.path()).map_err(storage_error)?.len();

    if destination.exists() {
        drop(temporary);
    } else {
        temporary.persist(&destination).map_err(|error| storage_error(error.error))?;
    }

    let name = source.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    inner.store.record_stored_file(&digest, &name, size)?;

    Ok(destination)
}

fn storage_error(error: std::io::Error) -> EngineError {
    EngineError::Storage(error.to_string())
}
