//! Client side of Sideloadly 0.60's recovered "private services": a self-update protocol
//! (`go-selfupdate` + BSDIFF40 patches), a feature-token verifier and the machine binary patcher.
//!
//! This crate implements only the *protocol shapes* recovered by reverse engineering. It ships no
//! endpoints, no verification keys and no claim names taken from the Sideloadly binary, and it never
//! contacts sideloadly.io or Patreon. Every server URL and every signing key is supplied by the
//! caller through [`ServiceConfig`]; with nothing configured the update check reports
//! [`UpdateStatus::NotConfigured`]. See `docs/SERVICES.md` for the recovered sources, the deliberate
//! deviations and the availability status (unknown).

#![forbid(unsafe_code)]

mod bspatch;
mod config;
mod token;
mod update;

#[cfg(test)]
mod testsupport;

pub use bspatch::bspatch;
pub use config::{FeatureState, ServiceConfig, Services, ServicesStatus};
pub use token::{FeatureToken, Features, TokenVerifier};
pub use update::{Manifest, UpdateEndpoints, UpdatePlan, UpdateStatus, Updater, swap};

use thiserror::Error;

/// Failures surfaced by the services client. Messages are written for end users and never echo a
/// configured URL or a token body.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("feature token rejected: {0}")]
    Token(&'static str),
    #[error("patch could not be applied: {0}")]
    Patch(&'static str),
    #[error("update manifest is malformed")]
    Manifest,
    #[error("downloaded update failed its checksum")]
    Checksum,
    #[error("update service returned HTTP {0}")]
    HttpStatus(u16),
    #[error("update response exceeds {0} bytes")]
    ResponseTooLarge(usize),
    #[error("network request failed")]
    Network(#[source] reqwest::Error),
    #[error("local I/O error: {0}")]
    Io(String),
    #[error("cancelled")]
    Cancelled,
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Network(error.without_url())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
