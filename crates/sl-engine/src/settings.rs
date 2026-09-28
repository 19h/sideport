//! `settings.json` shared by every process on one data directory (desktop app, CLI, daemon).
//!
//! Saves replace the file by rename, so each save gives it a new identity ([`Revision`]); readers
//! compare identities and reload after another process saved. Changes run as read-modify-write
//! on the file's current contents while the engine holds the state database's writer lock, so two
//! processes changing different fields both keep their change.

use crate::{EngineError, Result, Settings};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

const MAX_SETTINGS_BYTES: u64 = 64 * 1024;

/// Identity of the settings file as last read or written. `None` when it does not exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Revision(Option<FileIdentity>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    inode: u64,
    modified: Option<SystemTime>,
    len: u64,
}

impl FileIdentity {
    fn of(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(metadata);
        #[cfg(not(unix))]
        let inode = 0;

        Self { inode, modified: metadata.modified().ok(), len: metadata.len() }
    }
}

fn path(directory: &Path) -> PathBuf {
    directory.join("settings.json")
}

/// The current identity of the settings file, without reading it.
pub(crate) fn revision(directory: &Path) -> Revision {
    Revision(fs::metadata(path(directory)).ok().map(|metadata| FileIdentity::of(&metadata)))
}

/// Read the settings and the identity of the file they came from.
pub(crate) fn load(directory: &Path) -> Result<(Settings, Revision)> {
    fs::create_dir_all(directory).map_err(storage_error)?;

    let file = match File::open(path(directory)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Settings::default(), Revision(None))),
        Err(error) => return Err(storage_error(error)),
    };

    // The identity of the open file matches the bytes read from it, even if a save renames a
    // new file into place meanwhile.
    let identity = FileIdentity::of(&file.metadata().map_err(storage_error)?);

    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES + 1).read_to_end(&mut bytes).map_err(storage_error)?;

    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(EngineError::Storage("settings exceed 64 KiB".into()));
    }

    let settings =
        serde_json::from_slice(&bytes).map_err(|error| EngineError::Storage(format!("invalid settings: {error}")))?;

    Ok((settings, Revision(Some(identity))))
}

/// Replace the settings file atomically; returns the new file's identity.
pub(crate) fn save(directory: &Path, settings: &Settings) -> Result<Revision> {
    let bytes = serde_json::to_vec_pretty(settings).map_err(|error| EngineError::Storage(error.to_string()))?;

    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(EngineError::Storage("settings exceed 64 KiB".into()));
    }

    let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(storage_error)?;
    temporary.write_all(&bytes).map_err(storage_error)?;
    temporary.as_file().sync_all().map_err(storage_error)?;

    let file = temporary.persist(path(directory)).map_err(|error| storage_error(error.error))?;
    let identity = FileIdentity::of(&file.metadata().map_err(storage_error)?);

    Ok(Revision(Some(identity)))
}

fn storage_error(error: std::io::Error) -> EngineError {
    EngineError::Storage(error.to_string())
}
