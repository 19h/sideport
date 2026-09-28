//! Command implementations.

use crate::jobs::{self, Answers};
use crate::{
    AccountCommand, Cli, Command, DeviceCommand, ExportArgs, ExportMode, Fixtures, InstallArgs, InstallMode,
    InstallationCommand, SettingsCommand,
};
use anyhow::{Context, Result, bail};
use futures::executor::block_on;
use serde::Serialize;
use sl_engine::{AnisetteSetting, Engine, EngineConfig, JobOutcome, JobSpec, SigningMode, Target};
use std::fs::File;
use std::io::{BufRead, Read};

pub fn dispatch(cli: Cli) -> Result<()> {
    let scheduler = matches!(cli.command, Command::Daemon);
    let engine = engine(cli.data_dir.clone(), &cli.fixtures, scheduler)?;
    let json = cli.json;

    match cli.command {
        Command::Inspect { source } => inspect(&engine, source, json),
        Command::Export(arguments) => {
            outcome(jobs::run(engine.start(export_spec(*arguments)), &Answers::default())?, json)
        }
        Command::Install(arguments) => {
            outcome(jobs::run(engine.start(install_spec(*arguments)), &Answers::default())?, json)
        }
        Command::Run { spec } => outcome(jobs::run(engine.start(read_spec(&spec)?), &Answers::default())?, json),
        Command::Anisette { remote, local } => anisette(&engine, remote.filter(|_| !local), json),
        Command::Settings(command) => settings(&engine, command, json),
        Command::Account(command) => account(&engine, command, json),
        Command::Certificates { apple_id, revoke } => certificates(&engine, apple_id, revoke, json),
        Command::AppIds { apple_id } => {
            print(&jobs::run(engine.app_ids(apple_id), &Answers::default())?, json, |app_ids| {
                for app_id in app_ids {
                    let expiry =
                        app_id.expires.map(|date| format!(" expires {}", date.format("%Y-%m-%d"))).unwrap_or_default();
                    println!("{}  {}  {}{expiry}", app_id.app_id_id, app_id.identifier, app_id.name);
                }
            })
        }
        Command::RegisteredDevices { apple_id } => {
            print(&jobs::run(engine.registered_devices(apple_id), &Answers::default())?, json, |devices| {
                for device in devices {
                    println!("{}  {}", device.udid, device.name);
                }
            })
        }
        Command::Devices => devices(&engine, json),
        Command::Device(command) => device(&engine, command, json),
        Command::Installations => installations(&engine, json),
        Command::Installation(command) => installation(&engine, command, json),
        Command::RefreshDue => {
            let refreshed = block_on(engine.refresh_due())?;

            print(&serde_json::json!({ "refreshed": refreshed }), json, |_| println!("Refreshed {refreshed}"))
        }
        Command::Daemon => daemon(engine),
    }
}

fn engine(data_dir: Option<std::path::PathBuf>, fixtures: &Fixtures, scheduler: bool) -> Result<Engine> {
    let profile_trust = match &fixtures.profile_anchors {
        Some(path) => {
            let pem = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
            let trust = sl_codesign::ProfileTrust::from_pem(
                &pem,
                "Apple iPhone OS Provisioning Profile Signing",
                "Apple iPhone Certification Authority",
            )?;

            Some(trust)
        }
        None => None,
    };

    let config = EngineConfig {
        data_dir,
        auth_origin: fixtures.auth_origin.clone(),
        portal_origin: fixtures.portal_origin.clone(),
        profile_trust,
        file_secrets: fixtures.file_secrets,
        disable_scheduler: !scheduler,
        ..EngineConfig::default()
    };

    Ok(Engine::new(config)?)
}

fn export_spec(arguments: ExportArgs) -> JobSpec {
    let apple_id = arguments.signing == ExportMode::AppleId;
    let options = arguments.edits.options(apple_id);

    let signing = match arguments.signing {
        ExportMode::AdHoc => SigningMode::AdHoc,
        ExportMode::Unsigned => SigningMode::Unsigned,
        ExportMode::Original => SigningMode::Original,
        ExportMode::AppleId => SigningMode::AppleId { apple_id: arguments.apple_id.unwrap_or_default() },
    };

    JobSpec { source: arguments.source, target: Target::ExportIpa { path: arguments.output }, signing, options }
}

