use cms::{
    content_info::{CmsVersion, ContentInfo},
    signed_data::{EncapsulatedContentInfo, SignedData, SignerInfos},
};
use const_oid::ObjectIdentifier;
use der::asn1::{OctetString, SetOfVec};
use der::{Any, Encode};
use sl_codesign::{ProfileTarget, ProvisioningProfile};

const TEAM: &str = "TEAM123456";
const MODERN_UDID: &str = "00008030-001A2D0C0E38802E";
const LEGACY_UDID: &str = "0123456789abcdef0123456789abcdef01234567";

// Intentionally unsigned CMS structure: these tests exercise decoding, not trust validation.
fn envelope(payload: &[u8], detached: bool) -> Vec<u8> {
    let econtent = if detached {
        None
    } else {
        let octets = OctetString::new(payload).expect("octets");

        Some(Any::encode_from(&octets).expect("any"))
    };

    let signed = SignedData {
        version: CmsVersion::V1,
        digest_algorithms: SetOfVec::new(),
        encap_content_info: EncapsulatedContentInfo {
            econtent_type: ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1"),
            econtent,
        },
        certificates: None,
        crls: None,
        signer_infos: SignerInfos(SetOfVec::new()),
    };

    ContentInfo {
        content_type: ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2"),
        content: Any::encode_from(&signed).expect("CMS"),
    }
    .to_der()
    .expect("DER")
}

fn payload() -> plist::Dictionary {
    let creation_date = plist::Date::from_xml_format("2026-09-01T00:00:00Z").expect("date");
    let expiration_date = plist::Date::from_xml_format("2026-09-08T00:00:00Z").expect("date");

    let mut entitlements = plist::Dictionary::new();
    entitlements.insert("application-identifier".into(), "OLDPREFIX.com.example.*".into());

    let mut profile = plist::Dictionary::new();
    profile.insert("Name".into(), "Test Profile".into());
    profile.insert("UUID".into(), "TEST-UUID".into());
    profile.insert("TeamIdentifier".into(), plist::Value::Array(vec![TEAM.into()]));
    profile.insert("ApplicationIdentifierPrefix".into(), plist::Value::Array(vec!["OLDPREFIX".into()]));
    profile.insert("Platform".into(), plist::Value::Array(vec!["iOS".into(), "xrOS".into()]));
    profile.insert("Entitlements".into(), plist::Value::Dictionary(entitlements));

    profile.insert("CreationDate".into(), plist::Value::Date(creation_date));
    profile.insert("ExpirationDate".into(), plist::Value::Date(expiration_date));
    profile.insert("TimeToLive".into(), 7u64.into());

    profile.insert("DeveloperCertificates".into(), plist::Value::Array(vec![plist::Value::Data(vec![1, 2, 3])]));
    profile.insert("ProvisionedDevices".into(), plist::Value::Array(vec![MODERN_UDID.into(), LEGACY_UDID.into()]));
    profile.insert("LocalProvision".into(), true.into());

    profile
}

fn parse(payload: &plist::Dictionary) -> sl_codesign::Result<ProvisioningProfile> {
    let xml = sl_codesign::entitlements::to_xml(payload).expect("plist");

    ProvisioningProfile::parse(&envelope(&xml, false))
}

fn parsed_profile() -> ProvisioningProfile {
    parse(&payload()).expect("profile")
}

fn target() -> ProfileTarget<'static> {
    let now = "2026-09-04T12:00:00Z".parse().expect("fixture time");

    ProfileTarget {
        team_id: TEAM,
        bundle_id: "com.example.demo",
        certificate_der: &[1, 2, 3],
        device_udid: Some(MODERN_UDID),
        platform: Some("iOS"),
        now,
    }
}

fn with_app_id(app_id: &str) -> ProvisioningProfile {
    let mut profile = parsed_profile();
    profile.entitlements.insert("application-identifier".into(), app_id.into());

    profile
}

#[test]
fn profile_decoding_preserves_envelope_dates_and_prefix_semantics() {
    let xml = sl_codesign::entitlements::to_xml(&payload()).expect("plist");
    let raw = envelope(&xml, false);

    assert_eq!(ProvisioningProfile::payload(&raw).expect("payload"), xml);

    let profile = ProvisioningProfile::parse(&raw).expect("profile");

    assert_eq!(profile.raw, raw);
    assert_eq!(profile.bundle_id(), Some("com.example.*"));
    assert_eq!(profile.team_identifiers, [TEAM]);
    assert_eq!(profile.application_identifier_prefixes, ["OLDPREFIX"]);
    assert_eq!(profile.platforms, ["iOS", "xrOS"]);
    assert_eq!(profile.provisioned_devices, [MODERN_UDID, LEGACY_UDID]);
    assert!(!profile.provisions_all_devices);
    assert_eq!(profile.time_to_live_days, Some(7));
    assert!(profile.local_provision);
    assert_eq!((profile.expiration_date - profile.creation_date).num_seconds(), 7 * 86400);
}

#[test]
fn profile_validation_accepts_the_matching_target() {
    parsed_profile().validate_for(target()).expect("matching profile");
}

