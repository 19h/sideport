//! In-process local ADI provisioning using Apple's Android libraries and AnisetteKit.

use crate::{Error, Result};
use futures::StreamExt;
use libloading::{Library, Symbol};
use sl_apple::anisette::AnisetteHeaders;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;
use zip::ZipArchive;

const APK_URL: &str = "https://apps.mzstatic.com/content/android-apple-music-apk/applemusic.apk";
const LIBRARIES: [&str; 2] = ["libstoreservicescore.so", "libCoreADI.so"];
const MAX_APK_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LIBRARY_BYTES: u64 = 32 * 1024 * 1024;

type GetHeaders = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut *mut c_char,
) -> i32;
type FreeHeaders = unsafe extern "C" fn(*mut c_char);

static REQUEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub async fn headers() -> Result<AnisetteHeaders> {
    let lock = REQUEST_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().await;

    let directory = state_directory()?;
    let identity = persistent_identity(&directory)?;
    let libraries = directory.join("libraries");
    let provisioning = directory.join("provisioning");

    ensure_libraries(&directory, &libraries).await?;
    private_directory(&provisioning)?;

    tokio::task::spawn_blocking(move || call_bridge(&libraries, &provisioning, identity))
        .await
        .map_err(|error| Error::System(format!("local ADI worker: {error}")))?
}

fn state_directory() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| Error::System("HOME is unset".into()))?;
    let directory = PathBuf::from(home).join("Library/Application Support/Sideport/Anisette");

    private_directory(&directory)?;

    Ok(directory)
}

fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| Error::System(format!("create {}: {error}", path.display())))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| Error::System(format!("protect {}: {error}", path.display())))
}

fn persistent_identity(directory: &Path) -> Result<Uuid> {
    let path = directory.join("identity");

    if path.exists() {
        return read_identity(&path);
    }

    let identity = Uuid::new_v4();
    let mut staged = tempfile::NamedTempFile::new_in(directory)
        .map_err(|error| Error::System(format!("stage identity: {error}")))?;

    staged
        .write_all(identity.to_string().as_bytes())
        .map_err(|error| Error::System(format!("write identity: {error}")))?;
    staged.as_file().sync_all().map_err(|error| Error::System(format!("sync identity: {error}")))?;

    match staged.persist_noclobber(&path) {
        Ok(_) => Ok(identity),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => read_identity(&path),
        Err(error) => Err(Error::System(format!("save identity: {}", error.error))),
    }
}

fn read_identity(path: &Path) -> Result<Uuid> {
    let contents = fs::read_to_string(path).map_err(|error| Error::System(format!("read identity: {error}")))?;

    Uuid::parse_str(contents.trim()).map_err(|error| Error::System(format!("invalid identity: {error}")))
}

async fn ensure_libraries(directory: &Path, libraries: &Path) -> Result<()> {
    if libraries_valid(libraries) {
        return Ok(());
    }

    private_directory(libraries)?;

    let response = reqwest::Client::new()
        .get(APK_URL)
        .send()
        .await
        .map_err(|error| Error::System(format!("download Apple Music APK: {error}")))?
        .error_for_status()
        .map_err(|error| Error::System(format!("Apple Music APK response: {error}")))?;

    if response.content_length().is_some_and(|size| size > MAX_APK_BYTES) {
        return Err(Error::System("Apple Music APK exceeds size limit".into()));
    }

    let archive = tempfile::NamedTempFile::new_in(directory)
        .map_err(|error| Error::System(format!("create APK staging file: {error}")))?;
    let mut destination = tokio::fs::File::from_std(
        archive.reopen().map_err(|error| Error::System(format!("open APK staging file: {error}")))?,
    );
    let mut stream = response.bytes_stream();
    let mut received = 0_u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| Error::System(format!("read Apple Music APK: {error}")))?;
        received = received.saturating_add(chunk.len() as u64);

        if received > MAX_APK_BYTES {
            return Err(Error::System("Apple Music APK exceeds size limit".into()));
        }

        destination
            .write_all(&chunk)
            .await
            .map_err(|error| Error::System(format!("stage Apple Music APK: {error}")))?;
    }

    destination.flush().await.map_err(|error| Error::System(format!("flush Apple Music APK: {error}")))?;
    drop(destination);

    let archive_path = archive.path().to_path_buf();
    let library_path = libraries.to_path_buf();

    tokio::task::spawn_blocking(move || extract_libraries(&archive_path, &library_path))
        .await
        .map_err(|error| Error::System(format!("APK extraction worker: {error}")))??;

    if !libraries_valid(libraries) {
        return Err(Error::System("Apple Music APK does not contain usable ARM64 ADI libraries".into()));
    }

    Ok(())
}

