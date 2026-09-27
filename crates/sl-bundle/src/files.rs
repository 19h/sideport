use crate::error::io;
use crate::{Control, Error, Result};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub(crate) const BUFFER_SIZE: usize = 128 * 1024;

/// Archive paths are POSIX-relative and retain their original Unicode spelling.
pub(crate) fn relative_path(name: &str) -> Result<PathBuf> {
    let name = name.trim_end_matches('/');

    if name.is_empty()
        || name.starts_with('/')
        || name.contains(['\\', '\0'])
        || name.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        || name.as_bytes().get(1) == Some(&b':')
    {
        return Err(Error::Path(name.into()));
    }

    let path = PathBuf::from(name);

    if path.components().any(|part| !matches!(part, Component::Normal(_))) {
        return Err(Error::Path(name.into()));
    }

    Ok(path)
}

pub(crate) fn archive_name(path: &Path) -> Result<String> {
    path.components()
        .map(|part| match part {
            Component::Normal(name) => name.to_str().ok_or_else(|| Error::Path(path.display().to_string())),
            _ => Err(Error::Path(path.display().to_string())),
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

pub(crate) fn validate_symlink(relative: &Path, target: &str) -> Result<()> {
    if target.is_empty()
        || target.starts_with('/')
        || target.contains(['\\', '\0'])
        || target.as_bytes().get(1) == Some(&b':')
    {
        return Err(Error::Path(format!("{} -> {target}", relative.display())));
    }

    let mut depth = relative.parent().map_or(0, |parent| parent.components().count());

    for component in target.split('/') {
        match component {
            "" | "." => {}
            ".." if depth != 0 => depth -= 1,
            ".." => return Err(Error::Path(format!("{} -> {target}", relative.display()))),
            _ => depth += 1,
        }
    }

    Ok(())
}

pub(crate) fn read_inside(root: &Path, relative: &Path) -> Result<PathBuf> {
    let canonical_root = io(root, fs::canonicalize(root))?;
    let path = root.join(relative);
    let canonical_path = io(&path, fs::canonicalize(&path))?;

    if !canonical_path.starts_with(&canonical_root) {
        return Err(Error::Path(format!("{} resolves outside {}", path.display(), root.display())));
    }

    Ok(canonical_path)
}

/// Mutations never traverse a symlink directory. A final symlink can be removed
/// or replaced without affecting its target.
pub(crate) fn write_inside(root: &Path, relative: &Path) -> Result<PathBuf> {
    let name = archive_name(relative)?;
    relative_path(&name)?;

    let mut current = root.to_owned();

    for part in relative.parent().into_iter().flat_map(Path::components) {
        current.push(part.as_os_str());

        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(Error::Path(format!("{} is not a real directory", current.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                io(&current, fs::create_dir(&current))?;
            }
            Err(error) => return Err(Error::Io { path: current, source: error }),
        }
    }

    Ok(root.join(relative))
}

pub(crate) fn remove(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => io(path, fs::remove_dir_all(path)),
        Ok(_) => io(path, fs::remove_file(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io { path: path.to_owned(), source }),
    }
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| Error::Path(path.display().to_string()))?;
    let existing = fs::symlink_metadata(path).ok();

    if existing.as_ref().is_some_and(|metadata| !metadata.is_file()) {
        return Err(Error::Path(format!("{} is not a regular file", path.display())));
    }

    let mut temporary = io(parent, tempfile::NamedTempFile::new_in(parent))?;
    io(path, temporary.write_all(bytes))?;

    if let Some(metadata) = existing {
        io(path, temporary.as_file().set_permissions(metadata.permissions()))?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            io(path, temporary.as_file().set_permissions(fs::Permissions::from_mode(0o644)))?;
        }
    }

    io(path, temporary.as_file().sync_all())?;
    temporary.persist(path).map_err(|error| Error::Io { path: path.to_owned(), source: error.error })?;

    Ok(())
}

pub(crate) fn copy_file(source: &Path, destination: &Path, control: Control<'_>) -> Result<u64> {
    let mut input = io(source, File::open(source))?;
    let mut output = io(destination, fs::OpenOptions::new().write(true).create_new(true).open(destination))?;
    let metadata = io(source, input.metadata())?;
    let mut buffer = vec![0; BUFFER_SIZE];
    let mut copied = 0;

    loop {
        control.check()?;

        let size = io(source, input.read(&mut buffer))?;

        if size == 0 {
            break;
        }

        io(destination, output.write_all(&buffer[..size]))?;
        copied += size as u64;
    }

    if copied != metadata.len() {
        return Err(Error::Archive(format!("{} changed while being copied", source.display())));
    }

    io(destination, output.set_permissions(metadata.permissions()))?;

    Ok(copied)
}

pub(crate) fn copy_tree(source: &Path, destination: &Path, control: Control<'_>) -> Result<()> {
    let metadata = io(source, fs::symlink_metadata(source))?;

    if !metadata.is_dir() {
        return Err(Error::Path(format!("{} is not a directory", source.display())));
    }

    io(destination, fs::create_dir_all(destination))?;

    let mut links = Vec::new();

    for entry in walkdir::WalkDir::new(source).follow_links(false).min_depth(1) {
        control.check()?;

        let entry = entry.map_err(|error| Error::Archive(error.to_string()))?;
        let relative = entry.path().strip_prefix(source).map_err(|error| Error::Path(error.to_string()))?;
        let name = archive_name(relative)?;
        let relative = relative_path(&name)?;
        let target = destination.join(&relative);

        if entry.file_type().is_symlink() {
            let link = io(entry.path(), fs::read_link(entry.path()))?;
            let text = link.to_str().ok_or_else(|| Error::Path(link.display().to_string()))?;
            validate_symlink(&relative, text)?;

            links.push((target, link));
        } else if entry.file_type().is_dir() {
            io(&target, fs::create_dir(&target))?;
        } else if entry.file_type().is_file() {
            copy_file(entry.path(), &target, control)?;
        } else {
            return Err(Error::Path(format!("unsupported file type: {}", entry.path().display())));
        }
    }

    for (path, target) in links {
        create_symlink(&target, &path)?;
    }

    Ok(())
}

#[cfg(unix)]
pub(crate) fn create_symlink(target: &Path, path: &Path) -> Result<()> {
    io(path, std::os::unix::fs::symlink(target, path))
}

#[cfg(windows)]
pub(crate) fn create_symlink(target: &Path, path: &Path) -> Result<()> {
    if path.parent().is_some_and(|parent| parent.join(target).is_dir()) {
        io(path, std::os::windows::fs::symlink_dir(target, path))
    } else {
        io(path, std::os::windows::fs::symlink_file(target, path))
    }
}
