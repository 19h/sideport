use crate::{
    AppOptions, AppSummary, BundleIdPolicy, EngineError, ExtensionInfo, ExtensionRemoval, Fact, InfoValue, JobContext,
    JobOutcome, JobSpec, PromptKind, PromptReply, Result, SigningMode, Stage, Target,
};
use plist::{Dictionary, Value};
use sl_bundle::{
    ArchiveLimits, BundleArchive, Control, Injection, OutputLayout, PackOptions, PatchOptions, ProfileRequirements,
    PropertyEdit, Replacement, SigningRequest,
};
use sl_codesign::{ProvisioningProfile, Signer, SigningIdentity};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

/// Entitlement override files are small plists; larger inputs are rejected before parsing.
const MAX_ENTITLEMENTS_BYTES: u64 = 1024 * 1024;

/// How the prepared bundle is signed.
#[derive(Debug)]
pub(crate) enum SigningPlan {
    Original,
    Unsigned,
    AdHoc,
    Identity(Box<IdentityPlan>),
}

/// Apple ID provisioning results applied to the prepared bundle.
#[derive(Debug)]
pub(crate) struct IdentityPlan {
    pub identity: Arc<SigningIdentity>,
    pub profile: ProvisioningProfile,
    pub extension_profiles: BTreeMap<String, ProvisioningProfile>,
    /// Final main-app identifier; extensions keep their suffixes.
    pub bundle_id: String,
    pub record_original_id: bool,
    pub device_udid: Option<String>,
    /// Profile `Platform` value: iOS or tvOS.
    pub platform: &'static str,
    /// Parsed user entitlement overrides, merged over each profile's entitlements.
    pub entitlements: Option<Dictionary>,
}

impl SigningPlan {
    fn for_mode(mode: &SigningMode) -> Result<Self> {
        match mode {
            SigningMode::Original => Ok(Self::Original),
            SigningMode::Unsigned => Ok(Self::Unsigned),
            SigningMode::AdHoc => Ok(Self::AdHoc),
            SigningMode::AppleId { .. } => {
                Err(EngineError::Other("Apple ID signing requires provisioning before export".into()))
            }
        }
    }
}

/// Inspection plus the input's original identifier (`ALTBundleIdentifier`, else the bundle ID).
pub(crate) struct Inspected {
    pub summary: AppSummary,
    pub original_bundle_id: String,
}

pub(crate) fn inspect(path: PathBuf, context: Option<&JobContext>) -> Result<AppSummary> {
    Ok(inspect_details(path, context)?.summary)
}

