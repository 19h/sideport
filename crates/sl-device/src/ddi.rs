//! Developer Disk Images: mirror catalog, download/cache, and the mount flow.
//!
//! Recovered `sideloadly/mobdev` `MountDeveloperImage`: pre-iOS-17 devices mount a versioned
//! `DeveloperDiskImage.dmg` + `.dmg.signature` from GitHub mirrors; iOS 17+ devices mount a
//! personalized image (`Image.dmg` + `Image.dmg.trustcache` + `BuildManifest.plist`) whose
//! signature is a TSS ticket. All endpoints are configurable so tests use wiremock; the version
//! selection falls back from `major.minor.patch` to `major.minor`, as the recovered
//! `getImageForVersion` does.

use crate::error::{DeviceError, Result};
use crate::mounter::{DEVELOPER, ImageMounting, Mounted, PERSONALIZED};
use crate::tss::TssClient;
use std::io::Read;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// iOS major version at which the personalized (iOS 17+) flow replaces the legacy one.
pub const PERSONALIZED_FROM_MAJOR: u32 = 17;

/// GitHub release mirrors: the asset of `releases/tags/<version>` is the image (a zip).
const RELEASE_REPOS: &[&str] =
    &["xushuduo/Xcode-iOS-Developer-Disk-Image", "mspvirajpatel/Xcode_Developer_Disk_Images"];

/// Raw mirror: `<repo>/master/<version>/DeveloperDiskImage.dmg` (+ `.signature`) directly.
const RAW_REPO: &str = "pdso/DeveloperDiskImage";

/// Personalized image repository (recovered `doronz88/DeveloperDiskImage`).
const PERSONALIZED_REPO: &str = "doronz88/DeveloperDiskImage";

/// Mirror endpoints; overridable so tests never contact GitHub or Apple.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// GitHub API base (default `https://api.github.com`).
    pub github_api: String,
    /// Raw content base (default `https://raw.githubusercontent.com`).
    pub raw_content: String,
    user_agent: String,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            github_api: "https://api.github.com".into(),
            raw_content: "https://raw.githubusercontent.com".into(),
            user_agent: format!("sideport/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl Catalog {
    /// A catalog pointing at the given GitHub API and raw-content bases (used by tests).
    pub fn with_endpoints(github_api: impl Into<String>, raw_content: impl Into<String>) -> Self {
        Self { github_api: github_api.into(), raw_content: raw_content.into(), ..Self::default() }
    }
}

/// The bytes needed to mount, either legacy or personalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Legacy { image: Vec<u8>, signature: Vec<u8> },
    Personalized { image: Vec<u8>, trust_cache: Vec<u8>, build_manifest: Vec<u8> },
}

/// What a mount did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    AlreadyMounted(Mounted),
    Mounted,
}

/// The `major.minor` of an iOS version string, and the parsed major.
fn major_minor(version: &str) -> Option<(String, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next().unwrap_or("0");

    Some((format!("{major}.{minor}"), major))
}

/// Whether the device version uses the personalized flow.
pub fn is_personalized(version: &str) -> bool {
    major_minor(version).map(|(_, major)| major >= PERSONALIZED_FROM_MAJOR).unwrap_or(false)
}

/// Candidate versions to try, in order: the exact version, then `major.minor` (recovered
/// `getImageForVersion` fallback).
fn version_candidates(version: &str) -> Vec<String> {
    let mut candidates = vec![version.to_owned()];

    if let Some((short, _)) = major_minor(version)
        && short != version
    {
        candidates.push(short);
    }

    candidates
}

impl Catalog {
    async fn get(&self, url: &str, cancel: &CancellationToken) -> Result<Option<Vec<u8>>> {
        let client = reqwest::Client::new();
        let request = client.get(url).header("User-Agent", &self.user_agent);

        let response = tokio::select! {
            response = request.send() => response,
            () = cancel.cancelled() => return Err(DeviceError::Cancelled),
        }
        .map_err(|error| DeviceError::Remote(error.to_string()))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            return Err(DeviceError::Remote(format!("{url}: HTTP {}", response.status())));
        }

        let body = tokio::select! {
            body = response.bytes() => body,
            () = cancel.cancelled() => return Err(DeviceError::Cancelled),
        }
        .map_err(|error| DeviceError::Remote(error.to_string()))?;

