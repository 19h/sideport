use crate::error::io;
use crate::files::{self, BUFFER_SIZE};
use crate::{BundleArchive, Control, Error, Phase, Result};
use flate2::{Compression, write::DeflateEncoder};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

// PKWARE APPNOTE 6.3.10 §§4.3, 4.4, 4.5.3. File payloads use data
// descriptors; no output seek or whole-file buffering is required.
const LOCAL_HEADER: u32 = 0x0403_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;
const END_DIRECTORY: u32 = 0x0605_4b50;
const ZIP64_END: u32 = 0x0606_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;

const UTF8_FLAG: u16 = 1 << 11;
const DESCRIPTOR_FLAG: u16 = 1 << 3;
const DOS_DATE: u16 = 0x21; // 1980-01-01, 00:00:00.
const DEFLATE: u16 = 8;
const STORED: u16 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLayout {
    /// Keep Payload and all other archive entries as originally arranged.
    Original,
    /// Emit the prepared app under Payload/<name>.app.
    Ipa,
}

#[derive(Debug, Clone, Copy)]
pub struct PackOptions {
    pub compression_level: u32,
    /// Exercise ZIP64 on small archives; large entries/offsets enable it automatically.
    pub force_zip64: bool,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self { compression_level: 6, force_zip64: false }
    }
}

#[derive(Debug)]
struct Item {
    path: PathBuf,
    name: String,
    mode: u32,
    size: u64,
    link: Option<Vec<u8>>,
    directory: bool,
}

#[derive(Debug)]
struct Record {
    name: String,
    flags: u16,
    method: u16,
    mode: u32,
    crc: u32,
    compressed: u64,
    size: u64,
    offset: u64,
    zip64: bool,
}

impl Record {
    fn version(&self) -> u16 {
        if self.zip64 { 45 } else { 20 }
    }
}

#[derive(Debug)]
struct CountingWriter<W> {
    inner: W,
    position: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let size = self.inner.write(bytes)?;
        self.position = self
            .position
            .checked_add(size as u64)
            .ok_or_else(|| std::io::Error::other("ZIP output offset overflow"))?;

        Ok(size)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl BundleArchive {
    /// Produce identical bytes for an unchanged staging tree and packing options.
    /// This accepts a forward-only sink such as an AFC upload channel.
    pub fn write_to<W: Write>(
        &self,
        writer: W,
        layout: OutputLayout,
        options: PackOptions,
        control: Control<'_>,
    ) -> Result<u64> {
        control.check()?;

        if options.compression_level > 9 {
            return Err(Error::Archive("compression level must be 0..=9".into()));
        }

        let (root, prefix) = match layout {
            OutputLayout::Original => (self.root().to_owned(), None),
            OutputLayout::Ipa => {
                let name = self
                    .bundle_relative()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| Error::Path(self.bundle_relative().display().to_string()))?;

                (self.bundle_path(), Some(format!("Payload/{name}")))
            }
        };

        let mut items = collect_items(&root, prefix.as_deref(), control)?;

        for item in &mut items {
            if item.directory {
                let relative = item.path.strip_prefix(self.root()).map_err(|error| Error::Path(error.to_string()))?;

                if let Some(mode) = self.directory_mode(relative) {
                    item.mode = mode;
                }
            }
        }

        write_items(writer, items, options, control)
    }