pub(crate) fn inspect_details(path: PathBuf, context: Option<&JobContext>) -> Result<Inspected> {
    let cancelled = || context.is_some_and(JobContext::is_cancelled);
    let control = Control { is_cancelled: Some(&cancelled), on_progress: None };
    let inspection = sl_bundle::inspect(&path, ArchiveLimits::default(), control).map_err(bundle_error)?;
    let info = &inspection.info;
    let bundle_id = string(info, "CFBundleIdentifier")
        .ok_or_else(|| EngineError::InvalidApp("missing bundle identifier".into()))?;
    let name = string(info, "CFBundleDisplayName")
        .or_else(|| string(info, "CFBundleName"))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| bundle_id.clone());
    let extensions = inspection
        .extensions
        .iter()
        .map(|extension| {
            Ok(ExtensionInfo {
                file_name: extension.file_name.clone(),
                bundle_id: string(&extension.info, "CFBundleIdentifier")
                    .ok_or_else(|| EngineError::InvalidApp("extension has no identifier".into()))?,
                display_name: string(&extension.info, "CFBundleDisplayName")
                    .or_else(|| string(&extension.info, "CFBundleName")),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let device_family = info
        .get("UIDeviceFamily")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_unsigned_integer)
        .filter_map(|value| u32::try_from(value).ok())
        .collect();

    let original_bundle_id =
        string(info, "ALTBundleIdentifier").filter(|value| !value.is_empty()).unwrap_or_else(|| bundle_id.clone());

    let summary = AppSummary {
        path,
        name,
        bundle_id,
        version: string(info, "CFBundleVersion"),
        short_version: string(info, "CFBundleShortVersionString"),
        minimum_os: string(info, "MinimumOSVersion"),
        icon_png: inspection.icon_png,
        extensions,
        has_watch_app: inspection.has_watch_app,
        file_size: inspection.file_size,
        encrypted: inspection.encrypted,
        device_family,
        warnings: inspection.warnings,
    };

    Ok(Inspected { summary, original_bundle_id })
}

/// Inspect on a blocking worker and report inspection warnings and encryption.
pub(crate) async fn inspect_job(context: &JobContext, spec: &JobSpec) -> Result<Inspected> {
    context.checkpoint()?;
    context.stage(Stage::Preparing);

    let inspect_context = context.clone();
    let source = spec.source.clone();
    let inspected = tokio::task::spawn_blocking(move || inspect_details(source, Some(&inspect_context)))
        .await
        .map_err(|error| EngineError::Other(format!("inspection worker failed: {error}")))??;

    for warning in &inspected.summary.warnings {
        context.warn(warning.clone());
    }

    if inspected.summary.encrypted {
        context.fact(Fact::EncryptedBinary);
        context.warn("The executable is encrypted; changing its signature does not decrypt it.");
    }

    Ok(inspected)
}

pub(crate) async fn run_export(context: JobContext, spec: JobSpec) -> Result<JobOutcome> {
    validate_export(&spec)?;

    let plan = SigningPlan::for_mode(&spec.signing)?;
    let inspected = inspect_job(&context, &spec).await?;
    let path = output_path(&context, &spec, &inspected.summary).await?;

    context.checkpoint()?;

    tokio::task::spawn_blocking(move || export(context, spec, inspected.summary, path, plan))
        .await
        .map_err(|error| EngineError::Other(format!("export worker failed: {error}")))?
}

/// Resolve the export destination, asking the front end when the job names none.
pub(crate) async fn output_path(context: &JobContext, spec: &JobSpec, summary: &AppSummary) -> Result<PathBuf> {
    match &spec.target {
        Target::ExportIpa { path: Some(path) } => Ok(path.clone()),
        Target::ExportIpa { path: None } => {
            let filename: String = summary
                .bundle_id
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                        character
                    } else {
                        '_'
                    }
                })
                .collect();

            match context.ask(PromptKind::SaveFile { suggested_name: format!("{filename}.ipa") }).await? {
                PromptReply::Path(path) => Ok(path),
                _ => Err(EngineError::Other("save-file prompt requires a file path".into())),
            }
        }
        Target::Device { .. } => {
            Err(EngineError::Unsupported("device installation is not connected to the engine yet".into()))
        }
    }
}

/// Reject options that the requested signing mode cannot apply, before any output changes.
pub(crate) fn validate_options(spec: &JobSpec) -> Result<()> {
    if spec.options.icon.is_some() && spec.signing != SigningMode::Original {
        return Err(EngineError::Unsupported("custom icon replacement is not connected to the engine yet".into()));
    }

    let identity = matches!(spec.signing, SigningMode::AppleId { .. });

    if spec.options.entitlements.is_some() && !identity && spec.signing != SigningMode::Original {
        return Err(EngineError::Unsupported(
            "entitlement overrides require Apple ID signing; ad-hoc and unsigned bundles carry no entitlements".into(),
        ));
    }

    Ok(())
}

fn validate_export(spec: &JobSpec) -> Result<()> {
    if matches!(spec.target, Target::Device { .. }) {
        return Err(EngineError::Unsupported("device installation is not connected to the engine yet".into()));
    }

    validate_options(spec)
}

