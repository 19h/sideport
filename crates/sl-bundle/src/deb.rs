//! Debian package injection inputs, reconstructed from the Go `slpy.(*injector).unpack`
//! (`decompiled/go/sideloadly_slpy.c`) and `notes/BUNDLE_NOTES.md` §7.4.
//!
//! A `.deb` is an `ar` archive; its `data.tar.{gz,xz,lzma,bz2,zst}` (or an uncompressed
//! `data.tar`) member holds the payload. The recovered injector extracts exactly what an app can
//! load and returns those items as injection sources:
//!
//!   * `Library/MobileSubstrate/DynamicLibraries/*.dylib` (and the rootless
//!     `var/jb/Library/MobileSubstrate/DynamicLibraries/*` and `usr/lib/*` equivalents),
//!   * `Library/Frameworks/*.framework` (CydiaSubstrate, with iGameGod special-cased),
//!   * `Library/Application Support/*.bundle`,
//!
//! while skipping `Applications/SubstituteSettings.app`. Path escapes (`..`, absolute paths,
//! symlink ancestors) are rejected.
//!
//! Deliberate deviations from the recovered Go, documented in docs/BUNDLE.md:
//!   * The recovered code stores a bundle's inner symlinks as `<name>%symlink` placeholder files
//!     plus a `.filenames_mangled` flag, because its Python zip layer converts them back on
//!     repack. Sideport does not mangle its staging tree, so bundle-internal symlinks are written
//!     as real (escape-checked) symlinks, which the existing injection copy and deterministic
//!     packer already preserve.
//!   * The recovered code reformats `CydiaSubstrate.framework/Info.plist` ("beautify"); Sideport
//!     copies it unchanged, which does not affect loading or signing.

use crate::error::io;
use crate::files;
use crate::{Control, Error, Result};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

/// Bounds on the two decode stages, independent of [`crate::ArchiveLimits`] (which governs ZIPs).
const MAX_MEMBER_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TAR_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
const COPY_CHUNK: usize = 128 * 1024;

const DYNAMIC_LIBRARIES: &str = "Library/MobileSubstrate/DynamicLibraries/";
const ROOTLESS_DYNAMIC_LIBRARIES: &str = "var/jb/Library/MobileSubstrate/DynamicLibraries/";
const IGAMEGOD_FRAMEWORK: &str = "Library/Frameworks/iGameGod.framework";
const SUBSTITUTE_SETTINGS: &str = "Applications/SubstituteSettings.app";

/// An extracted `.deb` payload plus the injection sources selected from it. Dropping it removes
/// the temporary tree, so callers copy the injections before it goes out of scope.
#[derive(Debug)]
pub struct DebPackage {
    _temporary: tempfile::TempDir,
    injections: Vec<PathBuf>,
}

impl DebPackage {
    /// Absolute paths of the selected dylibs, frameworks and bundles, ready to inject.
    pub fn injections(&self) -> &[PathBuf] {
        &self.injections
    }
}

/// Parse a `.deb`, extract its `data.tar.*` payload and select the injection sources.
pub fn extract_deb(path: &Path, control: Control<'_>) -> Result<DebPackage> {
    control.check()?;

    let tar_bytes = read_data_tar(path, control)?;

    let temporary = io(path, tempfile::Builder::new().prefix("sideport-deb-").tempdir())?;
    let mut selection = Selection::default();

    let mut archive = tar::Archive::new(Cursor::new(tar_bytes));
    let entries = archive.entries().map_err(|error| Error::Archive(format!("{}: {error}", path.display())))?;

    for entry in entries {
        control.check()?;

        let mut entry = entry.map_err(|error| Error::Archive(format!("{}: {error}", path.display())))?;
        let raw = entry.path().map_err(|error| Error::Archive(error.to_string()))?.into_owned();

        let Some(relative) = clean_relative(&raw) else {
            continue;
        };

        selection.consider(&mut entry, &relative, temporary.path(), control)?;
    }

    let injections = selection.finish()?;

    Ok(DebPackage { _temporary: temporary, injections })
}