fn install_spec(arguments: InstallArgs) -> JobSpec {
    let apple_id = arguments.signing == InstallMode::AppleId;
    let mut options = arguments.edits.options(apple_id);

    options.track_for_refresh = arguments.track;
    options.stream_upload = arguments.stream;
    options.upload_chunk_mib = arguments.chunk_mib;
    options.tvos_for_apple_tv = arguments.tvos;

    let signing = match arguments.signing {
        InstallMode::AppleId => SigningMode::AppleId { apple_id: arguments.apple_id.unwrap_or_default() },
        InstallMode::AdHoc => SigningMode::AdHoc,
        InstallMode::Original => SigningMode::Original,
    };
    let target = Target::Device { udid: arguments.device, prefer_network: arguments.network };

    JobSpec { source: arguments.source, target, signing, options }
}

fn read_spec(path: &std::path::Path) -> Result<JobSpec> {
    let mut bytes = Vec::new();

    File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;

    if bytes.len() > 1024 * 1024 {
        bail!("job specification exceeds 1 MiB");
    }

    serde_json::from_slice(&bytes).context("decode JobSpec JSON")
}

/// JSON to stdout with `--json`, else the text rendering.
fn print<T: Serialize>(value: &T, json: bool, text: impl FnOnce(&T)) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        text(value);
    }

    Ok(())
}

fn outcome(outcome: JobOutcome, json: bool) -> Result<()> {
    print(&outcome, json, |outcome| {
        if let Some(path) = &outcome.exported_to {
            println!("{}", path.display());
        } else {
            println!("Installed {}", outcome.bundle_id);
        }

        if let Some(expires) = outcome.expires {
            println!("Expires {}", expires.format("%Y-%m-%d %H:%M UTC"));
        }

        if let Some(id) = outcome.installation_id {
            println!("Tracked as installation {id}");
        }
    })
}

fn inspect(engine: &Engine, source: std::path::PathBuf, json: bool) -> Result<()> {
    let mut app = block_on(engine.inspect(source))?;

    if json {
        // Icon pixels belong to the GUI; CLI metadata stays compact.
        app.icon_png = None;
        println!("{}", serde_json::to_string_pretty(&app)?);

        return Ok(());
    }

    println!("{} ({})", app.name, app.bundle_id);
    println!(
        "Version: {} ({})",
        app.short_version.as_deref().unwrap_or("unknown"),
        app.version.as_deref().unwrap_or("unknown")
    );
    println!("Minimum OS: {}", app.minimum_os.as_deref().unwrap_or("unknown"));
    println!("Input bytes: {}", app.file_size);
    println!("Extensions: {}; Watch app: {}; encrypted: {}", app.extensions.len(), app.has_watch_app, app.encrypted);

    for warning in app.warnings {
        eprintln!("{warning}");
    }

    Ok(())
}

fn anisette(engine: &Engine, remote: Option<String>, json: bool) -> Result<()> {
    let setting = remote.map_or(AnisetteSetting::Local, |url| AnisetteSetting::Remote { url });
    let description = block_on(engine.test_anisette(setting))?;

    print(&serde_json::json!({ "description": description }), json, |_| println!("{description}"))
}

fn settings(engine: &Engine, command: SettingsCommand, json: bool) -> Result<()> {
    let mut settings = engine.settings();

    match command {
        SettingsCommand::Show => {}

        SettingsCommand::Anisette { remote, local } => {
            settings.anisette = match (remote, local) {
                (Some(url), _) => AnisetteSetting::Remote { url },
                (None, true) => AnisetteSetting::Local,
                (None, false) => bail!("pass --remote URL or --local"),
            };
        }

        SettingsCommand::AlternateAnisette { remote, none } => {
            settings.alternate_anisette = match (remote, none) {
                (Some(url), _) => Some(AnisetteSetting::Remote { url }),
                (None, true) => None,
                (None, false) => bail!("pass --remote URL or --none"),
            };
        }

        SettingsCommand::Refresh { enabled, threshold_hours, interval_minutes, allow_network } => {
            let refresh = &mut settings.refresh;

            refresh.enabled = enabled.unwrap_or(refresh.enabled);
            refresh.threshold_hours = threshold_hours.unwrap_or(refresh.threshold_hours);
            refresh.check_interval_minutes = interval_minutes.unwrap_or(refresh.check_interval_minutes).max(1);
            refresh.allow_network = allow_network.unwrap_or(refresh.allow_network);
        }
    }

    engine.update_settings(settings.clone())?;

    print(&settings, json, |settings| println!("{settings:#?}"))
}

