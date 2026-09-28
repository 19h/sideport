use sl_engine::{
    AppOptions, AppSummary, BundleIdPolicy, ExtensionRemoval, InfoOverride, InfoValue, JobSpec, SigningMode, Target,
};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

/// How the prepared app is signed. The name predates device installation; the mode applies to
/// both export and install targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportMode {
    #[default]
    Unsigned,
    AdHoc,
    Original,
    /// Provision with an Apple ID account and sign with its development certificate.
    AppleId,
}

impl ExportMode {
    /// Modes in the order the editor offers them.
    pub const ALL: [Self; 4] = [Self::AppleId, Self::AdHoc, Self::Unsigned, Self::Original];

    pub fn label(self) -> &'static str {
        match self {
            Self::Unsigned => "Unsigned",
            Self::AdHoc => "Ad-hoc signed",
            Self::Original => "Original",
            Self::AppleId => "Apple ID",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Unsigned => "Apply your edits and remove existing signatures. Unsigned apps can only be exported.",
            Self::AdHoc => "Apply your edits and create an ad-hoc signature. No Apple provisioning is added.",
            Self::Original => "Use the original app with its files and signatures unchanged.",
            Self::AppleId => {
                "Register the app with your Apple ID, then sign it with a development certificate and profile."
            }
        }
    }
}

/// How Apple ID signing chooses the bundle identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdentifierPolicy {
    /// Free teams append `.<TEAMID>`; paid teams keep the original identifier.
    #[default]
    Automatic,
    /// Always keep the app's identifier.
    Original,
    /// Use the bundle identifier field.
    Custom,
}

impl IdentifierPolicy {
    pub const ALL: [Self; 3] = [Self::Automatic, Self::Original, Self::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Automatic",
            Self::Original => "Keep original",
            Self::Custom => "Custom",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Automatic => "Free teams add the team ID to the identifier; paid teams keep the original.",
            Self::Original => "Always use the app's own identifier.",
            Self::Custom => "Use the bundle identifier entered under App details.",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub name: String,
    pub identifier: String,
    pub version: String,
    pub short_version: String,
    pub minimum_os: String,
    /// JSON object: strings, booleans, signed integers, or null (remove).
    pub extra_info: String,
    pub options: AppOptions,
    /// Account used for Apple ID signing.
    pub apple_id: Option<String>,
    pub identifier_policy: IdentifierPolicy,
    /// Upload chunk size in MiB as entered; blank uses the engine default.
    pub upload_chunk: String,
}

impl Draft {
    pub fn for_app(app: &AppSummary) -> Self {
        Self {
            name: app.name.clone(),
            identifier: app.bundle_id.clone(),
            version: app.version.clone().unwrap_or_default(),
            short_version: app.short_version.clone().unwrap_or_default(),
            minimum_os: app.minimum_os.clone().unwrap_or_default(),
            ..Self::default()
        }
    }

    /// An export job; `destination` `None` lets the engine ask for the output path.
    pub fn job(&self, app: &AppSummary, mode: ExportMode, destination: Option<PathBuf>) -> Result<JobSpec, String> {
        self.spec(app, mode, Target::ExportIpa { path: destination })
    }

    /// A job for either target. Options that do not apply to the mode or target are left at
    /// their defaults, so the engine never receives an option it would reject.
    pub fn spec(&self, app: &AppSummary, mode: ExportMode, target: Target) -> Result<JobSpec, String> {
        let installing = matches!(target, Target::Device { .. });

        if installing && mode == ExportMode::Unsigned {
            return Err("Unsigned apps cannot be installed. Choose another signing mode, or export the IPA.".into());
        }

        let signing = match mode {
            ExportMode::Unsigned => SigningMode::Unsigned,
            ExportMode::AdHoc => SigningMode::AdHoc,
            ExportMode::Original => SigningMode::Original,
            ExportMode::AppleId => {
                let apple_id = self.apple_id.as_deref().filter(|apple_id| !apple_id.is_empty());
                let apple_id = apple_id.ok_or_else(|| "Choose an Apple ID account to sign with.".to_string())?;

                SigningMode::AppleId { apple_id: apple_id.into() }
            }
        };

        let mut options = if mode == ExportMode::Original { AppOptions::default() } else { self.edits(app, mode)? };

        if installing {
            options.stream_upload = self.options.stream_upload;
            options.upload_chunk_mib = upload_chunk(&self.upload_chunk)?;
        }
        if installing && mode == ExportMode::AppleId {
            options.track_for_refresh = self.options.track_for_refresh;
            options.tvos_for_apple_tv = self.options.tvos_for_apple_tv;
        }

        Ok(JobSpec { source: app.path.clone(), target, signing, options })
    }

