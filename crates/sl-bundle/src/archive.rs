use crate::error::io;
use crate::files::{self, BUFFER_SIZE};
use crate::reader::ArchiveReader;
use crate::{Control, Error, Phase, Result};
use rayon::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use unicode_normalization::UnicodeNormalization;
use zip::ZipArchive;

#[derive(Debug, Clone, Copy)]
pub struct ArchiveLimits {
    pub max_entries: u64,
    pub max_entry_bytes: u64,
    pub max_total_bytes: u64,
    pub max_directory_bytes: u64,
    pub max_path_bytes: usize,
    pub max_path_depth: usize,
    pub max_index_path_bytes: usize,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_entry_bytes: 16 * 1024 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024 * 1024,
            max_directory_bytes: 64 * 1024 * 1024,
            max_path_bytes: 4096,
            max_path_depth: 128,
            max_index_path_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Ipa,
    AppZip,
    AppDirectory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Directory,
    File,
    Symlink,
}

#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) relative: PathBuf,
    pub(crate) kind: EntryKind,
    pub(crate) size: u64,
    mode: u32,
}

/// Owns a private staging tree. Dropping it removes the extracted data, including
/// after failed/cancelled preparation. Original inputs are never modified.
#[derive(Debug)]
pub struct BundleArchive {
    temporary: tempfile::TempDir,
    bundle_relative: PathBuf,
    kind: ArchiveKind,
    directory_modes: BTreeMap<PathBuf, u32>,
}

impl BundleArchive {
    pub fn unpack(input: &Path, limits: ArchiveLimits, control: Control<'_>) -> Result<Self> {
        control.check()?;

        let temporary = io(input, tempfile::Builder::new().prefix("sideport-bundle-").tempdir())?;

        if io(input, fs::metadata(input))?.is_dir() {
            let name = input
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::Path(input.display().to_string()))?;
            let relative = files::relative_path(name)?;

            if !name.ends_with(".app") {
                return Err(Error::Bundle("directory input must be an .app".into()));
            }

            validate_directory(input, limits, control)?;
            files::copy_tree(input, &temporary.path().join(&relative), control)?;

            let mut archive = Self {
                temporary,
                bundle_relative: relative,
                kind: ArchiveKind::AppDirectory,
                directory_modes: BTreeMap::new(),
            };
            archive.record_directory_modes(input, &archive.bundle_relative.clone())?;
            archive.validate_info()?;

            return Ok(archive);
        }

        let (zip, entries, total) = open_zip(input, limits, control)?;
        let (bundle_relative, kind) = find_app(&entries)?;
        extract_entries(&zip, &entries, temporary.path(), total, control)?;

        let directory_modes = entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .map(|entry| (entry.relative.clone(), entry.mode))
            .collect();
        let archive = Self { temporary, bundle_relative, kind, directory_modes };
        archive.validate_info()?;

        Ok(archive)
    }

    pub fn root(&self) -> &Path {
        self.temporary.path()
    }

    pub fn bundle_path(&self) -> PathBuf {
        self.root().join(&self.bundle_relative)
    }

    pub fn bundle_relative(&self) -> &Path {
        &self.bundle_relative
    }

    pub fn kind(&self) -> ArchiveKind {
        self.kind
    }

    pub(crate) fn directory_mode(&self, relative: &Path) -> Option<u32> {
        self.directory_modes.get(relative).copied()
    }

    pub(crate) fn forget_directory_modes(&mut self, relative: &Path) {
        self.directory_modes.retain(|path, _| !path.starts_with(relative));
    }

    pub(crate) fn record_directory_modes(&mut self, source: &Path, relative: &Path) -> Result<()> {
        for entry in walkdir::WalkDir::new(source).follow_links(false) {
            let entry = entry.map_err(|error| Error::Archive(error.to_string()))?;

            if entry.file_type().is_dir() {
                let suffix = entry.path().strip_prefix(source).map_err(|error| Error::Path(error.to_string()))?;
                let metadata = io(entry.path(), fs::symlink_metadata(entry.path()))?;

                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;

                    metadata.permissions().mode()
                };

                #[cfg(not(unix))]
                let mode = {
                    let _ = metadata;
                    0o40755
                };

                self.directory_modes.insert(relative.join(suffix), mode);
            }
        }

        Ok(())
    }

    fn validate_info(&self) -> Result<()> {
        let path = files::read_inside(self.root(), &self.bundle_relative.join("Info.plist"))?;
        let info = crate::read_dictionary(&path)?;

        if !info.get("CFBundleIdentifier").and_then(plist::Value::as_string).is_some_and(|id| !id.is_empty()) {
            return Err(Error::Bundle("Info.plist has no nonempty CFBundleIdentifier".into()));
        }

        Ok(())
    }
}