    /// Write the prepared app as `<destination>/Payload/<name>.app`, the recovered folder output
    /// used for Apple Silicon installs. The destination must not exist; the tree is built in a
    /// sibling temporary directory and renamed into place after the copy completes.
    pub fn save_folder(&self, destination: &Path, control: Control<'_>) -> Result<()> {
        control.check()?;

        match fs::symlink_metadata(destination) {
            Ok(_) => return Err(Error::Path(format!("{} already exists", destination.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(Error::Io { path: destination.to_owned(), source }),
        }

        let parent = destination.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let canonical_parent = io(parent, fs::canonicalize(parent))?;
        let canonical_root = io(self.root(), fs::canonicalize(self.root()))?;

        if canonical_parent.starts_with(canonical_root) {
            return Err(Error::Path("output must be outside the staging tree".into()));
        }

        let name = self
            .bundle_relative()
            .file_name()
            .ok_or_else(|| Error::Path(self.bundle_relative().display().to_string()))?;

        let staging = io(parent, tempfile::Builder::new().prefix(".sideport-folder").tempdir_in(parent))?;
        let payload = staging.path().join("Payload").join(name);

        files::copy_tree(&self.bundle_path(), &payload, control)?;
        control.check()?;

        let staged = staging.keep();
        io(destination, fs::rename(&staged, destination))
    }

    /// Replace the destination only after packing, flushing and syncing succeed.
    pub fn save(
        &self,
        destination: &Path,
        layout: OutputLayout,
        options: PackOptions,
        control: Control<'_>,
    ) -> Result<u64> {
        let parent = destination.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let existing = match fs::symlink_metadata(destination) {
            Ok(metadata) if metadata.is_file() => Some(metadata),
            Ok(_) => return Err(Error::Path(format!("{} is not a regular file", destination.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(Error::Io { path: destination.to_owned(), source }),
        };

        let canonical_parent = io(parent, fs::canonicalize(parent))?;

        let canonical_root = io(self.root(), fs::canonicalize(self.root()))?;

        if canonical_parent.starts_with(canonical_root) {
            return Err(Error::Path("output must be outside the staging tree".into()));
        }

        let mut temporary = io(parent, tempfile::NamedTempFile::new_in(parent))?;
        let size = self.write_to(&mut temporary, layout, options, control)?;

        io(destination, temporary.flush())?;

        if let Some(metadata) = existing {
            io(destination, temporary.as_file().set_permissions(metadata.permissions()))?;
        }

        control.check()?;
        io(destination, temporary.as_file().sync_all())?;
        temporary
            .persist(destination)
            .map_err(|error| Error::Io { path: destination.to_owned(), source: error.error })?;

        Ok(size)
    }
}

fn collect_items(root: &Path, prefix: Option<&str>, control: Control<'_>) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    let min_depth = usize::from(prefix.is_none());

    for entry in walkdir::WalkDir::new(root).follow_links(false).min_depth(min_depth) {
        control.check()?;

        let entry = entry.map_err(|error| Error::Archive(error.to_string()))?;
        let relative = entry.path().strip_prefix(root).map_err(|error| Error::Path(error.to_string()))?;
        let relative_name = files::archive_name(relative)?;

        let mut name = match prefix {
            Some(prefix) if relative_name.is_empty() => prefix.to_owned(),
            Some(prefix) => format!("{prefix}/{relative_name}"),
            None => relative_name,
        };

        let metadata = io(entry.path(), fs::symlink_metadata(entry.path()))?;

        if !(metadata.is_file() || metadata.is_dir() || metadata.file_type().is_symlink()) {
            return Err(Error::Path(format!("unsupported output file type: {name}")));
        }

        let link = if metadata.file_type().is_symlink() {
            let target = io(entry.path(), fs::read_link(entry.path()))?;
            let text = target.to_str().ok_or_else(|| Error::Path(target.display().to_string()))?;
            files::validate_symlink(&files::relative_path(&name)?, text)?;

            Some(text.as_bytes().to_vec())
        } else {
            None
        };

        if metadata.is_dir() {
            name.push('/');
        }

        if name.len() > u16::MAX as usize {
            return Err(Error::Limit("ZIP filename bytes"));
        }

        let size = link.as_ref().map_or(metadata.len(), |bytes| bytes.len() as u64);
        let mode = unix_mode(&metadata);

        items.push(Item {
            path: entry.path().to_owned(),
            name,
            mode,
            size: if metadata.is_dir() { 0 } else { size },
            link,
            directory: metadata.is_dir(),
        });
    }

    items.sort_unstable_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));

    Ok(items)
}

fn write_items<W: Write>(writer: W, items: Vec<Item>, options: PackOptions, control: Control<'_>) -> Result<u64> {
    let total =
        items.iter().try_fold(0u64, |total, item| total.checked_add(item.size).ok_or(Error::Limit("packing bytes")))?;

    let mut output = CountingWriter { inner: writer, position: 0 };
    let mut records = Vec::with_capacity(items.len());
    let mut completed = 0u64;

    control.report(Phase::Pack, 0, total);

    for item in items {
        control.check()?;

        let streamed = !item.directory && item.link.is_none();
        // Reserve enough headroom for DEFLATE's worst-case expansion.
        let zip64 = options.force_zip64 || item.size >= u64::from(u32::MAX) - 8 * 1024 * 1024;

        let mut record = Record {
            name: item.name,
            flags: UTF8_FLAG | if streamed { DESCRIPTOR_FLAG } else { 0 },
            method: if streamed { DEFLATE } else { STORED },
            mode: item.mode,
            crc: item.link.as_ref().map_or(0, |bytes| crc32fast::hash(bytes)),
            compressed: if streamed { 0 } else { item.size },
            size: item.size,
            offset: output.position,
            zip64,
        };

        io(&item.path, output.write_all(&local_header(&record)))?;

        if let Some(bytes) = item.link {
            io(&item.path, output.write_all(&bytes))?;
            completed += bytes.len() as u64;
        } else if streamed {
            let start = output.position;
            let mut encoder = DeflateEncoder::new(&mut output, Compression::new(options.compression_level));
            let mut input = io(&item.path, File::open(&item.path))?;
            let mut buffer = vec![0; BUFFER_SIZE];
            let mut crc = crc32fast::Hasher::new();
            let mut consumed = 0u64;

            loop {
                control.check()?;

                let size = io(&item.path, input.read(&mut buffer))?;

                if size == 0 {
                    break;
                }

                consumed = consumed.checked_add(size as u64).ok_or(Error::Limit("packing entry bytes"))?;

                if consumed > item.size {
                    return Err(Error::Archive(format!("{} grew while packing", item.path.display())));
                }

                crc.update(&buffer[..size]);
                io(&item.path, encoder.write_all(&buffer[..size]))?;
                completed += size as u64;

                control.report(Phase::Pack, completed, total);
            }

            if consumed != item.size {
                return Err(Error::Archive(format!("{} shrank while packing", item.path.display())));
            }

            io(&item.path, encoder.finish())?;

            record.crc = crc.finalize();
            record.compressed = output.position - start;

            if !record.zip64 && record.compressed >= u64::from(u32::MAX) {
                return Err(Error::Limit("compressed entry requires ZIP64"));
            }

            io(&item.path, output.write_all(&data_descriptor(&record)))?;
        }

        records.push(record);
        control.report(Phase::Pack, completed, total);
    }

    let central_offset = output.position;

    for record in &records {
        control.check()?;
        io(Path::new("<ZIP output>"), output.write_all(&central_header(record)))?;
    }

    let central_size = output.position - central_offset;
    let zip64 = options.force_zip64
        || records.iter().any(|record| record.zip64)
        || records.len() >= u16::MAX as usize
        || central_offset >= u64::from(u32::MAX)
        || central_size >= u64::from(u32::MAX);

    let ending = end_directory(records.len() as u64, central_offset, central_size, output.position, zip64);
    io(Path::new("<ZIP output>"), output.write_all(&ending))?;
    io(Path::new("<ZIP output>"), output.flush())?;

    control.report(Phase::Pack, total, total);

    Ok(output.position)
}

#[rustfmt::skip]
fn local_header(record: &Record) -> Vec<u8> {
    let descriptor = record.flags & DESCRIPTOR_FLAG != 0;
    let extra = if record.zip64 { zip64_extra(&[
        if descriptor { 0 } else { record.size },
        if descriptor { 0 } else { record.compressed },
    ]) } else { Vec::new() };

    let size = if record.zip64 { u32::MAX } else if descriptor { 0 } else { record.size as u32 };
    let compressed = if record.zip64 { u32::MAX } else if descriptor { 0 } else { record.compressed as u32 };
    let mut bytes = Vec::with_capacity(30 + record.name.len() + extra.len());

    word(&mut bytes, LOCAL_HEADER); short(&mut bytes, record.version());
    short(&mut bytes, record.flags); short(&mut bytes, record.method);
    short(&mut bytes, 0); short(&mut bytes, DOS_DATE);
    word(&mut bytes, if descriptor { 0 } else { record.crc });
    word(&mut bytes, compressed); word(&mut bytes, size);
    short(&mut bytes, record.name.len() as u16); short(&mut bytes, extra.len() as u16);
    bytes.extend(record.name.as_bytes()); bytes.extend(extra);

    bytes
}

#[rustfmt::skip]
fn central_header(record: &Record) -> Vec<u8> {
    let offset64 = record.offset >= u64::from(u32::MAX);
    let mut values = Vec::new();

    if record.zip64 {
        values.extend([record.size, record.compressed]);
    }

    if offset64 {
        values.push(record.offset);
    }

    let extra = if values.is_empty() { Vec::new() } else { zip64_extra(&values) };
    let needed = if offset64 { 45 } else { record.version() };
    let mut bytes = Vec::with_capacity(46 + record.name.len() + extra.len());

    word(&mut bytes, CENTRAL_HEADER); short(&mut bytes, (3 << 8) | needed); short(&mut bytes, needed);
    short(&mut bytes, record.flags); short(&mut bytes, record.method);
    short(&mut bytes, 0); short(&mut bytes, DOS_DATE); word(&mut bytes, record.crc);
    word(&mut bytes, if record.zip64 { u32::MAX } else { record.compressed as u32 });
    word(&mut bytes, if record.zip64 { u32::MAX } else { record.size as u32 });
    short(&mut bytes, record.name.len() as u16); short(&mut bytes, extra.len() as u16);
    short(&mut bytes, 0); short(&mut bytes, 0); short(&mut bytes, 0); // comment, disk, internal attributes
    word(&mut bytes, (record.mode << 16) | (u32::from(record.name.ends_with('/')) * 0x10));
    word(&mut bytes, if offset64 { u32::MAX } else { record.offset as u32 });
    bytes.extend(record.name.as_bytes()); bytes.extend(extra);

    bytes
}

#[rustfmt::skip]
fn data_descriptor(record: &Record) -> Vec<u8> {
    let mut bytes = Vec::new();

    word(&mut bytes, DESCRIPTOR); word(&mut bytes, record.crc);

    if record.zip64 {
        long(&mut bytes, record.compressed); long(&mut bytes, record.size);
    } else {
        word(&mut bytes, record.compressed as u32); word(&mut bytes, record.size as u32);
    }

    bytes
}

#[rustfmt::skip]
fn end_directory(count: u64, offset: u64, size: u64, end: u64, zip64: bool) -> Vec<u8> {
    let mut bytes = Vec::new();

    if zip64 {
        word(&mut bytes, ZIP64_END); long(&mut bytes, 44);
        short(&mut bytes, (3 << 8) | 45); short(&mut bytes, 45);
        word(&mut bytes, 0); word(&mut bytes, 0);
        long(&mut bytes, count); long(&mut bytes, count); long(&mut bytes, size); long(&mut bytes, offset);

        word(&mut bytes, ZIP64_LOCATOR); word(&mut bytes, 0); long(&mut bytes, end); word(&mut bytes, 1);
    }

    word(&mut bytes, END_DIRECTORY); short(&mut bytes, 0); short(&mut bytes, 0);
    short(&mut bytes, if zip64 { u16::MAX } else { count as u16 });
    short(&mut bytes, if zip64 { u16::MAX } else { count as u16 });
    word(&mut bytes, if zip64 { u32::MAX } else { size as u32 });
    word(&mut bytes, if zip64 { u32::MAX } else { offset as u32 }); short(&mut bytes, 0);

    bytes
}

fn zip64_extra(values: &[u64]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + values.len() * 8);
    short(&mut bytes, 1);
    short(&mut bytes, (values.len() * 8) as u16);

    for value in values {
        long(&mut bytes, *value);
    }

    bytes
}

fn short(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend(value.to_le_bytes());
}

fn word(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend(value.to_le_bytes());
}

fn long(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend(value.to_le_bytes());
}

#[cfg(unix)]
fn unix_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn unix_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.file_type().is_symlink() {
        0o120777
    } else if metadata.is_dir() {
        0o40755
    } else {
        0o100644
    }
}
