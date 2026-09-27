use crate::archive::{self, Entry, EntryKind};
use crate::error::{from_io, io};
use crate::files;
use crate::reader::ArchiveReader;
use crate::{ArchiveKind, ArchiveLimits, Control, Error, Result, parse_dictionary};
use plist::{Dictionary, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};
use zip::ZipArchive;

const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ICON_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ExtensionMetadata {
    pub file_name: String,
    pub info: Dictionary,
}

#[derive(Debug, Clone)]
pub struct BundleInspection {
    pub kind: ArchiveKind,
    pub info: Dictionary,
    pub extensions: Vec<ExtensionMetadata>,
    pub has_watch_app: bool,
    pub file_size: u64,
    pub encrypted: bool,
    /// Largest successfully decoded declared/legacy PNG, normalized to ordinary RGBA PNG.
    /// Asset-catalog-only icons are not decoded by this inspector.
    pub icon_png: Option<Vec<u8>>,
    /// Optional icon failures do not prevent inspection of the app's metadata.
    pub warnings: Vec<String>,
}

/// Inspect bounded metadata and executable headers without extracting payload files.
/// ZIP CRCs are checked for fully read metadata/icons; unvisited payloads are not verified.
pub fn inspect(path: &Path, limits: ArchiveLimits, control: Control<'_>) -> Result<BundleInspection> {
    control.check()?;

    let (mut source, kind, file_size) = Source::open(path, limits, control)?;
    let info = source.dictionary(Path::new("Info.plist"))?;
    identifier(&info)?;

    let mut extensions = Vec::new();
    let mut has_watch_app = false;

    for relative in source.paths() {
        if relative.file_name().and_then(|name| name.to_str()) != Some("Info.plist") {
            continue;
        }

        let parts: Vec<_> = relative.components().collect();
        let extension = parts.len() == 3
            && matches!(parts[0].as_os_str().to_str(), Some("PlugIns" | "Extensions"))
            && Path::new(parts[1].as_os_str()).extension().and_then(|value| value.to_str()) == Some("appex");
        let watch = parts.first().is_some_and(|part| part.as_os_str() == "Watch");

        if extension || watch {
            let child = source.dictionary(&relative)?;

            if extension {
                identifier(&child)?;
                extensions.push(ExtensionMetadata {
                    file_name: parts[1].as_os_str().to_string_lossy().into_owned(),
                    info: child.clone(),
                });
            }

            has_watch_app |= child.get("WKWatchKitApp").and_then(Value::as_boolean) == Some(true);
        }
    }

    let executable = match info.get("CFBundleExecutable") {
        Some(Value::String(name)) => name.clone(),
        Some(_) => return Err(Error::Bundle("CFBundleExecutable must be a string".into())),
        None => source
            .root()
            .file_stem()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .ok_or_else(|| Error::Bundle("app executable has no filename".into()))?,
    };
    let executable = files::relative_path(&executable)?;

    if executable.components().count() != 1 {
        return Err(Error::Bundle("CFBundleExecutable must be a filename".into()));
    }

    let encrypted = source.with_reader(&executable, |reader, size| {
        sl_macho::BinaryMetadata::read(reader, size).map(|metadata| metadata.encrypted()).map_err(|error| match error {
            sl_macho::Error::Io(source) => from_io(path, source),
            error => Error::MachO(error),
        })
    })?;

    let paths = source.paths();
    let candidates = icon_candidates(&info, &paths);
    let mut icon = None;
    let mut largest_area = 0;
    let mut warnings = Vec::new();

    for candidate in candidates {
        control.check()?;

        let decoded = source.bytes(&candidate, MAX_ICON_BYTES).and_then(|bytes| crate::icons::normalize(&bytes));

        match decoded {
            Ok((png, area)) if area > largest_area => {
                largest_area = area;
                icon = Some(png);
            }
            Ok(_) => {}
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => warnings.push(format!("{}: {error}", candidate.display())),
        }
    }

    if icon.is_none() && paths.iter().any(|path| path == Path::new("Assets.car")) {
        warnings.push("Asset-catalog icons are not decoded yet.".into());
    }

    extensions.sort_unstable_by(|left, right| left.file_name.cmp(&right.file_name));

    Ok(BundleInspection { kind, info, extensions, has_watch_app, file_size, encrypted, icon_png: icon, warnings })
}

