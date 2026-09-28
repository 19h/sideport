use thiserror::Error;

/// Errors surfaced to front-ends. Messages are written for end users.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EngineError {
    #[error("Cancelled")]
    Cancelled,
    /// Apple ID sign-in failed (wrong password, locked account, 2FA failure, …).
    #[error("Sign-in failed: {0}")]
    Auth(String),
    /// Developer portal refused a request. `code` is Apple's `resultCode`.
    #[error("Apple developer service error {code}: {message}")]
    Portal { code: i64, message: String },
    #[error("Anisette unavailable: {0}")]
    Anisette(String),
    #[error("Device error: {0}")]
    Device(String),
    #[error("The device is not reachable: {0}")]
    DeviceUnavailable(String),
    #[error("Invalid app: {0}")]
    InvalidApp(String),
    #[error("Signing failed: {0}")]
    Signing(String),
    #[error("Installation failed: {0}")]
    Install(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Storage error: {0}")]
    Storage(String),
    /// Talking to another Sideport process on this machine failed.
    #[error("Local connection failed: {0}")]
    Ipc(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;