#[test]
fn validity_includes_creation_and_excludes_expiration() {
    let profile = parsed_profile();
    let second = chrono::Duration::seconds(1);

    for now in [profile.creation_date, profile.expiration_date - second] {
        profile.validate_for(ProfileTarget { now, ..target() }).expect("inside validity");
    }

    for now in [profile.creation_date - second, profile.expiration_date, profile.expiration_date + second] {
        assert!(profile.validate_for(ProfileTarget { now, ..target() }).is_err(), "{now}");
    }
}

#[test]
fn listed_legacy_prefix_is_accepted_and_unlisted_prefixes_are_rejected() {
    with_app_id("OLDPREFIX.com.example.demo").validate_for(target()).expect("listed legacy prefix");

    let unlisted_team_prefix = with_app_id("TEAM123456.com.example.demo");
    assert!(unlisted_team_prefix.validate_for(target()).is_err());

    let mut other_list = parsed_profile();
    other_list.application_identifier_prefixes = vec!["DIFFERENT".into()];
    assert!(other_list.validate_for(target()).is_err());
}

#[test]
fn absent_prefix_list_requires_the_app_id_prefix_to_equal_the_team() {
    let mut fields = payload();
    fields.remove("ApplicationIdentifierPrefix");

    let mut entitlements = plist::Dictionary::new();
    entitlements.insert("application-identifier".into(), "TEAM123456.com.example.*".into());
    fields.insert("Entitlements".into(), plist::Value::Dictionary(entitlements));

    let profile = parse(&fields).expect("profile without prefix list");
    assert!(profile.application_identifier_prefixes.is_empty());
    profile.validate_for(target()).expect("Team ID prefix");

    let mut legacy = profile;
    legacy.entitlements.insert("application-identifier".into(), "OLDPREFIX.com.example.*".into());
    assert!(legacy.validate_for(target()).is_err(), "legacy prefix cannot be associated without the list");
}

#[test]
fn explicit_app_ids_match_exactly() {
    let profile = with_app_id("OLDPREFIX.com.example.demo");

    profile.validate_for(target()).expect("exact bundle identifier");

    for bundle_id in ["com.example.demo.widget", "com.example.dem", "COM.EXAMPLE.DEMO", "com.example.demo*", ""] {
        assert!(profile.validate_for(ProfileTarget { bundle_id, ..target() }).is_err(), "{bundle_id:?}");
    }
}

#[test]
fn wildcard_app_ids_cover_only_nonempty_suffixes_of_their_stem() {
    let dotted = with_app_id("OLDPREFIX.com.example.*");

    for bundle_id in ["com.example.demo", "com.example.demo.widget", "com.example.d"] {
        dotted.validate_for(ProfileTarget { bundle_id, ..target() }).expect(bundle_id);
    }

    for bundle_id in ["com.example", "com.example.", "com.examplex", "com.examples.demo", "org.example.demo"] {
        assert!(dotted.validate_for(ProfileTarget { bundle_id, ..target() }).is_err(), "{bundle_id:?}");
    }

    let team = with_app_id("OLDPREFIX.*");
    team.validate_for(ProfileTarget { bundle_id: "org.any.app", ..target() }).expect("team wildcard");
    assert!(team.validate_for(ProfileTarget { bundle_id: "com.example.*", ..target() }).is_err());

    let undotted = with_app_id("OLDPREFIX.com.example*");
    undotted.validate_for(ProfileTarget { bundle_id: "com.exampleapp", ..target() }).expect("trailing stem");
    assert!(undotted.validate_for(ProfileTarget { bundle_id: "com.example", ..target() }).is_err());

    let interior = with_app_id("OLDPREFIX.com.*.demo");
    assert!(interior.validate_for(target()).is_err());
    assert!(interior.validate_for(ProfileTarget { bundle_id: "com.*.demo", ..target() }).is_err());

    let double = with_app_id("OLDPREFIX.com.*.*");
    assert!(double.validate_for(target()).is_err());
}

#[test]
fn malformed_or_missing_application_identifiers_are_rejected() {
    for app_id in ["", ".com.example.demo", "OLDPREFIX.", "OLDPREFIX", "OLDPREFIXcom.example.demo"] {
        assert!(with_app_id(app_id).validate_for(target()).is_err(), "{app_id:?}");
    }

    let mut non_string = parsed_profile();
    non_string.entitlements.insert("application-identifier".into(), true.into());
    assert!(non_string.validate_for(target()).is_err());

    let mut missing = parsed_profile();
    missing.entitlements.remove("application-identifier");
    assert!(missing.validate_for(target()).is_err());
}

