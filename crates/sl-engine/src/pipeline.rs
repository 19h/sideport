use crate::{
    AppOptions, AppSummary, BundleIdPolicy, EngineError, ExtensionInfo, ExtensionRemoval, Fact, InfoValue, JobContext,
    JobOutcome, JobSpec, PromptKind, PromptReply, Result, SigningMode, Stage, Target,
};
use plist::Value;
use sl_bundle::{
    ArchiveLimits, BundleArchive, Control, Injection, OutputLayout, PackOptions, PatchOptions, ProfileRequirements,
    PropertyEdit, Replacement, SigningRequest,
};
use sl_codesign::Signer;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) fn inspect(path: PathBuf, context: Option<&JobContext>) -> Result<AppSummary> {
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

    Ok(AppSummary {
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
    })
}

pub(crate) async fn run_export(context: JobContext, spec: JobSpec) -> Result<JobOutcome> {
    validate_export(&spec)?;
    context.checkpoint()?;
    context.stage(Stage::Preparing);

    let inspect_context = context.clone();
    let source = spec.source.clone();
    let summary = tokio::task::spawn_blocking(move || inspect(source, Some(&inspect_context)))
        .await
        .map_err(|error| EngineError::Other(format!("inspection worker failed: {error}")))??;

    for warning in &summary.warnings {
        context.warn(warning.clone());
    }

    if summary.encrypted {
        context.fact(Fact::EncryptedBinary);
        context.warn("The executable is encrypted; changing its signature does not decrypt it.");
    }

    let path = match &spec.target {
        Target::ExportIpa { path: Some(path) } => path.clone(),
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
                PromptReply::Path(path) => path,
                _ => return Err(EngineError::Other("save-file prompt requires a file path".into())),
            }
        }
        Target::Device { .. } => {
            return Err(EngineError::Unsupported("device installation is not connected to the engine yet".into()));
        }
    };

    context.checkpoint()?;

    tokio::task::spawn_blocking(move || export(context, spec, summary, path))
        .await
        .map_err(|error| EngineError::Other(format!("export worker failed: {error}")))?
}

fn validate_export(spec: &JobSpec) -> Result<()> {
    if matches!(spec.target, Target::Device { .. }) {
        return Err(EngineError::Unsupported("device installation is not connected to the engine yet".into()));
    }

    if matches!(spec.signing, SigningMode::AppleId { .. }) {
        return Err(EngineError::Unsupported(
            "Apple ID authentication and provisioning are not connected to the engine yet".into(),
        ));
    }

    if spec.options.icon.is_some() && spec.signing != SigningMode::Original {
        return Err(EngineError::Unsupported("custom icon replacement is not connected to the engine yet".into()));
    }

    if spec.options.entitlements.is_some() && spec.signing != SigningMode::Original {
        return Err(EngineError::Unsupported(
            "entitlement overrides require identity signing, which is not connected to the engine yet".into(),
        ));
    }

    Ok(())
}

fn export(context: JobContext, spec: JobSpec, summary: AppSummary, path: PathBuf) -> Result<JobOutcome> {
    validate_destination(&spec.source, &path)?;
    context.checkpoint()?;

    let cancelled = || context.is_cancelled();
    let progress = |progress: sl_bundle::Progress| context.progress(progress.completed, progress.total);
    let control = Control { is_cancelled: Some(&cancelled), on_progress: Some(&progress) };
    let bundle_id;

    if spec.signing == SigningMode::Original && spec.source.is_file() {
        bundle_id = summary.bundle_id;
        context.fact(Fact::BundleId(bundle_id.clone()));
        context.info("Exporting the original archive without changing its bytes.");
        context.stage(Stage::Packaging);
        copy_original(&spec.source, &path, &context)?;
    } else {
        let mut archive =
            BundleArchive::unpack(&spec.source, ArchiveLimits::default(), control).map_err(bundle_error)?;

        if spec.signing != SigningMode::Original {
            context.stage(Stage::Patching);
            let patch = patch_options(&spec.options)?;
            archive.patch(&patch, control).map_err(bundle_error)?;

            if !spec.options.injections.is_empty() {
                let injections = spec
                    .options
                    .injections
                    .iter()
                    .map(|injection| Injection { source: injection.source.clone(), name: injection.name.clone() })
                    .collect::<Vec<_>>();
                let report = archive.inject(&injections, control).map_err(bundle_error)?;
                context.info(format!("Prepared {} injected items.", report.copied.len()));
            }

            context.stage(Stage::Signing);

            let signer = match spec.signing {
                SigningMode::AdHoc => Some(Signer::AdHoc),
                SigningMode::Unsigned => None,
                _ => return Err(EngineError::Unsupported("signing mode is not connected to this pipeline".into())),
            };
            let request = SigningRequest {
                signer: signer.as_ref(),
                profile: None,
                profiles: None,
                entitlements: None,
                deep: true,
                requirements: ProfileRequirements::default(),
            };
            let report = archive.sign(request, control).map_err(bundle_error)?;

            for skipped in report.skipped {
                context.warn(format!("Skipped {}: {}", skipped.path.display(), skipped.reason));
            }
        }

        bundle_id = archive.bundle().map_err(bundle_error)?.identifier().map_err(bundle_error)?.to_owned();
        context.fact(Fact::BundleId(bundle_id.clone()));
        context.stage(Stage::Packaging);
        archive.save(&path, OutputLayout::Ipa, PackOptions::default(), control).map_err(bundle_error)?;
    }

    context.stage(Stage::Done);
    context.info(format!("Exported {}", path.display()));

    Ok(JobOutcome { bundle_id, exported_to: Some(path), expires: None, installation_id: None })
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

fn bundle_error(error: sl_bundle::Error) -> EngineError {
    match error {
        sl_bundle::Error::Cancelled => EngineError::Cancelled,
        sl_bundle::Error::Io { .. } => EngineError::Storage(error.to_string()),
        sl_bundle::Error::Codesign(_) | sl_bundle::Error::Profile { .. } => EngineError::Signing(error.to_string()),
        _ => EngineError::InvalidApp(error.to_string()),
    }
}