        Ok(Some(body.to_vec()))
    }

    /// The direct `.dmg` URL for `version` from the raw mirror, and its `.signature`.
    fn raw_urls(&self, version: &str) -> (String, String) {
        let image = format!("{}/{RAW_REPO}/master/{version}/DeveloperDiskImage.dmg", self.raw_content);
        let signature = format!("{image}.signature");

        (image, signature)
    }

    /// Resolve and download a legacy image and its signature, trying each candidate version.
    async fn legacy(&self, version: &str, cancel: &CancellationToken) -> Result<(Vec<u8>, Vec<u8>)> {
        for candidate in version_candidates(version) {
            if let Some(pair) = self.legacy_exact(&candidate, cancel).await? {
                return Ok(pair);
            }
        }

        Err(DeviceError::Unsupported(format!("no Developer Disk Image for iOS {version}")))
    }

    /// Download the image and signature for one exact version, or `None` if the mirrors lack it.
    async fn legacy_exact(&self, version: &str, cancel: &CancellationToken) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        for repo in RELEASE_REPOS {
            if let Some(asset) = self.release_asset(repo, version, cancel).await? {
                let archive = self.get(&asset, cancel).await?;

                if let Some(archive) = archive {
                    return extract_zip(&archive).map(Some);
                }
            }
        }

        let (image_url, signature_url) = self.raw_urls(version);
        let image = self.get(&image_url, cancel).await?;
        let signature = self.get(&signature_url, cancel).await?;

        match (image, signature) {
            (Some(image), Some(signature)) => Ok(Some((image, signature))),
            _ => Ok(None),
        }
    }

    /// The single release asset's download URL for `version`, if the release exists.
    async fn release_asset(&self, repo: &str, version: &str, cancel: &CancellationToken) -> Result<Option<String>> {
        let url = format!("{}/repos/{repo}/releases/tags/{version}", self.github_api);

        let Some(body) = self.get(&url, cancel).await? else {
            return Ok(None);
        };

        let release: serde_json::Value =
            serde_json::from_slice(&body).map_err(|error| DeviceError::Remote(error.to_string()))?;

        let asset = release
            .get("assets")
            .and_then(|assets| assets.as_array())
            .and_then(|assets| assets.first())
            .and_then(|asset| asset.get("browser_download_url"))
            .and_then(|url| url.as_str())
            .map(str::to_owned);

        Ok(asset)
    }

    /// Download the personalized image, its trust cache and its build manifest.
    async fn personalized(&self, cancel: &CancellationToken) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        let base =
            format!("{}/{PERSONALIZED_REPO}/main/PersonalizedImages/Xcode_iOS_DDI_Personalized", self.raw_content);

        let image = self.require(&format!("{base}/Image.dmg"), cancel).await?;
        let trust_cache = self.require(&format!("{base}/Image.dmg.trustcache"), cancel).await?;
        let build_manifest = self.require(&format!("{base}/BuildManifest.plist"), cancel).await?;

        Ok((image, trust_cache, build_manifest))
    }

    async fn require(&self, url: &str, cancel: &CancellationToken) -> Result<Vec<u8>> {
        self.get(url, cancel)
            .await?
            .ok_or_else(|| DeviceError::Unsupported(format!("personalized DDI file missing: {url}")))
    }
}

/// A cache directory for downloaded images (recovered `xdg.CacheFile("sideloadly/…")`).
#[derive(Debug, Clone)]
pub struct Store {
    directory: PathBuf,
}

impl Store {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into() }
    }

    fn read(&self, name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.directory.join(name)).ok()
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.directory).map_err(local)?;
        std::fs::write(self.directory.join(name), bytes).map_err(local)
    }

    /// Obtain the legacy image/signature for `version`, from the cache or the mirrors.
    pub async fn legacy(&self, catalog: &Catalog, version: &str, cancel: &CancellationToken) -> Result<Plan> {
        let short = major_minor(version).map(|(short, _)| short).unwrap_or_else(|| version.to_owned());
        let image_name = format!("devimg-{short}.dmg");
        let signature_name = format!("{image_name}.signature");

        if let (Some(image), Some(signature)) = (self.read(&image_name), self.read(&signature_name)) {
            return Ok(Plan::Legacy { image, signature });
        }

        let (image, signature) = catalog.legacy(version, cancel).await?;

        self.write(&image_name, &image)?;
        self.write(&signature_name, &signature)?;

        Ok(Plan::Legacy { image, signature })
    }

    /// Obtain the personalized image/trust cache/build manifest, from the cache or the mirror.
    pub async fn personalized(&self, catalog: &Catalog, cancel: &CancellationToken) -> Result<Plan> {
        let names =
            ("devimg-personalized.dmg", "devimg-personalized.dmg.trustcache", "devimg-personalized.buildmanifest");

        if let (Some(image), Some(trust_cache), Some(build_manifest)) =
            (self.read(names.0), self.read(names.1), self.read(names.2))
        {
            return Ok(Plan::Personalized { image, trust_cache, build_manifest });
        }

        let (image, trust_cache, build_manifest) = catalog.personalized(cancel).await?;

        self.write(names.0, &image)?;
        self.write(names.1, &trust_cache)?;
        self.write(names.2, &build_manifest)?;

        Ok(Plan::Personalized { image, trust_cache, build_manifest })
    }
}

