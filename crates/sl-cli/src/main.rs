//! `sideport`: scriptable access to the Sideport engine.

mod commands;
mod edits;
mod jobs;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use edits::EditArgs;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "sideport", version, about = "Inspect, sign, export and install Apple app bundles")]
struct Cli {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[arg(long, global = true, help = "Print machine-readable results to stdout")]
    json: bool,
    #[command(flatten)]
    fixtures: Fixtures,
    #[command(subcommand)]
    command: Command,
}

/// Service overrides for controlled fixtures; not part of the user interface.
#[derive(Debug, Args)]
struct Fixtures {
    #[arg(long, global = true, hide = true)]
    auth_origin: Option<String>,
    #[arg(long, global = true, hide = true)]
    portal_origin: Option<String>,
    /// PEM anchors for downloaded profiles, with Apple's signer names.
    #[arg(long, global = true, hide = true)]
    profile_anchors: Option<PathBuf>,
    /// Keep secrets in a 0600 file under the data directory instead of the keychain.
    #[arg(long, global = true, hide = true)]
    file_secrets: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read app metadata and executable headers without extracting the app.
    Inspect { source: PathBuf },
    /// Apply edits, sign, and export an IPA. Ad-hoc signatures do not grant Apple provisioning.
    Export(Box<ExportArgs>),
    /// Apply edits, sign, and install on a connected device.
    Install(Box<InstallArgs>),
    /// Execute a serialized engine JobSpec.
    Run { spec: PathBuf },
    /// Download a `sideloadly:` link or HTTP(S) IPA URL. Sources of other commands may also be links.
    Download {
        link: String,
        /// Copy the (unflipped) IPA here instead of printing the cached path.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Check an anisette provider and describe the machine Apple will list.
    Anisette {
        #[arg(long, value_name = "URL", required_unless_present = "local", conflicts_with = "local")]
        remote: Option<String>,
        /// This Mac's own provisioning (AOSKit).
        #[arg(long)]
        local: bool,
    },
    /// Show or change settings.
    #[command(subcommand)]
    Settings(SettingsCommand),
    /// Apple ID accounts.
    #[command(subcommand)]
    Account(AccountCommand),
    /// Development certificates of an account's team.
    Certificates {
        apple_id: String,
        /// Revoke the certificate with this serial number.
        #[arg(long, value_name = "SERIAL")]
        revoke: Option<String>,
    },
    /// App IDs of an account's team.
    AppIds { apple_id: String },
    /// Devices registered with an account's team.
    RegisteredDevices { apple_id: String },
    /// Connected devices.
    Devices,
    /// Apps, profiles and pairing of one device.
    #[command(subcommand)]
    Device(DeviceCommand),
    /// Installations tracked for automatic refresh.
    Installations,
    /// Refresh, forget or configure one installation.
    #[command(subcommand)]
    Installation(InstallationCommand),
    /// Refresh installations that are due and whose device is reachable, once.
    RefreshDue,
    /// Run the refresh scheduler until interrupted.
    Daemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ExportMode {
    AdHoc,
    Unsigned,
    Original,
    AppleId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum InstallMode {
    AppleId,
    AdHoc,
    Original,
}

#[derive(Debug, Args)]
struct ExportArgs {
    source: PathBuf,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "ad-hoc")]
    signing: ExportMode,
    /// Account for `--signing apple-id`.
    #[arg(long, required_if_eq("signing", "apple-id"))]
    apple_id: Option<String>,
    #[command(flatten)]
    edits: EditArgs,
}

#[derive(Debug, Args)]
struct InstallArgs {
    source: PathBuf,
    /// Device UDID (see `sideport devices`).
    #[arg(long)]
    device: String,
    /// Prefer a Wi-Fi connection when the device offers one.
    #[arg(long)]
    network: bool,
    #[arg(long, value_enum, default_value = "apple-id")]
    signing: InstallMode,
    /// Account for `--signing apple-id`.
    #[arg(long, required_if_eq("signing", "apple-id"))]
    apple_id: Option<String>,
    /// Record the installation for automatic refresh (Apple ID only).
    #[arg(long)]
    track: bool,
    /// Stream the signed IPA to the device instead of writing a temporary file.
    #[arg(long)]
    stream: bool,
    /// Upload chunk size in MiB (1–64).
    #[arg(long, value_name = "MIB")]
    chunk_mib: Option<u32>,
    /// Provision Apple TV targets for tvOS.
    #[arg(long)]
    tvos: bool,
    #[command(flatten)]
    edits: EditArgs,
}

#[derive(Debug, Subcommand)]
enum SettingsCommand {
    Show,
    /// Primary anisette provider.
    Anisette {
        #[arg(long, value_name = "URL", conflicts_with = "local")]
        remote: Option<String>,
        #[arg(long)]
        local: bool,
    },
    /// Provider used once after an anisette mismatch (-36607).
    AlternateAnisette {
        #[arg(long, value_name = "URL", conflicts_with = "none")]
        remote: Option<String>,
        #[arg(long)]
        none: bool,
    },
    Refresh {
        #[arg(long)]
        enabled: Option<bool>,
        #[arg(long)]
        threshold_hours: Option<u32>,
        #[arg(long)]
        interval_minutes: Option<u32>,
        #[arg(long)]
        allow_network: Option<bool>,
    },
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    List,
    /// Sign in; asks for the password and verification codes.
    Login {
        apple_id: String,
        /// Keep the password in the keychain for unattended refresh.
        #[arg(long)]
        remember: bool,
        /// Read the password from the first line of standard input.
        #[arg(long)]
        password_stdin: bool,
    },
    Logout {
        apple_id: String,
    },
    /// Import GrandSlam sessions from Sideloadly's sessions.json.
    Import {
        path: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum DeviceCommand {
    Apps {
        udid: String,
    },
    Uninstall {
        udid: String,
        bundle_id: String,
    },
    Profiles {
        udid: String,
    },
    RemoveProfile {
        udid: String,
        uuid: String,
    },
    /// Ask the device to trust this computer.
    Pair {
        udid: String,
    },
}

#[derive(Debug, Subcommand)]
enum InstallationCommand {
    Refresh {
        id: i64,
    },
    Forget {
        id: i64,
    },
    AutoRefresh {
        id: i64,
        #[arg(value_parser = clap::builder::BoolishValueParser::new())]
        enabled: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    commands::dispatch(cli)
}
