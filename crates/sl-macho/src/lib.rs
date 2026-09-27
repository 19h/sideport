//! Mach-O and fat binary parsing and load-command editing.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Malformed(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
