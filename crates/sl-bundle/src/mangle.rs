//! Portable filename indirection, reconstructed from the Sideloadly `isign.zip` fork
//! (`reconstructed_python/isign_zip.py`, `notes/BUNDLE_NOTES.md` §2a).
//!
//! The recovered client unpacks every archive component (except a small allow-list) to a short
//! token `%%<n><suffix>` on disk and records the original name in a per-directory
//! `.filenames_mangled` map, so long, non-ASCII, case-colliding or Windows-illegal names never
//! reach the filesystem. Symlinks are stored as regular files named `<name>%symlink` whose
//! contents are the (original, unmangled) link target. Output unmangles everything again.
//!
//! Sideport's own extraction keeps original Unicode names (see `files::relative_path`); this
//! module provides the recovered scheme as a standalone, tested utility. The `deb` extractor
//! reuses [`SYMLINK_SUFFIX`]/[`MANGLED_FLAG`] and the escape checks here.
//!
//! Recovered oddity, preserved as documentation: `isign.zip.do_mangle_filenames` prints an
//! undefined global `un` on its non-`--full` branch (`print(un)`), so the `mangle` sub-command
//! raises `NameError` unless `--full` is given. Nothing in the pipeline calls that branch, so the
//! defect never affects preparation. Sideport implements the working `--full` behavior
//! ([`mangle_relative`]) and omits the broken branch.

