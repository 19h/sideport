//! Entitlements encodings.
use crate::Result;

/// XML plist encoding used for the `0xfade7171` blob.
pub fn to_xml(entitlements: &plist::Dictionary) -> Result<Vec<u8>> {
    let _ = entitlements;
    unimplemented!()
}

/// DER encoding used for the `0xfade7172` blob.
pub fn to_der(entitlements: &plist::Dictionary) -> Result<Vec<u8>> {
    let _ = entitlements;
    unimplemented!()
}
