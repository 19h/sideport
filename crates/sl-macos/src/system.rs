//! The Mac's identity from documented tools and files.

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

/// The Apple Silicon provisioning UDID used to register the Mac as a device. The recovered
/// `get_m1_udid` helper reads `MGCopyAnswer("ProvisioningUniqueDeviceID")`; System Information
/// reports the same value as `provisioning_UDID`.
pub fn provisioning_udid() -> Result<String> {
    if !cfg!(target_os = "macos") {
        return Err(Error::Unsupported);
    }

    let output = Command::new("/usr/sbin/system_profiler")
        .args(["SPHardwareDataType", "-json"])
        .output()
        .map_err(system_error)?;

    if !output.status.success() {
        return Err(Error::System("system_profiler failed".into()));
    }

    parse_provisioning_udid(&output.stdout)
}

pub(crate) fn parse_provisioning_udid(json: &[u8]) -> Result<String> {
    let value: serde_json::Value = serde_json::from_slice(json).map_err(|error| Error::System(error.to_string()))?;

    let udid = value["SPHardwareDataType"][0]["provisioning_UDID"]
        .as_str()
        .filter(|udid| {
            !udid.is_empty() && udid.chars().all(|character| character.is_ascii_hexdigit() || character == '-')
        })
        .ok_or_else(|| Error::System("no provisioning UDID (Intel Macs have none)".into()))?;

    Ok(udid.to_owned())
}

fn system_error(error: std::io::Error) -> Error {
    Error::System(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_udid_parsing_accepts_hexadecimal_and_rejects_absence() {
        let apple_silicon = br#"{"SPHardwareDataType":[{"provisioning_UDID":"00006041-001A2B3C4D5E6F70"}]}"#;
        assert_eq!(parse_provisioning_udid(apple_silicon).expect("UDID"), "00006041-001A2B3C4D5E6F70");

        for json in
            [&br#"{"SPHardwareDataType":[{}]}"#[..], br#"{"SPHardwareDataType":[{"provisioning_UDID":"x y"}]}"#, b"[]"]
        {
            assert!(parse_provisioning_udid(json).is_err());
        }
    }
}