use crate::{Error, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Symlinks become a regular file `<name>%symlink` holding the link target text.
pub const SYMLINK_SUFFIX: &str = "%symlink";

/// Marks a directory whose child names are mangled; also stores the `short%=long` mapping.
pub const MANGLED_FLAG: &str = ".filenames_mangled";

/// Percent-quoted even though they are ASCII, because they are illegal on Windows or would
/// confuse the mapping syntax (`%`). Matches the recovered `BAD_CHARS`.
const BAD_CHARS: &[u8] = b"%<>:\"\\|?*";

/// Components kept verbatim so `*.app/*.framework/...` globbing and Info.plist lookups still work.
const DO_NOT_MANGLE: [&str; 5] = ["Info.plist", "Frameworks", "PlugIns", "Extensions", "InfoPlist.strings"];

/// Percent-quote one path component the way the recovered `filename_mangle` does: keep printable
/// ASCII that is not a bad character, quote every other byte as `%XX`, and replace a trailing dot
/// (illegal on Windows) with `%2E`.
pub fn quote_component(name: &str) -> String {
    let mut quoted = String::with_capacity(name.len());

    for byte in name.bytes() {
        let keep = (0x20..=0x7f).contains(&byte) && !BAD_CHARS.contains(&byte);

        if keep {
            quoted.push(byte as char);
        } else {
            quoted.push('%');
            quoted.push(hex_digit(byte >> 4));
            quoted.push(hex_digit(byte & 0xf));
        }
    }

    if let Some(stripped) = quoted.strip_suffix('.') {
        return format!("{stripped}%2E");
    }

    quoted
}

/// Reverse [`quote_component`]: decode `%XX` back to bytes and interpret the result as UTF-8,
/// falling back to a lossy decode for inputs that were not produced here.
pub fn unquote(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        let high = (bytes[index] == b'%').then(|| from_hex(bytes.get(index + 1).copied())).flatten();
        let low = high.and(from_hex(bytes.get(index + 2).copied()));

        if let (Some(high), Some(low)) = (high, low) {
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

/// A per-directory `short%=long` table, one for each mangled directory in a tree.
#[derive(Debug, Default)]
struct DirectoryMap {
    /// Insertion order preserves the recovered id assignment (ids increase per directory).
    entries: Vec<(String, String)>,
    next_id: u64,
}

impl DirectoryMap {
    fn short_for(&self, quoted_long: &str) -> Option<&str> {
        self.entries.iter().find(|(_, long)| long == quoted_long).map(|(short, _)| short.as_str())
    }

    fn assign(&mut self, quoted_long: &str, suffix: &str) -> String {
        self.next_id += 1;
        let short = format!("%%{}{suffix}", self.next_id);
        self.entries.push((short.clone(), quoted_long.to_owned()));

        short
    }

    /// Serialize as the recovered `.filenames_mangled` file: `short%=long\n` per entry.
    fn serialize(&self) -> String {
        let mut text = String::new();

        for (short, long) in &self.entries {
            text.push_str(short);
            text.push_str("%=");
            text.push_str(long);
            text.push('\n');
        }

        text
    }
}

/// Builds mangled names for one destination tree, keeping the `.filenames_mangled` maps in memory.
#[derive(Debug, Default)]
pub struct Mangler {
    /// Keyed by the mangled directory path (relative to the tree root; empty key = root).
    directories: BTreeMap<PathBuf, DirectoryMap>,
}

impl Mangler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mangle one relative POSIX path, allocating short tokens for previously unseen components.
    /// Rejects absolute paths and `.`/`..` traversal before any mapping is created.
    pub fn mangle_relative(&mut self, name: &str) -> Result<PathBuf> {
        validate_relative(name)?;

        let mut mangled = PathBuf::new();

        for component in name.split('/') {
            let quoted = quote_component(component);

            if DO_NOT_MANGLE.contains(&component) {
                mangled.push(&quoted);
                continue;
            }

            let suffix = extension_suffix(component);
            let map = self.directories.entry(mangled.clone()).or_default();

            let short = match map.short_for(&quoted) {
                Some(existing) => existing.to_owned(),
                None => map.assign(&quoted, &suffix),
            };

            mangled.push(short);
        }

        Ok(mangled)
    }

    /// The `.filenames_mangled` files this mangler would write, as `(directory, contents)` pairs.
    pub fn maps(&self) -> Vec<(PathBuf, String)> {
        self.directories
            .iter()
            .filter(|(_, map)| !map.entries.is_empty())
            .map(|(directory, map)| (directory.clone(), map.serialize()))
            .collect()
    }
}

/// Reverse a mangled tree, resolving each `%%<n>` token through the in-memory maps.
#[derive(Debug, Default)]
pub struct Unmangler {
    directories: BTreeMap<PathBuf, BTreeMap<String, String>>,
}

impl Unmangler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load one directory's `.filenames_mangled` contents. `directory` is the mangled directory
    /// path relative to the tree root (empty for the root).
    pub fn load(&mut self, directory: &Path, contents: &str) {
        let map = self.directories.entry(directory.to_owned()).or_default();

        for line in contents.lines() {
            if let Some((short, long)) = line.split_once("%=")
                && short.starts_with("%%")
            {
                map.insert(short.to_owned(), long.to_owned());
            }
        }
    }

    /// Restore the original name of a mangled relative path, stripping a trailing `%symlink`.
    pub fn unmangle_relative(&self, mangled: &str) -> String {
        let trimmed = mangled.strip_suffix(SYMLINK_SUFFIX).unwrap_or(mangled);

        let mut mangled_prefix = PathBuf::new();
        let mut original = String::new();

        for component in trimmed.split('/') {
            let quoted_long = if component.starts_with("%%") {
                self.directories
                    .get(&mangled_prefix)
                    .and_then(|map| map.get(component))
                    .map_or(component, String::as_str)
            } else {
                component
            };

            if !original.is_empty() {
                original.push('/');
            }

            original.push_str(&unquote(quoted_long));
            mangled_prefix.push(component);
        }

        original
    }
}

/// The extension a short token keeps, including the leading dot (`Foo.dylib` → `.dylib`). Empty
/// when the component has no extension, so `%%<n>` globs keep matching `*.app`, `*.dylib`, etc.
fn extension_suffix(component: &str) -> String {
    Path::new(component)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!(".{extension}"))
        .unwrap_or_default()
}

/// Reject inputs the mangler must never persist: absolute paths, empty parts, and `.`/`..`.
fn validate_relative(name: &str) -> Result<()> {
    if name.is_empty() || name.starts_with('/') || name.contains(['\\', '\0']) {
        return Err(Error::Path(name.into()));
    }

    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(Error::Path(name.into()));
        }
    }

    Ok(())
}

fn hex_digit(value: u8) -> char {
    char::from_digit(u32::from(value), 16).unwrap_or('0').to_ascii_uppercase()
}

