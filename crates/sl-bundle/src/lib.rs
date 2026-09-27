//! IPA and app-bundle handling.

#![forbid(unsafe_code)]

mod archive;
mod control;
mod error;
mod files;
mod icons;
mod inject;
mod inspect;
mod model;
mod pack;
mod patch;
mod plists;
mod reader;
mod sign;

pub use archive::{ArchiveKind, ArchiveLimits, BundleArchive};
pub use control::{Control, Phase, Progress};
pub use error::{Error, Result};
pub use inject::{Injection, InjectionReport};
pub use inspect::{BundleInspection, ExtensionMetadata, inspect};
pub use model::{Bundle, BundleKind};
pub use pack::{OutputLayout, PackOptions};
pub use patch::{InfoEdits, PatchOptions, PatchReport, PropertyEdit, Replacement};
pub use plists::{parse_dictionary, read_dictionary};
pub use sign::{ProfileRequirements, SignReport, SigningRequest, SkippedBinary};