#[test]
fn team_and_team_entitlement_must_match_the_selected_team() {
    let profile = parsed_profile();

    for team_id in ["OTHERTEAM", "", "team123456"] {
        assert!(profile.validate_for(ProfileTarget { team_id, ..target() }).is_err(), "{team_id:?}");
    }

    let mut matching = parsed_profile();
    matching.entitlements.insert("com.apple.developer.team-identifier".into(), TEAM.into());
    matching.validate_for(target()).expect("matching team entitlement");

    let mut other = parsed_profile();
    other.entitlements.insert("com.apple.developer.team-identifier".into(), "OTHERTEAM".into());
    assert!(other.validate_for(target()).is_err());

    let mut non_string = parsed_profile();
    non_string.entitlements.insert("com.apple.developer.team-identifier".into(), 1u64.into());
    assert!(non_string.validate_for(target()).is_err());
}

#[test]
fn certificate_must_be_listed_byte_for_byte() {
    let profile = parsed_profile();

    for certificate_der in [&[9, 9, 9][..], &[1, 2][..], &[1, 2, 3, 4][..], &[][..]] {
        assert!(profile.validate_for(ProfileTarget { certificate_der, ..target() }).is_err(), "{certificate_der:?}");
    }
}

#[test]
fn platform_is_checked_only_when_the_profile_lists_platforms() {
    let profile = parsed_profile();

    profile.validate_for(ProfileTarget { platform: Some("xrOS"), ..target() }).expect("listed platform");
    profile.validate_for(ProfileTarget { platform: None, ..target() }).expect("unchecked platform");
    assert!(profile.validate_for(ProfileTarget { platform: Some("tvOS"), ..target() }).is_err());
    assert!(profile.validate_for(ProfileTarget { platform: Some("ios"), ..target() }).is_err());

    let mut unlisted = parsed_profile();
    unlisted.platforms.clear();
    unlisted.validate_for(ProfileTarget { platform: Some("tvOS"), ..target() }).expect("profile without Platform");
}

#[test]
fn device_udids_match_case_and_hyphen_insensitively() {
    let profile = parsed_profile();

    for device_udid in [
        MODERN_UDID,
        "00008030-001a2d0c0e38802e",
        "00008030001A2D0C0E38802E",
        LEGACY_UDID,
        "0123456789ABCDEF0123456789ABCDEF01234567",
    ] {
        profile.validate_for(ProfileTarget { device_udid: Some(device_udid), ..target() }).expect(device_udid);
    }

    profile.validate_for(ProfileTarget { device_udid: None, ..target() }).expect("unchecked device");
}

#[test]
fn absent_malformed_and_unprovisioned_devices_are_rejected() {
    let profile = parsed_profile();

    for device_udid in ["00008030-001A2D0C0E38802F", "0123456789abcdef", "", "-", "device-1", "00008030 001A2D0C"] {
        let case = ProfileTarget { device_udid: Some(device_udid), ..target() };

        assert!(profile.validate_for(case).is_err(), "{device_udid:?}");
    }

    let mut empty = parsed_profile();
    empty.provisioned_devices.clear();
    assert!(empty.validate_for(target()).is_err());

    let mut malformed_entry = parsed_profile();
    malformed_entry.provisioned_devices = vec!["not-a-udid".into()];
    assert!(malformed_entry.validate_for(ProfileTarget { device_udid: Some("a"), ..target() }).is_err());
}

#[test]
fn provisions_all_devices_accepts_any_wellformed_udid() {
    let mut fields = payload();
    fields.remove("ProvisionedDevices");
    fields.insert("ProvisionsAllDevices".into(), true.into());

    let profile = parse(&fields).expect("in-house profile");

    assert!(profile.provisions_all_devices);
    profile.validate_for(ProfileTarget { device_udid: Some("00008103-000A1B2C3D4E5F60"), ..target() }).expect("any");
    assert!(profile.validate_for(ProfileTarget { device_udid: Some("not-a-udid"), ..target() }).is_err());
}

#[test]
fn profile_rejects_missing_payload_invalid_types_dates_and_non_cms() {
    assert!(ProvisioningProfile::parse(b"not CMS").is_err());
    assert!(ProvisioningProfile::payload(&envelope(b"", true)).is_err());

    for key in ["Name", "UUID", "TeamIdentifier", "Entitlements", "CreationDate", "DeveloperCertificates"] {
        let mut profile = payload();
        profile.remove(key);

        assert!(parse(&profile).is_err(), "{key}");
    }

    let mut profile = payload();
    profile.insert("ExpirationDate".into(), profile["CreationDate"].clone());
    assert!(parse(&profile).is_err());

    let invalid_values = [
        ("TimeToLive", (-1i64).into()),
        ("ApplicationIdentifierPrefix", "OLDPREFIX".into()),
        ("ApplicationIdentifierPrefix", plist::Value::Array(vec!["".into()])),
        ("Platform", plist::Value::Array(vec![1u64.into()])),
        ("ProvisionsAllDevices", "true".into()),
        ("ProvisionedDevices", plist::Value::Array(vec![true.into()])),
        ("TeamIdentifier", plist::Value::Array(Vec::new())),
        ("DeveloperCertificates", plist::Value::Array(vec!["certificate".into()])),
    ];

    for (key, value) in invalid_values {
        let mut profile = payload();
        profile.insert(key.into(), value);

        assert!(parse(&profile).is_err(), "{key}");
    }
}