pub(crate) fn open_zip<'a>(
    input: &Path,
    limits: ArchiveLimits,
    control: Control<'a>,
) -> Result<(ZipArchive<ArchiveReader<'a>>, Vec<Entry>, u64)> {
    let mut reader = ArchiveReader::open(input, control)?;
    let directory = reader.directory_info()?;

    if directory.entries > limits.max_entries {
        return Err(Error::Limit("entry count"));
    }

    if directory.size > limits.max_directory_bytes {
        return Err(Error::Limit("central directory bytes"));
    }

    reader.validate_directory(&directory, control)?;

    let mut zip = ZipArchive::new(reader)?;

    // zip 2.x collapses exact duplicate names while building its index.
    if zip.len() as u64 != directory.entries {
        return Err(Error::Archive("duplicate or inconsistent ZIP directory entries".into()));
    }

    let (entries, total) = inspect_entries(&mut zip, limits, control)?;

    Ok((zip, entries, total))
}

fn inspect_entries(
    zip: &mut ZipArchive<ArchiveReader<'_>>,
    limits: ArchiveLimits,
    control: Control<'_>,
) -> Result<(Vec<Entry>, u64)> {
    let mut entries = Vec::with_capacity(zip.len());
    let mut total = 0u64;
    let mut kinds = BTreeMap::new();
    let mut spellings = BTreeMap::new();
    let mut index_path_bytes = 0usize;

    for index in 0..zip.len() {
        control.check()?;

        let entry = zip.by_index(index)?;
        let relative = files::relative_path(entry.name())?;
        let name = files::archive_name(&relative)?;

        if name.len() > limits.max_path_bytes || relative.components().count() > limits.max_path_depth {
            return Err(Error::Limit("path bytes or depth"));
        }

        index_path_bytes = index_path_bytes.checked_add(name.len() * 2).ok_or(Error::Limit("index path bytes"))?;
        let mode = entry.unix_mode().unwrap_or(if entry.is_dir() { 0o40755 } else { 0o100644 });

        let kind = match mode & 0o170000 {
            0o120000 if !entry.is_dir() => EntryKind::Symlink,
            0o040000 => EntryKind::Directory,
            0 | 0o100000 if entry.is_dir() => EntryKind::Directory,
            0 | 0o100000 => EntryKind::File,
            _ => return Err(Error::Archive(format!("unsupported file type: {name}"))),
        };

        if entry.encrypted() {
            return Err(Error::Archive(format!("password-encrypted ZIP entry: {name}")));
        }

        if entry.size() > limits.max_entry_bytes {
            return Err(Error::Limit("entry bytes"));
        }

        if kind == EntryKind::Directory && entry.size() != 0 {
            return Err(Error::Archive(format!("directory has a payload: {name}")));
        }

        if kind == EntryKind::Symlink && entry.size() > 4096 {
            return Err(Error::Limit("symlink target bytes"));
        }

        total = total.checked_add(entry.size()).ok_or(Error::Limit("total bytes"))?;

        if total > limits.max_total_bytes {
            return Err(Error::Limit("total bytes"));
        }

        if kinds.insert(name.clone(), kind).is_some() {
            return Err(Error::Archive(format!("duplicate path: {name}")));
        }

        let mut prefix = String::new();

        for part in name.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }

            prefix.push_str(part);
            let folded: String = prefix.nfc().flat_map(char::to_lowercase).collect();

            match spellings.entry(folded) {
                std::collections::btree_map::Entry::Occupied(entry) if entry.get() != &prefix => {
                    return Err(Error::Archive(format!("filesystem name collision: {} / {prefix}", entry.get())));
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
                std::collections::btree_map::Entry::Vacant(entry) => {
                    index_path_bytes = index_path_bytes
                        .checked_add(entry.key().len() + prefix.len())
                        .ok_or(Error::Limit("index path bytes"))?;

                    if index_path_bytes > limits.max_index_path_bytes {
                        return Err(Error::Limit("index path bytes"));
                    }

                    entry.insert(prefix.clone());
                }
            }
        }

        entries.push(Entry { index, name, relative, kind, size: entry.size(), mode });
    }

    for entry in &entries {
        for parent in entry.relative.ancestors().skip(1).filter(|path| !path.as_os_str().is_empty()) {
            let name = files::archive_name(parent)?;

            if kinds.get(&name).is_some_and(|kind| *kind != EntryKind::Directory) {
                return Err(Error::Archive(format!("entry traverses a file or symlink: {}", entry.name)));
            }
        }
    }

    Ok((entries, total))
}

pub(crate) fn find_app(entries: &[Entry]) -> Result<(PathBuf, ArchiveKind)> {
    let mut ipa_roots = BTreeSet::new();
    let mut app_roots = BTreeSet::new();

    for entry in entries {
        let parts: Vec<_> = entry.name.split('/').collect();

        if parts.len() >= 2 && parts[0] == "Payload" && parts[1].ends_with(".app") {
            ipa_roots.insert(PathBuf::from("Payload").join(parts[1]));
        } else if parts[0].ends_with(".app") {
            app_roots.insert(PathBuf::from(parts[0]));
        }
    }

    let (roots, kind) =
        if ipa_roots.is_empty() { (app_roots, ArchiveKind::AppZip) } else { (ipa_roots, ArchiveKind::Ipa) };

    if roots.len() != 1 {
        return Err(Error::Bundle(format!("expected one main app, found {}", roots.len())));
    }

    let relative = roots.into_iter().next().ok_or_else(|| Error::Bundle("missing app".into()))?;

    Ok((relative, kind))
}

