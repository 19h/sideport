use thiserror::Error;

/// Errors produced while signing or parsing signing material.
#[derive(Debug, Error)]
pub enum Error {
    #[error("malformed Mach-O: {0}")]
    Macho(#[from] sl_macho::Error),
    #[error("load commands do not fit in the header padding of {0}")]
    HeaderSpace(String),
    #[error("certificate error: {0}")]
    Certificate(String),
    #[error("private key error: {0}")]
    Key(String),
    #[error("CMS error: {0}")]
    Cms(String),
    #[error("provisioning profile error: {0}")]
    Profile(String),
    #[error("provisioning profile trust error: {0}")]
    ProfileTrust(String),
    #[error("entitlements error: {0}")]
    Entitlements(String),
    #[error("plist error: {0}")]
    Plist(#[from] plist::Error),
    #[error("I/O error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The caller's cancellation callback returned true.
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
