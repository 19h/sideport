use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use sl_engine::{
    AnisetteSetting, AppOptions, BundleIdPolicy, Engine, EngineConfig, ExtensionRemoval, FileReplacement, InfoOverride,
    InfoValue, JobEvent, JobOutcome, JobSpec, LibraryInjection, LogLevel, PromptKind, PromptReply, SigningMode, Target,
};
use std::{
    fs::File,
    io::{IsTerminal, Read},
    path::PathBuf,
};

#[derive(Debug, Parser)]
#[command(name = "sideport", version, about = "Inspect, prepare, and export Apple app bundles")]
struct Cli {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[arg(long, global = true, help = "Print machine-readable results to stdout")]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read app metadata and executable headers without extracting the app.
    Inspect { source: PathBuf },
    /// Apply edits and export an IPA. Ad-hoc signatures do not grant Apple provisioning.
    Export(Box<ExportArgs>),
    /// Execute a serialized engine JobSpec.
    Run { spec: PathBuf },
    /// Check a configured anisette service and describe its machine.
    Anisette {
        #[arg(long, value_name = "URL")]
        remote: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ExportMode {
    AdHoc,
    Unsigned,
    Original,
}

#[derive(Debug, Args)]
struct ExportArgs {
    source: PathBuf,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "ad-hoc")]
    signing: ExportMode,
    #[arg(long)]
    bundle_id: Option<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    version: Option<String>,
    #[arg(long)]
    short_version: Option<String>,
    #[arg(long)]
    minimum_os: Option<String>,
    #[arg(long)]
    file_sharing: bool,
    #[arg(long)]
    remove_device_restrictions: bool,
    #[arg(long, conflicts_with = "remove_extension")]
    remove_all_extensions: bool,
    #[arg(long, value_name = "NAME.appex")]
    remove_extension: Vec<String>,
    #[arg(long)]
    keep_watch_app: bool,
    #[arg(long, value_name = "FILE_OR_DIRECTORY")]
    inject: Vec<PathBuf>,
    #[arg(long, value_name = "TARGET=SOURCE", value_parser = parse_replacement)]
    replace: Vec<FileReplacement>,
    #[arg(long, value_name = "TARGET")]
    remove_file: Vec<PathBuf>,
    #[arg(long, value_name = "KEY=TEXT", value_parser = parse_string)]
    set: Vec<InfoOverride>,
    #[arg(long, value_name = "KEY=true|false", value_parser = parse_bool)]
    set_bool: Vec<InfoOverride>,
    #[arg(long, value_name = "KEY=INTEGER", value_parser = parse_integer)]
    set_integer: Vec<InfoOverride>,
    #[arg(long, value_name = "KEY")]
    remove_key: Vec<String>,
}

