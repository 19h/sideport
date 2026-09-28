//! Apple ID provisioning for a signing job: team, bundle identifier, device registration,
//! key/certificate, App IDs and provisioning profiles.
//!
//! Follows the recovered `Impactor.run` steps 3–8 (docs/APPLE.md). Deliberate differences:
//! paid teams register the original identifier instead of registering the mangled one and
//! unmangling afterwards, and every certificate revocation is confirmed by the user.

use super::portal::{PortalJob, portal_call};
use super::{Inner, state};
use crate::error::{EngineError, Result};
use crate::job::{Fact, JobContext, PromptKind, PromptReply};
use crate::store::StoredCertificate;
use crate::types::{BundleIdPolicy, TeamKind, TeamSummary};
use chrono::{DateTime, Utc};
use rsa::RsaPrivateKey;
use sl_apple::portal::{AppIdRecord, CertificateRecord, Platform};
use sl_codesign::{ProvisioningProfile, SigningIdentity};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Recovered certificate-limit code: the team already has its maximum development certificates.
const CERTIFICATE_LIMIT: i64 = 7460;

/// Recovered free-team App ID allowance within its expiry window.
const FREE_APP_IDS: usize = 10;

/// Recovered mangling threshold: devices before iOS 13.3.1 keep the original identifier.
const MANGLE_FROM: [u64; 3] = [13, 3, 1];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceTarget {
    pub udid: String,
    pub name: Option<String>,
    /// Lockdown `DeviceClass`, such as iPhone or AppleTV.
    pub device_class: Option<String>,
    /// Lockdown `ProductVersion`.
    pub os_version: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProvisionRequest {
    pub apple_id: String,
    pub policy: BundleIdPolicy,
    /// `ALTBundleIdentifier`, else `CFBundleIdentifier`, of the input app.
    pub original_bundle_id: String,
    /// Current `CFBundleIdentifier` of the input app.
    pub current_bundle_id: String,
    pub app_name: String,
    /// Current identifiers of the app's top-level extensions.
    pub extensions: Vec<String>,
    pub device: Option<DeviceTarget>,
    pub tvos_for_apple_tv: bool,
    /// Register an App ID and profile for each extension (not done by the recovered client).
    pub provision_extensions: bool,
}

#[derive(Debug)]
pub(crate) struct Provisioned {
    pub team: TeamSummary,
    pub identity: Arc<SigningIdentity>,
    /// Final main-app `CFBundleIdentifier`.
    pub bundle_id: String,
    /// Record `ALTBundleIdentifier` (original identifier) in each prepared bundle.
    pub record_original_id: bool,
    pub platform: Platform,
    pub profile: ProvisioningProfile,
    /// Profiles keyed by the prepared extension identifier.
    pub extension_profiles: BTreeMap<String, ProvisioningProfile>,
}

pub(crate) async fn provision(
    inner: Arc<Inner>,
    context: JobContext,
    request: ProvisionRequest,
) -> Result<Provisioned> {
    let mut job = PortalJob::new(inner, context, request.apple_id.clone()).await?;
    let team = job.team().await?;

    let platform = platform(&request);
    let (bundle_id, record_original_id) = bundle_identifier(&request, &team);

    job.context.fact(Fact::BundleId(bundle_id.clone()));
    job.context.info(format!("Using team \"{}\" ({}) with id {}", team.name, kind_label(&team.kind), team.team_id));

    if let Some(device) = &request.device {
        ensure_device(&mut job, &team.team_id, platform, device).await?;
    }

    job.context.info("Checking private key");
    let key = state::signing_key(&job.inner, true)?.ok_or_else(|| EngineError::Signing("no signing key".into()))?;
    let identity = Arc::new(ensure_certificate(&mut job, &team.team_id, &key).await?);

    let app_ids = portal_call!(job, |client, access| client.list_app_ids(&team.team_id, platform, access));
    report_quota(&job, &team, &app_ids);

    let app_id = ensure_app_id(&mut job, &team.team_id, platform, &app_ids, &bundle_id, &request.app_name).await?;
    let profile = fetch_profile(&mut job, &team.team_id, platform, &app_id).await?;

    let mut extension_profiles = BTreeMap::new();

    if request.provision_extensions {
        let app_ids = portal_call!(job, |client, access| client.list_app_ids(&team.team_id, platform, access));

        for extension in &request.extensions {
            let prepared = prepared_extension_id(extension, &request.current_bundle_id, &bundle_id);
            let suffix = prepared.rsplit('.').next().unwrap_or(&prepared);
            let name = format!("{} {suffix}", request.app_name);

            let app_id = ensure_app_id(&mut job, &team.team_id, platform, &app_ids, &prepared, &name).await?;
            let profile = fetch_profile(&mut job, &team.team_id, platform, &app_id).await?;

            extension_profiles.insert(prepared, profile);
        }
    }

    Ok(Provisioned { team, identity, bundle_id, record_original_id, platform, profile, extension_profiles })
}

fn platform(request: &ProvisionRequest) -> Platform {
    let apple_tv = request.device.as_ref().and_then(|device| device.device_class.as_deref()) == Some("AppleTV");

    if apple_tv && request.tvos_for_apple_tv { Platform::Tvos } else { Platform::Ios }
}

/// Recovered policy: a custom identifier wins and records the original; `Auto` appends
/// `.<TEAMID>` for free teams on devices from iOS 13.3.1 (or when the version is unknown);
/// paid teams keep the original identifier.
fn bundle_identifier(request: &ProvisionRequest, team: &TeamSummary) -> (String, bool) {
    let original = &request.original_bundle_id;

    match &request.policy {
        BundleIdPolicy::Custom(identifier) => (identifier.clone(), true),

        BundleIdPolicy::Auto if team.kind == TeamKind::Free && mangles(request.device.as_ref()) => {
            (format!("{original}.{}", team.team_id), true)
        }

        BundleIdPolicy::Auto | BundleIdPolicy::Original => (original.clone(), false),
    }
}

fn mangles(device: Option<&DeviceTarget>) -> bool {
    let Some(version) = device.and_then(|device| device.os_version.as_deref()) else {
        return true;
    };

    let mut components = [0u64; 3];

    for (index, part) in version.split('.').take(3).enumerate() {
        let Ok(value) = part.parse() else {
            return true;
        };

        components[index] = value;
    }

    components >= MANGLE_FROM
}

/// Extensions keep their suffix after the main identifier changes, as `sl-bundle` rewrites them.
pub(crate) fn prepared_extension_id(extension: &str, current_main: &str, prepared_main: &str) -> String {
    match extension.strip_prefix(current_main) {
        Some(suffix) => format!("{prepared_main}{suffix}"),
        None => extension.into(),
    }
}

fn kind_label(kind: &TeamKind) -> &str {
    match kind {
        TeamKind::Free => "free",
        TeamKind::Individual => "individual",
        TeamKind::Organization => "organization",
        TeamKind::Other(name) => name,
    }
}

/// Recovered `Impactor._format_udid`: casefold and remove hyphens before comparing.
pub(crate) fn format_udid(udid: &str) -> String {
    udid.chars().filter(|character| *character != '-').flat_map(char::to_lowercase).collect()
}

async fn ensure_device(job: &mut PortalJob, team_id: &str, platform: Platform, device: &DeviceTarget) -> Result<()> {
    job.context.info(format!("Making sure device ID {} is registered", device.udid));

    let devices = portal_call!(job, |client, access| client.list_devices(team_id, platform, access));
    let target = format_udid(&device.udid);

    if devices.iter().any(|registered| format_udid(&registered.device_number) == target) {
        job.context.info(format!("Device {} is already registered", device.udid));

        return Ok(());
    }

    let name = device.name.clone().unwrap_or_else(|| format!("device-{}", device.udid));
    portal_call!(job, |client, access| client.add_device(team_id, platform, &device.udid, &name, access));
    job.context.info(format!("Registered device with UDID {} as {name}", device.udid));

    Ok(())
}

async fn ensure_certificate(job: &mut PortalJob, team_id: &str, key: &RsaPrivateKey) -> Result<SigningIdentity> {
    let mut certificates = portal_call!(job, |client, access| client.list_certificates(team_id, Platform::Ios, access));

    if let Some(identity) = owned_identity(job, team_id, key, &certificates)? {
        return Ok(identity);
    }

    job.context.info("Making CSR");

    let csr = sl_codesign::build_csr_pem(key, "Sideport", "Sideport").map_err(signing_error)?;
    let machine_id = state::machine_id(&job.inner.store)?;
    let machine_name = gethostname::gethostname().to_string_lossy().into_owned();
    let machine_name = if machine_name.is_empty() { "Sideport".to_owned() } else { machine_name };

    job.context.info(format!("Signing certificate for {machine_name}"));

    let serial = loop {
        let submitted = portal_call!(job, |client, access| async {
            let outcome =
                client.submit_development_csr(team_id, Platform::Ios, &csr, machine_id, &machine_name, access);

            match outcome.await {
                Err(sl_apple::Error::Service { code: CERTIFICATE_LIMIT, .. }) => Ok(None),
                outcome => outcome.map(Some),
            }
        });

        if let Some(serial) = submitted {
            break serial;
        }

        let oldest = oldest_certificate(&certificates)
            .ok_or_else(|| EngineError::Portal { code: CERTIFICATE_LIMIT, message: "Nothing to revoke".into() })?;
        let name = oldest.machine_name.clone().unwrap_or_else(|| format!("[{}]", oldest.serial_number));

        confirm_revocation(job, &oldest.serial_number, &name).await?;
        job.context.info(format!("Revoking cert {} for {name}", oldest.serial_number));

        let serial = oldest.serial_number.clone();
        portal_call!(job, |client, access| client.revoke_development_certificate(
            team_id,
            Platform::Ios,
            &serial,
            access
        ));

        certificates = portal_call!(job, |client, access| client.list_certificates(team_id, Platform::Ios, access));
    };

    let certificates = portal_call!(job, |client, access| client.list_certificates(team_id, Platform::Ios, access));
    let issued: Vec<_> = certificates.iter().filter(|record| record.serial_number == serial).collect();

    let [issued] = issued.as_slice() else {
        return Err(EngineError::Signing(format!("Failed to download certificate with serial={serial:?}")));
    };

    let der = issued
        .content_der
        .as_deref()
        .ok_or_else(|| EngineError::Signing(format!("certificate {serial} was issued without certificate content")))?;

    let identity = SigningIdentity::new(der, key.clone()).map_err(signing_error)?;
    let stored = StoredCertificate { serial: serial.clone(), der: der.to_vec() };

    job.inner.store.save_certificate(team_id, &stored)?;

    Ok(identity)
}

/// Reuse a listed certificate whose public key is this machine's key (recovered modulus match).
fn owned_identity(
    job: &PortalJob,
    team_id: &str,
    key: &RsaPrivateKey,
    certificates: &[CertificateRecord],
) -> Result<Option<SigningIdentity>> {
    for record in certificates {
        let Some(der) = record.content_der.as_deref() else {
            job.context.warn(format!("Certificate {} has no certificate content", record.serial_number));

            continue;
        };

        if !state::owns_certificate(Some(key), Some(der)) {
            continue;
        }

        let identity = SigningIdentity::new(der, key.clone()).map_err(signing_error)?;
        let stored = StoredCertificate { serial: record.serial_number.clone(), der: der.to_vec() };

        job.inner.store.save_certificate(team_id, &stored)?;
        job.context.info(format!("Reusing certificate {}", record.serial_number));

        return Ok(Some(identity));
    }

    Ok(None)
}

/// Recovered choice: the certificate with the earliest expiration; a missing date sorts last.
fn oldest_certificate(certificates: &[CertificateRecord]) -> Option<&CertificateRecord> {
    certificates.iter().min_by_key(|record| (record.expiration.is_none(), record.expiration))
}

async fn confirm_revocation(job: &PortalJob, serial: &str, name: &str) -> Result<()> {
    let prompt = PromptKind::Confirm {
        title: "Certificate limit reached".into(),
        message: format!(
            "The team has reached its development certificate limit. Revoke certificate {serial} ({name})? \
             Apps signed with it stop launching, and other tools or computers using it must create a new one."
        ),
        confirm_label: "Revoke".into(),
        destructive: true,
    };

    match job.context.ask(prompt).await? {
        PromptReply::Confirmed(true) => Ok(()),
        _ => Err(EngineError::Portal { code: CERTIFICATE_LIMIT, message: "certificate limit reached".into() }),
    }
}

/// Recovered `[appids]` report: free teams have ten App IDs; the earliest expiry frees one.
fn report_quota(job: &PortalJob, team: &TeamSummary, app_ids: &[AppIdRecord]) {
    if team.kind != TeamKind::Free {
        return;
    }

    let remaining = FREE_APP_IDS.saturating_sub(app_ids.len());
    let next_release: Option<DateTime<Utc>> = app_ids.iter().filter_map(|record| record.expiration).min();

    job.context.fact(Fact::AppIdQuota { remaining: remaining as u32, next_release });
}

async fn ensure_app_id(
    job: &mut PortalJob,
    team_id: &str,
    platform: Platform,
    app_ids: &[AppIdRecord],
    bundle_id: &str,
    name: &str,
) -> Result<String> {
    job.context.info("Looking up app ID");

    if let Some(existing) = app_ids.iter().find(|record| record.identifier == bundle_id) {
        job.context.info(format!("Using app ID \"{}\" with id {}", existing.name, existing.app_id_id));

        return Ok(existing.app_id_id.clone());
    }

    let name = portal_name(name, bundle_id);
    let created = portal_call!(job, |client, access| client.add_app_id(team_id, platform, bundle_id, &name, access));

    job.context.info(format!("Registered app ID {}", created.app_id_id));

    Ok(created.app_id_id)
}

/// Recovered App ID name sanitizing: ASCII alphanumerics and collapsed spaces from the app name,
/// else from the bundle identifier, else a random UUID with spaces.
pub(crate) fn portal_name(name: &str, bundle_id: &str) -> String {
    for candidate in [name, bundle_id] {
        let replaced: String = candidate
            .chars()
            .map(|character| if character.is_alphanumeric() || character.is_whitespace() { character } else { ' ' })
            .filter(char::is_ascii)
            .collect();

        let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");

        if !collapsed.is_empty() {
            return collapsed;
        }
    }

    uuid::Uuid::new_v4().to_string().replace('-', " ")
}

async fn fetch_profile(
    job: &mut PortalJob,
    team_id: &str,
    platform: Platform,
    app_id_id: &str,
) -> Result<ProvisioningProfile> {
    let raw = portal_call!(job, |client, access| client.download_profile(team_id, platform, app_id_id, access));

    let profile = ProvisioningProfile::parse(&raw).map_err(signing_error)?;
    profile.verify_trust(&job.inner.profile_trust).map_err(signing_error)?;

    job.context.fact(Fact::ProfileExpiry { expires: profile.expiration_date, ttl_days: profile.time_to_live_days });
    job.context.info(format!(
        "Provisioning profile TTL: {} days, local: {}",
        profile.time_to_live_days.map_or_else(|| "unknown".into(), |days| days.to_string()),
        profile.local_provision
    ));

    Ok(profile)
}

fn signing_error(error: sl_codesign::Error) -> EngineError {
    EngineError::Signing(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(policy: BundleIdPolicy, os_version: Option<&str>) -> ProvisionRequest {
        let device = os_version.map(|version| DeviceTarget {
            udid: "00008030-001A2D0C0E38802E".into(),
            name: None,
            device_class: Some("iPhone".into()),
            os_version: Some(version.into()),
        });

        ProvisionRequest {
            apple_id: "fixture@example.test".into(),
            policy,
            original_bundle_id: "com.example.app".into(),
            current_bundle_id: "com.example.app".into(),
            app_name: "App".into(),
            extensions: Vec::new(),
            device,
            tvos_for_apple_tv: false,
            provision_extensions: false,
        }
    }

    fn team(kind: TeamKind) -> TeamSummary {
        TeamSummary { team_id: "TEAM123456".into(), name: "Fixture".into(), kind }
    }

    #[test]
    fn bundle_identifier_policy_follows_the_recovered_mangling_rules() {
        let free = team(TeamKind::Free);
        let paid = team(TeamKind::Individual);

        let cases = [
            (request(BundleIdPolicy::Auto, None), &free, "com.example.app.TEAM123456", true),
            (request(BundleIdPolicy::Auto, Some("13.3.1")), &free, "com.example.app.TEAM123456", true),
            (request(BundleIdPolicy::Auto, Some("17")), &free, "com.example.app.TEAM123456", true),
            (request(BundleIdPolicy::Auto, Some("13.3")), &free, "com.example.app", false),
            (request(BundleIdPolicy::Auto, Some("12.5.7")), &free, "com.example.app", false),
            (request(BundleIdPolicy::Auto, Some("beta")), &free, "com.example.app.TEAM123456", true),
            (request(BundleIdPolicy::Auto, None), &paid, "com.example.app", false),
            (request(BundleIdPolicy::Original, None), &free, "com.example.app", false),
            (request(BundleIdPolicy::Custom("org.custom.id".into()), None), &paid, "org.custom.id", true),
        ];

        for (request, team, expected, records) in cases {
            assert_eq!(bundle_identifier(&request, team), (expected.to_owned(), records), "{request:?}");
        }
    }

    #[test]
    fn apple_tv_uses_tvos_only_when_requested() {
        let mut tv = request(BundleIdPolicy::Auto, Some("17.0"));
        tv.device.as_mut().expect("device").device_class = Some("AppleTV".into());

        assert_eq!(platform(&tv), Platform::Ios);

        tv.tvos_for_apple_tv = true;
        assert_eq!(platform(&tv), Platform::Tvos);

        let mut phone = request(BundleIdPolicy::Auto, Some("17.0"));
        phone.tvos_for_apple_tv = true;
        assert_eq!(platform(&phone), Platform::Ios);
    }

    #[test]
    fn portal_names_udids_and_extension_ids_are_normalized() {
        assert_eq!(portal_name("  Café — Notes!!  2 ", "com.example.app"), "Caf Notes 2");
        assert_eq!(portal_name("✨✨", "com.example.app"), "com example app");
        assert_eq!(portal_name("", "").split(' ').count(), 5);

        assert_eq!(format_udid("00008030-001A2D0C0E38802E"), "00008030001a2d0c0e38802e");
        assert_eq!(
            prepared_extension_id("com.example.app.widget", "com.example.app", "com.example.app.T"),
            "com.example.app.T.widget"
        );
        assert_eq!(prepared_extension_id("org.other.ext", "com.example.app", "com.example.app.T"), "org.other.ext");
    }

    #[test]
    fn the_oldest_certificate_has_the_earliest_known_expiration() {
        let record = |serial: &str, expiration: Option<&str>| CertificateRecord {
            serial_number: serial.into(),
            machine_name: None,
            expiration: expiration.map(|text| text.parse().expect("date")),
            content_der: None,
        };

        let records = [
            record("NEVER", None),
            record("LATE", Some("2027-01-01T00:00:00Z")),
            record("EARLY", Some("2026-10-01T00:00:00Z")),
        ];

        assert_eq!(oldest_certificate(&records).map(|record| record.serial_number.as_str()), Some("EARLY"));
        assert_eq!(oldest_certificate(&records[..1]).map(|record| record.serial_number.as_str()), Some("NEVER"));
        assert!(oldest_certificate(&[]).is_none());
    }
}
