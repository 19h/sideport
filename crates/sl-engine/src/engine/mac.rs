//! Installing iOS apps on this Apple Silicon Mac (recovered `slpy.m1ConvertAndInstall`).
//!
//! The Mac is registered with its provisioning UDID and appears as a device. The signed app is
//! written as a folder, then converted: `Payload` becomes `Wrapper`, `WrappedBundle` links to
//! `Wrapper/<App>.app`, the main executable is made executable, and `sideloadly.tag` records
//! the installation token. Placement replaces an application whose tag holds the same token,
//! else uses `<display name>.app`, overwriting only an application with the same bundle ID and
//! otherwise choosing `<name>-<n>.app`.

use crate::error::{EngineError, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// The recovered tag file name, kept for compatibility with wrappers Sideloadly installed.
pub(crate) const TAG: &str = "sideloadly.tag";

/// This Mac as an install target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacTarget {
    /// Provisioning UDID (Apple Silicon only).
    pub udid: String,
    pub name: String,
    /// `hw.model`.
    pub model: String,
    pub os_version: String,
    /// Where converted apps are placed, normally `/Applications`.
    pub applications: PathBuf,
}

/// Whether this Mac is offered as an install target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum MacTargetSetting {
    /// Detect an Apple Silicon Mac when the system device layer is used.
    #[default]
    Detect,
    Disabled,
    Fixed(MacTarget),
}

/// Detect this Mac when it has a provisioning UDID (Apple Silicon).
pub(crate) fn detect() -> Option<MacTarget> {
    #[cfg(target_os = "macos")]
    {
        let udid = sl_macos::provisioning_udid().ok()?;
        let model = sl_macos::hardware_model().unwrap_or_default();
        let (os_version, _) = sl_macos::os_version().unwrap_or_default();
        let name =
            sl_macos::computer_name().unwrap_or_else(|_| gethostname::gethostname().to_string_lossy().into_owned());

        Some(MacTarget { udid, name, model, os_version, applications: PathBuf::from("/Applications") })
    }

    #[cfg(not(target_os = "macos"))]
    None
}

/// A token that stays the same for refreshes of the same bundle on this Mac.
pub(crate) fn token(bundle_id: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(format!("sideport-mac:{bundle_id}").as_bytes());

    hex::encode(&digest[..16])
}

/// Convert `<folder>/Payload/<App>.app` into the Mac wrapper layout.
pub(crate) fn wrap(folder: &Path, token: &str) -> Result<PathBuf> {
    let payload = folder.join("Payload");
    let apps = app_directories(&payload)?;

    let app = match apps.as_slice() {
        [] => return Err(invalid("No .app in bundle!")),
        [app] => app.clone(),
        _ => return Err(invalid("More than one .app in bundle!")),
    };

    let wrapper = folder.join("Wrapper");
    fs::rename(&payload, &wrapper).map_err(|error| storage(format!("Failed to rename Payload: {error}")))?;

    let name = app.file_name().ok_or_else(|| invalid("No .app in bundle!"))?;
    let relative = Path::new("Wrapper").join(name);

    symlink(&relative, &folder.join("WrappedBundle"))?;

    let wrapped = wrapper.join(name);
    let executable = executable(&wrapped)?;
    set_mode(&executable, 0o755)?;

    fs::write(folder.join(TAG), token).map_err(|error| storage(format!("Could not write token: {error}")))?;

    Ok(wrapped)
}

/// Move a wrapped folder into `applications` and return its final path.
pub(crate) fn place(
    folder: &Path,
    applications: &Path,
    token: &str,
    bundle_id: &str,
    display_name: &str,
) -> Result<PathBuf> {
    if !token.is_empty()
        && let Some(existing) = tagged(applications, token)?
    {
        remove(&existing)?;
        rename(folder, &existing)?;

        return Ok(existing);
    }

    let name = display_name.replace('/', "_");

    for index in 0..1000 {
        let candidate = if index == 0 {
            applications.join(format!("{name}.app"))
        } else {
            applications.join(format!("{name}-{index}.app"))
        };

        if !candidate.exists() {
            rename(folder, &candidate)?;

            return Ok(candidate);
        }

        if wrapped_bundle_id(&candidate).as_deref() == Some(bundle_id) {
            remove(&candidate)?;
            rename(folder, &candidate)?;

            return Ok(candidate);
        }
    }

    Err(storage(format!("no free application name for {name}")))
}

