//! Plain data types shared by the engine and its front-ends. CONTRACT: `sl-cli` and `sl-app` depend on
//! these definitions; extend them compatibly.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ------------------------------------------------------------------------------------------------
// Devices

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Connection {
    Usb,
    Network,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub udid: String,
    /// User-visible name (`DeviceName`), e.g. "Jane's iPhone".
    pub name: String,
    /// `ProductType`, e.g. `iPhone15,2`.
    pub product_type: String,
    /// Marketing model name derived from `product_type` when known, e.g. "iPhone 14 Pro".
    pub model_name: Option<String>,
    /// `ProductVersion`, e.g. `17.5.1`.
    pub os_version: String,
    /// `DeviceClass`: `iPhone`, `iPad`, `iPod`, `AppleTV`, …
    pub device_class: String,
    /// All transports the device is currently reachable over (a device can be on USB and Wi-Fi).
    pub connections: Vec<Connection>,
    /// `false` when lockdown refused the host (locked/not trusted); name/version may then be empty.
    pub paired: bool,
}

impl DeviceInfo {
    pub fn is_apple_tv(&self) -> bool {
        self.device_class == "AppleTV"
    }
    pub fn preferred_connection(&self) -> Option<Connection> {
        if self.connections.contains(&Connection::Usb) {
            Some(Connection::Usb)
        } else {
            self.connections.first().copied()
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Accounts

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamKind {
    /// Personal team of a free Apple ID (7-day profiles, 10 app IDs per 7 days, 3 active apps).
    Free,
    Individual,
    Organization,
    /// Team type returned by the portal but not classified by the recovered policy.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSummary {
    pub team_id: String,
    pub name: String,
    pub kind: TeamKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSummary {
    pub apple_id: String,
    /// Teams enumerated by the portal (empty until enumeration succeeds).
    pub teams: Vec<TeamSummary>,
    /// Team used by default for this account (chosen once when several exist).
    pub default_team: Option<String>,
    /// A GSA session token is held by the engine; portal validity is separate.
    pub has_session: bool,
    /// The password is stored in the keychain (enables unattended refresh).
    pub remembers_password: bool,
    pub last_login: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateSummary {
    pub serial: String,
    pub name: String,
    pub machine_name: Option<String>,
    pub expires: Option<DateTime<Utc>>,
    /// Created by this installation (matches our signing key).
    pub is_ours: bool,
}

/// Result of importing sessions from the recovered client's `sessions.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionImport {
    /// Apple IDs whose sessions were imported.
    pub imported: Vec<String>,
    /// Entries left out, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// A device registered with the selected developer team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredDevice {
    /// Portal `deviceNumber` (the device UDID as registered).
    pub udid: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppIdSummary {
    pub app_id_id: String,
    pub identifier: String,
    pub name: String,
    pub expires: Option<DateTime<Utc>>,
}

// ------------------------------------------------------------------------------------------------
// Apps

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionInfo {
    /// Directory name, e.g. `Widget.appex`.
    pub file_name: String,
    pub bundle_id: String,
    pub display_name: Option<String>,
}

/// What we know about an `.ipa` before processing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSummary {
    pub path: PathBuf,
    pub name: String,
    pub bundle_id: String,
    pub version: Option<String>,
    pub short_version: Option<String>,
    pub minimum_os: Option<String>,
    /// Decoded PNG of the largest app icon found (standard PNG, CgBI already normalized).
    pub icon_png: Option<Vec<u8>>,
    pub extensions: Vec<ExtensionInfo>,
    pub has_watch_app: bool,
    pub file_size: u64,
    /// Main executable is FairPlay-encrypted (will not run after re-signing).
    pub encrypted: bool,
    /// `UIDeviceFamily` values (1 = iPhone, 2 = iPad, 3 = TV, …).
    pub device_family: Vec<u32>,
    /// Optional inspection issues, such as an undecodable icon. Empty for older stored records.
    #[serde(default)]
    pub warnings: Vec<String>,
}

// ------------------------------------------------------------------------------------------------
// Jobs

/// Where the processed app goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// Install on an attached device.
    Device { udid: String, prefer_network: bool },
    /// Write an `.ipa` file. `None` = ask via [`crate::PromptKind::SaveFile`].
    ExportIpa { path: Option<PathBuf> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SigningMode {
    /// Provision with an Apple ID and sign with a development certificate.
    AppleId { apple_id: String },
    /// Ad-hoc signature (no account; for jailbroken or developer-mode setups that accept it).
    AdHoc,
    /// Apply patches only and leave the bundle unsigned (export only).
    Unsigned,
    /// Install/export the original file untouched.
    Original,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BundleIdPolicy {
    /// Keep the original id with paid teams; for free teams append `.<TEAMID>` (avoids id collisions).
    #[default]
    Auto,
    /// Always keep the original id.
    Original,
    /// Use exactly this id (extensions keep their suffixes).
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InfoValue {
    String(String),
    Bool(bool),
    Integer(i64),
    /// Remove the key.
    Remove,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoOverride {
    pub key: String,
    pub value: InfoValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ExtensionRemoval {
    #[default]
    Keep,
    All,
    /// Remove these `*.appex` directory names.
    Selected(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryInjection {
    pub source: PathBuf,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReplacement {
    pub target: PathBuf,
    /// None removes the target from the prepared app.
    pub source: Option<PathBuf>,
}

/// Every user-selectable option of a job. `Default` = "just sign and install".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppOptions {
    pub bundle_id: BundleIdPolicy,
    pub display_name: Option<String>,
    /// `CFBundleVersion`.
    pub version: Option<String>,
    /// `CFBundleShortVersionString`.
    pub short_version: Option<String>,
    /// `MinimumOSVersion` override (lets older OS versions attempt to run the app).
    pub minimum_os: Option<String>,
    /// Remove `UISupportedDevices`.
    pub remove_device_restrictions: bool,
    /// Set `UIFileSharingEnabled` + `LSSupportsOpeningDocumentsInPlace`.
    pub enable_file_sharing: bool,
    pub extra_info: Vec<InfoOverride>,
    pub remove_extensions: ExtensionRemoval,
    /// Defaults to true, matching the recovered phone-profile preparation policy.
    pub remove_watch_app: bool,
    /// PNG to use as the new app icon.
    pub icon: Option<PathBuf>,
    /// Plist merged over the profile's entitlements (keys the profile does not grant will fail install).
    pub entitlements: Option<PathBuf>,
    #[serde(default)]
    pub injections: Vec<LibraryInjection>,
    #[serde(default)]
    pub replacements: Vec<FileReplacement>,
    /// Stream the signed IPA straight into the device upload instead of writing a temporary file.
    pub stream_upload: bool,
    /// AFC write size in MiB (default 1).
    pub upload_chunk_mib: Option<u32>,
    /// Provision Apple TV targets for tvOS (`subPlatform=tvOS`).
    pub tvos_for_apple_tv: bool,
    /// Register an App ID and profile for each extension instead of the recovered single
    /// main-app profile (uses one free-team App ID per extension).
    #[serde(default)]
    pub provision_extensions: bool,
    /// Remember this job for automatic refresh (Apple ID + device targets only).
    pub track_for_refresh: bool,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            bundle_id: BundleIdPolicy::Auto,
            display_name: None,
            version: None,
            short_version: None,
            minimum_os: None,
            remove_device_restrictions: false,
            enable_file_sharing: false,
            extra_info: Vec::new(),
            remove_extensions: ExtensionRemoval::Keep,
            remove_watch_app: true,
            icon: None,
            entitlements: None,
            injections: Vec::new(),
            replacements: Vec::new(),
            stream_upload: false,
            upload_chunk_mib: None,
            tvos_for_apple_tv: false,
            provision_extensions: false,
            track_for_refresh: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub source: PathBuf,
    pub target: Target,
    pub signing: SigningMode,
    pub options: AppOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobOutcome {
    pub bundle_id: String,
    /// Path of the exported IPA, if any.
    pub exported_to: Option<PathBuf>,
    /// Profile expiry, when provisioned with an Apple ID.
    pub expires: Option<DateTime<Utc>>,
    /// Id of the tracked installation, when `track_for_refresh` applied.
    pub installation_id: Option<i64>,
}

// ------------------------------------------------------------------------------------------------
// Installations (refresh tracking)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Installation {
    pub id: i64,
    pub app_name: String,
    pub bundle_id: String,
    pub original_bundle_id: String,
    pub version: Option<String>,
    pub device_udid: String,
    pub device_name: String,
    pub apple_id: String,
    pub team_id: String,
    pub installed_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub auto_refresh: bool,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    pub icon_png: Option<Vec<u8>>,
    /// The job that produced it (replayed on refresh).
    pub spec: JobSpec,
}

impl Installation {
    /// Days until expiry (negative when expired).
    pub fn days_left(&self, now: DateTime<Utc>) -> Option<i64> {
        self.expires_at.map(|e| (e - now).num_days())
    }
}

// ------------------------------------------------------------------------------------------------
// Device contents

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceApp {
    pub bundle_id: String,
    pub name: String,
    pub version: Option<String>,
    /// Installed with a development/ad-hoc profile (i.e. sideloaded rather than from the App Store).
    pub is_developer_app: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub uuid: String,
    pub name: String,
    pub app_id: Option<String>,
    pub team_id: Option<String>,
    pub expires: Option<DateTime<Utc>>,
    pub is_free: bool,
}

// ------------------------------------------------------------------------------------------------
// Settings

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AnisetteSetting {
    /// Use this Mac's own provisioning (macOS only).
    #[default]
    Local,
    /// A server returning anisette headers as JSON (`GET <url>`).
    Remote { url: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RefreshSettings {
    pub enabled: bool,
    /// Refresh when fewer than this many hours remain.
    pub threshold_hours: u32,
    pub check_interval_minutes: u32,
    /// Allow refreshing over Wi-Fi (device screen must be on).
    pub allow_network: bool,
}

impl Default for RefreshSettings {
    fn default() -> Self {
        Self { enabled: true, threshold_hours: 48, check_interval_minutes: 30, allow_network: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub anisette: AnisetteSetting,
    /// Optional provider tried after a GSA complete-operation anisette mismatch.
    pub alternate_anisette: Option<AnisetteSetting>,
    pub refresh: RefreshSettings,
    /// Default for [`AppOptions::stream_upload`].
    pub stream_upload: bool,
    pub theme: ThemePreference,
    /// Default for the "remember password" checkbox.
    pub remember_passwords: bool,
}