    /// The edited options for every mode except original; target options stay at defaults.
    fn edits(&self, app: &AppSummary, mode: ExportMode) -> Result<AppOptions, String> {
        let defaults = AppOptions::default();
        let apple_id = mode == ExportMode::AppleId;

        let mut options = AppOptions {
            stream_upload: defaults.stream_upload,
            upload_chunk_mib: defaults.upload_chunk_mib,
            tvos_for_apple_tv: defaults.tvos_for_apple_tv,
            track_for_refresh: defaults.track_for_refresh,
            ..self.options.clone()
        };

        if !apple_id {
            options.entitlements = None;
            options.provision_extensions = false;
        }

        options.bundle_id = self.bundle_policy(app, mode)?;
        options.display_name = changed(&self.name, Some(&app.name))?;
        options.version = changed(&self.version, app.version.as_deref())?;
        options.short_version = changed(&self.short_version, app.short_version.as_deref())?;
        options.minimum_os = changed(&self.minimum_os, app.minimum_os.as_deref())?;
        options.extra_info = parse_overrides(&self.extra_info)?;

        for replacement in &options.replacements {
            validate_relative(&replacement.target)?;
        }

        if let ExtensionRemoval::Selected(names) = &options.remove_extensions {
            let available: BTreeSet<_> = app.extensions.iter().map(|extension| extension.file_name.as_str()).collect();

            if names.iter().any(|name| !available.contains(name.as_str())) {
                return Err("An extension selected for removal is not present in this app.".into());
            }
        }

        Ok(options)
    }

    fn bundle_policy(&self, app: &AppSummary, mode: ExportMode) -> Result<BundleIdPolicy, String> {
        if mode == ExportMode::AppleId {
            match self.identifier_policy {
                IdentifierPolicy::Automatic => return Ok(BundleIdPolicy::Auto),
                IdentifierPolicy::Original => return Ok(BundleIdPolicy::Original),
                IdentifierPolicy::Custom => {}
            }
        }

        let identifier = self.identifier.trim();
        validate_identifier(identifier)?;

        if identifier == app.bundle_id {
            Ok(BundleIdPolicy::Original)
        } else {
            Ok(BundleIdPolicy::Custom(identifier.into()))
        }
    }
}

fn validate_identifier(identifier: &str) -> Result<(), String> {
    let allowed = |character: char| character.is_ascii_alphanumeric() || matches!(character, '.' | '-');

    if identifier.is_empty() || identifier.split('.').any(str::is_empty) || !identifier.chars().all(allowed) {
        return Err(
            "Bundle identifier: use nonempty components containing letters, digits, and hyphens, separated by periods."
                .into(),
        );
    }

    Ok(())
}

fn upload_chunk(text: &str) -> Result<Option<u32>, String> {
    let text = text.trim();

    if text.is_empty() {
        return Ok(None);
    }

    match text.parse::<u32>() {
        Ok(mebibytes @ 1..=64) => Ok(Some(mebibytes)),
        _ => Err("Upload chunk: enter a whole number of MiB from 1 to 64, or leave it blank.".into()),
    }
}

fn changed(value: &str, previous: Option<&str>) -> Result<Option<String>, String> {
    if value.contains('\0') {
        return Err("App metadata must not contain NUL characters.".into());
    }

    if previous.unwrap_or_default() == value {
        return Ok(None);
    }
    if value.trim().is_empty() {
        return Err("Clear a metadata key through the Info.plist overrides using null.".into());
    }

    Ok(Some(value.into()))
}

pub(crate) fn validate_relative(path: &Path) -> Result<(), String> {
    let text = path.to_str().ok_or_else(|| "App-relative paths must be UTF-8.".to_string())?;

    if text.is_empty()
        || text.contains(['\\', '\0'])
        || text.as_bytes().get(1) == Some(&b':')
        || text.split('/').any(|part| part.is_empty() || matches!(part, "." | ".."))
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(
            "Use a path inside the app, such as Frameworks/Example.dylib, without '.' or '..' components.".into()
        );
    }

    Ok(())
}

fn parse_overrides(text: &str) -> Result<Vec<InfoOverride>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    if text.len() > 64 * 1024 {
        return Err("Info.plist overrides exceed 64 KiB.".into());
    }

    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("Info.plist overrides: {error}"))?;
    let fields = value.as_object().ok_or_else(|| "Info.plist overrides must be a JSON object.".to_string())?;

    fields
        .iter()
        .map(|(key, value)| {
            if key.is_empty() || key.contains('\0') {
                return Err("Info.plist keys must be nonempty and contain no NUL.".into());
            }

            let value = match value {
                serde_json::Value::String(value) if !value.contains('\0') => InfoValue::String(value.clone()),
                serde_json::Value::Bool(value) => InfoValue::Bool(*value),
                serde_json::Value::Number(value) => InfoValue::Integer(
                    value.as_i64().ok_or_else(|| format!("{key}: integer must fit signed 64-bit range."))?,
                ),
                serde_json::Value::Null => InfoValue::Remove,
                _ => return Err(format!("{key}: use a string, boolean, signed integer, or null.")),
            };

            Ok(InfoOverride { key: key.clone(), value })
        })
        .collect()
}
