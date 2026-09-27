use cms::{
    content_info::{CmsVersion, ContentInfo},
    signed_data::{EncapsulatedContentInfo, SignedData, SignerInfos},
};
use const_oid::ObjectIdentifier;
use der::asn1::{OctetString, SetOfVec};
use der::{Any, Encode};
use sl_codesign::ProvisioningProfile;

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
    profile.insert("TeamIdentifier".into(), plist::Value::Array(vec!["TEAM123456".into()]));
    profile.insert("Entitlements".into(), plist::Value::Dictionary(entitlements));

    profile.insert("CreationDate".into(), plist::Value::Date(creation_date));
    profile.insert("ExpirationDate".into(), plist::Value::Date(expiration_date));
    profile.insert("TimeToLive".into(), 7u64.into());

    profile.insert("DeveloperCertificates".into(), plist::Value::Array(vec![plist::Value::Data(vec![1, 2, 3])]));
    profile.insert("ProvisionedDevices".into(), plist::Value::Array(vec!["device-1".into()]));
    profile.insert("LocalProvision".into(), true.into());

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
    assert_eq!(profile.team_identifiers, ["TEAM123456"]);
    assert_eq!(profile.provisioned_devices, ["device-1"]);
    assert_eq!(profile.time_to_live_days, Some(7));
    assert!(profile.local_provision);
    assert_eq!((profile.expiration_date - profile.creation_date).num_seconds(), 7 * 86400);
}

#[test]
fn profile_rejects_missing_payload_invalid_types_dates_and_non_cms() {
    assert!(ProvisioningProfile::parse(b"not CMS").is_err());
    assert!(ProvisioningProfile::payload(&envelope(b"", true)).is_err());

    for key in ["Name", "UUID", "TeamIdentifier", "Entitlements", "CreationDate", "DeveloperCertificates"] {
        let mut profile = payload();
        profile.remove(key);

        let xml = sl_codesign::entitlements::to_xml(&profile).expect("plist");
        let raw = envelope(&xml, false);

        assert!(ProvisioningProfile::parse(&raw).is_err(), "{key}");
    }

    let mut profile = payload();
    profile.insert("ExpirationDate".into(), profile["CreationDate"].clone());

    let xml = sl_codesign::entitlements::to_xml(&profile).expect("plist");

    assert!(ProvisioningProfile::parse(&envelope(&xml, false)).is_err());

    let mut profile = payload();
    profile.insert("TimeToLive".into(), (-1i64).into());

    let xml = sl_codesign::entitlements::to_xml(&profile).expect("plist");

    assert!(ProvisioningProfile::parse(&envelope(&xml, false)).is_err());
}
