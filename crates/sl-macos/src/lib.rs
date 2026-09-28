//! macOS integration: local anisette through AOSKit and the Mac's identity.
//!
//! Only [`aoskit`] uses `unsafe` (Objective-C messages to a private framework). Everything else
//! reads documented system tools and files. On other platforms every function reports
//! [`Error::Unsupported`].

#[cfg(target_os = "macos")]
mod aoskit;
mod system;

pub use system::{computer_name, hardware_model, os_version, provisioning_udid};

use futures::FutureExt;
use futures::future::BoxFuture;
use sl_apple::anisette::{MachineSource, MachineValues};

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("not supported on this platform")]
    Unsupported,
    /// Recovered "AOS incompatible": AOSKit or a required method is missing.
    #[error("AOSKit is unavailable: {0}")]
    AosIncompatible(&'static str),
    /// Recovered "AOS fail": AOSKit returned no usable value.
    #[error("AOSKit did not return {0}")]
    AosFailed(&'static str),
    #[error("system query failed: {0}")]
    System(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Names of the headers AOSKit returns (values withheld), for diagnosing local anisette.
pub fn aoskit_header_names() -> Result<Vec<String>> {
    #[cfg(target_os = "macos")]
    {
        aoskit::otp_header_names()
    }

    #[cfg(not(target_os = "macos"))]
    Err(Error::Unsupported)
}

/// Local anisette values from AOSKit and the system, gathered on a blocking worker.
#[derive(Debug, Clone, Copy, Default)]
pub struct AosKitSource;

impl MachineSource for AosKitSource {
    fn values(&self) -> BoxFuture<'_, sl_apple::Result<MachineValues>> {
        async {
            let values = tokio::task::spawn_blocking(machine_values).await.map_err(|_| sl_apple::Error::Worker)?;

            values.map_err(|error| match error {
                Error::Unsupported => sl_apple::Error::Invalid("local anisette requires macOS"),
                Error::AosIncompatible(_) => sl_apple::Error::Invalid("AOSKit is unavailable"),
                Error::AosFailed(_) => sl_apple::Error::Invalid("AOSKit returned no one-time password"),
                Error::System(_) => sl_apple::Error::Invalid("system identity is unavailable"),
            })
        }
        .boxed()
    }
}

/// Everything the recovered private anisetter and Go host combine.
pub fn machine_values() -> Result<MachineValues> {
    #[cfg(target_os = "macos")]
    {
        let otp = aoskit::otp_headers()?;
        let (os_version, os_build) = os_version()?;

        Ok(MachineValues {
            otp: otp.otp,
            machine_token: otp.machine_token,
            serial: otp.serial,
            device_id: otp.device_id,
            locale: otp.locale,
            time_zone: otp.time_zone,
            hardware_model: hardware_model()?,
            os_version,
            os_build,
        })
    }

    #[cfg(not(target_os = "macos"))]
    Err(Error::Unsupported)
}
