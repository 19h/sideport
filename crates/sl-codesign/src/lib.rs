//! Apple code-signature generation for iOS/tvOS bundles.
//!
//! This crate turns an unsigned (or previously signed) Mach-O into a freshly signed one, entirely in
//! process: SHA-1 + SHA-256 CodeDirectories, a requirements set, XML and DER entitlements, and a CMS
//! signature carrying Apple's CDHash attributes. It also builds `_CodeSignature/CodeResources` seals and
//! decodes provisioning profiles.
//!
//! The format follows Sideloadly 0.60's recovered signer and Apple's own `codesign` output.
//!
//! # API contract
//! Items marked "CONTRACT" are relied on by `sl-bundle` and `sl-engine`; keep their signatures stable.

#![forbid(unsafe_code)]

pub mod blob;
pub mod cms;
pub mod code_resources;
pub mod entitlements;
pub mod error;
pub mod identity;
pub mod profile;
pub mod requirements;
pub mod signature;
pub mod trust;

pub use error::{Error, Result};
pub use identity::{SigningIdentity, build_csr_pem, generate_signing_key};
pub use profile::{ProfileTarget, ProvisioningProfile};
pub use sl_macho as macho;
pub use trust::ProfileTrust;

use std::sync::Arc;

/// Who signs. CONTRACT.
#[derive(Debug, Clone)]
pub enum Signer {
    /// Ad-hoc signature: no certificate, no CMS, empty requirements, no entitlements, `CS_ADHOC` flag.
    AdHoc,
    /// A development certificate + private key (plus the embedded Apple intermediate/root chain).
    Identity(Arc<SigningIdentity>),
}

impl Signer {
    /// Team identifier embedded in CodeDirectories (`""` for ad-hoc).
    pub fn team_id(&self) -> &str {
        match self {
            Signer::AdHoc => "",
            Signer::Identity(id) => id.team_id(),
        }
    }

    pub fn is_adhoc(&self) -> bool {
        matches!(self, Signer::AdHoc)
    }
}

/// What kind of code a binary is; decides special slots and exec-segment flags. CONTRACT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeKind {
    /// `CFBundleExecutable` of an `.app` (sets `CS_EXECSEG_MAIN_BINARY`).
    MainExecutable,
    /// `CFBundleExecutable` of an `.appex` (also a main binary for its own process).
    AppExtension,
    /// Executable inside a `.framework`.
    Framework,
    /// Any other Mach-O (loose `.dylib`, helper binaries).
    Dylib,
}

/// Per-binary signing parameters. CONTRACT.
#[derive(Debug, Clone, Copy)]
pub struct SignOptions<'a> {
    /// CodeDirectory identifier, normally the owning bundle's `CFBundleIdentifier`
    /// (for loose dylibs: the file name without extension).
    pub identifier: &'a str,
    pub kind: CodeKind,
    /// Entitlements to embed for bundle executables, extensions and frameworks; ignored for dylibs and ad-hoc.
    pub entitlements: Option<&'a plist::Dictionary>,
    /// Raw bytes of the owning bundle's `Info.plist` (special slot -1).
    pub info_plist: Option<&'a [u8]>,
    /// Raw bytes of the owning bundle's `_CodeSignature/CodeResources` (special slot -3).
    pub code_resources: Option<&'a [u8]>,
}

/// Sign a thin or fat Mach-O image, returning the complete new file. CONTRACT.
///
/// Every slice is signed from scratch; any existing signature is discarded. Fat files are re-laid out
/// with 16 KiB slice alignment.
pub fn sign_macho(input: &[u8], signer: &Signer, opts: &SignOptions<'_>) -> Result<Vec<u8>> {
    signature::sign_file(input, signer, opts)
}

/// Remove `LC_CODE_SIGNATURE` and the signature blob from every slice. CONTRACT.
pub fn strip_signature(input: &[u8]) -> Result<Vec<u8>> {
    signature::strip_file(input)
}

/// `true` if `data` is a Mach-O (thin or fat) containing at least one ARM64/ARM64E slice. CONTRACT.
pub fn is_arm64_macho(data: &[u8]) -> bool {
    signature::is_arm64_macho(data)
}

/// `true` if any slice has `LC_ENCRYPTION_INFO(_64)` with `cryptid != 0` (FairPlay-encrypted). CONTRACT.
pub fn is_encrypted(data: &[u8]) -> Result<bool> {
    signature::is_encrypted(data)
}
