//! Device errors and their recovered retry classes.
//!
//! The recovered installer keys on libimobiledevice error names: mux errors retry
//! automatically, `IDEVICE_E_NO_DEVICE` waits for the device, pairing states ask the user,
//! and oversize/size-mismatch/extraction failures remove the staged file and retry.

use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum DeviceError {
    /// usbmuxd is not running or refused the connection.
    #[error("the device service (usbmuxd) is unavailable: {0}")]
    MuxUnavailable(String),

    /// The device is not attached (recovered `IDEVICE_E_NO_DEVICE`).
    #[error("device {0} is not connected")]
    NotConnected(String),

    /// A connection dropped mid-operation (recovered `*_E_MUX_ERROR`, `MISAGENT_E_CONN_FAILED`).
    #[error("the connection to the device was interrupted: {0}")]
    Interrupted(String),

    #[error("the device is locked with a passcode; unlock it and try again")]
    PasswordProtected,

    #[error("the device declined to trust this computer")]
    UserDeniedPairing,

    #[error("confirm \"Trust This Computer\" on the device")]
    PairingDialogPending,

    /// No pairing record exists, or the device rejected it.
    #[error("the device is not paired with this computer")]
    NotPaired,

    /// Recovered `FileTooBig`: the staged file is larger than the package.
    #[error("the staged package ({staged} bytes) is larger than the upload ({expected} bytes)")]
    StagedTooLarge { staged: u64, expected: u64 },

    #[error("size mismatch: expected {expected} bytes, the device has {actual}")]
    SizeMismatch { expected: u64, actual: u64 },

    /// An installation-proxy failure with its `Error`, `ErrorDescription` and `ErrorDetail`.
    #[error("{}", install_message(name, description.as_deref(), *detail))]
    Install { name: String, description: Option<String>, detail: Option<u64> },

    /// An installation failure after which the recovered client retries the staged package.
    #[error("{0}; retrying")]
    InstallRetry(String),

    #[error("AFC error {code}: {message}")]
    Afc { code: u64, message: String },

    #[error("device protocol error: {0}")]
    Protocol(String),

    #[error("local I/O error: {0}")]
    Local(String),

    #[error("cancelled")]
    Cancelled,
}

fn install_message(name: &str, description: Option<&str>, detail: Option<u64>) -> String {
    let mut message = format!("installation failed: {name}");

    if let Some(description) = description {
        message.push_str(&format!(" ({description})"));
    }

    if let Some(detail) = detail {
        message.push_str(&format!(" [{detail}]"));
    }

    message
}

/// How the recovered installer reacts to a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Retry without asking (bounded by the automatic-retry budget).
    Automatic,
    /// Wait for the device to reappear, then retry.
    AwaitDevice,
    /// Ask the user to fix the device state (unlock, trust) before retrying.
    AskUser,
    /// Stop.
    Fatal,
}

/// installd errors after which retrying the same package cannot succeed. The recovered client
/// retries every error while the staged file exists; Sideport stops for these.
const TERMINAL_INSTALL_ERRORS: &[&str] = &[
    "ApplicationVerificationFailed",
    "DeviceOSVersionTooLow",
    "IncorrectArchitecture",
    "MismatchedApplicationIdentifierEntitlement",
    "BundleValidationFailed",
];

impl DeviceError {
    pub fn recovery(&self) -> Recovery {
        match self {
            Self::Interrupted(_) | Self::InstallRetry(_) | Self::StagedTooLarge { .. } | Self::SizeMismatch { .. } => {
                Recovery::Automatic
            }
            Self::NotConnected(_) => Recovery::AwaitDevice,
            Self::PasswordProtected | Self::UserDeniedPairing | Self::PairingDialogPending => Recovery::AskUser,
            // The installer turns retryable installation failures into `InstallRetry`; an
            // `Install` error that leaves an attempt is terminal or was declined by the user.
            Self::Install { .. }
            | Self::MuxUnavailable(_)
            | Self::NotPaired
            | Self::Afc { .. }
            | Self::Protocol(_)
            | Self::Local(_)
            | Self::Cancelled => Recovery::Fatal,
        }
    }

    /// installd failures that retrying the same package cannot fix.
    pub fn is_terminal_install(&self) -> bool {
        matches!(self, Self::Install { name, .. } if TERMINAL_INSTALL_ERRORS.contains(&name.as_str()))
    }

    /// Recovered special case: "Could not extract archive" retries the upload automatically.
    pub fn is_extraction_failure(&self) -> bool {
        matches!(self, Self::Install { description: Some(description), .. } if description.contains("Could not extract archive"))
            || matches!(self, Self::Install { name, .. } if name.contains("Could not extract archive"))
    }
}

impl From<idevice::IdeviceError> for DeviceError {
    fn from(error: idevice::IdeviceError) -> Self {
        use idevice::IdeviceError as Idevice;

        match error {
            Idevice::PasswordProtected | Idevice::DeviceLocked => Self::PasswordProtected,
            Idevice::UserDeniedPairing => Self::UserDeniedPairing,
            Idevice::PairingDialogResponsePending => Self::PairingDialogPending,
            Idevice::InvalidHostID | Idevice::SessionInactive => Self::NotPaired,
            Idevice::DeviceNotFound => Self::NotConnected("requested device".into()),
            Idevice::Socket(error) => Self::Interrupted(error.to_string()),
            Idevice::Timeout => Self::Interrupted("timed out".into()),
            Idevice::Afc(error) => {
                let message = error.to_string();

                match error {
                    idevice::afc::errors::AfcError::MuxError => Self::Interrupted(message),
                    error => Self::Afc { code: error as u64, message },
                }
            }
            Idevice::Usbmuxd(error) => Self::Interrupted(error.to_string()),
            other => Self::Protocol(other.to_string()),
        }
    }
}

pub type Result<T, E = DeviceError> = std::result::Result<T, E>;