/// Extract `DeveloperDiskImage.dmg` and its `.signature` from a downloaded zip (recovered
/// `extractFromZip`: the `.dmg` file and the `.dmg.signature` file, ignoring `__MACOSX`).
fn extract_zip(bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|error| local_str(error.to_string()))?;

    let mut image = None;
    let mut signature = None;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| local_str(error.to_string()))?;
        let name = entry.name().to_owned();

        if name.contains("__MACOSX") || name.rsplit('/').next().is_some_and(|base| base.starts_with('.')) {
            continue;
        }

        let target = if name.ends_with(".dmg.signature") {
            &mut signature
        } else if name.ends_with(".dmg") {
            &mut image
        } else {
            continue;
        };

        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(local)?;
        *target = Some(bytes);
    }

    match (image, signature) {
        (Some(image), Some(signature)) => Ok((image, signature)),
        _ => Err(DeviceError::Unsupported(
            "the developer disk image archive is missing the .dmg or its signature".into(),
        )),
    }
}

/// Mount `plan` on the device through `mounter`, fetching a personalized signature from `tss`
/// when the device has no manifest. Returns early if a developer image is already mounted.
pub async fn mount(
    mounter: &mut dyn ImageMounting,
    tss: &TssClient,
    plan: &Plan,
    log: &(dyn Fn(&str) + Send + Sync),
) -> Result<Outcome> {
    if let Some(mounted) = mounter.mounted().await? {
        log("A developer image is already mounted");

        return Ok(Outcome::AlreadyMounted(mounted));
    }

    match plan {
        Plan::Legacy { image, signature } => {
            log("Mounting developer image");
            mounter.mount(DEVELOPER, image, signature.clone(), None, None).await?;
        }

        Plan::Personalized { image, trust_cache, build_manifest } => {
            let signature = personalized_signature(mounter, tss, image, build_manifest, log).await?;

            log("Mounting personalized image");
            mounter.mount(PERSONALIZED, image, signature, Some(trust_cache.clone()), None).await?;
        }
    }

    Ok(Outcome::Mounted)
}

/// The personalized manifest: the one the device already holds, else a fresh TSS ticket
/// (recovered order — a failed device query closes the socket, so reconnect before TSS).
async fn personalized_signature(
    mounter: &mut dyn ImageMounting,
    tss: &TssClient,
    image: &[u8],
    build_manifest: &[u8],
    log: &(dyn Fn(&str) + Send + Sync),
) -> Result<Vec<u8>> {
    if let Some(manifest) = mounter.device_manifest(image).await? {
        log("Using the device's personalization manifest");

        return Ok(manifest);
    }

    mounter.reconnect().await?;
    log("Requesting a personalization manifest from Apple");

    let identifiers = mounter.personalization_identifiers().await?;
    let nonce = mounter.nonce().await?;

    tss.personalization_manifest(&identifiers, nonce, build_manifest).await
}

fn local(error: std::io::Error) -> DeviceError {
    DeviceError::Local(error.to_string())
}

fn local_str(message: String) -> DeviceError {
    DeviceError::Local(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personalized_starts_at_ios_17() {
        assert!(!is_personalized("16.7.2"));
        assert!(is_personalized("17.0"));
        assert!(is_personalized("18.1.1"));
    }

    #[test]
    fn version_candidates_fall_back_to_major_minor() {
        assert_eq!(version_candidates("17.5.1"), vec!["17.5.1".to_string(), "17.5".to_string()]);
        assert_eq!(version_candidates("16.4"), vec!["16.4".to_string()]);
    }

    #[test]
    fn zip_extraction_finds_the_image_and_signature_and_ignores_macos_metadata() {
        let mut buffer = Vec::new();
        {
            use std::io::Write;
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
            let options = zip::write::SimpleFileOptions::default();

            zip.start_file("16.5/DeveloperDiskImage.dmg", options).expect("entry");
            zip.write_all(b"image").expect("write");
            zip.start_file("16.5/DeveloperDiskImage.dmg.signature", options).expect("entry");
            zip.write_all(b"sig").expect("write");
            zip.start_file("__MACOSX/._DeveloperDiskImage.dmg", options).expect("entry");
            zip.write_all(b"junk").expect("write");
            zip.finish().expect("finish");
        }

        let (image, signature) = extract_zip(&buffer).expect("extracted");
        assert_eq!(image, b"image");
        assert_eq!(signature, b"sig");
    }
}