/// Locate the `ar` member named `data.tar[.*]` and return its decompressed bytes.
fn read_data_tar(path: &Path, control: Control<'_>) -> Result<Vec<u8>> {
    let file = io(path, fs::File::open(path))?;
    let mut archive = ar::Archive::new(file);

    while let Some(entry) = archive.next_entry() {
        control.check()?;

        let entry = entry.map_err(|error| Error::Archive(format!("{}: {error}", path.display())))?;
        let identifier = String::from_utf8_lossy(entry.header().identifier()).into_owned();
        let name = identifier.trim_end_matches('/');

        let Some(codec) = Codec::for_member(name) else {
            continue;
        };

        let size = entry.header().size();

        if size > MAX_MEMBER_BYTES {
            return Err(Error::Limit("deb data member bytes"));
        }

        let mut compressed = Vec::with_capacity(size as usize);
        entry.take(MAX_MEMBER_BYTES + 1).read_to_end(&mut compressed).map_err(|error| from_io(path, error))?;

        if compressed.len() as u64 > MAX_MEMBER_BYTES {
            return Err(Error::Limit("deb data member bytes"));
        }

        return codec.decompress(&compressed);
    }

    Err(Error::Archive(format!("{}: no data.tar member found in deb", path.display())))
}

/// Which decoder a `data.tar[.ext]` member needs.
#[derive(Debug, Clone, Copy)]
enum Codec {
    Plain,
    Gzip,
    Xz,
    Lzma,
    Bzip2,
    Zstd,
}

impl Codec {
    fn for_member(name: &str) -> Option<Self> {
        match name {
            "data.tar" => Some(Self::Plain),
            "data.tar.gz" | "data.tgz" => Some(Self::Gzip),
            "data.tar.xz" => Some(Self::Xz),
            "data.tar.lzma" => Some(Self::Lzma),
            "data.tar.bz2" => Some(Self::Bzip2),
            "data.tar.zst" => Some(Self::Zstd),
            _ => None,
        }
    }

    fn decompress(self, compressed: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let bounded = || Error::Limit("deb decompressed bytes");

        match self {
            Self::Plain => {
                if compressed.len() as u64 > MAX_TAR_BYTES {
                    return Err(bounded());
                }

                output.extend_from_slice(compressed);
            }
            Self::Gzip => bounded_read(flate2::read::GzDecoder::new(compressed), &mut output)?,
            Self::Bzip2 => bounded_read(bzip2::read::BzDecoder::new(compressed), &mut output)?,
            Self::Zstd => {
                let decoder =
                    ruzstd::StreamingDecoder::new(compressed).map_err(|error| Error::Archive(error.to_string()))?;

                bounded_read(decoder, &mut output)?;
            }
            Self::Xz => {
                let mut reader = Cursor::new(compressed);
                lzma_rs::xz_decompress(&mut reader, &mut output).map_err(|error| Error::Archive(error.to_string()))?;
            }
            Self::Lzma => {
                let mut reader = Cursor::new(compressed);
                lzma_rs::lzma_decompress(&mut reader, &mut output)
                    .map_err(|error| Error::Archive(error.to_string()))?;
            }
        }

        if output.len() as u64 > MAX_TAR_BYTES {
            return Err(bounded());
        }

        Ok(output)
    }
}

/// Read a streaming decoder into `output`, one bounded chunk at a time so a decompression bomb
/// cannot exhaust memory before the size check.
fn bounded_read(mut reader: impl Read, output: &mut Vec<u8>) -> Result<()> {
    reader
        .by_ref()
        .take(MAX_TAR_BYTES + 1)
        .read_to_end(output)
        .map_err(|error| Error::Archive(format!("deb decompression: {error}")))?;

    if output.len() as u64 > MAX_TAR_BYTES {
        return Err(Error::Limit("deb decompressed bytes"));
    }

    Ok(())
}

