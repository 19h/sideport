use crate::error::io;
use crate::files;
use crate::{Bundle, BundleArchive, BundleKind, Control, Error, Phase, Result};
use plist::Dictionary;
use rayon::prelude::*;
use sl_codesign::{CodeKind, ProfileTarget, ProfileTrust, ProvisioningProfile, SignOptions, Signer, SigningIdentity};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy)]
pub struct SigningRequest<'a> {
    /// None strips signatures. AdHoc needs no provisioning profile.
    pub signer: Option<&'a Signer>,
    pub profile: Option<&'a ProvisioningProfile>,
    /// Optional per-bundle profiles, keyed by the prepared bundle identifier.
    pub profiles: Option<&'a BTreeMap<String, ProvisioningProfile>>,
    /// Merged over each profile's entitlements, matching alternate-entitlement semantics.
    pub entitlements: Option<&'a Dictionary>,
    pub deep: bool,
    /// Target checks applied to every profile that identity signing will embed.
    pub requirements: ProfileRequirements<'a>,
}

/// Target-specific checks for identity signing. Every embedded profile is checked against its
/// own bundle identifier before the signing pass changes any file.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProfileRequirements<'a> {
    /// Device UDID that each embedded profile must provision.
    pub device_udid: Option<&'a str>,
    /// Profile `Platform` entry required for the target, such as iOS or tvOS.
    pub platform: Option<&'a str>,
    /// Verify each embedded profile's CMS signature and chain with these anchors.
    pub trust: Option<&'a ProfileTrust>,
    /// Validation time; `None` uses the current time.
    pub now: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug)]
pub struct SkippedBinary {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct SignReport {
    /// Child entries precede their owning bundle's executable.
    pub signed: Vec<PathBuf>,
    pub encrypted: Vec<PathBuf>,
    pub skipped: Vec<SkippedBinary>,
}

impl SignReport {
    fn append(&mut self, other: Self) {
        self.signed.extend(other.signed);
        self.encrypted.extend(other.encrypted);
        self.skipped.extend(other.skipped);
    }
}

impl BundleArchive {
    pub fn sign(&mut self, request: SigningRequest<'_>, control: Control<'_>) -> Result<SignReport> {
        control.check()?;

        let bundle = self.bundle()?;

        if let Some(Signer::Identity(identity)) = request.signer {
            if embedded_profile(&bundle, request)?.is_none() {
                return Err(Error::Bundle("identity signing requires the main app's provisioning profile".into()));
            }

            let now = request.requirements.now.unwrap_or_else(chrono::Utc::now);
            preflight_profiles(&bundle, request, identity, now, control)?;
        }

        let inherited = request.profile.map(|profile| profile.entitlements.clone());
        let report = sign_bundle(&bundle, request, inherited.as_ref(), control)?;

        control.report(Phase::Sign, report.signed.len() as u64, report.signed.len() as u64);

        Ok(report)
    }
}

fn sign_bundle(
    bundle: &Bundle,
    request: SigningRequest<'_>,
    inherited: Option<&Dictionary>,
    control: Control<'_>,
) -> Result<SignReport> {
    control.check()?;

    let profile = embedded_profile(bundle, request)?;
    let mut entitlements = profile.map(|profile| profile.entitlements.clone()).or_else(|| inherited.cloned());

    if let Some(overrides) = request.entitlements {
        let fields = entitlements.get_or_insert_with(Dictionary::new);

        for (key, value) in overrides {
            fields.insert(key.clone(), value.clone());
        }
    }

    if request.signer.is_some_and(|signer| !signer.is_adhoc())
        && let Some(profile) = profile
    {
        let path = files::write_inside(bundle.root(), Path::new("embedded.mobileprovision"))?;
        files::remove(&path)?;
        files::atomic_write(&path, &profile.raw)?;
    }

    let mut report = SignReport::default();
    let children = if request.deep { bundle.signing_children()? } else { Vec::new() };

    if request.deep {
        for frameworks in [true, false] {
            let results = children
                .par_iter()
                .filter(|child| (child.kind() == BundleKind::Framework) == frameworks)
                .map(|child| sign_bundle(child, request, entitlements.as_ref(), control))
                .collect::<Result<Vec<_>>>()?;

            for result in results {
                report.append(result);
            }
        }

        report.append(sign_loose(bundle, &children, request.signer, control)?);
    }

    let executable = bundle.executable_path()?;
    let bytes = io(&executable, fs::read(&executable))?;

    if sl_codesign::is_encrypted(&bytes)? {
        report.encrypted.push(executable.clone());
    }

    if let Some(signer) = request.signer {
        let relative = executable.strip_prefix(bundle.root()).map_err(|error| Error::Path(error.to_string()))?;
        let relative = files::archive_name(relative)?;
        let cancelled = || control.is_cancelled.is_some_and(|cancelled| cancelled());
        let resources =
            sl_codesign::code_resources::build_seal_cancellable(bundle.root(), Some(&relative), &cancelled)?;
        let signature_dir = files::write_inside(bundle.root(), Path::new("_CodeSignature"))?;

        if !io(&signature_dir, signature_dir.try_exists())? {
            io(&signature_dir, fs::create_dir(&signature_dir))?;
        }

        let resource_path = files::write_inside(bundle.root(), Path::new("_CodeSignature/CodeResources"))?;
        files::atomic_write(&resource_path, &resources)?;

        let info = io(bundle.info_path(), fs::read(bundle.info_path()))?;
        let kind = match bundle.kind() {
            BundleKind::App => CodeKind::MainExecutable,
            BundleKind::Extension => CodeKind::AppExtension,
            _ => CodeKind::Framework,
        };
        let options = SignOptions {
            identifier: bundle.identifier()?,
            kind,
            entitlements: entitlements.as_ref(),
            info_plist: Some(&info),
            code_resources: Some(&resources),
            is_cancelled: control.is_cancelled,
        };

        control.check()?;

        let signed = sl_codesign::sign_macho(&bytes, signer, &options)?;
        files::atomic_write(&executable, &signed)?;
    } else {
        let stripped = sl_codesign::strip_signature(&bytes)?;
        files::atomic_write(&executable, &stripped)?;

        let signature_dir = files::write_inside(bundle.root(), Path::new("_CodeSignature"))?;
        files::remove(&signature_dir)?;
    }

    report.signed.push(executable);

    Ok(report)
}

/// The profile written to a bundle: its own entry in `profiles`, else the request profile for apps.
fn embedded_profile<'a>(bundle: &Bundle, request: SigningRequest<'a>) -> Result<Option<&'a ProvisioningProfile>> {
    let identifier = bundle.identifier()?;
    let app_profile = if bundle.kind() == BundleKind::App { request.profile } else { None };

    Ok(request.profiles.and_then(|profiles| profiles.get(identifier)).or(app_profile))
}

