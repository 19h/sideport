//! App edits shared by `export` and `install`.

use clap::Args;
use sl_engine::{
    AppOptions, BundleIdPolicy, ExtensionRemoval, FileReplacement, InfoOverride, InfoValue, LibraryInjection,
};
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct EditArgs {
    /// Use exactly this bundle identifier (extensions keep their suffixes).
    #[arg(long, conflicts_with = "keep_bundle_id")]
    pub bundle_id: Option<String>,
    /// Apple ID signing: keep the original identifier even for free teams.
    #[arg(long)]
    pub keep_bundle_id: bool,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub version: Option<String>,
    #[arg(long)]
    pub short_version: Option<String>,
    #[arg(long)]
    pub minimum_os: Option<String>,
    #[arg(long)]
    pub file_sharing: bool,
    #[arg(long)]
    pub remove_device_restrictions: bool,
    #[arg(long, conflicts_with = "remove_extension")]
    pub remove_all_extensions: bool,
    #[arg(long, value_name = "NAME.appex")]
    pub remove_extension: Vec<String>,
    #[arg(long)]
    pub keep_watch_app: bool,
    #[arg(long, value_name = "FILE_OR_DIRECTORY")]
    pub inject: Vec<PathBuf>,
    #[arg(long, value_name = "TARGET=SOURCE", value_parser = parse_replacement)]
    pub replace: Vec<FileReplacement>,
    #[arg(long, value_name = "TARGET")]
    pub remove_file: Vec<PathBuf>,
    #[arg(long, value_name = "KEY=TEXT", value_parser = parse_string)]
    pub set: Vec<InfoOverride>,
    #[arg(long, value_name = "KEY=true|false", value_parser = parse_bool)]
    pub set_bool: Vec<InfoOverride>,
    #[arg(long, value_name = "KEY=INTEGER", value_parser = parse_integer)]
    pub set_integer: Vec<InfoOverride>,
    #[arg(long, value_name = "KEY")]
    pub remove_key: Vec<String>,
    /// Apple ID signing: merge this entitlements plist over the profile's entitlements.
    #[arg(long, value_name = "PLIST")]
    pub entitlements: Option<PathBuf>,
    /// Apple ID signing: register an App ID and profile for every extension.
    #[arg(long)]
    pub provision_extensions: bool,
}

impl EditArgs {
    /// `apple_id` selects the recovered automatic identifier policy as the default.
    pub fn options(self, apple_id: bool) -> AppOptions {
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

        let bundle_id = match (self.bundle_id, self.keep_bundle_id, apple_id) {
            (Some(identifier), _, _) => BundleIdPolicy::Custom(identifier),
            (None, false, true) => BundleIdPolicy::Auto,
            (None, _, _) => BundleIdPolicy::Original,
        };

        AppOptions {
            bundle_id,
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
            entitlements: self.entitlements,
            provision_extensions: self.provision_extensions,
            ..AppOptions::default()
        }
    }
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