/// Accumulated injection sources and the flags that pick the final subset.
#[derive(Debug, Default)]
struct Selection {
    dynamic_libraries: Vec<PathBuf>,
    frameworks: Vec<PathBuf>,
    bundles: Vec<PathBuf>,
    saw_dynamic_libraries_dir: bool,
    saw_igamegod: bool,
    saw_substitute_settings: bool,
}

impl Selection {
    /// Classify one already-cleaned tar entry and extract it if the recovered rules select it.
    fn consider(
        &mut self,
        entry: &mut tar::Entry<'_, Cursor<Vec<u8>>>,
        relative: &str,
        root: &Path,
        control: Control<'_>,
    ) -> Result<()> {
        if relative == SUBSTITUTE_SETTINGS || relative.starts_with(&format!("{SUBSTITUTE_SETTINGS}/")) {
            self.saw_substitute_settings = true;

            return Ok(());
        }

        if relative.starts_with("Applications/") {
            return Ok(());
        }

        let inside_bundle = has_bundle_ancestor(relative);

        match entry.header().entry_type() {
            tar::EntryType::Directory => self.consider_directory(relative, root, inside_bundle),
            tar::EntryType::Symlink => self.consider_symlink(entry, relative, root, inside_bundle),
            tar::EntryType::Regular | tar::EntryType::GNUSparse => {
                self.consider_file(entry, relative, root, inside_bundle, control)
            }
            _ => Ok(()),
        }
    }

    fn consider_directory(&mut self, relative: &str, root: &Path, inside_bundle: bool) -> Result<()> {
        let trimmed = relative.trim_end_matches('/');

        if trimmed == "Library/MobileSubstrate/DynamicLibraries"
            || trimmed == "var/jb/Library/MobileSubstrate/DynamicLibraries"
        {
            self.saw_dynamic_libraries_dir = true;
        }

        let is_bundle_root = !inside_bundle && (trimmed.ends_with(".framework") || trimmed.ends_with(".bundle"));

        if !is_bundle_root && !inside_bundle {
            return Ok(());
        }

        let destination = files::write_inside(root, Path::new(trimmed))?;

        if !io(&destination, destination.try_exists())? {
            io(&destination, fs::create_dir(&destination))?;
        }

        if is_bundle_root {
            if trimmed == IGAMEGOD_FRAMEWORK {
                self.saw_igamegod = true;
            }

            if trimmed.ends_with(".framework") {
                self.frameworks.push(destination);
            } else {
                self.bundles.push(destination);
            }
        }

        Ok(())
    }

    fn consider_file(
        &mut self,
        entry: &mut tar::Entry<'_, Cursor<Vec<u8>>>,
        relative: &str,
        root: &Path,
        inside_bundle: bool,
        control: Control<'_>,
    ) -> Result<()> {
        let extension = lower_extension(relative);
        let is_loadable = extension.as_deref() == Some("dylib") || extension.as_deref() == Some("plist");
        let in_library_dir = relative.starts_with("usr/lib/")
            || relative.starts_with(DYNAMIC_LIBRARIES)
            || relative.starts_with(ROOTLESS_DYNAMIC_LIBRARIES);

        if !inside_bundle && !(is_loadable && in_library_dir) {
            return Ok(());
        }

        let destination = files::write_inside(root, Path::new(relative))?;
        write_entry(entry, &destination, control)?;

        let is_dynamic_library = extension.as_deref() == Some("dylib")
            && (relative.starts_with(DYNAMIC_LIBRARIES) || relative.starts_with(ROOTLESS_DYNAMIC_LIBRARIES));

        if !inside_bundle && is_dynamic_library {
            self.dynamic_libraries.push(destination);
        }

        Ok(())
    }

