//! The Mac's identity from system tools, files and MobileGestalt.

use crate::{Error, Result};
use std::process::Command;

/// `hw.model`, e.g. `Mac16,5` (recovered `sysctl -n hw.model`).
pub fn hardware_model() -> Result<String> {
    if !cfg!(target_os = "macos") {
        return Err(Error::Unsupported);
    }

    let output = Command::new("/usr/sbin/sysctl").args(["-n", "hw.model"]).output().map_err(system_error)?;
    let model = String::from_utf8_lossy(&output.stdout).trim().to_owned();

    if !output.status.success() || model.is_empty() {
        return Err(Error::System("hw.model is unavailable".into()));
    }

    Ok(model)
}

/// The user-visible computer name (System Settings › General › About).
pub fn computer_name() -> Result<String> {
    if !cfg!(target_os = "macos") {
        return Err(Error::Unsupported);
    }

    let output = Command::new("/usr/sbin/scutil").args(["--get", "ComputerName"]).output().map_err(system_error)?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();

    if !output.status.success() || name.is_empty() {
        return Err(Error::System("the computer name is unavailable".into()));
    }

    Ok(name)
}

/// `(ProductVersion, ProductBuildVersion)` from SystemVersion.plist (recovered `sw_vers`).
pub fn os_version() -> Result<(String, String)> {
    if !cfg!(target_os = "macos") {
        return Err(Error::Unsupported);
    }

    let path = "/System/Library/CoreServices/SystemVersion.plist";
    let value = plist::Value::from_file(path).map_err(|error| Error::System(error.to_string()))?;
    let fields =
        value.as_dictionary().ok_or_else(|| Error::System("SystemVersion.plist is not a dictionary".into()))?;

    let text = |key: &str| {
        fields
            .get(key)
            .and_then(plist::Value::as_string)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| Error::System(format!("SystemVersion.plist has no {key}")))
    };

    Ok((text("ProductVersion")?, text("ProductBuildVersion")?))
}

/// The Apple Silicon provisioning UDID used to register the Mac as a device. Matches the
/// recovered `get_m1_udid` call to `MGCopyAnswer("ProvisioningUniqueDeviceID")`.
pub fn provisioning_udid() -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        crate::mobile_gestalt::provisioning_udid()
    }

    #[cfg(not(target_os = "macos"))]
    Err(Error::Unsupported)
}

fn system_error(error: std::io::Error) -> Error {
    Error::System(error.to_string())
}
