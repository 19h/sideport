//! `sideport-tray`: the refresh scheduler with a menu-bar menu, as the recovered
//! `sideloadly-daemon` runs its scheduler behind a tray icon.

#[cfg(any(target_os = "macos", windows))]
mod native;

use clap::Parser;
use sl_engine::{Engine, EngineConfig};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "sideport-tray", about = "Refresh tracked apps in the background from the menu bar")]
struct Arguments {
    /// Override the directory containing settings and account state.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Accepted so a login item written for `sideport daemon` can start the tray.
    #[arg(hide = true)]
    mode: Option<String>,
    /// Print the menu the current state produces and exit (no menu-bar icon, no scheduler).
    #[arg(long, hide = true)]
    print_menu: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let arguments = Arguments::parse();
    let config = EngineConfig {
        data_dir: arguments.data_dir,
        disable_scheduler: arguments.print_menu,
        ..EngineConfig::default()
    };
    let engine = Engine::new(config)?;

    #[cfg(any(target_os = "macos", windows))]
    if arguments.print_menu {
        return native::print_menu(engine);
    }

    #[cfg(any(target_os = "macos", windows))]
    return native::run(engine);

    #[cfg(not(any(target_os = "macos", windows)))]
    {
        drop(engine);
        anyhow::bail!("the menu-bar daemon runs on macOS and Windows; run `sideport daemon` here instead")
    }
}
