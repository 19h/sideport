//! _CodeSignature/CodeResources seal generation. CONTRACT.

use crate::{Error, Result};
use plist::{Dictionary, Value};
use rayon::prelude::*;
use regex::Regex;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::{
    io::{Cursor, Read},
    path::{Component, Path, PathBuf},
};

#[derive(Debug)]
struct Resource {
    path: PathBuf,
    relative_path: String,
    is_symlink: bool,
}

#[derive(Debug)]
struct SealedResource {
    relative_path: String,
    legacy: Option<Value>,
    modern: Option<Value>,
}

#[derive(Debug)]
struct ResourceDigests {
    sha1: Vec<u8>,
    sha256: Vec<u8>,
}

#[derive(Debug)]
struct Rule {
    pattern: Regex,
    weight: f64,
    omit: bool,
    optional: bool,
    exclusion: bool,
}

/// Walk bundle_dir and build the CodeResources XML plist (files, files2, rules, rules2) using
/// the iOS rule template. main_executable (relative path) is excluded from the seal. An existing
/// top-level _CodeSignature directory is ignored. Hashing runs in parallel.
pub fn build_seal(bundle_dir: &Path, main_executable: Option<&str>) -> Result<Vec<u8>> {
    build_seal_cancellable(bundle_dir, main_executable, &|| false)
}

/// [`build_seal`], polling `is_cancelled` before each resource and after every 128 KiB read;
/// `true` stops with [`Error::Cancelled`].
pub fn build_seal_cancellable(
    bundle_dir: &Path,
    main_executable: Option<&str>,
    is_cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<Vec<u8>> {
    if !std::fs::symlink_metadata(bundle_dir).map_err(|error| io_error(bundle_dir, error))?.is_dir() {
        return Err(Error::Other("resource root must be a real directory".into()));
    }

    if main_executable
        .is_some_and(|path| Path::new(path).components().any(|part| !matches!(part, Component::Normal(_))))
    {
        return Err(Error::Other("main executable must be a relative bundle path".into()));
    }

    let mut template = Value::from_reader(Cursor::new(include_bytes!("../resources/code-resources.xml")))?
        .into_dictionary()
        .ok_or_else(|| Error::Other("resource template is not a dictionary".into()))?;

    let legacy_rules = rules(&template, "rules")?;
    let modern_rules = rules(&template, "rules2")?;

    let resources = collect_resources(bundle_dir, main_executable)?;
    let entries = resources
        .par_iter()
        .map(|resource| seal_resource(resource, &legacy_rules, &modern_rules, is_cancelled))
        .collect::<Result<Vec<_>>>()?;

    let mut files = Dictionary::new();
    let mut files2 = Dictionary::new();

    for entry in entries {
        if let Some(value) = entry.legacy {
            files.insert(entry.relative_path.clone(), value);
        }

        if let Some(value) = entry.modern {
            files2.insert(entry.relative_path, value);
        }
    }

    template.insert("files".into(), Value::Dictionary(files));
    template.insert("files2".into(), Value::Dictionary(files2));

    crate::entitlements::to_xml(&template)
}

fn collect_resources(bundle_dir: &Path, main_executable: Option<&str>) -> Result<Vec<Resource>> {
    let entries = walkdir::WalkDir::new(bundle_dir)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| entry.depth() != 1 || entry.file_name() != "_CodeSignature");

    let mut resources = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|error| Error::Other(format!("resource traversal: {error}")))?;

        if entry.depth() == 0 || entry.file_type().is_dir() || entry.file_name() == ".filenames_mangled" {
            continue;
        }

        let path = entry.path();
        let relative_path = path
            .strip_prefix(bundle_dir)
            .map_err(|error| Error::Other(error.to_string()))?
            .to_str()
            .ok_or_else(|| Error::Other("non-UTF-8 resource path".into()))?
            .replace(std::path::MAIN_SEPARATOR, "/");

        if main_executable == Some(relative_path.as_str()) {
            continue;
        }

        if !entry.file_type().is_file() && !entry.file_type().is_symlink() {
            return Err(Error::Other(format!("unsupported resource file type: {relative_path}")));
        }

        resources.push(Resource {
            path: path.to_path_buf(),
            relative_path,
            is_symlink: entry.file_type().is_symlink(),
        });
    }

    Ok(resources)
}