    fn consider_symlink(
        &mut self,
        entry: &tar::Entry<'_, Cursor<Vec<u8>>>,
        relative: &str,
        root: &Path,
        inside_bundle: bool,
    ) -> Result<()> {
        let target = entry
            .link_name()
            .map_err(|error| Error::Archive(error.to_string()))?
            .ok_or_else(|| Error::Archive(format!("{relative}: symlink has no target")))?;
        let target = target.to_str().ok_or_else(|| Error::Path(relative.into()))?.to_owned();
        let relative_path = files::relative_path(relative)?;

        files::validate_symlink(&relative_path, &target)?;

        let parent = root.join(&relative_path).parent().map(Path::to_owned);

        if !parent.as_deref().is_some_and(|parent| parent.is_dir()) {
            return Ok(());
        }

        if inside_bundle {
            let destination = files::write_inside(root, &relative_path)?;
            files::remove(&destination)?;
            files::create_symlink(Path::new(&target), &destination)?;

            return Ok(());
        }

        let is_top_level_dylib = lower_extension(relative).as_deref() == Some("dylib") && !target.contains('/');
        let in_dynamic_libraries =
            relative.starts_with(DYNAMIC_LIBRARIES) || relative.starts_with(ROOTLESS_DYNAMIC_LIBRARIES);

        if is_top_level_dylib && in_dynamic_libraries {
            let source = root.join(relative_path.parent().unwrap_or(Path::new(""))).join(&target);

            if io(&source, source.try_exists())? {
                let destination = files::write_inside(root, &relative_path)?;
                io(&destination, fs::copy(&source, &destination))?;
                self.dynamic_libraries.push(destination);
            }
        }

        Ok(())
    }

    /// Apply the recovered final filter and reject a package that yielded nothing loadable.
    fn finish(mut self) -> Result<Vec<PathBuf>> {
        if self.saw_substitute_settings {
            self.bundles.sort_unstable();

            return non_empty(self.bundles, "deb has a SubstituteSettings.app but no injectable bundle");
        }

        if self.saw_igamegod {
            let framework = self.frameworks.into_iter().filter(|path| path.ends_with(IGAMEGOD_FRAMEWORK)).collect();

            return non_empty(framework, "deb declared iGameGod but its framework was not found");
        }

        let mut injections = self.dynamic_libraries;
        injections.extend(self.frameworks);
        injections.extend(self.bundles);
        injections.sort_unstable();
        injections.dedup();

        if injections.is_empty() && !self.saw_dynamic_libraries_dir {
            return Err(Error::Archive("deb looks malformed: no DynamicLibraries directory found".into()));
        }

        non_empty(injections, "nothing to inject from deb")
    }
}

/// Copy a regular tar entry into `destination`, bounding size and honoring cancellation.
fn write_entry(entry: &mut tar::Entry<'_, Cursor<Vec<u8>>>, destination: &Path, control: Control<'_>) -> Result<()> {
    let declared = entry.header().size().map_err(|error| from_io(destination, error))?;

    if declared > MAX_ENTRY_BYTES {
        return Err(Error::Limit("deb entry bytes"));
    }

    files::remove(destination)?;

    let mut output = io(destination, fs::File::create(destination))?;
    let mut buffer = vec![0; COPY_CHUNK];
    let mut written = 0u64;

    loop {
        control.check()?;

        let read = entry.read(&mut buffer).map_err(|error| from_io(destination, error))?;

        if read == 0 {
            break;
        }

        written += read as u64;

        if written > MAX_ENTRY_BYTES {
            return Err(Error::Limit("deb entry bytes"));
        }

        std::io::Write::write_all(&mut output, &buffer[..read]).map_err(|error| from_io(destination, error))?;
    }

    apply_mode(entry, &output)?;

    Ok(())
}

