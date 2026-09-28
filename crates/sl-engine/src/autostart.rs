//! Starting the refresh scheduler at login (recovered LaunchAgent `io.sideloadly.daemon`,
//! `RunAtLoad`, and its autostart toggle).
//!
//! macOS: `~/Library/LaunchAgents/io.sideport.refresh.plist` running `<program> daemon`.
//! Linux: `$XDG_CONFIG_HOME/autostart/io.sideport.refresh.desktop`. The entry takes effect at
//! the next login; loading it immediately is left to the caller (`launchctl bootstrap`).

use crate::error::{EngineError, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub const LABEL: &str = "io.sideport.refresh";

/// Where login items live on this platform, or `None` where autostart is unsupported.
pub fn default_directory() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return dirs::home_dir().map(|home| home.join("Library/LaunchAgents"));
    }

    if cfg!(target_os = "linux") {
        return dirs::config_dir().map(|config| config.join("autostart"));
    }

    None
}

fn entry(directory: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        directory.join(format!("{LABEL}.plist"))
    } else {
        directory.join(format!("{LABEL}.desktop"))
    }
}

pub fn is_enabled(directory: &Path) -> bool {
    entry(directory).is_file()
}

/// Write or remove the login item that runs `<program> [--data-dir DIR] daemon`.
pub fn set(directory: &Path, program: &Path, data_dir: Option<&Path>, enabled: bool) -> Result<()> {
    let path = entry(directory);

    if !enabled {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(storage(error)),
        };
    }

    if !program.is_absolute() {
        return Err(EngineError::Storage("the autostart program must be an absolute path".into()));
    }

    let mut arguments = vec![program.to_string_lossy().into_owned()];

    if let Some(data_dir) = data_dir {
        arguments.push("--data-dir".into());
        arguments.push(data_dir.to_string_lossy().into_owned());
    }

    arguments.push("daemon".into());

    let contents = if cfg!(target_os = "macos") { launch_agent(&arguments)? } else { desktop_entry(&arguments) };

    fs::create_dir_all(directory).map_err(storage)?;

    let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(storage)?;
    std::io::Write::write_all(&mut temporary, &contents).map_err(storage)?;
    temporary.persist(&path).map_err(|error| storage(error.error))?;

    Ok(())
}

fn launch_agent(arguments: &[String]) -> Result<Vec<u8>> {
    let mut agent = plist::Dictionary::new();
    agent.insert("Label".into(), LABEL.into());
    agent.insert("ProgramArguments".into(), plist::Value::Array(arguments.iter().cloned().map(Into::into).collect()));
    agent.insert("RunAtLoad".into(), true.into());
    agent.insert("ProcessType".into(), "Background".into());

    let mut keep_alive = plist::Dictionary::new();
    keep_alive.insert("SuccessfulExit".into(), false.into());
    agent.insert("KeepAlive".into(), plist::Value::Dictionary(keep_alive));

    let mut bytes = Vec::new();
    plist::Value::Dictionary(agent)
        .to_writer_xml(&mut bytes)
        .map_err(|error| EngineError::Storage(error.to_string()))?;

    Ok(bytes)
}

/// XDG desktop entry; arguments are quoted per the Desktop Entry `Exec` rules.
fn desktop_entry(arguments: &[String]) -> Vec<u8> {
    let quoted: Vec<String> = arguments
        .iter()
        .map(|argument| {
            let escaped = argument.replace('\\', "\\\\").replace('"', "\\\"").replace('`', "\\`").replace('$', "\\$");

            format!("\"{escaped}\"")
        })
        .collect();

    format!(
        "[Desktop Entry]\nType=Application\nName=Sideport refresh\nExec={}\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
        quoted.join(" ")
    )
    .into_bytes()
}

fn storage(error: std::io::Error) -> EngineError {
    EngineError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabling_writes_a_login_item_that_runs_the_daemon_and_disabling_removes_it() {
        let directory = tempfile::tempdir().expect("tempdir");
        let agents = directory.path().join("LaunchAgents");
        let program = Path::new("/Applications/Sideport.app/Contents/MacOS/sideport");
        let data_dir = Path::new("/Users/fixture/Library/Application Support/Sideport");

        assert!(!is_enabled(&agents));
        set(&agents, program, Some(data_dir), true).expect("enable");
        assert!(is_enabled(&agents));

        let written = fs::read(entry(&agents)).expect("entry");

        if cfg!(target_os = "macos") {
            let value = plist::Value::from_reader_xml(written.as_slice()).expect("plist");
            let agent = value.as_dictionary().expect("dictionary");
            let arguments: Vec<_> = agent["ProgramArguments"]
                .as_array()
                .expect("arguments")
                .iter()
                .filter_map(plist::Value::as_string)
                .collect();

            assert_eq!(agent["Label"].as_string(), Some(LABEL));
            assert_eq!(agent["RunAtLoad"].as_boolean(), Some(true));
            assert_eq!(
                arguments,
                [program.to_str().expect("path"), "--data-dir", data_dir.to_str().expect("path"), "daemon"]
            );
        } else {
            let text = String::from_utf8(written).expect("desktop entry");
            assert!(text.contains("\"--data-dir\" \"/Users/fixture/Library/Application Support/Sideport\" \"daemon\""));
        }

        set(&agents, program, None, false).expect("disable");
        set(&agents, program, None, false).expect("disable twice");
        assert!(!is_enabled(&agents));

        assert!(set(&agents, Path::new("relative/sideport"), None, true).is_err());
    }
}
