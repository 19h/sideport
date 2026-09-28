//! Acquisition channels: `sideloadly:` links, resumable downloads and App Store enrichment.

#![forbid(unsafe_code)]

pub mod countries;
pub mod download;
mod enrich;
pub mod flip;
pub mod link;

pub use download::{Downloader, Observer, Request};
pub use link::{Digest, Enrichment, Link};

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("{0}")]
    Link(String),
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Html(String),
    #[error("Not a valid IPA file!")]
    NotIpa,
    #[error("{0}")]
    Hash(String),
    #[error("{0}")]
    Enrich(String),
    #[error("local I/O error: {0}")]
    Local(String),
    #[error("Download cancelled")]
    Cancelled,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
