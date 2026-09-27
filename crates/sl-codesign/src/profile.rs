//! Provisioning profile decoding (CMS-wrapped XML plist). CONTRACT.

use crate::Result;
use chrono::{DateTime, Utc};

/// A decoded `.mobileprovision`.
#[derive(Debug, Clone)]
pub struct ProvisioningProfile {
    /// Original CMS bytes (written verbatim to `embedded.mobileprovision`).
    pub raw: Vec<u8>,
    pub name: String,
    pub uuid: String,
    pub team_identifiers: Vec<String>,
    pub app_id_name: Option<String>,
    pub entitlements: plist::Dictionary,
    pub creation_date: DateTime<Utc>,
    pub expiration_date: DateTime<Utc>,
    pub time_to_live_days: Option<u64>,
    /// `LocalProvision` — true for free (personal team) profiles.
    pub local_provision: bool,
    pub provisioned_devices: Vec<String>,
    /// DER certificates listed in `DeveloperCertificates`.
    pub developer_certificates: Vec<Vec<u8>>,
}

impl ProvisioningProfile {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let _ = raw;
        unimplemented!()
    }

    /// `application-identifier` with the team prefix removed (may end in `*`).
    pub fn bundle_id(&self) -> Option<&str> {
        unimplemented!()
    }

    /// Raw plist payload of the CMS envelope.
    pub fn payload(raw: &[u8]) -> Result<Vec<u8>> {
        let _ = raw;
        unimplemented!()
    }
}