fn seal_resource(
    resource: &Resource,
    legacy_rules: &[Rule],
    modern_rules: &[Rule],
    is_cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<SealedResource> {
    if is_cancelled() {
        return Err(Error::Cancelled);
    }

    let legacy_rule = matching(legacy_rules, &resource.relative_path);
    let modern_rule = matching(modern_rules, &resource.relative_path);

    let include_legacy = !resource.is_symlink && !legacy_rule.is_some_and(|rule| rule.exclusion);
    let include_modern = !modern_rule.is_some_and(|rule| rule.exclusion || rule.omit);

    let optional_legacy = legacy_rule.is_some_and(|rule| rule.optional);
    let optional_modern = modern_rule.is_some_and(|rule| rule.optional);

    let mut entry = SealedResource { relative_path: resource.relative_path.clone(), legacy: None, modern: None };

    if resource.is_symlink {
        let value = symlink_entry(&resource.path, optional_modern)?;
        entry.modern = include_modern.then_some(value);

        return Ok(entry);
    }

    if !include_legacy && !include_modern {
        return Ok(entry);
    }

    let digests = hash_file(&resource.path, is_cancelled)?;

    entry.legacy = include_legacy.then(|| legacy_entry(digests.sha1.clone(), optional_legacy));
    entry.modern = include_modern.then(|| modern_entry(digests, optional_modern));

    Ok(entry)
}

fn symlink_entry(path: &Path, optional: bool) -> Result<Value> {
    let target = std::fs::read_link(path).map_err(|error| io_error(path, error))?;
    let target = target.to_str().ok_or_else(|| Error::Other("non-UTF-8 symlink target".into()))?;

    let mut value = Dictionary::new();
    value.insert("symlink".into(), target.into());

    if optional {
        value.insert("optional".into(), true.into());
    }

    Ok(Value::Dictionary(value))
}

fn legacy_entry(sha1: Vec<u8>, optional: bool) -> Value {
    if !optional {
        return Value::Data(sha1);
    }

    let mut value = Dictionary::new();
    value.insert("hash".into(), Value::Data(sha1));
    value.insert("optional".into(), true.into());

    Value::Dictionary(value)
}

fn modern_entry(digests: ResourceDigests, optional: bool) -> Value {
    let mut value = Dictionary::new();
    value.insert("hash".into(), Value::Data(digests.sha1));
    value.insert("hash2".into(), Value::Data(digests.sha256));

    if optional {
        value.insert("optional".into(), true.into());
    }

    Value::Dictionary(value)
}

fn rules(template: &Dictionary, key: &str) -> Result<Vec<Rule>> {
    let entries = template
        .get(key)
        .and_then(Value::as_dictionary)
        .ok_or_else(|| Error::Other(format!("missing resource {key}")))?;

    entries
        .iter()
        .map(|(pattern, properties)| {
            let properties = properties.as_dictionary();
            let flag = |key| properties.and_then(|fields| fields.get(key)).and_then(Value::as_boolean).unwrap_or(false);
            let weight = properties.and_then(|fields| fields.get("weight")).and_then(Value::as_real).unwrap_or(1.0);

            Ok(Rule {
                pattern: Regex::new(pattern).map_err(|error| Error::Other(error.to_string()))?,
                weight,
                omit: flag("omit"),
                optional: flag("optional"),
                exclusion: flag("exclusion"),
            })
        })
        .collect()
}

fn matching<'a>(rules: &'a [Rule], path: &str) -> Option<&'a Rule> {
    let mut best: Option<&Rule> = None;

    for rule in rules {
        if !rule.pattern.find(path).is_some_and(|matched| matched.start() == 0) {
            continue;
        }

        if rule.exclusion {
            return Some(rule);
        }

        if best.is_none_or(|current| rule.weight >= current.weight) {
            best = Some(rule);
        }
    }

    best
}