pub(crate) fn export(
    context: JobContext,
    spec: JobSpec,
    summary: AppSummary,
    path: PathBuf,
    plan: SigningPlan,
) -> Result<JobOutcome> {
    validate_destination(&spec.source, &path)?;
    context.checkpoint()?;

    let cancelled = || context.is_cancelled();
    let progress = |progress: sl_bundle::Progress| context.progress(progress.completed, progress.total);
    let control = Control { is_cancelled: Some(&cancelled), on_progress: Some(&progress) };
    let expires = match &plan {
        SigningPlan::Identity(identity) => Some(identity.profile.expiration_date),
        _ => None,
    };
    let bundle_id;

    if matches!(plan, SigningPlan::Original) && spec.source.is_file() {
        bundle_id = summary.bundle_id;
        context.fact(Fact::BundleId(bundle_id.clone()));
        context.info("Exporting the original archive without changing its bytes.");
        context.stage(Stage::Packaging);
        copy_original(&spec.source, &path, &context)?;
    } else {
        let mut archive =
            BundleArchive::unpack(&spec.source, ArchiveLimits::default(), control).map_err(bundle_error)?;

        if !matches!(plan, SigningPlan::Original) {
            prepare(&mut archive, &context, &spec.options, &plan, control)?;
        }

        bundle_id = archive.bundle().map_err(bundle_error)?.identifier().map_err(bundle_error)?.to_owned();
        context.fact(Fact::BundleId(bundle_id.clone()));
        context.stage(Stage::Packaging);
        archive.save(&path, OutputLayout::Ipa, PackOptions::default(), control).map_err(bundle_error)?;
    }

    context.stage(Stage::Done);
    context.info(format!("Exported {}", path.display()));

    Ok(JobOutcome { bundle_id, exported_to: Some(path), expires, installation_id: None })
}

/// Patch, inject and sign an unpacked archive according to `plan`.
pub(crate) fn prepare(
    archive: &mut BundleArchive,
    context: &JobContext,
    options: &AppOptions,
    plan: &SigningPlan,
    control: Control<'_>,
) -> Result<()> {
    context.stage(Stage::Patching);

    let mut patch = patch_options(options)?;

    if let SigningPlan::Identity(identity) = plan {
        let current = archive.bundle().map_err(bundle_error)?.identifier().map_err(bundle_error)?.to_owned();

        if identity.bundle_id != current {
            patch
                .info
                .insert("CFBundleIdentifier".into(), PropertyEdit::Set(Value::String(identity.bundle_id.clone())));
        }

        if identity.record_original_id {
            patch.info.insert("ALTBundleIdentifier".into(), PropertyEdit::Set(Value::String(current)));
        }
    }

    archive.patch(&patch, control).map_err(bundle_error)?;

    if !options.injections.is_empty() {
        let injections = options
            .injections
            .iter()
            .map(|injection| Injection { source: injection.source.clone(), name: injection.name.clone() })
            .collect::<Vec<_>>();
        let report = archive.inject(&injections, control).map_err(bundle_error)?;
        context.info(format!("Prepared {} injected items.", report.copied.len()));
    }

    context.stage(Stage::Signing);

    let identity_signer;
    let signer = match plan {
        SigningPlan::AdHoc => Some(&Signer::AdHoc),
        SigningPlan::Unsigned => None,
        SigningPlan::Identity(identity) => {
            identity_signer = Signer::Identity(identity.identity.clone());

            Some(&identity_signer)
        }
        SigningPlan::Original => return Ok(()),
    };

    let request = match plan {
        SigningPlan::Identity(identity) => {
            if let Some(overrides) = &identity.entitlements {
                for warning in entitlement_warnings(&identity.profile, overrides) {
                    context.warn(warning);
                }
            }

            SigningRequest {
                signer,
                profile: Some(&identity.profile),
                profiles: Some(&identity.extension_profiles),
                entitlements: identity.entitlements.as_ref(),
                deep: true,
                requirements: ProfileRequirements {
                    device_udid: identity.device_udid.as_deref(),
                    platform: Some(identity.platform),
                    trust: None,
                    now: None,
                },
            }
        }

        _ => SigningRequest {
            signer,
            profile: None,
            profiles: None,
            entitlements: None,
            deep: true,
            requirements: ProfileRequirements::default(),
        },
    };

    let report = archive.sign(request, control).map_err(bundle_error)?;

    for skipped in report.skipped {
        context.warn(format!("Skipped {}: {}", skipped.path.display(), skipped.reason));
    }

    Ok(())
}

/// Load a user entitlement override plist (recovered "alternate entitlements", merged with
/// `dict.update` over the profile's entitlements).
pub(crate) fn load_entitlements(path: &Path) -> Result<Dictionary> {
    let file = File::open(path).map_err(storage_error)?;
    let mut bytes = Vec::new();

    file.take(MAX_ENTITLEMENTS_BYTES + 1).read_to_end(&mut bytes).map_err(storage_error)?;

    if bytes.len() as u64 > MAX_ENTITLEMENTS_BYTES {
        return Err(EngineError::InvalidApp("entitlement override file exceeds 1 MiB".into()));
    }

    sl_bundle::parse_dictionary(&bytes).map_err(bundle_error)
}