fn from_hex(byte: Option<u8>) -> Option<u8> {
    byte.and_then(|byte| (byte as char).to_digit(16)).map(|value| value as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_bad_characters_and_high_bytes_but_keeps_plain_ascii() {
        assert_eq!(quote_component("Info.plist"), "Info.plist");
        assert_eq!(quote_component("a<b>:c*?.dylib"), "a%3Cb%3E%3Ac%2A%3F.dylib");
        assert_eq!(quote_component("50%.png"), "50%25.png");
        assert_eq!(quote_component("café"), "caf%C3%A9");
    }

    #[test]
    fn escapes_trailing_dot_which_windows_forbids() {
        assert_eq!(quote_component("trailing."), "trailing%2E");
        assert_eq!(quote_component("kept.dylib"), "kept.dylib");
    }

    #[test]
    fn shortens_each_component_and_keeps_its_extension() {
        let mut mangler = Mangler::new();

        let mangled = mangler.mangle_relative("Payload/Foo.app/Foo").expect("mangle");

        assert_eq!(mangled, PathBuf::from("%%1/%%1.app/%%1"));
    }

    #[test]
    fn allow_list_components_survive_verbatim() {
        let mut mangler = Mangler::new();

        let mangled = mangler.mangle_relative("Payload/Foo.app/Frameworks/Bar.dylib").expect("mangle");

        assert_eq!(mangled, PathBuf::from("%%1/%%1.app/Frameworks/%%1.dylib"));
    }

    #[test]
    fn reuses_one_token_for_a_repeated_name_and_distinct_tokens_for_distinct_names() {
        let mut mangler = Mangler::new();

        let first = mangler.mangle_relative("dir/same.dylib").expect("first");
        let again = mangler.mangle_relative("dir/same.dylib").expect("again");
        let other = mangler.mangle_relative("dir/other.dylib").expect("other");

        assert_eq!(first, again);
        assert_eq!(first, PathBuf::from("%%1/%%1.dylib"));
        assert_eq!(other, PathBuf::from("%%1/%%2.dylib"));
    }

    #[test]
    fn unicode_normalization_variants_do_not_collide_and_round_trip() {
        let composed = "café.dylib"; // U+00E9
        let decomposed = "cafe\u{0301}.dylib"; // 'e' + combining acute

        let mut mangler = Mangler::new();
        let first = mangler.mangle_relative(&format!("dir/{composed}")).expect("composed");
        let second = mangler.mangle_relative(&format!("dir/{decomposed}")).expect("decomposed");

        assert_ne!(first, second, "distinct Unicode names get distinct tokens");

        let mut unmangler = Unmangler::new();
        for (directory, contents) in mangler.maps() {
            unmangler.load(&directory, &contents);
        }

        assert_eq!(unmangler.unmangle_relative(first.to_str().expect("utf-8")), format!("dir/{composed}"));
        assert_eq!(unmangler.unmangle_relative(second.to_str().expect("utf-8")), format!("dir/{decomposed}"));
    }

    #[test]
    fn long_names_and_windows_reserved_names_become_short_tokens() {
        let long = "a".repeat(300);
        let mut mangler = Mangler::new();

        let long_mangled = mangler.mangle_relative(&format!("dir/{long}.dylib")).expect("long");
        let reserved = mangler.mangle_relative("dir/CON.dylib").expect("reserved");

        assert_eq!(long_mangled, PathBuf::from("%%1/%%1.dylib"));
        assert_eq!(reserved, PathBuf::from("%%1/%%2.dylib"));
        assert!(long_mangled.to_str().expect("utf-8").len() < long.len());
    }

    #[test]
    fn rejects_absolute_paths_and_traversal() {
        let mut mangler = Mangler::new();

        assert!(mangler.mangle_relative("/etc/passwd").is_err());
        assert!(mangler.mangle_relative("a/../../b").is_err());
        assert!(mangler.mangle_relative("a\\b").is_err());
    }

    #[test]
    fn unmangle_strips_symlink_suffix_and_falls_back_when_unmapped() {
        let mut mangler = Mangler::new();
        let mangled = mangler.mangle_relative("dir/link.dylib").expect("mangle");

        let mut unmangler = Unmangler::new();
        for (directory, contents) in mangler.maps() {
            unmangler.load(&directory, &contents);
        }

        let placeholder = format!("{}{SYMLINK_SUFFIX}", mangled.to_str().expect("utf-8"));

        assert_eq!(unmangler.unmangle_relative(&placeholder), "dir/link.dylib");

        // A component that is not a `%%` token is returned percent-decoded verbatim.
        assert_eq!(unmangler.unmangle_relative("plain%2Ename"), "plain.name");
    }

    #[test]
    fn serialized_map_uses_the_recovered_line_syntax() {
        let mut mangler = Mangler::new();
        mangler.mangle_relative("dir/one.dylib").expect("one");
        mangler.mangle_relative("dir/two.dylib").expect("two");

        let maps = mangler.maps();
        let (_, contents) = maps.iter().find(|(directory, _)| directory == Path::new("%%1")).expect("dir map");

        assert_eq!(contents, "%%1.dylib%=one.dylib\n%%2.dylib%=two.dylib\n");
    }
}
