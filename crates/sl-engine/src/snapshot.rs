//! Inputs that change while a job reads them (ENGINE.md A1, HANDOVER H5).
//!
//! A job records the identity of every local input when it starts: the source (each entry of a
//! directory source), the entitlements file, injected items and replacement files. Before it
//! commits output or uploads to a device it records them again; any difference fails the job
//! instead of producing output built from a mix of old and new bytes.
//!
//! The identity is file metadata, not content: kind, device, inode, length, modification and
//! status-change times, and a symlink's target. A write changes the modification or status-change
//! time even when the length stays the same, and the status-change time cannot be set back by
//! ordinary programs. Reading every byte twice would double the job's I/O.

use crate::error::{EngineError, Result};
use crate::types::JobSpec;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Directory entries recorded per input before the job refuses to track it.
const MAX_ENTRIES: usize = 250_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    File,
    Directory,
    Symlink(PathBuf),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    kind: Kind,
    device: u64,
    inode: u64,
    len: u64,
    modified: Option<SystemTime>,
    changed: Option<i128>,
}

impl Identity {
    fn read(path: &Path) -> Option<Self> {
        let metadata = fs::symlink_metadata(path).ok()?;
        let file_type = metadata.file_type();

        let kind = if file_type.is_symlink() {
            Kind::Symlink(fs::read_link(path).unwrap_or_default())
        } else if file_type.is_dir() {
            Kind::Directory
        } else if file_type.is_file() {
            Kind::File
        } else {
            Kind::Other
        };

        #[cfg(unix)]
        let (device, inode, changed) = {
            use std::os::unix::fs::MetadataExt;
            let changed = i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec());

            (metadata.dev(), metadata.ino(), Some(changed))
        };
        #[cfg(not(unix))]
        let (device, inode, changed) = (0, 0, None);

        Some(Self { kind, device, inode, len: metadata.len(), modified: metadata.modified().ok(), changed })
    }
}

/// The recorded identities of a job's local inputs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InputSnapshot {
    entries: Vec<(PathBuf, Option<Identity>)>,
}

impl InputSnapshot {
    /// Record every local input of `spec`. Missing inputs are recorded as missing; the job
    /// reports them when it opens them.
    pub(crate) fn capture(spec: &JobSpec) -> Result<Self> {
        let options = &spec.options;

        let injected = options.injections.iter().map(|injection| injection.source.as_path());
        let replaced = options.replacements.iter().filter_map(|replacement| replacement.source.as_deref());
        let roots = std::iter::once(spec.source.as_path())
            .chain(options.entitlements.as_deref())
            .chain(injected)
            .chain(replaced);

        let mut snapshot = Self::default();

        for root in roots {
            snapshot.record(root)?;
        }

        Ok(snapshot)
    }

    fn record(&mut self, root: &Path) -> Result<()> {
        let identity = Identity::read(root);
        let is_directory = identity.as_ref().is_some_and(|identity| identity.kind == Kind::Directory);

        self.entries.push((root.to_owned(), identity));

        if !is_directory {
            return Ok(());
        }

        let mut recorded = 0;

        for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false).sort_by_file_name() {
            let entry = entry.map_err(|error| EngineError::Storage(format!("cannot read the input: {error}")))?;

            recorded += 1;

            if recorded > MAX_ENTRIES {
                return Err(EngineError::InvalidApp(format!("{} has too many entries", root.display())));
            }

            self.entries.push((entry.path().to_owned(), Identity::read(entry.path())));
        }

        Ok(())
    }

    /// Fail when any input differs from its recorded identity. The message names a changed file
    /// rather than the directory containing it when there is one.
    pub(crate) fn verify(&self, spec: &JobSpec) -> Result<()> {
        let current = Self::capture(spec)?;

        if current == *self {
            return Ok(());
        }

        let recorded: BTreeMap<_, _> = self.entries.iter().map(|(path, identity)| (path, identity)).collect();
        let now: BTreeMap<_, _> = current.entries.iter().map(|(path, identity)| (path, identity)).collect();

        let changed: Vec<&PathBuf> =
            recorded.keys().chain(now.keys()).copied().filter(|path| recorded.get(path) != now.get(path)).collect();

        let is_directory = |path: &&PathBuf| {
            [recorded.get(path), now.get(path)]
                .into_iter()
                .flatten()
                .any(|identity| identity.as_ref().is_some_and(|identity| identity.kind == Kind::Directory))
        };

        let named = changed
            .iter()
            .find(|path| !is_directory(path))
            .or(changed.first())
            .map_or_else(|| spec.source.clone(), |path| (*path).clone());

        Err(EngineError::InvalidApp(format!(
            "{} changed while the job was reading it; start the job again",
            named.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AppOptions, FileReplacement, LibraryInjection, SigningMode, Target};
    use std::io::Write;

    fn spec(source: &Path, injected: &Path, replacement: &Path) -> JobSpec {
        let options = AppOptions {
            injections: vec![LibraryInjection { source: injected.into(), name: None }],
            replacements: vec![FileReplacement { target: "Resources/a".into(), source: Some(replacement.into()) }],
            ..AppOptions::default()
        };

        JobSpec {
            source: source.into(),
            target: Target::ExportIpa { path: None },
            signing: SigningMode::AdHoc,
            options,
        }
    }

    #[test]
    fn same_size_rewrites_additions_removals_and_symlink_retargets_are_detected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let app = directory.path().join("App.app");
        let injected = directory.path().join("Tweak.dylib");
        let replacement = directory.path().join("replacement.txt");

        fs::create_dir_all(app.join("Frameworks")).expect("app");
        fs::write(app.join("App"), b"executable").expect("executable");
        fs::write(&injected, b"dylib").expect("dylib");
        fs::write(&replacement, b"replacement").expect("replacement");

        let spec = spec(&app, &injected, &replacement);
        let recorded = InputSnapshot::capture(&spec).expect("capture");
        recorded.verify(&spec).expect("unchanged inputs");

        let changed = |recorded: &InputSnapshot| match recorded.verify(&spec) {
            Err(EngineError::InvalidApp(message)) => message,
            other => panic!("expected a change, got {other:?}"),
        };

        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut file = fs::OpenOptions::new().write(true).open(app.join("App")).expect("open");
        file.write_all(b"EXECUTABLE").expect("same-size rewrite");
        drop(file);
        assert!(changed(&recorded).contains("App.app/App"));

        let recorded = InputSnapshot::capture(&spec).expect("capture");
        fs::write(app.join("Frameworks/New.dylib"), b"new").expect("addition");
        assert!(changed(&recorded).contains("Frameworks/New.dylib"));

        let recorded = InputSnapshot::capture(&spec).expect("capture");
        fs::remove_file(&injected).expect("removal");
        assert!(changed(&recorded).contains("Tweak.dylib"));
        fs::write(&injected, b"dylib").expect("restore");

        #[cfg(unix)]
        {
            let link = app.join("Link");
            std::os::unix::fs::symlink("App", &link).expect("symlink");
            let recorded = InputSnapshot::capture(&spec).expect("capture");

            fs::remove_file(&link).expect("unlink");
            std::os::unix::fs::symlink("Frameworks", &link).expect("retarget");
            assert!(changed(&recorded).contains("Link"));
        }
    }
}