fn hash_file(path: &Path, is_cancelled: &(dyn Fn() -> bool + Sync)) -> Result<ResourceDigests> {
    let mut file = std::fs::File::open(path).map_err(|error| io_error(path, error))?;
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();

    // Each worker reads a file once and uses 128 KiB, regardless of resource size.
    let mut buffer = vec![0; 128 * 1024];

    loop {
        if is_cancelled() {
            return Err(Error::Cancelled);
        }

        let size = file.read(&mut buffer).map_err(|error| io_error(path, error))?;

        if size == 0 {
            break;
        }

        sha1.update(&buffer[..size]);
        sha256.update(&buffer[..size]);
    }

    Ok(ResourceDigests { sha1: sha1.finalize().to_vec(), sha256: sha256.finalize().to_vec() })
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io { path: path.display().to_string(), source }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_dual_seal_and_rule_precedence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        for directory in ["en.lproj", "Base.lproj", "_CodeSignature", "Frameworks/Test.framework"] {
            std::fs::create_dir_all(root.join(directory)).expect("directory");
        }

        for file in [
            "main",
            "Info.plist",
            "PkgInfo",
            ".DS_Store",
            "data",
            "en.lproj/x",
            "en.lproj/locversion.plist",
            "Base.lproj/x",
            "_CodeSignature/old",
            "Frameworks/Test.framework/Test",
        ] {
            std::fs::write(root.join(file), b"abc").expect("file");
        }

        let seal = build_seal(root, Some("main")).expect("seal");

        assert_eq!(seal, build_seal(root, Some("main")).expect("seal"));

        let value = Value::from_reader(Cursor::new(seal)).expect("plist");
        let dict = value.as_dictionary().expect("dict");
        let legacy = dict["files"].as_dictionary().expect("files");
        let modern = dict["files2"].as_dictionary().expect("files2");

        assert!(!legacy.contains_key("main"));
        assert!(!modern.contains_key("_CodeSignature/old"));

        assert!(legacy.contains_key("Info.plist"));
        assert!(!modern.contains_key("Info.plist"));

        assert!(legacy.contains_key("en.lproj/locversion.plist"));
        assert!(!modern.contains_key("en.lproj/locversion.plist"));
        assert!(!modern.contains_key(".DS_Store"));

        let data = modern["data"].as_dictionary().expect("resource");

        assert_eq!(data["hash"].as_data().expect("SHA1"), &Sha1::digest(b"abc")[..]);
        assert_eq!(data["hash2"].as_data().expect("SHA256"), &Sha256::digest(b"abc")[..]);

        assert_eq!(modern["en.lproj/x"].as_dictionary().expect("dict")["optional"].as_boolean(), Some(true));
        assert!(!modern["Base.lproj/x"].as_dictionary().expect("dict").contains_key("optional"));
        assert!(modern.contains_key("Frameworks/Test.framework/Test"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_sealed_without_following_targets() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink("missing", tmp.path().join("link")).expect("symlink");

        let seal = build_seal(tmp.path(), None).expect("seal");
        let value = Value::from_reader(Cursor::new(seal)).expect("plist");
        let dict = value.as_dictionary().expect("dict");

        assert!(!dict["files"].as_dictionary().expect("files").contains_key("link"));

        let link = dict["files2"].as_dictionary().expect("files2")["link"].as_dictionary().expect("link");

        assert_eq!(link["symlink"].as_string(), Some("missing"));
    }

    #[test]
    fn cancellation_stops_sealing_within_one_read_and_before_each_resource() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        let tmp = tempfile::tempdir().expect("tempdir");
        let large = vec![0x5a; 64 * 1024 * 1024];
        std::fs::write(tmp.path().join("large.bin"), &large).expect("large resource");

        for index in 0..32 {
            std::fs::write(tmp.path().join(format!("small-{index}")), b"small").expect("small resource");
        }

        // Deterministic: the callback answers true from its third poll on.
        let polls = AtomicUsize::new(0);
        let cancelled = || polls.fetch_add(1, Ordering::SeqCst) >= 2;
        assert!(matches!(build_seal_cancellable(tmp.path(), None, &cancelled), Err(Error::Cancelled)));

        let polled = polls.load(Ordering::SeqCst);
        assert!(polled < 64, "sealing stopped early after {polled} polls (512 reads and 33 resources otherwise)");

        // Latency: cancel while the 64 MiB resource is being hashed.
        let flag = std::sync::atomic::AtomicBool::new(false);
        let started = Instant::now();

        let (result, requested) = std::thread::scope(|scope| {
            let sealing = scope.spawn(|| build_seal_cancellable(tmp.path(), None, &|| flag.load(Ordering::SeqCst)));

            std::thread::sleep(Duration::from_millis(20));
            let requested = Instant::now();
            flag.store(true, Ordering::SeqCst);

            (sealing.join().expect("sealing thread"), requested)
        });

        let latency = requested.elapsed();
        assert!(matches!(result, Err(Error::Cancelled)), "{:?}", result.map(|seal| seal.len()));
        assert!(latency < Duration::from_millis(250), "cancellation took {latency:?}");
        assert!(started.elapsed() < Duration::from_secs(5));

        assert!(build_seal_cancellable(tmp.path(), None, &|| false).is_ok(), "an unset flag seals normally");
    }
}