fn account(engine: &Engine, command: AccountCommand, json: bool) -> Result<()> {
    match command {
        AccountCommand::List => print(&engine.accounts()?, json, |accounts| {
            for account in accounts {
                let session = if account.has_session { "signed in" } else { "session needed" };
                let remembered = if account.remembers_password { ", password remembered" } else { "" };
                println!("{} ({session}{remembered})", account.apple_id);

                for team in &account.teams {
                    let default = if account.default_team.as_ref() == Some(&team.team_id) { " [default]" } else { "" };
                    println!("  {} {} {:?}{default}", team.team_id, team.name, team.kind);
                }
            }
        }),

        AccountCommand::Login { apple_id, remember, password_stdin } => {
            let password = if password_stdin {
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line).context("read password from stdin")?;

                Some(line.trim_end_matches(['\r', '\n']).to_owned())
            } else {
                None
            };

            let summary = jobs::run(engine.login(apple_id, password, remember), &Answers { remember })?;

            print(&summary, json, |summary| {
                println!("Signed in as {} ({} team(s))", summary.apple_id, summary.teams.len());
            })
        }

        AccountCommand::Logout { apple_id } => {
            block_on(engine.logout(apple_id.clone()))?;

            print(&serde_json::json!({ "signed_out": apple_id }), json, |_| println!("Signed out {apple_id}"))
        }

        AccountCommand::Import { path } => {
            let path = path
                .or_else(Engine::recovered_sessions_path)
                .context("no Sideloadly sessions.json location on this platform; pass a path")?;
            let report = engine.import_sessions(path)?;

            print(&report, json, |report| {
                for apple_id in &report.imported {
                    println!("Imported {apple_id}");
                }

                for (key, reason) in &report.skipped {
                    println!("Skipped {key}: {reason}");
                }
            })
        }
    }
}

fn certificates(engine: &Engine, apple_id: String, revoke: Option<String>, json: bool) -> Result<()> {
    if let Some(serial) = revoke {
        jobs::run(engine.revoke_certificate(apple_id, serial.clone()), &Answers::default())?;

        return print(&serde_json::json!({ "revoked": serial }), json, |_| println!("Revoked {serial}"));
    }

    print(&jobs::run(engine.certificates(apple_id), &Answers::default())?, json, |certificates| {
        for certificate in certificates {
            let ours = if certificate.is_ours { " [this computer]" } else { "" };
            let machine = certificate.machine_name.as_deref().unwrap_or("unknown machine");
            let expiry =
                certificate.expires.map(|date| format!(" expires {}", date.format("%Y-%m-%d"))).unwrap_or_default();

            println!("{}  {}  ({machine}){expiry}{ours}", certificate.serial, certificate.name);
        }
    })
}

fn devices(engine: &Engine, json: bool) -> Result<()> {
    print(&block_on(engine.devices())?, json, |devices| {
        if devices.is_empty() {
            println!("No devices connected");
        }

        for device in devices {
            let model = device.model_name.as_deref().unwrap_or(&device.product_type);
            let links: Vec<_> = device.connections.iter().map(|connection| format!("{connection:?}")).collect();
            let paired = if device.paired { "" } else { " (not paired: run `sideport device pair`)" };

            println!(
                "{}  {}  {model}  {} {}  [{}]{paired}",
                device.udid,
                device.name,
                device.device_class,
                device.os_version,
                links.join(", ")
            );
        }
    })
}

