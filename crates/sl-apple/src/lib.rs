//! Apple ID authentication, anisette and developer services.

#![forbid(unsafe_code)]

pub mod anisette;
pub mod auth;
mod error;
pub mod portal;
pub mod srp;
mod transport;
mod wire;

pub use error::{Error, Result};
