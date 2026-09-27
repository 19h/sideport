use crate::error::io;
use crate::files;
use crate::{Bundle, BundleArchive, Control, Error, Phase, Result, read_dictionary};
use plist::{Dictionary, Value};
use std::{
    collections::BTreeMap,
    fmt, fs,
    path::{Path, PathBuf},
    sync::Arc,
};

type PropertyTransform = dyn Fn(Option<&Value>) -> Result<Option<Value>> + Send + Sync;

#[derive(Clone)]
pub enum PropertyEdit {
    Set(Value),
    Remove,
    Transform(Arc<PropertyTransform>),
}

impl fmt::Debug for PropertyEdit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Set(value) => formatter.debug_tuple("Set").field(value).finish(),
            Self::Remove => formatter.write_str("Remove"),
            Self::Transform(_) => formatter.write_str("Transform(..)"),
        }
    }
}

impl PropertyEdit {
    fn resolve(&self, previous: Option<&Value>) -> Result<Option<Value>> {
        match self {
            Self::Set(value) => Ok(Some(value.clone())),
            Self::Remove => Ok(None),
            Self::Transform(transform) => transform(previous),
        }
    }
}

pub type InfoEdits = BTreeMap<String, PropertyEdit>;

#[derive(Debug, Clone)]
pub struct Replacement {
    pub target: PathBuf,
    /// None deletes the target; a file/directory replaces or creates it.
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct PatchOptions {
    pub info: InfoEdits,
    pub drop_plugins: bool,
    pub remove_extensions: Vec<String>,
    pub remove_watch_apps: bool,
    pub replacements: Vec<Replacement>,
}

impl Default for PatchOptions {
    fn default() -> Self {
        Self {
            info: BTreeMap::new(),
            drop_plugins: false,
            remove_extensions: Vec::new(),
            remove_watch_apps: true,
            replacements: Vec::new(),
        }
    }
}

#[derive(Debug, Default)]
pub struct PatchReport {
    pub removed: Vec<PathBuf>,
    pub replaced: Vec<PathBuf>,
    pub metadata_changed: Vec<PathBuf>,
}

impl BundleArchive {
    pub fn bundle(&self) -> Result<Bundle> {
        Bundle::open(&self.bundle_path())
    }

    pub fn patch(&mut self, options: &PatchOptions, control: Control<'_>) -> Result<PatchReport> {
        control.check()?;

        let mut report = PatchReport::default();
        let root = self.bundle_path();

        if options.remove_watch_apps {
            remove_watch(&root, &mut report, control)?;
        }

        if options.drop_plugins {
            let path = files::write_inside(&root, Path::new("PlugIns"))?;
            files::remove(&path)?;
            report.removed.push(PathBuf::from("PlugIns"));
        }

        for name in &options.remove_extensions {
            let relative = files::relative_path(name)?;

            if relative.components().count() != 1 || !name.ends_with(".appex") {
                return Err(Error::Path(format!("extension selection must be an .appex filename: {name}")));
            }

            for directory in ["PlugIns", "Extensions"] {
                let relative = Path::new(directory).join(&relative);
                let path = files::write_inside(&root, &relative)?;

                if io(&path, path.try_exists())? {
                    files::remove(&path)?;
                    report.removed.push(relative);
                }
            }
        }

        for replacement in &options.replacements {
            control.check()?;
            replace(&root, replacement, control)?;
            let relative = self.bundle_relative().join(&replacement.target);
            self.forget_directory_modes(&relative);

            if let Some(source) = &replacement.source
                && io(source, fs::symlink_metadata(source))?.is_dir()
            {
                self.record_directory_modes(source, &relative)?;
            }

            if replacement.source.is_some() {
                report.replaced.push(replacement.target.clone());
            } else {
                report.removed.push(replacement.target.clone());
            }
        }

        cleanup_sinf(&root)?;

        if !options.info.is_empty() {
            let mut bundle = Bundle::open(&root)?;
            let original_id = bundle.identifier()?.to_owned();
            let original_top = bundle.original_info().clone();
            let canonical_root = bundle.root().to_owned();

            let update = InfoUpdate {
                root: &canonical_root,
                edits: &options.info,
                top_id: &original_id,
                top_info: &original_top,
                control,
            };

            update.apply(&mut bundle, None, &mut report)?;
        }

        control.report(Phase::Patch, 1, 1);

        Ok(report)
    }
}

fn remove_watch(root: &Path, report: &mut PatchReport, control: Control<'_>) -> Result<()> {
    let mut parents = Vec::new();

    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        control.check()?;

        let entry = entry.map_err(|error| Error::Bundle(error.to_string()))?;

        if !entry.file_type().is_dir() || !entry.path().join("Info.plist").is_file() {
            continue;
        }

        let info = entry.path().join("Info.plist");
        let relative = info.strip_prefix(root).map_err(|error| Error::Path(error.to_string()))?;
        let info_path = files::read_inside(root, relative)?;
        let info = read_dictionary(&info_path)?;

        if info.get("WKWatchKitApp").and_then(Value::as_boolean) == Some(true) {
            let parent = entry.path().parent().ok_or_else(|| Error::Path(entry.path().display().to_string()))?;

            if parent == root || !parent.starts_with(root) {
                return Err(Error::Bundle("cannot remove the main app as a WatchKit container".into()));
            }

            parents.push(parent.to_owned());
        }
    }

    parents.sort_unstable();

    for path in parents {
        control.check()?;

        if io(&path, path.try_exists())? {
            let relative = path.strip_prefix(root).map_err(|error| Error::Path(error.to_string()))?;
            files::remove(&path)?;
            report.removed.push(relative.to_owned());
        }
    }

    Ok(())
}