fn extract_libraries(archive_path: &Path, libraries: &Path) -> Result<()> {
    let file = File::open(archive_path).map_err(|error| Error::System(format!("open APK archive: {error}")))?;
    let mut archive = ZipArchive::new(file).map_err(|error| Error::System(format!("parse APK archive: {error}")))?;

    for name in LIBRARIES {
        let entry_path = format!("lib/arm64-v8a/{name}");
        let mut entry = archive.by_name(&entry_path).map_err(|error| Error::System(format!("find {name}: {error}")))?;

        if entry.size() > MAX_LIBRARY_BYTES {
            return Err(Error::System(format!("{name} exceeds size limit")));
        }

        let mut staged = tempfile::NamedTempFile::new_in(libraries)
            .map_err(|error| Error::System(format!("stage {name}: {error}")))?;
        std::io::copy(&mut entry, &mut staged).map_err(|error| Error::System(format!("extract {name}: {error}")))?;
        staged.flush().map_err(|error| Error::System(format!("flush {name}: {error}")))?;

        if !is_arm64_elf(staged.path()) {
            return Err(Error::System(format!("{name} is not an ARM64 ELF library")));
        }

        staged.persist(libraries.join(name)).map_err(|error| Error::System(format!("save {name}: {}", error.error)))?;
    }

    Ok(())
}

fn libraries_valid(directory: &Path) -> bool {
    LIBRARIES.iter().all(|name| is_arm64_elf(&directory.join(name)))
}

fn is_arm64_elf(path: &Path) -> bool {
    let mut header = [0_u8; 20];
    let Ok(mut file) = File::open(path) else { return false };

    file.read_exact(&mut header).is_ok()
        && &header[..4] == b"\x7fELF"
        && header[4] == 2
        && header[5] == 1
        && u16::from_le_bytes([header[18], header[19]]) == 183
}

fn call_bridge(libraries: &Path, provisioning: &Path, identity: Uuid) -> Result<AnisetteHeaders> {
    let library_path = env!("SIDEPORT_ANISETTE_BRIDGE");
    let model = crate::hardware_model()?;
    let (version, build) = crate::os_version()?;
    let client_info = format!("<{model}> <macOS;{version};{build}> <com.apple.AuthKit/1 (com.apple.akd/1.0)>");
    let user_agent = format!("AuthKit/1 (Macintosh; OS X {version}) (com.apple.akd/1.0)");

    let libraries = CString::new(libraries.to_string_lossy().as_bytes())
        .map_err(|error| Error::System(format!("ADI library path: {error}")))?;
    let provisioning = CString::new(provisioning.to_string_lossy().as_bytes())
        .map_err(|error| Error::System(format!("ADI provisioning path: {error}")))?;
    let identity =
        CString::new(identity.to_string()).map_err(|error| Error::System(format!("ADI identity: {error}")))?;
    let client_info = CString::new(client_info).map_err(|error| Error::System(format!("ADI client info: {error}")))?;
    let user_agent = CString::new(user_agent).map_err(|error| Error::System(format!("ADI user agent: {error}")))?;

    // SAFETY: SwiftPM built this dylib with the declared C ABI, and it remains loaded until the call and deallocation finish.
    let bridge = unsafe { Library::new(library_path) }
        .map_err(|error| Error::System(format!("load local anisette bridge: {error}")))?;

    // SAFETY: The exported symbols have the C signatures declared in Bridge.swift.
    let get_headers: Symbol<GetHeaders> = unsafe { bridge.get(b"sideport_anisette_get_headers") }
        .map_err(|error| Error::System(format!("find local anisette entrypoint: {error}")))?;
    // SAFETY: The exported symbol deallocates the string returned by the same bridge.
    let free_headers: Symbol<FreeHeaders> = unsafe { bridge.get(b"sideport_anisette_free") }
        .map_err(|error| Error::System(format!("find local anisette deallocator: {error}")))?;

    let mut output = std::ptr::null_mut();

    // SAFETY: All input strings are valid NUL-terminated C strings, and output points to writable pointer storage.
    let status = unsafe {
        get_headers(
            libraries.as_ptr(),
            provisioning.as_ptr(),
            identity.as_ptr(),
            client_info.as_ptr(),
            user_agent.as_ptr(),
            &mut output,
        )
    };

    if output.is_null() {
        return Err(Error::System(format!("local anisette bridge returned {status} without a response")));
    }

    // SAFETY: The bridge returns a NUL-terminated allocation that remains valid until free_headers is called.
    let message = unsafe { CStr::from_ptr(output).to_string_lossy().into_owned() };
    // SAFETY: output was allocated by the same bridge and is freed exactly once.
    unsafe { free_headers(output) };

    if status != 0 {
        return Err(Error::System(format!("local ADI provisioning failed: {message}")));
    }

    let mut values: BTreeMap<String, String> =
        serde_json::from_str(&message).map_err(|error| Error::System(format!("decode local anisette: {error}")))?;

    values.retain(|name, _| {
        let folded = name.to_ascii_lowercase();
        folded.starts_with("x-apple-") || folded.starts_with("x-mme-")
    });

    AnisetteHeaders::new(values).map_err(|error| Error::System(format!("validate local anisette: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_persistent_and_private() {
        let directory = tempfile::tempdir().expect("state directory");
        let first = persistent_identity(directory.path()).expect("new identity");
        let second = persistent_identity(directory.path()).expect("saved identity");
        let mode = fs::metadata(directory.path().join("identity")).expect("identity file").permissions().mode();

        assert_eq!(first, second);
        assert_eq!(mode & 0o777, 0o600);
    }
}
