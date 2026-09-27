//! Secret storage for sessions, remembered passwords and the signing key.
//!
//! The recovered client keeps sessions in a plaintext `sessions.json` restricted to mode 0600.
//! [`FileSecrets`] preserves that model for tests and hosts without a keychain. On macOS the
//! default is the login keychain, scoped by a service name derived from the data directory.

use crate::error::{EngineError, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const MAX_SECRETS_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SECRET_BYTES: usize = 256 * 1024;

/// Key/value secret storage. Keys are ASCII names such as `session/<apple id>`.
pub(crate) trait SecretStore: fmt::Debug + Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Zeroizing<Vec<u8>>>>;
    fn set(&self, key: &str, value: &[u8]) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

pub(crate) fn open(data_dir: &Path, file_secrets: bool) -> Result<Box<dyn SecretStore>> {
    #[cfg(target_os = "macos")]
    if !file_secrets {
        return Ok(Box::new(keychain::KeychainSecrets::new(data_dir)));
    }

    #[cfg(not(target_os = "macos"))]
    let _ = file_secrets;

    Ok(Box::new(FileSecrets::new(data_dir)))
}

fn check(key: &str, value: Option<&[u8]>) -> Result<()> {
    let valid_key = !key.is_empty() && key.len() <= 1024 && !key.chars().any(char::is_control);

    if !valid_key {
        return Err(EngineError::Storage("invalid secret name".into()));
    }

    if value.is_some_and(|value| value.len() > MAX_SECRET_BYTES) {
        return Err(EngineError::Storage("secret exceeds 256 KiB".into()));
    }

    Ok(())
}

/// A JSON map of base64 values in `<data_dir>/secrets.json`, created with mode 0600 and replaced
/// atomically. One process writes it at a time; the engine serializes its own writers.
pub(crate) struct FileSecrets {
    path: PathBuf,
    lock: Mutex<()>,
}

impl fmt::Debug for FileSecrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FileSecrets").field("path", &self.path).finish_non_exhaustive()
    }
}

impl FileSecrets {
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self { path: data_dir.join("secrets.json"), lock: Mutex::new(()) }
    }

    fn load(&self) -> Result<BTreeMap<String, String>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(error) => return Err(storage_error(error)),
        };

        let mut bytes = Zeroizing::new(Vec::new());
        file.take(MAX_SECRETS_BYTES + 1).read_to_end(&mut bytes).map_err(storage_error)?;

        if bytes.len() as u64 > MAX_SECRETS_BYTES {
            return Err(EngineError::Storage("secret file exceeds 4 MiB".into()));
        }

        serde_json::from_slice(&bytes).map_err(|_| EngineError::Storage("secret file is corrupted".into()))
    }

    fn save(&self, secrets: &BTreeMap<String, String>) -> Result<()> {
        let bytes =
            Zeroizing::new(serde_json::to_vec(secrets).map_err(|error| EngineError::Storage(error.to_string()))?);
        let directory = self.path.parent().unwrap_or(Path::new("."));

        fs::create_dir_all(directory).map_err(storage_error)?;

        let mut temporary =
            tempfile::Builder::new().prefix(".secrets").tempfile_in(directory).map_err(storage_error)?;
        restrict(temporary.as_file())?;

        temporary.write_all(&bytes).map_err(storage_error)?;
        temporary.as_file().sync_all().map_err(storage_error)?;
        temporary.persist(&self.path).map_err(|error| storage_error(error.error))?;

        Ok(())
    }
}

