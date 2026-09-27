use crate::{EngineError, Result, Settings};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

const MAX_SETTINGS_BYTES: u64 = 64 * 1024;

pub(crate) fn load(directory: &Path) -> Result<Settings> {
    fs::create_dir_all(directory).map_err(storage_error)?;
    let path = directory.join("settings.json");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(error) => return Err(storage_error(error)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES + 1).read_to_end(&mut bytes).map_err(storage_error)?;

    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(EngineError::Storage("settings exceed 64 KiB".into()));
    }

    serde_json::from_slice(&bytes).map_err(|error| EngineError::Storage(format!("invalid settings: {error}")))
}

pub(crate) fn save(directory: &Path, settings: &Settings) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(settings).map_err(|error| EngineError::Storage(error.to_string()))?;

    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(EngineError::Storage("settings exceed 64 KiB".into()));
    }

    let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(storage_error)?;
    temporary.write_all(&bytes).map_err(storage_error)?;
    temporary.as_file().sync_all().map_err(storage_error)?;
    temporary.persist(directory.join("settings.json")).map_err(|error| storage_error(error.error))?;

    Ok(())
}

fn storage_error(error: std::io::Error) -> EngineError {
    EngineError::Storage(error.to_string())
}