/// Check every profile the signing pass would embed, recursing like `sign_bundle`, before any
/// file changes. Children without their own profile inherit entitlements and embed nothing.
fn preflight_profiles(
    bundle: &Bundle,
    request: SigningRequest<'_>,
    identity: &SigningIdentity,
    now: chrono::DateTime<chrono::Utc>,
    control: Control<'_>,
) -> Result<()> {
    control.check()?;

    if let Some(profile) = embedded_profile(bundle, request)? {
        let requirements = request.requirements;
        let target = ProfileTarget {
            team_id: identity.team_id(),
            bundle_id: bundle.identifier()?,
            certificate_der: identity.certificate_der(),
            device_udid: requirements.device_udid,
            platform: requirements.platform,
            now,
        };

        profile.validate_for(target).map_err(|error| profile_error(bundle, error))?;

        if let Some(trust) = requirements.trust {
            profile.verify_trust(trust).map_err(|error| profile_error(bundle, error))?;
        }
    }

    if request.deep {
        for child in bundle.signing_children()? {
            preflight_profiles(&child, request, identity, now, control)?;
        }
    }

    Ok(())
}

fn profile_error(bundle: &Bundle, source: sl_codesign::Error) -> Error {
    let name = bundle.root().file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();

    Error::Profile { bundle: name, source }
}

fn sign_loose(
    bundle: &Bundle,
    children: &[Bundle],
    signer: Option<&Signer>,
    control: Control<'_>,
) -> Result<SignReport> {
    let executable = bundle.executable_path()?;
    let mut paths = Vec::new();

    let walker = walkdir::WalkDir::new(bundle.root())
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| !children.iter().any(|child| entry.path().starts_with(child.root())));

    for entry in walker {
        control.check()?;

        let entry = entry.map_err(|error| Error::Bundle(error.to_string()))?;

        if !entry.file_type().is_file() || entry.path() == executable {
            continue;
        }

        let explicit_dylib = entry.path().parent() == Some(bundle.root().join("Frameworks").as_path())
            && entry.path().extension().and_then(|extension| extension.to_str()) == Some("dylib");
        let metadata = io(entry.path(), entry.metadata().map_err(std::io::Error::other))?;

        if metadata.len() < 4 || (metadata.len() < 512 && !explicit_dylib) {
            continue;
        }

        let mut file = io(entry.path(), fs::File::open(entry.path()))?;
        let mut magic = [0; 4];
        io(entry.path(), file.read_exact(&mut magic))?;

        if matches!(
            magic,
            [0xce, 0xfa, 0xed, 0xfe]
                | [0xcf, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xce]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
                | [0xca, 0xfe, 0xba, 0xbf]
                | [0xbf, 0xba, 0xfe, 0xca]
        ) {
            paths.push((entry.path().to_owned(), explicit_dylib));
        }
    }

    paths.sort_unstable_by(|left, right| left.0.cmp(&right.0));

    let results = paths
        .par_iter()
        .map(|(path, explicit)| {
            control.check()?;

            let bytes = io(path, fs::read(path))?;
            let mut report = SignReport::default();
            let binary = match sl_macho::Binary::parse(&bytes) {
                Ok(binary) => binary,
                Err(error) => {
                    report.skipped.push(SkippedBinary { path: path.clone(), reason: error.to_string() });

                    return Ok(report);
                }
            };

            if !explicit && !binary.slices.iter().any(|slice| slice.cpu_type == sl_macho::CPU_TYPE_ARM64) {
                return Ok(report);
            }

            if binary.slices.iter().any(|slice| slice.image.encrypted) {
                report.encrypted.push(path.clone());
            }

            let processed = if let Some(signer) = signer {
                let identifier = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| Error::Path(path.display().to_string()))?;
                let options = SignOptions {
                    identifier,
                    kind: CodeKind::Dylib,
                    entitlements: None,
                    info_plist: None,
                    code_resources: None,
                    is_cancelled: control.is_cancelled,
                };

                sl_codesign::sign_macho(&bytes, signer, &options)
            } else {
                sl_codesign::strip_signature(&bytes)
            };

            match processed {
                Ok(bytes) => {
                    files::atomic_write(path, &bytes)?;
                    report.signed.push(path.clone());
                }
                Err(error) => report.skipped.push(SkippedBinary { path: path.clone(), reason: error.to_string() }),
            }

            Ok(report)
        })
        .collect::<Result<Vec<_>>>()?;

    let mut report = SignReport::default();

    for result in results {
        report.append(result);
    }

    Ok(report)
}
