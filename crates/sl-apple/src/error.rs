use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid authentication data: {0}")]
    Invalid(&'static str),
    #[error("authentication verification failed: {0}")]
    Verification(&'static str),
    #[error("network request failed")]
    Network(#[source] reqwest::Error),
    #[error("service returned HTTP {0}")]
    HttpStatus(u16),
    #[error("service response exceeds {0} bytes")]
    ResponseTooLarge(usize),
    #[error("operating system random source failed")]
    Entropy,
    #[error("cancelled")]
    Cancelled,
    #[error("Apple {operation} service returned error {code}")]
    Service { operation: &'static str, code: i64 },
    #[error("Apple account requires {0} at https://developer.apple.com/account")]
    AccountAction(&'static str),
    #[error("authentication retry limit reached: {0}")]
    RetryLimit(&'static str),
    #[error("authentication worker failed")]
    Worker,
    #[error("local anisette failed: {0}")]
    LocalAnisette(String),
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Network(error.without_url())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
