//! "Enable JIT for Apps": launch/attach an installed bundle through debugserver so it runs with
//! debugging enabled (recovered `sideloadly/mobdev` `StartJIT`).
//!
//! This is the pre-iOS-17 lockdown `com.apple.debugserver[.DVTSecureSocketProxy]` path the
//! recovered client uses. iOS 17+ moves debugserver behind the RSD/CoreDevice tunnel, which
//! needs a root network tunnel and is not part of the recovered client (see docs/DEVICE.md).

use crate::error::{DeviceError, Result};
use futures::future::BoxFuture;

/// Where an installed app lives, for the debugserver launch/attach (recovered
/// `GetPathForBundleId` + `GetWorkingDirAndExeNameForBundleId`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppLaunch {
    /// The `.app` bundle path (recovered `SetArgv[0]`).
    pub path: String,
    /// The app's data container, used as the debugserver working directory.
    pub container: Option<String>,
    /// `CFBundleExecutable`, hex-encoded for `vAttachOrWait` when attaching.
    pub executable: String,
}

/// The debugserver operations the JIT flow uses. [`crate::backend::IdeviceDebugger`] implements
/// it with `idevice`'s debug proxy; tests implement it with a fake.
pub trait Debugger: Send {
    /// Send a GDB-remote command with optional (hex-encoded) arguments and read the reply.
    fn command<'a>(&'a mut self, name: &'a str, argv: &'a [String]) -> BoxFuture<'a, Result<Option<String>>>;

    /// The `A` set-argv packet (recovered `SetArgv`).
    fn set_argv<'a>(&'a mut self, argv: &'a [String]) -> BoxFuture<'a, Result<String>>;
}

/// Launch (`launch = true`) or attach (`launch = false`) the app so it runs with JIT enabled,
/// then detach so it keeps running (recovered `StartJIT`).
pub async fn enable_jit(debugger: &mut dyn Debugger, app: &AppLaunch, launch: bool) -> Result<()> {
    if launch {
        debugger.command("QSetLogging:bitmask=LOG_ALL|LOG_RNB_REMOTE|LOG_RNB_PACKETS", &[]).await?;
        debugger.command("QSetMaxPacketSize:", &["1024".into()]).await?;

        // The recovered client ignores a working-directory error (code 60) and continues.
        if let Some(container) = &app.container {
            let _ = debugger.command("QSetWorkingDir:", std::slice::from_ref(container)).await;
        }

        debugger.set_argv(std::slice::from_ref(&app.path)).await?;
        debugger.command("qLaunchSuccess", &[]).await?;
    } else {
        let attach = format!("vAttachOrWait;{}", hex_encode(app.executable.as_bytes()));
        debugger.command(&attach, &[]).await?;
    }

    // Detach so the process keeps running with debugging (and thus JIT) enabled.
    debugger.command("D", &[]).await?;

    Ok(())
}

/// Uppercase hex, as debugserver's `vAttachOrWait` argument is encoded.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;

    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut output, byte| {
        let _ = write!(output, "{byte:02X}");

        output
    })
}

/// A response that is neither `OK`/empty nor a `$`-framed payload is an error (the caller maps
/// `E<code>` responses; anything else is unexpected).
pub(crate) fn interpret(name: &str, response: Option<String>) -> Result<Option<String>> {
    match response.as_deref() {
        None | Some("OK") | Some("") => Ok(response),
        Some(text) if text.starts_with('E') => {
            Err(DeviceError::Protocol(format!("debugserver rejected {name}: {text}")))
        }
        Some(_) => Ok(response),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encoding_is_uppercase() {
        assert_eq!(hex_encode(b"App"), "417070");
    }

    #[test]
    fn error_responses_are_rejected() {
        assert!(interpret("qLaunchSuccess", Some("E08".into())).is_err());
        assert!(interpret("D", Some("OK".into())).is_ok());
        assert!(interpret("D", None).is_ok());
    }
}
