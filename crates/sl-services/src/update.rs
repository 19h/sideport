//! The recovered self-update protocol: a JSON version manifest, a BSDIFF40 patch or a gzip full
//! binary, SHA-256 verification and the `.new`/`.old`/`.bak` file swap.
//!
//! Recovered from `sideloadly/updating` and its embedded `go-selfupdate`/`go-update` fork. The
//! layout is the public `go-selfupdate` contract: an info document at `<base><cmd>/<platform>.json`,
//! a patch at `<base><cmd>/<old>/<new>/<platform>` and a full binary at
//! `<base><cmd>/<new>/<platform>.gz`. This crate carries **no** endpoints: every base URL, the
//! command name and the platform token are supplied by the caller, and with nothing configured the
//! engine reports [`UpdateStatus::NotConfigured`]. Downloads are staged and verified; installing the
//! result operates only on caller-provided paths and never on a real running binary.

use crate::{Error, Result};
use flate2::read::GzDecoder;
use reqwest::header::{HeaderValue, USER_AGENT};
use reqwest::{Client, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Bound on any single update response after decompression. Deliberately large for full binaries,
/// but explicit rather than unbounded.
pub const MAX_RESPONSE_BYTES: usize = 512 * 1024 * 1024;

/// Configurable update endpoints. There are no defaults; the caller sets every field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateEndpoints {
    /// Base for the version manifest, e.g. `https://host/updates/`.
    pub info_base: String,
    /// Base for binary-diff patches.
    pub diff_base: String,
    /// Base for full gzip binaries.
    pub binary_base: String,
    /// Component name, e.g. `exe`, `lib` or `daemon`.
    pub command: String,
    /// Platform token, e.g. `darwin-arm64`. Caller-supplied and expected to be URL-safe.
    pub platform: String,
}

/// A parsed version manifest: a version string and the 32-byte SHA-256 of the target binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub sha256: [u8; 32],
}

#[derive(Deserialize)]
struct WireManifest {
    #[serde(alias = "Version")]
    version: String,
    /// base64, as Go marshals `[]byte`.
    #[serde(alias = "Sha256")]
    sha256: String,
}

/// The result of a version check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// No update endpoints are configured, so no check was performed.
    NotConfigured,
    /// The running version matches the manifest.
    UpToDate { version: String },
    /// A newer version is offered.
    Available { manifest: Manifest },
}

/// Which download path produced a staged binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePlan {
    /// The binary diff patched the current binary and matched the checksum.
    Patched,
    /// The full gzip binary was downloaded and matched the checksum.
    FullDownload,
}

/// Fetches and verifies updates for one component.
#[derive(Debug, Clone)]
pub struct Updater {
    client: Client,
    endpoints: UpdateEndpoints,
    user_agent: String,
}

impl Updater {
    /// Build an updater. `user_agent` is generic (e.g. `sideport/<version>`); the recovered
    /// `sideloadly/<ver> darwin` string is deliberately not reproduced.
    pub fn new(endpoints: UpdateEndpoints, user_agent: &str) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self { client, endpoints, user_agent: user_agent.into() })
    }

    /// Check the manifest and compare its version with `current_version`.
    pub async fn check(&self, current_version: &str) -> Result<UpdateStatus> {
        let url = format!("{}{}/{}.json", self.endpoints.info_base, self.endpoints.command, self.endpoints.platform);
        let body = self.get(&url, &CancellationToken::new()).await?;

        let manifest = parse_manifest(&body)?;

        if manifest.version == current_version {
            Ok(UpdateStatus::UpToDate { version: manifest.version })
        } else {
            Ok(UpdateStatus::Available { manifest })
        }
    }

    /// Stage the target binary at `staged_path`, trying the patch first and falling back to the
    /// full binary, and verifying the SHA-256 of the result against `manifest`. `current_binary`
    /// is the local file the patch applies to; it is read, never modified.
    pub async fn stage(
        &self,
        current_version: &str,
        current_binary: &Path,
        manifest: &Manifest,
        staged_path: &Path,
        cancel: &CancellationToken,
    ) -> Result<UpdatePlan> {
        let old = std::fs::read(current_binary).map_err(|error| Error::Io(error.to_string()))?;

        if let Ok(patched) = self.try_patch(current_version, manifest, &old, cancel).await {
            write_executable(staged_path, &patched)?;

            return Ok(UpdatePlan::Patched);
        }

        let full = self.download_full(manifest, cancel).await?;

        if Sha256::digest(&full).as_slice() != manifest.sha256 {
            return Err(Error::Checksum);
        }

        write_executable(staged_path, &full)?;

        Ok(UpdatePlan::FullDownload)
    }

    /// Fetch and apply the binary diff, returning the patched bytes only when they match the
    /// manifest checksum.
    async fn try_patch(
        &self,
        current_version: &str,
        manifest: &Manifest,
        old: &[u8],
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "{}{}/{}/{}/{}",
            self.endpoints.diff_base,
            self.endpoints.command,
            current_version,
            manifest.version,
            self.endpoints.platform
        );
        let patch = self.get(&url, cancel).await?;

        let patched = crate::bspatch(old, &patch)?;

        if Sha256::digest(&patched).as_slice() != manifest.sha256 {
            return Err(Error::Checksum);
        }

        Ok(patched)
    }

    /// Fetch and gunzip the full binary; the caller verifies its checksum.
    async fn download_full(&self, manifest: &Manifest, cancel: &CancellationToken) -> Result<Vec<u8>> {
        let url = format!(
            "{}{}/{}/{}.gz",
            self.endpoints.binary_base, self.endpoints.command, manifest.version, self.endpoints.platform
        );
        let compressed = self.get(&url, cancel).await?;

        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut binary = Vec::new();
        let mut limited = (&mut decoder).take(MAX_RESPONSE_BYTES as u64 + 1);

        limited.read_to_end(&mut binary).map_err(|_| Error::Invalid("update is not gzip"))?;

        if binary.len() > MAX_RESPONSE_BYTES {
            return Err(Error::ResponseTooLarge(MAX_RESPONSE_BYTES));
        }

        Ok(binary)
    }

    async fn get(&self, url: &str, cancel: &CancellationToken) -> Result<Vec<u8>> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let agent = HeaderValue::from_str(&self.user_agent).map_err(|_| Error::Invalid("user agent"))?;
        let request = self.client.get(url).header(USER_AGENT, agent).send();

        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            response = request => read_bounded(response?, MAX_RESPONSE_BYTES).await,
        }
    }
}