/// Overrides the profile does not grant are signed as requested; the device decides whether to
/// accept them. A value is considered granted when it equals the profile's value, or when the
/// profile grants a wildcard (`*` or a `PREFIX.*` string) for that key.
pub(crate) fn entitlement_warnings(profile: &ProvisioningProfile, overrides: &Dictionary) -> Vec<String> {
    let mut warnings = Vec::new();

    for (key, value) in overrides {
        let granted = match profile.entitlements.get(key) {
            None => false,
            Some(granted) => granted == value || wildcard_grant(granted, value),
        };

        if !granted {
            warnings
                .push(format!("Entitlement {key} is not granted by the provisioning profile; installation can fail."));
        }
    }

    warnings
}

fn wildcard_grant(granted: &Value, requested: &Value) -> bool {
    let matches = |pattern: &str, value: &str| match pattern.strip_suffix('*') {
        Some(stem) => value.starts_with(stem),
        None => pattern == value,
    };

    let patterns: Vec<&str> = match granted {
        Value::String(pattern) => vec![pattern.as_str()],
        Value::Array(patterns) => patterns.iter().filter_map(Value::as_string).collect(),
        _ => return false,
    };

    let values: Vec<&str> = match requested {
        Value::String(value) => vec![value.as_str()],
        Value::Array(values) => values.iter().filter_map(Value::as_string).collect(),
        _ => return false,
    };

    !values.is_empty() && values.iter().all(|value| patterns.iter().any(|pattern| matches(pattern, value)))
}

fn patch_options(options: &AppOptions) -> Result<PatchOptions> {
    let mut info = BTreeMap::new();

    if let BundleIdPolicy::Custom(identifier) = &options.bundle_id {
        if identifier.is_empty()
            || !identifier.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-'))
            || identifier.split('.').any(str::is_empty)
        {
            return Err(EngineError::InvalidApp(
                "custom bundle identifier must contain nonempty alphanumeric/hyphen components separated by periods"
                    .into(),
            ));
        }

        info.insert("CFBundleIdentifier".into(), PropertyEdit::Set(Value::String(identifier.clone())));
    }

    for (key, value) in [
        ("CFBundleDisplayName", &options.display_name),
        ("CFBundleVersion", &options.version),
        ("CFBundleShortVersionString", &options.short_version),
        ("MinimumOSVersion", &options.minimum_os),
    ] {
        if let Some(value) = value {
            info.insert(key.into(), PropertyEdit::Set(Value::String(value.clone())));
        }
    }

    if options.remove_device_restrictions {
        info.insert("UISupportedDevices".into(), PropertyEdit::Remove);
    }

    if options.enable_file_sharing {
        for key in ["UIFileSharingEnabled", "LSSupportsOpeningDocumentsInPlace"] {
            info.insert(key.into(), PropertyEdit::Set(Value::Boolean(true)));
        }
    }

    for override_ in &options.extra_info {
        if override_.key.is_empty() || override_.key.contains('\0') {
            return Err(EngineError::InvalidApp("an Info.plist override has an empty or NUL-containing key".into()));
        }

        let edit = match &override_.value {
            InfoValue::String(value) => PropertyEdit::Set(Value::String(value.clone())),
            InfoValue::Bool(value) => PropertyEdit::Set(Value::Boolean(*value)),
            InfoValue::Integer(value) => PropertyEdit::Set(Value::Integer((*value).into())),
            InfoValue::Remove => PropertyEdit::Remove,
        };
        info.insert(override_.key.clone(), edit);
    }

    let mut replacements = Vec::new();

    if options.remove_extensions == ExtensionRemoval::All {
        replacements.push(Replacement { target: "Extensions".into(), source: None });
    }

    replacements.extend(
        options
            .replacements
            .iter()
            .map(|replacement| Replacement { target: replacement.target.clone(), source: replacement.source.clone() }),
    );

    Ok(PatchOptions {
        info,
        drop_plugins: options.remove_extensions == ExtensionRemoval::All,
        remove_extensions: match &options.remove_extensions {
            ExtensionRemoval::Selected(names) => names.clone(),
            _ => Vec::new(),
        },
        remove_watch_apps: options.remove_watch_app,
        replacements,
    })
}

