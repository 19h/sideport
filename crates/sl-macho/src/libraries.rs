use crate::{
    Endian, Error, LC_ID_DYLIB, LC_LOAD_DYLIB, LC_LOAD_WEAK_DYLIB, LC_RPATH, MachO, Result, align_up, malformed, name,
    to_u32,
};
use std::collections::{BTreeMap, BTreeSet};

const FRAMEWORK_RPATH: &str = "@executable_path/Frameworks";

/// Normalize ordinary dylibs and versioned framework paths to their dependency key.
pub fn library_key(path: &str) -> String {
    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    let last = parts.last().copied().unwrap_or("");

    if let Some(framework) = parts.iter().find(|part| part.ends_with(".framework")) {
        format!("{framework}/{last}")
    } else {
        last.to_owned()
    }
}

impl MachO<'_> {
    /// Rewrite library IDs/dependencies, add supplied loads and the framework search
    /// path in one header edit. Existing unknown commands and version fields survive.
    pub fn edit_libraries(&self, replacements: &BTreeMap<String, String>, additions: &[String]) -> Result<Vec<u8>> {
        let replacements: BTreeMap<_, _> =
            replacements.iter().map(|(old, new)| (library_key(old), new.as_str())).collect();
        let mut commands = Vec::with_capacity(self.commands.len() + additions.len() + 1);
        let mut existing = BTreeSet::new();
        let mut has_rpath = false;

        for command in &self.commands {
            let mut bytes = command.bytes.to_vec();

            if matches!(
                command.kind,
                LC_ID_DYLIB | LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | 0x8000_001f | 0x8000_0023 | 0x20
            ) {
                let path_offset = self.endian.u32(&bytes, 8)? as usize;
                let old_path = name(&bytes[path_offset..])?;
                let new_path = replacements.get(&library_key(&old_path)).copied().unwrap_or(&old_path);

                if new_path != old_path {
                    bytes = dylib_command(
                        self.endian,
                        self.is_64,
                        command.kind,
                        new_path,
                        self.endian.u32(command.bytes, 12)?,
                        self.endian.u32(command.bytes, 16)?,
                        self.endian.u32(command.bytes, 20)?,
                    )?;
                }

                if command.kind != LC_ID_DYLIB {
                    existing.insert(new_path.to_owned());
                }
            } else if command.kind == LC_RPATH {
                let path_offset = self.endian.u32(&bytes, 8)? as usize;
                has_rpath |= name(&bytes[path_offset..])? == FRAMEWORK_RPATH;
            }

            commands.push(bytes);
        }

        for path in additions {
            validate_path(path)?;

            if existing.insert(path.clone()) {
                commands.push(dylib_command(self.endian, self.is_64, LC_LOAD_DYLIB, path, 0, 0, 0)?);
            }
        }

        if !additions.is_empty() && !has_rpath {
            let size = align_up(12 + FRAMEWORK_RPATH.len() + 1, if self.is_64 { 8 } else { 4 })?;
            let mut command = vec![0; size];

            self.endian.put_u32(&mut command, 0, LC_RPATH)?;
            self.endian.put_u32(&mut command, 4, to_u32(size, "rpath size")?)?;
            self.endian.put_u32(&mut command, 8, 12)?;
            command[12..12 + FRAMEWORK_RPATH.len()].copy_from_slice(FRAMEWORK_RPATH.as_bytes());

            commands.push(command);
        }

        self.rewrite_commands(&commands)
    }
}

fn dylib_command(
    endian: Endian,
    is_64: bool,
    kind: u32,
    path: &str,
    timestamp: u32,
    current_version: u32,
    compatibility_version: u32,
) -> Result<Vec<u8>> {
    validate_path(path)?;

    let unaligned =
        24usize.checked_add(path.len()).and_then(|size| size.checked_add(1)).ok_or(Error::Overflow("dylib path"))?;
    let size = align_up(unaligned, if is_64 { 8 } else { 4 })?;
    let mut command = vec![0; size];

    endian.put_u32(&mut command, 0, kind)?;
    endian.put_u32(&mut command, 4, to_u32(size, "dylib size")?)?;
    endian.put_u32(&mut command, 8, 24)?;

    endian.put_u32(&mut command, 12, timestamp)?;
    endian.put_u32(&mut command, 16, current_version)?;
    endian.put_u32(&mut command, 20, compatibility_version)?;
    command[24..24 + path.len()].copy_from_slice(path.as_bytes());

    Ok(command)
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() || path.as_bytes().contains(&0) {
        return Err(malformed("empty or NUL-containing library path"));
    }

    Ok(())
}
