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