/// The application whose tag file holds `token`.
fn tagged(applications: &Path, token: &str) -> Result<Option<PathBuf>> {
    let Ok(entries) = fs::read_dir(applications) else {
        return Ok(None);
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.extension().and_then(|extension| extension.to_str()) != Some("app") {
            continue;
        }

        let Ok(contents) = fs::read_to_string(path.join(TAG)) else {
            continue;
        };

        if contents.trim() == token {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

fn wrapped_bundle_id(application: &Path) -> Option<String> {
    let apps = app_directories(&application.join("Wrapper")).ok()?;
    let info = sl_bundle::read_dictionary(&apps.first()?.join("Info.plist")).ok()?;

    info.get("CFBundleIdentifier").and_then(plist::Value::as_string).map(str::to_owned)
}

fn app_directories(directory: &Path) -> Result<Vec<PathBuf>> {
    let entries = fs::read_dir(directory).map_err(|_| invalid("Package has no payload"))?;

    let mut apps: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.extension().and_then(|extension| extension.to_str()) == Some("app"))
        .collect();

    apps.sort();

    Ok(apps)
}

fn executable(app: &Path) -> Result<PathBuf> {
    let info = sl_bundle::read_dictionary(&app.join("Info.plist")).map_err(|error| invalid(&error.to_string()))?;
    let name = info
        .get("CFBundleExecutable")
        .and_then(plist::Value::as_string)
        .ok_or_else(|| invalid("the app has no CFBundleExecutable"))?;

    if name.is_empty() || name.contains('/') {
        return Err(invalid("invalid CFBundleExecutable"));
    }

    Ok(app.join(name))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| storage(format!("Failed to chmod: {error}")))
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).map_err(|error| storage(format!("Failed to symlink: {error}")))
}

#[cfg(not(unix))]
fn symlink(_: &Path, _: &Path) -> Result<()> {
    Err(EngineError::Unsupported("Mac installation requires macOS".into()))
}

fn remove(path: &Path) -> Result<()> {
    fs::remove_dir_all(path)
        .map_err(|error| storage(format!("Could not remove existing app {}: {error}", path.display())))
}

fn rename(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(|error| storage(format!("Failed to install: {error}")))
}

fn invalid(message: &str) -> EngineError {
    EngineError::InvalidApp(message.into())
}

fn storage(message: String) -> EngineError {
    EngineError::Storage(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(root: &Path, name: &str, bundle_id: &str) -> PathBuf {
        let folder = root.join(format!("{name}-folder"));
        let app = folder.join("Payload").join(format!("{name}.app"));
        fs::create_dir_all(&app).expect("app");

        let mut info = plist::Dictionary::new();
        info.insert("CFBundleIdentifier".into(), bundle_id.into());
        info.insert("CFBundleExecutable".into(), name.into());
        plist::Value::Dictionary(info).to_file_xml(app.join("Info.plist")).expect("info");
        fs::write(app.join(name), b"binary").expect("executable");

        folder
    }

    #[test]
    fn wrapping_builds_the_recovered_layout() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let folder = folder(temporary.path(), "Game", "com.example.game");

        let wrapped = wrap(&folder, "token-1").expect("wrap");

        assert_eq!(wrapped, folder.join("Wrapper/Game.app"));
        assert_eq!(fs::read_link(folder.join("WrappedBundle")).expect("link"), Path::new("Wrapper/Game.app"));
        assert!(folder.join("WrappedBundle/Info.plist").is_file(), "the relative link resolves");
        assert_eq!(fs::read_to_string(folder.join(TAG)).expect("tag"), "token-1");
        assert!(!folder.join("Payload").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(wrapped.join("Game")).expect("executable").permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }

        let empty = temporary.path().join("empty");
        fs::create_dir_all(empty.join("Payload")).expect("payload");
        assert!(matches!(wrap(&empty, ""), Err(EngineError::InvalidApp(message)) if message == "No .app in bundle!"));
    }

    #[test]
    fn placement_reuses_tags_and_bundle_ids_and_avoids_name_collisions() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let applications = temporary.path().join("Applications");
        fs::create_dir(&applications).expect("applications");

        let first = folder(temporary.path(), "Game", "com.example.game");
        wrap(&first, "token-1").expect("wrap");
        let placed = place(&first, &applications, "token-1", "com.example.game", "Game").expect("place");
        assert_eq!(placed, applications.join("Game.app"));

        let renamed = temporary.path().join("renamed");
        fs::create_dir(&renamed).expect("renamed");
        let second = folder(&renamed, "Game", "com.example.game");
        wrap(&second, "token-1").expect("wrap");
        fs::rename(applications.join("Game.app"), applications.join("Moved.app")).expect("user rename");

        let replaced = place(&second, &applications, "token-1", "com.example.game", "Game").expect("tag match");
        assert_eq!(replaced, applications.join("Moved.app"), "the tagged app is replaced where the user moved it");

        let other = temporary.path().join("other");
        fs::create_dir(&other).expect("other");
        let unrelated = folder(&other, "Moved", "org.other.app");
        wrap(&unrelated, "").expect("wrap");
        let collided = place(&unrelated, &applications, "", "org.other.app", "Moved").expect("collision");
        assert_eq!(collided, applications.join("Moved-1.app"));

        let again = temporary.path().join("again");
        fs::create_dir(&again).expect("again");
        let same = folder(&again, "Moved", "org.other.app");
        wrap(&same, "").expect("wrap");
        let overwritten = place(&same, &applications, "", "org.other.app", "Moved").expect("same bundle");
        assert_eq!(overwritten, applications.join("Moved-1.app"), "the same bundle ID is overwritten");

        let slash = temporary.path().join("slash");
        fs::create_dir(&slash).expect("slash");
        let named = folder(&slash, "Slash", "com.example.slash");
        wrap(&named, "").expect("wrap");
        assert_eq!(
            place(&named, &applications, "", "com.example.slash", "A/B").expect("slash"),
            applications.join("A_B.app")
        );
    }
}