fn device(engine: &Engine, command: DeviceCommand, json: bool) -> Result<()> {
    match command {
        DeviceCommand::Apps { udid } => print(&block_on(engine.device_apps(udid))?, json, |apps| {
            for app in apps {
                let developer = if app.is_developer_app { " [developer]" } else { "" };
                println!("{}  {} {}{developer}", app.bundle_id, app.name, app.version.as_deref().unwrap_or(""));
            }
        }),

        DeviceCommand::Uninstall { udid, bundle_id } => {
            block_on(engine.uninstall_app(udid, bundle_id.clone()))?;

            print(&serde_json::json!({ "uninstalled": bundle_id }), json, |_| println!("Uninstalled {bundle_id}"))
        }

        DeviceCommand::Profiles { udid } => print(&block_on(engine.device_profiles(udid))?, json, |profiles| {
            for profile in profiles {
                let free = if profile.is_free { " [free]" } else { "" };
                let expiry =
                    profile.expires.map(|date| format!(" expires {}", date.format("%Y-%m-%d"))).unwrap_or_default();
                println!(
                    "{}  {}  {}{expiry}{free}",
                    profile.uuid,
                    profile.name,
                    profile.app_id.as_deref().unwrap_or("")
                );
            }
        }),

        DeviceCommand::RemoveProfile { udid, uuid } => {
            block_on(engine.remove_profile(udid, uuid.clone()))?;

            print(&serde_json::json!({ "removed": uuid }), json, |_| println!("Removed profile {uuid}"))
        }

        DeviceCommand::Pair { udid } => {
            eprintln!("Unlock the device and confirm \"Trust This Computer?\".");
            block_on(engine.pair_device(udid.clone()))?;

            print(&serde_json::json!({ "paired": udid }), json, |_| println!("Paired {udid}"))
        }
    }
}

fn installations(engine: &Engine, json: bool) -> Result<()> {
    let mut installations = engine.installations()?;

    for installation in &mut installations {
        installation.icon_png = None;
    }

    let now = chrono::Utc::now();

    print(&installations, json, |installations| {
        if installations.is_empty() {
            println!("No tracked installations");
        }

        for installation in installations {
            let days = installation.days_left(now).map_or("unknown".into(), |days| format!("{days} days left"));
            let refresh = if installation.auto_refresh { "auto" } else { "manual" };
            let failure = if installation.consecutive_failures >= 3 { " [FAIL]" } else { "" };

            println!(
                "{}  {} ({})  on {}  {days}  {refresh}{failure}",
                installation.id, installation.app_name, installation.bundle_id, installation.device_name
            );

            if let Some(error) = &installation.last_error {
                println!("    last error: {error}");
            }
        }
    })
}

fn installation(engine: &Engine, command: InstallationCommand, json: bool) -> Result<()> {
    match command {
        InstallationCommand::Refresh { id } => outcome(jobs::run(engine.refresh(id), &Answers::default())?, json),

        InstallationCommand::Forget { id } => {
            engine.forget_installation(id)?;

            print(&serde_json::json!({ "forgotten": id }), json, |_| println!("Forgot installation {id}"))
        }

        InstallationCommand::AutoRefresh { id, enabled } => {
            engine.set_auto_refresh(id, enabled)?;

            print(&serde_json::json!({ "id": id, "auto_refresh": enabled }), json, |_| {
                println!("Automatic refresh {} for installation {id}", if enabled { "enabled" } else { "disabled" });
            })
        }
    }
}

/// Keep the engine and its scheduler alive until SIGINT. Refresh events go to stderr.
fn daemon(engine: Engine) -> Result<()> {
    let events = engine.subscribe_refresh();
    let (stop, stopped) = std::sync::mpsc::channel();

    ctrlc::set_handler(move || {
        let _ = stop.send(());
    })
    .context("register interrupt handler")?;

    std::thread::spawn(move || {
        while let Ok(event) = block_on(events.recv()) {
            eprintln!("{event:?}");
        }
    });

    eprintln!("Refresh scheduler running; press Ctrl-C to stop.");
    let _ = stopped.recv();

    Ok(())
}
