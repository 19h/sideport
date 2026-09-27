use crate::error::io;
use crate::files;
use crate::{Bundle, BundleArchive, Control, Error, Phase, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone)]
pub struct Injection {
    pub source: PathBuf,
    pub name: Option<String>,
}

#[derive(Debug, Default)]
pub struct InjectionReport {
    pub copied: Vec<PathBuf>,
    pub load_paths: Vec<String>,
    pub patched_binaries: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InjectionKind {
    Dylib,
    Framework,
    Resource,
}

#[derive(Debug)]
struct PreparedInjection<'a> {
    source: &'a Path,
    name: String,
    kind: InjectionKind,
    executable: Option<String>,
}

impl BundleArchive {
    /// Install already prepared local libraries/resources. Remote and deb-package
    /// preparation can feed the same source/name interface.
    pub fn inject(&mut self, inputs: &[Injection], control: Control<'_>) -> Result<InjectionReport> {
        control.check()?;

        let mut prepared = Vec::with_capacity(inputs.len());
        let mut names = BTreeSet::new();

        for input in inputs {
            let name = input
                .name
                .as_deref()
                .or_else(|| input.source.file_name().and_then(|name| name.to_str()))
                .ok_or_else(|| Error::Path(input.source.display().to_string()))?;
            let relative = files::relative_path(name)?;

            if relative.components().count() != 1 {
                return Err(Error::Path(format!("injection name must be a filename: {name}")));
            }

            let folded: String = name.nfc().flat_map(char::to_lowercase).collect();

            if !names.insert(folded) {
                return Err(Error::Bundle(format!("duplicate injection name: {name}")));
            }

            let metadata = io(&input.source, fs::symlink_metadata(&input.source))?;

            if !(metadata.is_file() || metadata.is_dir()) {
                return Err(Error::Path(format!(
                    "injection source must be a file or directory: {}",
                    input.source.display()
                )));
            }

            let kind = match input.source.extension().and_then(|extension| extension.to_str()) {
                Some("dylib") if metadata.is_file() => InjectionKind::Dylib,
                Some("framework") if metadata.is_dir() && name.ends_with(".framework") => InjectionKind::Framework,
                Some("framework") => {
                    return Err(Error::Bundle("framework injections require a .framework directory name".into()));
                }
                _ => InjectionKind::Resource,
            };

            let executable = if kind == InjectionKind::Framework {
                let path = input.source.join("Info.plist");

                if io(&path, path.try_exists())? {
                    Some(files::archive_name(&Bundle::open(&input.source)?.executable_relative()?)?)
                } else {
                    Some(
                        input
                            .source
                            .file_stem()
                            .and_then(|name| name.to_str())
                            .ok_or_else(|| Error::Path(input.source.display().to_string()))?
                            .to_owned(),
                    )
                }
            } else {
                None
            };

            prepared.push(PreparedInjection { source: &input.source, name: name.to_owned(), kind, executable });
        }

        let root = self.bundle_path();
        let frameworks = files::write_inside(&root, Path::new("Frameworks"))?;

        if !io(&frameworks, frameworks.try_exists())? {
            io(&frameworks, fs::create_dir(&frameworks))?;
        }

        let mut report = InjectionReport::default();
        let mut replacements = BTreeMap::new();
        let mut binaries = Vec::new();

        for input in &prepared {
            let load_path = match input.kind {
                InjectionKind::Dylib => Some(format!("@executable_path/Frameworks/{}", input.name)),
                InjectionKind::Framework => Some(format!(
                    "@executable_path/Frameworks/{}/{}",
                    input.name,
                    input.executable.as_deref().ok_or_else(|| Error::Bundle("framework has no executable".into()))?
                )),
                InjectionKind::Resource => None,
            };

            if let Some(load_path) = load_path {
                replacements.insert(sl_macho::library_key(&load_path), load_path.clone());
                report.load_paths.push(load_path);
            }
        }

        let substrate = "CydiaSubstrate.framework/CydiaSubstrate";

        if names.contains("cydiasubstrate.framework")
            || io(&frameworks.join(substrate), frameworks.join(substrate).try_exists())?
        {
            let path = format!("@executable_path/Frameworks/{substrate}");
            replacements.insert(substrate.into(), path.clone());
            replacements.insert("libsubstrate.dylib".into(), path);
        }

        for (index, input) in prepared.iter().enumerate() {
            control.check()?;

            let relative = if input.kind == InjectionKind::Resource {
                PathBuf::from(&input.name)
            } else {
                Path::new("Frameworks").join(&input.name)
            };
            let destination = files::write_inside(&root, &relative)?;
            files::remove(&destination)?;

            if io(input.source, fs::symlink_metadata(input.source))?.is_dir() {
                files::copy_tree(input.source, &destination, control)?;
                let archive_relative = self.bundle_relative().join(&relative);
                self.forget_directory_modes(&archive_relative);
                self.record_directory_modes(input.source, &archive_relative)?;
            } else {
                files::copy_file(input.source, &destination, control)?;
            }

            if input.kind != InjectionKind::Resource {
                let binary = if let Some(executable) = &input.executable {
                    files::read_inside(&destination, &files::relative_path(executable)?)?
                } else {
                    destination
                };

                binaries.push(binary);
            }

            report.copied.push(relative);
            control.report(Phase::Patch, (index + 1) as u64, prepared.len() as u64);
        }

        for path in binaries {
            control.check()?;
            patch_binary(&path, &replacements, &[])?;
            report.patched_binaries.push(path);
        }

        if !report.load_paths.is_empty() {
            let unity = root.join("UnityFramework");
            let target = if io(&unity, unity.try_exists())? {
                files::read_inside(&root, Path::new("UnityFramework"))?
            } else {
                self.bundle()?.executable_path()?
            };

            patch_binary(&target, &replacements, &report.load_paths)?;
            report.patched_binaries.push(target);
        }

        Ok(report)
    }
}

fn patch_binary(path: &Path, replacements: &BTreeMap<String, String>, additions: &[String]) -> Result<()> {
    let bytes = io(path, fs::read(path))?;
    let binary = sl_macho::Binary::parse(&bytes)?;

    let slices = binary
        .slices
        .iter()
        .map(|slice| slice.image.edit_libraries(replacements, additions))
        .collect::<sl_macho::Result<Vec<_>>>()?;

    let edited = binary.rebuild(&slices, 14)?;
    files::atomic_write(path, &edited)
}
