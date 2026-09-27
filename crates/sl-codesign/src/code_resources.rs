//! `_CodeSignature/CodeResources` seal generation. CONTRACT.

use crate::Result;
use std::path::Path;

/// Walk `bundle_dir` and build the CodeResources XML plist (`files`, `files2`, `rules`, `rules2`) using
/// the iOS rule template. `main_executable` (relative path) is excluded from the seal. An existing
/// top-level `_CodeSignature` directory is ignored. Hashing runs in parallel.
pub fn build_seal(bundle_dir: &Path, main_executable: Option<&str>) -> Result<Vec<u8>> {
    let _ = (bundle_dir, main_executable);
    unimplemented!()
}