fn extract_entries(
    zip: &ZipArchive<ArchiveReader<'_>>,
    entries: &[Entry],
    root: &Path,
    total: u64,
    control: Control<'_>,
) -> Result<()> {
    let completed = AtomicU64::new(0);
    control.report(Phase::Extract, 0, total);

    // Construct real directories first. Symlinks are installed only after all
    // payload writes finish, so no worker can traverse a link during extraction.
    let mut directories = BTreeSet::new();

    for entry in entries {
        control.check()?;

        let directory = if entry.kind == EntryKind::Directory {
            entry.relative.as_path()
        } else {
            entry.relative.parent().ok_or_else(|| Error::Path(entry.name.clone()))?
        };

        for relative in directory.ancestors().filter(|path| !path.as_os_str().is_empty()) {
            directories.insert(relative.to_owned());
        }
    }

    // Each logical directory is created exactly once. Unexpected AlreadyExists
    // also detects aliases specific to the host filesystem's Unicode rules.
    for relative in directories {
        control.check()?;
        let directory = root.join(relative);

        io(&directory, fs::create_dir(&directory))?;
    }

    let links = entries
        .par_iter()
        .filter(|entry| entry.kind != EntryKind::Directory)
        .map_init(
            || zip.clone(),
            |archive, entry| {
                control.check()?;

                let path = root.join(&entry.relative);
                let mut input = archive.by_index(entry.index)?;

                if entry.kind == EntryKind::Symlink {
                    let mut bytes = Vec::new();
                    io(&path, (&mut input).take(entry.size + 1).read_to_end(&mut bytes))?;

                    if bytes.len() as u64 != entry.size {
                        return Err(Error::Archive(format!("inconsistent symlink size: {}", entry.name)));
                    }

                    let target = String::from_utf8(bytes).map_err(|_| Error::Path(entry.name.clone()))?;
                    files::validate_symlink(&entry.relative, &target)?;

                    let position = completed.fetch_add(entry.size, Ordering::Relaxed) + entry.size;
                    control.report(Phase::Extract, position, total);

                    return Ok(Some((path, PathBuf::from(target))));
                }

                let mut output = io(&path, fs::OpenOptions::new().write(true).create_new(true).open(&path))?;
                let mut buffer = vec![0; BUFFER_SIZE];
                let mut written = 0u64;

                loop {
                    control.check()?;

                    let size = io(&path, input.read(&mut buffer))?;

                    if size == 0 {
                        break;
                    }

                    written = written.checked_add(size as u64).ok_or(Error::Limit("entry bytes"))?;

                    if written > entry.size {
                        return Err(Error::Archive(format!("inconsistent entry size: {}", entry.name)));
                    }

                    io(&path, output.write_all(&buffer[..size]))?;

                    let position = completed.fetch_add(size as u64, Ordering::Relaxed) + size as u64;
                    control.report(Phase::Extract, position, total);
                }

                if written != entry.size {
                    return Err(Error::Archive(format!("truncated entry: {}", entry.name)));
                }

                set_mode(&path, entry.mode)?;

                Ok(None)
            },
        )
        .collect::<Result<Vec<_>>>()?;

    for (path, target) in links.into_iter().flatten() {
        control.check()?;
        files::create_symlink(&target, &path)?;
    }

    control.report(Phase::Extract, total, total);

    Ok(())
}

pub(crate) fn validate_directory(root: &Path, limits: ArchiveLimits, control: Control<'_>) -> Result<()> {
    let mut count = 0;
    let mut total = 0u64;

    for entry in walkdir::WalkDir::new(root).follow_links(false).min_depth(1) {
        control.check()?;

        let entry = entry.map_err(|error| Error::Archive(error.to_string()))?;
        let metadata = io(entry.path(), fs::symlink_metadata(entry.path()))?;
        let relative = entry.path().strip_prefix(root).map_err(|error| Error::Path(error.to_string()))?;
        let name = files::archive_name(relative)?;
        count += 1;

        if count > limits.max_entries
            || metadata.len() > limits.max_entry_bytes
            || name.len() > limits.max_path_bytes
            || relative.components().count() > limits.max_path_depth
        {
            return Err(Error::Limit("directory entries or entry bytes"));
        }

        if metadata.is_file() {
            total = total.checked_add(metadata.len()).ok_or(Error::Limit("total bytes"))?;
        }

        if total > limits.max_total_bytes {
            return Err(Error::Limit("total bytes"));
        }
    }

    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    io(path, fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777)))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}