fn identifier(info: &Dictionary) -> Result<&str> {
    info.get("CFBundleIdentifier")
        .and_then(Value::as_string)
        .filter(|id| !id.is_empty() && !id.contains('\0'))
        .ok_or_else(|| Error::Bundle("Info.plist has no valid CFBundleIdentifier".into()))
}

enum Source<'a> {
    Zip { zip: ZipArchive<ArchiveReader<'a>>, entries: BTreeMap<PathBuf, Entry>, root: PathBuf, control: Control<'a> },
    Directory { root: PathBuf, paths: Vec<PathBuf>, control: Control<'a> },
}

impl<'a> Source<'a> {
    fn open(path: &Path, limits: ArchiveLimits, control: Control<'a>) -> Result<(Self, ArchiveKind, u64)> {
        let metadata = io(path, fs::metadata(path))?;

        if metadata.is_dir() {
            if path.extension().and_then(|value| value.to_str()) != Some("app") {
                return Err(Error::Bundle("directory input must be an .app".into()));
            }

            archive::validate_directory(path, limits, control)?;

            let root = io(path, fs::canonicalize(path))?;
            let mut paths = Vec::new();
            let mut file_size = 0u64;

            for entry in walkdir::WalkDir::new(&root).follow_links(false).min_depth(1) {
                control.check()?;

                let entry = entry.map_err(|error| Error::Bundle(error.to_string()))?;
                let relative = entry.path().strip_prefix(&root).map_err(|error| Error::Path(error.to_string()))?;
                paths.push(relative.to_owned());

                if entry.file_type().is_file() {
                    let size = io(entry.path(), fs::metadata(entry.path()))?.len();
                    file_size = file_size.checked_add(size).ok_or(Error::Limit("directory bytes"))?;
                }
            }

            return Ok((Self::Directory { root, paths, control }, ArchiveKind::AppDirectory, file_size));
        }

        let (zip, entries, _) = archive::open_zip(path, limits, control)?;
        let (root, kind) = archive::find_app(&entries)?;
        let entries = entries.into_iter().map(|entry| (entry.relative.clone(), entry)).collect();

        Ok((Self::Zip { zip, entries, root, control }, kind, metadata.len()))
    }

    fn root(&self) -> &Path {
        match self {
            Self::Zip { root, .. } | Self::Directory { root, .. } => root,
        }
    }

    fn paths(&self) -> Vec<PathBuf> {
        match self {
            Self::Zip { entries, root, .. } => entries
                .keys()
                .filter_map(|path| path.strip_prefix(root).ok().filter(|path| !path.as_os_str().is_empty()))
                .map(Path::to_owned)
                .collect(),
            Self::Directory { paths, .. } => paths.clone(),
        }
    }