#[cfg(unix)]
fn apply_mode(entry: &tar::Entry<'_, Cursor<Vec<u8>>>, output: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(mode) = entry.header().mode() {
        let permissions = fs::Permissions::from_mode(mode & 0o777);
        output.set_permissions(permissions).map_err(|error| Error::Io { path: PathBuf::new(), source: error })?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn apply_mode(_entry: &tar::Entry<'_, Cursor<Vec<u8>>>, _output: &fs::File) -> Result<()> {
    Ok(())
}

/// True when any ancestor directory of `relative` is a `.framework` or `.bundle` (so the entry is
/// part of a bundle taken wholesale).
fn has_bundle_ancestor(relative: &str) -> bool {
    let components: Vec<&str> = relative.trim_end_matches('/').split('/').collect();
    let ancestors = &components[..components.len().saturating_sub(1)];

    ancestors.iter().any(|component| component.ends_with(".framework") || component.ends_with(".bundle"))
}

/// Reduce a raw tar path to a safe relative POSIX path, or reject it. Leading `./` is stripped and
/// any `..`, absolute or drive-letter component is refused.
fn clean_relative(path: &Path) -> Option<String> {
    let text = path.to_str()?.replace('\\', "/");
    let trimmed = text.trim_start_matches("./").trim_end_matches('/');

    if trimmed.is_empty() || trimmed.starts_with('/') {
        return None;
    }

    let mut components = Vec::new();

    for component in trimmed.split('/') {
        match component {
            "" | "." => continue,
            ".." => return None,
            component if component.as_bytes().get(1) == Some(&b':') => return None,
            component => components.push(component),
        }
    }

    (!components.is_empty()).then(|| components.join("/"))
}

fn lower_extension(relative: &str) -> Option<String> {
    Path::new(relative).extension().and_then(|extension| extension.to_str()).map(str::to_ascii_lowercase)
}

fn non_empty(injections: Vec<PathBuf>, message: &'static str) -> Result<Vec<PathBuf>> {
    if injections.is_empty() {
        return Err(Error::Archive(message.into()));
    }

    Ok(injections)
}

fn from_io(path: &Path, source: std::io::Error) -> Error {
    crate::error::from_io(path, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tar::{EntryType, Header};

    /// One tar member for a fixture package.
    enum Item {
        File(&'static str, &'static [u8]),
        Dir(&'static str),
        Symlink(&'static str, &'static str),
    }

    fn build_tar(items: &[Item]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());

        for item in items {
            let mut header = Header::new_gnu();

            match item {
                Item::File(path, data) => {
                    header.set_entry_type(EntryType::Regular);
                    header.set_mode(0o644);
                    header.set_size(data.len() as u64);
                    builder.append_data(&mut header, path, *data).expect("file");
                }
                Item::Dir(path) => {
                    header.set_entry_type(EntryType::Directory);
                    header.set_mode(0o755);
                    header.set_size(0);
                    builder.append_data(&mut header, path, std::io::empty()).expect("dir");
                }
                Item::Symlink(path, target) => {
                    header.set_entry_type(EntryType::Symlink);
                    header.set_mode(0o777);
                    header.set_size(0);
                    builder.append_link(&mut header, path, target).expect("symlink");
                }
            }
        }

        builder.into_inner().expect("tar")
    }

    fn compress(kind: &str, tar: &[u8]) -> Vec<u8> {
        match kind {
            "data.tar" => tar.to_vec(),
            "data.tar.gz" => {
                let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                encoder.write_all(tar).expect("gz");
                encoder.finish().expect("gz")
            }
            "data.tar.bz2" => {
                let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
                encoder.write_all(tar).expect("bz2");
                encoder.finish().expect("bz2")
            }
            "data.tar.xz" => {
                let mut output = Vec::new();
                lzma_rs::xz_compress(&mut Cursor::new(tar), &mut output).expect("xz");
                output
            }
            "data.tar.lzma" => {
                let mut output = Vec::new();
                lzma_rs::lzma_compress(&mut Cursor::new(tar), &mut output).expect("lzma");
                output
            }
            "data.tar.zst" => zstd::encode_all(tar, 3).expect("zst"),
            other => panic!("unknown codec {other}"),
        }
    }

    fn build_deb(member: &str, items: &[Item]) -> tempfile::NamedTempFile {
        let payload = compress(member, &build_tar(items));

        let mut archive = ar::Builder::new(Vec::new());
        archive.append(&ar::Header::new(b"debian-binary".to_vec(), 4), &b"2.0\n"[..]).expect("binary");
        archive.append(&ar::Header::new(member.as_bytes().to_vec(), payload.len() as u64), &payload[..]).expect("data");
        let bytes = archive.into_inner().expect("ar");

        let mut file = tempfile::Builder::new().suffix(".deb").tempfile().expect("temp deb");
        file.write_all(&bytes).expect("write deb");
        file.flush().expect("flush");

        file
    }

    fn names(injections: &[PathBuf]) -> Vec<String> {
        injections.iter().map(|path| path.file_name().expect("value").to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn selects_dylib_framework_and_bundle_but_not_helpers_or_plists() {
        let deb = build_deb(
            "data.tar.gz",
            &[
                Item::Dir("Library/MobileSubstrate/DynamicLibraries"),
                Item::File("Library/MobileSubstrate/DynamicLibraries/MyTweak.dylib", b"\xca\xfe\xba\xbe dylib"),
                Item::File("Library/MobileSubstrate/DynamicLibraries/MyTweak.plist", b"filter"),
                Item::Dir("Library/Frameworks/CydiaSubstrate.framework"),
                Item::File("Library/Frameworks/CydiaSubstrate.framework/CydiaSubstrate", b"binary"),
                Item::File("Library/Frameworks/CydiaSubstrate.framework/Info.plist", b"<plist/>"),
                Item::Dir("Library/Application Support/Foo.bundle"),
                Item::File("Library/Application Support/Foo.bundle/data.bin", b"resource"),
                Item::File("usr/lib/libhelper.dylib", b"helper"),
            ],
        );

        let package = extract_deb(deb.path(), Control::default()).expect("extract");
        let selected = names(package.injections());

        // Sorted by full staging path: Application Support < Frameworks < MobileSubstrate.
        assert_eq!(selected, vec!["Foo.bundle", "CydiaSubstrate.framework", "MyTweak.dylib"]);

        let framework =
            package.injections().iter().find(|path| path.ends_with("CydiaSubstrate.framework")).expect("value");
        assert!(framework.join("CydiaSubstrate").is_file());
        assert!(framework.join("Info.plist").is_file());

        let dylib = package.injections().iter().find(|path| path.ends_with("MyTweak.dylib")).expect("value");
        assert_eq!(fs::read(dylib).expect("value"), b"\xca\xfe\xba\xbe dylib");
    }

    #[test]
    fn every_supported_codec_yields_the_same_dylib() {
        for member in ["data.tar", "data.tar.gz", "data.tar.xz", "data.tar.lzma", "data.tar.bz2", "data.tar.zst"] {
            let deb = build_deb(
                member,
                &[
                    Item::Dir("Library/MobileSubstrate/DynamicLibraries"),
                    Item::File("Library/MobileSubstrate/DynamicLibraries/Tweak.dylib", b"payload"),
                ],
            );

            let package = extract_deb(deb.path(), Control::default()).expect(member);

            assert_eq!(names(package.injections()), vec!["Tweak.dylib"], "codec {member}");
        }
    }

    #[test]
    fn substitute_settings_app_is_skipped_and_only_bundles_are_returned() {
        let deb = build_deb(
            "data.tar.gz",
            &[
                Item::Dir("Applications/SubstituteSettings.app"),
                Item::File("Applications/SubstituteSettings.app/SubstituteSettings", b"gui"),
                Item::Dir("Library/Application Support/Substitute.bundle"),
                Item::File("Library/Application Support/Substitute.bundle/settings.plist", b"cfg"),
                Item::File("Library/MobileSubstrate/DynamicLibraries/Substitute.dylib", b"lib"),
            ],
        );

        let package = extract_deb(deb.path(), Control::default()).expect("extract");

        assert_eq!(names(package.injections()), vec!["Substitute.bundle"]);
        assert!(!package.injections().iter().any(|path| path.to_string_lossy().contains("SubstituteSettings")));
    }

    #[test]
    fn igamegod_framework_is_selected_alone() {
        let deb = build_deb(
            "data.tar.gz",
            &[
                Item::Dir("Library/Frameworks/iGameGod.framework"),
                Item::File("Library/Frameworks/iGameGod.framework/iGameGod", b"bin"),
                Item::Dir("Library/MobileSubstrate/DynamicLibraries"),
                Item::File("Library/MobileSubstrate/DynamicLibraries/iGameGod.dylib", b"loader"),
            ],
        );

        let package = extract_deb(deb.path(), Control::default()).expect("extract");

        assert_eq!(names(package.injections()), vec!["iGameGod.framework"]);
    }

    #[cfg(unix)]
    #[test]
    fn bundle_internal_symlinks_become_real_symlinks() {
        let deb = build_deb(
            "data.tar.gz",
            &[
                Item::Dir("Library/Frameworks/CydiaSubstrate.framework"),
                Item::File("Library/Frameworks/CydiaSubstrate.framework/CydiaSubstrate", b"binary"),
                Item::Symlink("Library/Frameworks/CydiaSubstrate.framework/Current", "CydiaSubstrate"),
            ],
        );

        let package = extract_deb(deb.path(), Control::default()).expect("extract");
        let framework = &package.injections()[0];
        let link = framework.join("Current");

        assert!(fs::symlink_metadata(&link).expect("value").file_type().is_symlink());
        assert_eq!(fs::read_link(&link).expect("value"), Path::new("CydiaSubstrate"));
    }

    #[test]
    fn a_deb_without_a_dynamic_libraries_directory_is_rejected() {
        let deb = build_deb("data.tar.gz", &[Item::File("usr/lib/libhelper.dylib", b"helper")]);

        let error = extract_deb(deb.path(), Control::default()).expect_err("expected error");

        assert!(matches!(error, Error::Archive(message) if message.contains("DynamicLibraries")));
    }

    #[test]
    fn a_deb_without_a_data_member_is_rejected() {
        let mut archive = ar::Builder::new(Vec::new());
        archive.append(&ar::Header::new(b"debian-binary".to_vec(), 4), &b"2.0\n"[..]).expect("binary");
        let bytes = archive.into_inner().expect("ar");

        let mut file = tempfile::Builder::new().suffix(".deb").tempfile().expect("temp");
        file.write_all(&bytes).expect("write");
        file.flush().expect("flush");

        let error = extract_deb(file.path(), Control::default()).expect_err("expected error");

        assert!(matches!(error, Error::Archive(message) if message.contains("no data.tar member")));
    }

    #[test]
    fn clean_relative_rejects_escapes_and_normalizes_dot_prefix() {
        assert_eq!(clean_relative(Path::new("./Library/x.dylib")).as_deref(), Some("Library/x.dylib"));
        assert_eq!(clean_relative(Path::new("Library/./x.dylib")).as_deref(), Some("Library/x.dylib"));
        assert_eq!(clean_relative(Path::new("a/../../etc/passwd")), None);
        assert_eq!(clean_relative(Path::new("/etc/passwd")), None);
        assert_eq!(clean_relative(Path::new("C:\\Windows")), None);
    }

    #[test]
    fn has_bundle_ancestor_only_counts_ancestors() {
        assert!(has_bundle_ancestor("Library/Frameworks/X.framework/Info.plist"));
        assert!(has_bundle_ancestor("Library/Application Support/Y.bundle/data"));
        assert!(!has_bundle_ancestor("Library/Frameworks/X.framework"));
        assert!(!has_bundle_ancestor("Library/MobileSubstrate/DynamicLibraries/T.dylib"));
    }
}