fn replace(root: &Path, replacement: &Replacement, control: Control<'_>) -> Result<()> {
    let path = files::write_inside(root, &replacement.target)?;

    if let Some(source) = &replacement.source {
        let metadata = io(source, fs::symlink_metadata(source))?;

        if !(metadata.is_file() || metadata.is_dir()) {
            return Err(Error::Path(format!("replacement source must be a file or directory: {}", source.display())));
        }

        files::remove(&path)?;

        if metadata.is_dir() {
            files::copy_tree(source, &path, control)?;
        } else {
            files::copy_file(source, &path, control)?;
        }
    } else {
        files::remove(&path)?;
    }

    Ok(())
}

fn cleanup_sinf(root: &Path) -> Result<()> {
    let relative = Path::new("SC_Info/Manifest.plist");
    let path = root.join(relative);

    if !io(&path, path.try_exists())? {
        return Ok(());
    }

    let path = files::read_inside(root, relative)?;
    let mut manifest = read_dictionary(&path)?;

    if let Some(value) = manifest.get_mut("SinfReplicationPaths") {
        let paths =
            value.as_array_mut().ok_or_else(|| Error::Bundle("SinfReplicationPaths must be an array".into()))?;
        let mut retained = Vec::with_capacity(paths.len());

        for value in paths.iter() {
            let name = value.as_string().ok_or_else(|| Error::Bundle("SINF path must be a string".into()))?;
            let relative = files::relative_path(name)?;

            if io(&root.join(&relative), root.join(&relative).try_exists())? {
                files::read_inside(root, &relative)?;
                retained.push(value.clone());
            }
        }

        *paths = retained;
    }

    let mut bytes = Vec::new();
    Value::Dictionary(manifest).to_writer_xml(&mut bytes)?;
    files::atomic_write(&path, &bytes)
}

#[derive(Debug)]
struct InfoUpdate<'a> {
    root: &'a Path,
    edits: &'a InfoEdits,
    top_id: &'a str,
    top_info: &'a Dictionary,
    control: Control<'a>,
}

impl InfoUpdate<'_> {
    fn apply(&self, bundle: &mut Bundle, parent_info: Option<&Dictionary>, report: &mut PatchReport) -> Result<()> {
        self.control.check()?;

        let original = bundle.original_info().clone();
        let old_id = bundle.identifier()?.to_owned();
        let mut info = bundle.info().clone();

        cleanup_localized(bundle, self.edits)?;

        let new_id = if let Some(edit) = self.edits.get("CFBundleIdentifier") {
            let value = edit
                .resolve(self.top_info.get("CFBundleIdentifier"))?
                .ok_or_else(|| Error::Bundle("cannot delete CFBundleIdentifier".into()))?;
            let identifier = value
                .as_string()
                .filter(|value| !value.is_empty() && !value.contains('\0'))
                .ok_or_else(|| Error::Bundle("CFBundleIdentifier must be a nonempty string".into()))?;

            old_id
                .strip_prefix(self.top_id)
                .map(|suffix| format!("{identifier}{suffix}"))
                .unwrap_or_else(|| old_id.clone())
        } else {
            old_id.clone()
        };

        if new_id != old_id
            && !self.edits.contains_key("CFBundleURLTypes")
            && let Some(Value::Array(types)) = info.get_mut("CFBundleURLTypes")
        {
            for value in types {
                if let Some(fields) = value.as_dictionary_mut()
                    && fields.get("CFBundleURLName").and_then(Value::as_string) == Some(&old_id)
                {
                    fields.insert("CFBundleURLName".into(), new_id.clone().into());
                }
            }
        }

        for (key, edit) in self.edits {
            let value = match key.as_str() {
                "CFBundleIdentifier" => Some(Value::String(new_id.clone())),
                "ALTBundleIdentifier" => Some(original.get(key).cloned().unwrap_or_else(|| old_id.clone().into())),
                _ => {
                    if parent_info.is_some_and(|parent| info.get(key).is_some() && info.get(key) == parent.get(key)) {
                        continue;
                    }

                    edit.resolve(info.get(key))?
                }
            };

            if let Some(value) = value {
                info.insert(key.clone(), value);
            } else {
                info.remove(key);
            }
        }

        if bundle.write_info(info)? {
            let relative = bundle.root().strip_prefix(self.root).map_err(|error| Error::Path(error.to_string()))?;

            report.metadata_changed.push(relative.join("Info.plist"));
        }

        for mut child in bundle.metadata_children()? {
            self.apply(&mut child, Some(&original), report)?;
        }

        Ok(())
    }
}
fn cleanup_localized(bundle: &Bundle, edits: &InfoEdits) -> Result<()> {
    for entry in io(bundle.root(), fs::read_dir(bundle.root()))? {
        let entry = io(bundle.root(), entry)?;
        let directory = entry.path();

        if !io(&directory, entry.file_type())?.is_dir()
            || directory.extension().and_then(|extension| extension.to_str()) != Some("lproj")
        {
            continue;
        }

        let relative = directory
            .strip_prefix(bundle.root())
            .map_err(|error| Error::Path(error.to_string()))?
            .join("InfoPlist.strings");
        let path = bundle.root().join(&relative);

        if !io(&path, path.try_exists())? {
            continue;
        }

        let path = files::read_inside(bundle.root(), &relative)?;
        let mut localized = read_dictionary(&path)?;
        let before = localized.len();

        for key in edits.keys() {
            if localized.get(key).is_some() && localized.get(key) != bundle.info().get(key) {
                localized.remove(key);
            }
        }

        if localized.len() != before {
            let mut bytes = Vec::new();
            Value::Dictionary(localized).to_writer_binary(&mut bytes)?;
            files::atomic_write(&path, &bytes)?;
        }
    }

    Ok(())
}