fn validate_destination(source: &Path, destination: &Path) -> Result<()> {
    let original = fs::canonicalize(source).map_err(storage_error)?;
    let parent = destination.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(storage_error)?;
    let filename = destination.file_name().ok_or_else(|| EngineError::Storage("output has no filename".into()))?;
    let output = parent.join(filename);

    if original == output || (original.is_dir() && output.starts_with(&original)) {
        return Err(EngineError::Storage("output must be separate from the original input".into()));
    }

    Ok(())
}

fn copy_original(source: &Path, destination: &Path, context: &JobContext) -> Result<()> {
    let existing = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_file() => Some(metadata),
        Ok(_) => return Err(EngineError::Storage("output must be a regular file".into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(storage_error(error)),
    };
    let parent = destination.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut input = File::open(source).map_err(storage_error)?;
    let total = input.metadata().map_err(storage_error)?.len();
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(storage_error)?;
    let mut buffer = vec![0; 128 * 1024];
    let mut copied = 0;

    loop {
        context.checkpoint()?;
        let size = input.read(&mut buffer).map_err(storage_error)?;

        if size == 0 {
            break;
        }

        temporary.write_all(&buffer[..size]).map_err(storage_error)?;
        copied += size as u64;
        context.progress(copied, total);
    }

    if copied != total {
        return Err(EngineError::Storage("source changed during original export".into()));
    }

    if let Some(metadata) = existing {
        temporary.as_file().set_permissions(metadata.permissions()).map_err(storage_error)?;
    }

    temporary.as_file().sync_all().map_err(storage_error)?;
    context.checkpoint()?;
    temporary.persist(destination).map_err(|error| storage_error(error.error))?;

    Ok(())
}

fn string(info: &plist::Dictionary, key: &str) -> Option<String> {
    info.get(key).and_then(Value::as_string).map(str::to_owned)
}

fn storage_error(error: std::io::Error) -> EngineError {
    EngineError::Storage(error.to_string())
}

pub(crate) fn bundle_error(error: sl_bundle::Error) -> EngineError {
    match error {
        sl_bundle::Error::Cancelled => EngineError::Cancelled,
        sl_bundle::Error::Io { .. } => EngineError::Storage(error.to_string()),
        sl_bundle::Error::Codesign(_) | sl_bundle::Error::Profile { .. } => EngineError::Signing(error.to_string()),
        _ => EngineError::InvalidApp(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(entitlements: Dictionary) -> ProvisioningProfile {
        ProvisioningProfile {
            raw: Vec::new(),
            name: "Fixture".into(),
            uuid: "UUID".into(),
            team_identifiers: vec!["TEAM123456".into()],
            application_identifier_prefixes: vec!["TEAM123456".into()],
            app_id_name: None,
            entitlements,
            creation_date: "2026-01-01T00:00:00Z".parse().expect("date"),
            expiration_date: "2026-01-08T00:00:00Z".parse().expect("date"),
            time_to_live_days: Some(7),
            local_provision: true,
            platforms: Vec::new(),
            provisions_all_devices: false,
            provisioned_devices: Vec::new(),
            developer_certificates: Vec::new(),
        }
    }

    #[test]
    fn entitlement_overrides_are_checked_against_profile_grants() {
        let mut granted = Dictionary::new();
        granted.insert("get-task-allow".into(), true.into());
        granted.insert("keychain-access-groups".into(), Value::Array(vec!["TEAM123456.*".into()]));
        granted.insert("com.apple.developer.team-identifier".into(), "TEAM123456".into());

        let mut overrides = Dictionary::new();
        overrides.insert("get-task-allow".into(), true.into());
        overrides.insert("keychain-access-groups".into(), Value::Array(vec!["TEAM123456.com.example.shared".into()]));
        assert!(entitlement_warnings(&profile(granted.clone()), &overrides).is_empty());

        overrides.insert("com.apple.developer.team-identifier".into(), "OTHERTEAM".into());
        overrides.insert("com.apple.developer.healthkit".into(), true.into());
        overrides.insert("keychain-access-groups".into(), Value::Array(vec!["OTHER.group".into()]));

        let warnings = entitlement_warnings(&profile(granted), &overrides);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().any(|warning| warning.contains("healthkit")));
    }
}