impl ExportArgs {
    fn into_spec(self) -> JobSpec {
        let mut replacements = self.replace;
        replacements.extend(self.remove_file.into_iter().map(|target| FileReplacement { target, source: None }));

        let mut extra_info = self.set;
        extra_info.extend(self.set_bool);
        extra_info.extend(self.set_integer);
        extra_info.extend(self.remove_key.into_iter().map(|key| InfoOverride { key, value: InfoValue::Remove }));

        let remove_extensions = if self.remove_all_extensions {
            ExtensionRemoval::All
        } else if self.remove_extension.is_empty() {
            ExtensionRemoval::Keep
        } else {
            ExtensionRemoval::Selected(self.remove_extension)
        };
        let options = AppOptions {
            bundle_id: self.bundle_id.map(BundleIdPolicy::Custom).unwrap_or(BundleIdPolicy::Original),
            display_name: self.name,
            version: self.version,
            short_version: self.short_version,
            minimum_os: self.minimum_os,
            enable_file_sharing: self.file_sharing,
            remove_device_restrictions: self.remove_device_restrictions,
            remove_extensions,
            remove_watch_app: !self.keep_watch_app,
            injections: self.inject.into_iter().map(|source| LibraryInjection { source, name: None }).collect(),
            replacements,
            extra_info,
            ..AppOptions::default()
        };
        let signing = match self.signing {
            ExportMode::AdHoc => SigningMode::AdHoc,
            ExportMode::Unsigned => SigningMode::Unsigned,
            ExportMode::Original => SigningMode::Original,
        };

        JobSpec { source: self.source, target: Target::ExportIpa { path: self.output }, signing, options }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let engine =
        Engine::new(EngineConfig { data_dir: cli.data_dir, disable_scheduler: true, ..EngineConfig::default() })?;

    match cli.command {
        Command::Inspect { source } => {
            let mut app = futures::executor::block_on(engine.inspect(source))?;

            if cli.json {
                // Icon pixels belong to the GUI; CLI metadata stays compact.
                app.icon_png = None;
                println!("{}", serde_json::to_string_pretty(&app)?);
            } else {
                println!("{} ({})", app.name, app.bundle_id);
                println!(
                    "Version: {} ({})",
                    app.short_version.as_deref().unwrap_or("unknown"),
                    app.version.as_deref().unwrap_or("unknown")
                );
                println!("Minimum OS: {}", app.minimum_os.as_deref().unwrap_or("unknown"));
                println!("Input bytes: {}", app.file_size);
                println!(
                    "Extensions: {}; Watch app: {}; encrypted: {}",
                    app.extensions.len(),
                    app.has_watch_app,
                    app.encrypted
                );

                for warning in app.warnings {
                    eprintln!("{warning}");
                }
            }
        }
        Command::Export(arguments) => print_outcome(run(&engine, arguments.into_spec())?, cli.json)?,
        Command::Anisette { remote } => {
            let setting = AnisetteSetting::Remote { url: remote };
            let description = futures::executor::block_on(engine.test_anisette(setting))?;

            if cli.json {
                println!("{}", serde_json::json!({ "description": description }));
            } else {
                println!("{description}");
            }
        }
        Command::Run { spec } => {
            let mut bytes = Vec::new();
            File::open(&spec)
                .with_context(|| format!("open {}", spec.display()))?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .with_context(|| format!("read {}", spec.display()))?;

            if bytes.len() > 1024 * 1024 {
                bail!("job specification exceeds 1 MiB");
            }

            let spec = serde_json::from_slice(&bytes).context("decode JobSpec JSON")?;
            print_outcome(run(&engine, spec)?, cli.json)?;
        }
    }

    Ok(())
}

fn run(engine: &Engine, spec: JobSpec) -> Result<JobOutcome> {
    let job = engine.start(spec);
    let events = job.events();
    let cancellation = job.cancellation_token();
    ctrlc::set_handler(move || cancellation.cancel()).context("register cancellation handler")?;

    let progress = if std::io::stderr().is_terminal() { ProgressBar::new_spinner() } else { ProgressBar::hidden() };
    progress
        .set_style(ProgressStyle::with_template("{spinner:.cyan} {msg} {wide_bar:.cyan/blue} {bytes}/{total_bytes}")?);
    progress.enable_steady_tick(std::time::Duration::from_millis(100));

    let outcome = futures::executor::block_on(async {
        while let Ok(event) = events.recv().await {
            match event {
                JobEvent::Stage(stage) => {
                    if progress.is_hidden() {
                        eprintln!("{}", stage.label());
                    }
                    progress.set_message(stage.label());
                    progress.set_length(0);
                    progress.set_position(0);
                }
                JobEvent::Progress { done, total } => {
                    progress.set_length(total);
                    progress.set_position(done);
                }
                JobEvent::Log { level, message } if level >= LogLevel::Info => {
                    if progress.is_hidden() {
                        eprintln!("{message}");
                    } else {
                        progress.println(message);
                    }
                }
                JobEvent::Prompt(prompt) => {
                    let reply = match &prompt.kind {
                        PromptKind::SaveFile { suggested_name } if std::io::stdin().is_terminal() => progress
                            .suspend(|| {
                                dialoguer::Input::<String>::new()
                                    .with_prompt("Export IPA to")
                                    .default(suggested_name.clone())
                                    .interact_text()
                            })
                            .map(|value| PromptReply::Path(value.into()))
                            .unwrap_or(PromptReply::Cancel),
                        _ => {
                            eprintln!("An interactive answer is required; pass --output for unattended exports.");
                            PromptReply::Cancel
                        }
                    };
                    prompt.answer(reply);
                }
                _ => {}
            }
        }

        job.result().await
    });
    progress.finish_and_clear();

    Ok(outcome?)
}

fn print_outcome(outcome: JobOutcome, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else if let Some(path) = outcome.exported_to {
        println!("{}", path.display());
    }

    Ok(())
}

fn key_value(argument: &str) -> std::result::Result<(String, &str), String> {
    let (key, value) = argument.split_once('=').ok_or_else(|| "expected KEY=VALUE".to_string())?;

    if key.is_empty() || key.contains('\0') {
        return Err("key must be nonempty and contain no NUL".into());
    }

    Ok((key.into(), value))
}

fn parse_string(argument: &str) -> std::result::Result<InfoOverride, String> {
    let (key, value) = key_value(argument)?;

    Ok(InfoOverride { key, value: InfoValue::String(value.into()) })
}

fn parse_bool(argument: &str) -> std::result::Result<InfoOverride, String> {
    let (key, value) = key_value(argument)?;
    let value = value.parse::<bool>().map_err(|_| "boolean must be true or false".to_string())?;

    Ok(InfoOverride { key, value: InfoValue::Bool(value) })
}

fn parse_integer(argument: &str) -> std::result::Result<InfoOverride, String> {
    let (key, value) = key_value(argument)?;
    let value = value.parse::<i64>().map_err(|_| "integer must fit signed 64-bit range".to_string())?;

    Ok(InfoOverride { key, value: InfoValue::Integer(value) })
}

fn parse_replacement(argument: &str) -> std::result::Result<FileReplacement, String> {
    let (target, source) = key_value(argument)?;

    if source.is_empty() {
        return Err("replacement source must be nonempty; use --remove-file to delete".into());
    }

    Ok(FileReplacement { target: target.into(), source: Some(source.into()) })
}