fn parse_manifest(body: &[u8]) -> Result<Manifest> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    let wire: WireManifest = serde_json::from_slice(body).map_err(|_| Error::Manifest)?;

    if wire.version.is_empty() {
        return Err(Error::Manifest);
    }

    let digest = STANDARD.decode(wire.sha256.trim()).map_err(|_| Error::Manifest)?;
    let sha256: [u8; 32] = digest.try_into().map_err(|_| Error::Manifest)?;

    Ok(Manifest { version: wire.version, sha256 })
}

async fn read_bounded(response: Response, limit: usize) -> Result<Vec<u8>> {
    if !response.status().is_success() {
        return Err(Error::HttpStatus(response.status().as_u16()));
    }

    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(Error::ResponseTooLarge(limit));
    }

    let mut body = Vec::new();
    let mut response = response;

    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > limit - body.len() {
            return Err(Error::ResponseTooLarge(limit));
        }

        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

fn write_executable(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|error| Error::Io(error.to_string()))?;

    set_executable(path)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(|error| Error::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// The recovered file swap and backup steps, operating only on caller-provided paths.
///
/// The recovered `go-update` replaces a file by writing `.<name>.new`, renaming the live file to
/// `.<name>.old`, renaming the new file into place, and deleting `.old`; a failed final rename
/// renames `.old` back. Separately, the recovered `updating.CheckAndUpdate` copies the library and
/// daemon to `.bak` before updating and, on failure, tries to restore them — but it renames the
/// *daemon* backup onto the *library* path, so a rolled-back library becomes the daemon binary.
/// That is a recovered bug; [`restore_from_backup`] restores each file from its own backup instead.
pub mod swap {
    use crate::{Error, Result};
    use std::path::{Path, PathBuf};

    /// Copy `path` to `<path>.bak`, returning the backup's path.
    pub fn backup(path: &Path) -> Result<PathBuf> {
        let backup = sibling(path, "bak")?;

        std::fs::copy(path, &backup).map_err(|error| Error::Io(error.to_string()))?;

        Ok(backup)
    }

    /// Move `staged` onto `target`, keeping the replaced file as `.old` until the move succeeds and
    /// rolling it back if the move fails.
    pub fn install(target: &Path, staged: &Path) -> Result<()> {
        let old = sibling(target, "old")?;
        let had_target = target.exists();

        if had_target {
            std::fs::rename(target, &old).map_err(|error| Error::Io(error.to_string()))?;
        }

        if let Err(error) = std::fs::rename(staged, target) {
            if had_target {
                let _ = std::fs::rename(&old, target);
            }

            return Err(Error::Io(error.to_string()));
        }

        if had_target {
            let _ = std::fs::remove_file(&old);
        }

        Ok(())
    }

    /// Restore `path` from a backup made by [`backup`]. Unlike the recovered client, the backup is
    /// restored onto its own file, not another component's.
    pub fn restore_from_backup(path: &Path, backup: &Path) -> Result<()> {
        std::fs::rename(backup, path).map_err(|error| Error::Io(error.to_string()))
    }

    /// `<dir>/.<file>.<suffix>`, mirroring the recovered `.new`/`.old`/`.bak` naming.
    fn sibling(path: &Path, suffix: &str) -> Result<PathBuf> {
        let directory = path.parent().unwrap_or(Path::new("."));
        let name = path.file_name().and_then(|name| name.to_str()).ok_or(Error::Invalid("target file name"))?;

        Ok(directory.join(format!(".{name}.{suffix}")))
    }
}

#[cfg(test)]
mod tests;
