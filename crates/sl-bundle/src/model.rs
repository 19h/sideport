use crate::error::io;
use crate::files;
use crate::{Error, Result, read_dictionary};
use plist::{Dictionary, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleKind {
    App,
    Extension,
    Framework,
    StickerPack,
    Resource,
}

#[derive(Debug, Clone)]
pub struct Bundle {
    root: PathBuf,
    info_path: PathBuf,
    info: Dictionary,
    original_info: Dictionary,
    kind: BundleKind,
}

impl Bundle {
    pub fn open(root: &Path) -> Result<Self> {
        let metadata = io(root, fs::symlink_metadata(root))?;

        if !metadata.is_dir() {
            return Err(Error::Bundle(format!("{} is not a real bundle directory", root.display())));
        }

        let root = io(root, fs::canonicalize(root))?;

        let info_path = files::read_inside(&root, Path::new("Info.plist"))?;
        let info = read_dictionary(&info_path)?;
        let extension = root.extension().and_then(|extension| extension.to_str());

        let kind = match extension {
            Some("app") => BundleKind::App,
            Some("appex") => BundleKind::Extension,
            Some("framework") => BundleKind::Framework,
            Some("stickerpack") => BundleKind::StickerPack,
            _ => BundleKind::Resource,
        };

        Ok(Self { root, info_path, original_info: info.clone(), info, kind })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn info_path(&self) -> &Path {
        &self.info_path
    }

    pub fn info(&self) -> &Dictionary {
        &self.info
    }

    pub fn original_info(&self) -> &Dictionary {
        &self.original_info
    }

    pub fn kind(&self) -> BundleKind {
        self.kind
    }

    pub fn identifier(&self) -> Result<&str> {
        self.info
            .get("CFBundleIdentifier")
            .and_then(Value::as_string)
            .filter(|identifier| !identifier.is_empty() && !identifier.as_bytes().contains(&0))
            .ok_or_else(|| Error::Bundle(format!("{} has no valid bundle identifier", self.root.display())))
    }

    pub fn executable_relative(&self) -> Result<PathBuf> {
        let name = match self.info.get("CFBundleExecutable") {
            Some(Value::String(name)) => name.as_str(),
            Some(_) => return Err(Error::Bundle("CFBundleExecutable must be a string".into())),
            None => self
                .root
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or_else(|| Error::Path(self.root.display().to_string()))?,
        };

        let relative = files::relative_path(name)?;

        if relative.components().count() != 1 {
            return Err(Error::Bundle("CFBundleExecutable must be a filename".into()));
        }

        Ok(relative)
    }

    pub fn executable_path(&self) -> Result<PathBuf> {
        files::read_inside(&self.root, &self.executable_relative()?)
    }

    pub fn metadata_children(&self) -> Result<Vec<Self>> {
        let mut children = self.children_in("PlugIns", Some("appex"))?;
        children.extend(self.children_in("Extensions", Some("appex"))?);
        children.extend(self.children_in("", Some("stickerpack"))?);
        children.sort_unstable_by(|left, right| left.root.cmp(&right.root));

        Ok(children)
    }

    pub fn signing_children(&self) -> Result<Vec<Self>> {
        let mut children = self.children_in("Frameworks", None)?;
        children.extend(self.children_in("PlugIns", Some("appex"))?);
        children.extend(self.children_in("Extensions", Some("appex"))?);

        // Frameworks precede extensions, matching the recovered preparation order.
        children.sort_unstable_by(|left, right| {
            let left_key = (left.kind != BundleKind::Framework, &left.root);
            let right_key = (right.kind != BundleKind::Framework, &right.root);

            left_key.cmp(&right_key)
        });

        Ok(children)
    }

    fn children_in(&self, directory: &str, extension: Option<&str>) -> Result<Vec<Self>> {
        let path = self.root.join(directory);

        if !io(&path, path.try_exists())? {
            return Ok(Vec::new());
        }

        let path = if directory.is_empty() {
            self.root.clone()
        } else {
            files::read_inside(&self.root, Path::new(directory))?
        };
        let mut children = Vec::new();

        for entry in io(&path, fs::read_dir(&path))? {
            let entry = io(&path, entry)?;
            let child = entry.path();

            if !io(&child, entry.file_type())?.is_dir()
                || extension
                    .is_some_and(|extension| child.extension().and_then(|value| value.to_str()) != Some(extension))
            {
                continue;
            }

            if io(&child, child.join("Info.plist").try_exists())? {
                let mut bundle = Self::open(&child)?;

                if directory == "Frameworks" {
                    bundle.kind = BundleKind::Framework;
                }

                children.push(bundle);
            }
        }

        Ok(children)
    }

    pub(crate) fn write_info(&mut self, info: Dictionary) -> Result<bool> {
        if self.info == info {
            return Ok(false);
        }

        let mut bytes = Vec::new();
        Value::Dictionary(info.clone()).to_writer_binary(&mut bytes)?;
        files::atomic_write(&self.info_path, &bytes)?;
        self.info = info;

        Ok(true)
    }
}