impl SecretStore for FileSecrets {
    fn get(&self, key: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        check(key, None)?;

        let _guard = self.lock.lock();
        let secrets = self.load()?;

        let Some(encoded) = secrets.get(key) else {
            return Ok(None);
        };

        let decoded = STANDARD.decode(encoded).map_err(|_| EngineError::Storage("secret file is corrupted".into()))?;

        Ok(Some(Zeroizing::new(decoded)))
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<()> {
        check(key, Some(value))?;

        let _guard = self.lock.lock();
        let mut secrets = self.load()?;

        secrets.insert(key.into(), STANDARD.encode(value));
        self.save(&secrets)
    }

    fn delete(&self, key: &str) -> Result<()> {
        check(key, None)?;

        let _guard = self.lock.lock();
        let mut secrets = self.load()?;

        if secrets.remove(key).is_some() {
            self.save(&secrets)?;
        }

        Ok(())
    }
}

#[cfg(unix)]
fn restrict(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600)).map_err(storage_error)
}

#[cfg(not(unix))]
fn restrict(_file: &File) -> Result<()> {
    Ok(())
}

fn storage_error(error: std::io::Error) -> EngineError {
    EngineError::Storage(error.to_string())
}

#[cfg(target_os = "macos")]
mod keychain {
    use super::{SecretStore, check};
    use crate::error::{EngineError, Result};
    use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};
    use sha2::{Digest, Sha256};
    use std::path::Path;
    use zeroize::Zeroizing;

    /// errSecItemNotFound.
    const NOT_FOUND: i32 = -25300;

    /// Generic-password items whose service names the data directory, so separate data
    /// directories (and test engines) never share items.
    #[derive(Debug)]
    pub(crate) struct KeychainSecrets {
        service: String,
    }

    impl KeychainSecrets {
        pub(crate) fn new(data_dir: &Path) -> Self {
            let digest = Sha256::digest(data_dir.to_string_lossy().as_bytes());
            let scope: String = digest[..6].iter().map(|byte| format!("{byte:02x}")).collect();

            Self { service: format!("Sideport ({scope})") }
        }
    }

    impl SecretStore for KeychainSecrets {
        fn get(&self, key: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
            check(key, None)?;

            match get_generic_password(&self.service, key) {
                Ok(value) => Ok(Some(Zeroizing::new(value))),
                Err(error) if error.code() == NOT_FOUND => Ok(None),
                Err(error) => Err(keychain_error(error)),
            }
        }

        fn set(&self, key: &str, value: &[u8]) -> Result<()> {
            check(key, Some(value))?;

            set_generic_password(&self.service, key, value).map_err(keychain_error)
        }

        fn delete(&self, key: &str) -> Result<()> {
            check(key, None)?;

            match delete_generic_password(&self.service, key) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == NOT_FOUND => Ok(()),
                Err(error) => Err(keychain_error(error)),
            }
        }
    }

    fn keychain_error(error: security_framework::base::Error) -> EngineError {
        EngineError::Storage(format!("keychain error {}", error.code()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_secrets_round_trip_with_private_mode_and_reject_corruption() {
        let directory = tempfile::tempdir().expect("data directory");
        let secrets = FileSecrets::new(directory.path());

        assert!(secrets.get("session/a").expect("empty").is_none());

        secrets.set("session/a", b"first").expect("set");
        secrets.set("session/b", &[0, 255, 1]).expect("binary");
        assert_eq!(secrets.get("session/a").expect("get").as_deref().map(Vec::as_slice), Some(&b"first"[..]));
        assert_eq!(secrets.get("session/b").expect("get").as_deref().map(Vec::as_slice), Some(&[0, 255, 1][..]));

        secrets.delete("session/a").expect("delete");
        secrets.delete("session/missing").expect("absent delete");
        assert!(secrets.get("session/a").expect("deleted").is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(directory.path().join("secrets.json")).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        assert!(secrets.set("", b"value").is_err());
        assert!(secrets.set("line\nbreak", b"value").is_err());
        assert!(secrets.set("large", &vec![0; MAX_SECRET_BYTES + 1]).is_err());

        fs::write(directory.path().join("secrets.json"), b"{not json").expect("corrupt");
        let error = secrets.get("session/b").expect_err("corrupted file");
        assert!(!error.to_string().contains("not json"));
    }
}
