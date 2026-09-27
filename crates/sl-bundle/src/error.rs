use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid archive: {0}")]
    Archive(String),

    #[error("invalid bundle: {0}")]
    Bundle(String),

    #[error("invalid bundle path: {0}")]
    Path(String),

    #[error("operation cancelled")]
    Cancelled,

    #[error("archive limit exceeded: {0}")]
    Limit(&'static str),

    #[error(transparent)]
    Zip(zip::result::ZipError),

    #[error(transparent)]
    Plist(#[from] plist::Error),

    #[error(transparent)]
    Codesign(#[from] sl_codesign::Error),

    #[error(transparent)]
    MachO(#[from] sl_macho::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io<T>(path: &std::path::Path, result: std::io::Result<T>) -> Result<T> {
    result.map_err(|source| from_io(path, source))
}

pub(crate) fn from_io(path: &std::path::Path, source: std::io::Error) -> Error {
    if source
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Error>())
        .is_some_and(|error| matches!(error, Error::Cancelled))
    {
        Error::Cancelled
    } else {
        Error::Io { path: path.to_owned(), source }
    }
}

impl From<zip::result::ZipError> for Error {
    fn from(error: zip::result::ZipError) -> Self {
        match error {
            zip::result::ZipError::Io(source) => from_io(std::path::Path::new("<archive>"), source),
            error => Self::Zip(error),
        }
    }
}
