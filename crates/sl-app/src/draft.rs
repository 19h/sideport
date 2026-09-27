use sl_engine::{
    AppOptions, AppSummary, BundleIdPolicy, ExtensionRemoval, InfoOverride, InfoValue, JobSpec, SigningMode, Target,
};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportMode {
    #[default]
    Unsigned,
    AdHoc,
    Original,
}

impl ExportMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unsigned => "Unsigned",
            Self::AdHoc => "Ad-hoc signed",
            Self::Original => "Original",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Unsigned => "Apply your edits and remove existing signatures.",
            Self::AdHoc => "Apply your edits and create an ad-hoc signature. No Apple provisioning is added.",
            Self::Original => "Export the original app with its files and signatures unchanged.",
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

    pub fn job(&self, app: &AppSummary, mode: ExportMode, destination: Option<PathBuf>) -> Result<JobSpec, String> {
        let options = if mode == ExportMode::Original {
            AppOptions::default()
        } else {
            let mut options = self.options.clone();
            let identifier = self.identifier.trim();

            if identifier.is_empty()
                || identifier.split('.').any(str::is_empty)
                || !identifier
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-'))
            {
                return Err("Bundle identifier: use nonempty components containing letters, digits, and hyphens, separated by periods.".into());
            }

            options.bundle_id = if identifier == app.bundle_id {
                BundleIdPolicy::Original
            } else {
                BundleIdPolicy::Custom(identifier.into())
            };
            options.display_name = changed(&self.name, Some(&app.name))?;
            options.version = changed(&self.version, app.version.as_deref())?;
            options.short_version = changed(&self.short_version, app.short_version.as_deref())?;
            options.minimum_os = changed(&self.minimum_os, app.minimum_os.as_deref())?;
            options.extra_info = parse_overrides(&self.extra_info)?;

            for replacement in &options.replacements {
                validate_relative(&replacement.target)?;
            }

            if let ExtensionRemoval::Selected(names) = &options.remove_extensions {
                let available: BTreeSet<_> =
                    app.extensions.iter().map(|extension| extension.file_name.as_str()).collect();

                if names.iter().any(|name| !available.contains(name.as_str())) {
                    return Err("An extension selected for removal is not present in this app.".into());
                }
            }

            options
        };
        let signing = match mode {
            ExportMode::Unsigned => SigningMode::Unsigned,
            ExportMode::AdHoc => SigningMode::AdHoc,
            ExportMode::Original => SigningMode::Original,
        };

        Ok(JobSpec { source: app.path.clone(), target: Target::ExportIpa { path: destination }, signing, options })
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