    fn with_reader<T>(
        &mut self,
        relative: &Path,
        operation: impl FnOnce(&mut CheckedReader<'_, 'a>, u64) -> Result<T>,
    ) -> Result<T> {
        match self {
            Self::Directory { root, control, .. } => {
                let path = files::read_inside(root, relative)?;
                let metadata = io(&path, fs::metadata(&path))?;

                if !metadata.is_file() {
                    return Err(Error::Bundle(format!("{} is not a regular metadata file", path.display())));
                }

                let mut file = io(&path, File::open(&path))?;
                let size = io(&path, file.metadata())?.len();
                let mut reader = CheckedReader { inner: &mut file, control: *control };

                operation(&mut reader, size)
            }
            Self::Zip { zip, entries, root, control } => {
                let mut path = root.join(relative);
                let mut visited = BTreeSet::new();

                loop {
                    control.check()?;

                    if !visited.insert(path.clone()) || visited.len() > 64 {
                        return Err(Error::Path("cyclic or excessive metadata symlinks".into()));
                    }

                    let entry = entries
                        .get(&path)
                        .ok_or_else(|| Error::Bundle(format!("missing archive entry: {}", path.display())))?;
                    let mut file = zip.by_index(entry.index)?;

                    match entry.kind {
                        EntryKind::Directory => return Err(Error::Bundle("metadata path is a directory".into())),
                        EntryKind::File => {
                            let mut reader = CheckedReader { inner: &mut file, control: *control };

                            return operation(&mut reader, entry.size);
                        }
                        EntryKind::Symlink => {
                            let mut target = String::new();
                            file.take(4097).read_to_string(&mut target).map_err(|error| from_io(&path, error))?;
                            files::validate_symlink(&path, &target)?;
                            let mut resolved = path.parent().unwrap_or(Path::new("")).to_owned();

                            for part in target.split('/') {
                                match part {
                                    "" | "." => {}
                                    ".." => {
                                        resolved.pop();
                                    }
                                    part => resolved.push(part),
                                }
                            }

                            if !resolved.starts_with(&*root) {
                                return Err(Error::Path("metadata symlink escapes the app".into()));
                            }

                            path = resolved;
                        }
                    }
                }
            }
        }
    }

    fn bytes(&mut self, relative: &Path, maximum: u64) -> Result<Vec<u8>> {
        self.with_reader(relative, |reader, size| {
            if size > maximum {
                return Err(Error::Limit("metadata or icon bytes"));
            }

            let mut bytes = Vec::with_capacity(size as usize);
            reader.take(maximum + 1).read_to_end(&mut bytes).map_err(|error| from_io(relative, error))?;

            if bytes.len() as u64 != size {
                return Err(Error::Archive("metadata size changed or is inconsistent".into()));
            }

            Ok(bytes)
        })
    }

    fn dictionary(&mut self, relative: &Path) -> Result<Dictionary> {
        parse_dictionary(&self.bytes(relative, MAX_METADATA_BYTES)?)
    }
}

struct CheckedReader<'reader, 'control> {
    inner: &'reader mut dyn Read,
    control: Control<'control>,
}

impl Read for CheckedReader<'_, '_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.control.check().map_err(std::io::Error::other)?;

        let amount = bytes.len().min(files::BUFFER_SIZE);

        self.inner.read(&mut bytes[..amount])
    }
}

fn icon_candidates(info: &Dictionary, paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut names = BTreeSet::new();

    for key in ["CFBundleIconFile", "CFBundleIconFiles"] {
        add_icon_names(info.get(key), &mut names);
    }

    for key in ["CFBundleIcons", "CFBundleIcons~ipad"] {
        let primary = info
            .get(key)
            .and_then(Value::as_dictionary)
            .and_then(|icons| icons.get("CFBundlePrimaryIcon"))
            .and_then(Value::as_dictionary);

        if let Some(primary) = primary {
            add_icon_names(primary.get("CFBundleIconFiles"), &mut names);
        }
    }

    paths
        .iter()
        .filter(|path| path.components().count() == 1)
        .filter(|path| {
            let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            let Some(stem) = filename.strip_suffix(".png") else {
                return false;
            };

            names.iter().any(|name| {
                let name = name.strip_suffix(".png").unwrap_or(name);

                stem == name || stem.strip_prefix(name).is_some_and(|suffix| suffix.starts_with(['@', '~']))
            }) || (names.is_empty() && stem.to_ascii_lowercase().starts_with("icon"))
        })
        .cloned()
        .collect()
}

fn add_icon_names(value: Option<&Value>, names: &mut BTreeSet<String>) {
    match value {
        Some(Value::String(name)) => {
            names.insert(name.clone());
        }
        Some(Value::Array(values)) => {
            for value in values {
                add_icon_names(Some(value), names);
            }
        }
        _ => {}
    }
}
