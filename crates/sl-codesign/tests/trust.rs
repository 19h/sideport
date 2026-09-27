//! Profile CMS trust fixtures. The generated chain mirrors the inspected Apple layout:
//! root → "Apple iPhone Certification Authority" → "Apple iPhone OS Provisioning Profile Signing".
//! Fixture names do not make these certificates Apple-trusted; only the configured anchor does.

use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{
    CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos,
};
use const_oid::ObjectIdentifier;
use der::asn1::{OctetString, SetOfVec, UtcTime};
use der::{Any, Decode, Encode, EncodePem};
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use signature::{SignatureEncoding, Signer as _};
use sl_codesign::{ProfileTarget, ProfileTrust, ProvisioningProfile};
use spki::{AlgorithmIdentifierOwned, SubjectPublicKeyInfoOwned};
use std::sync::OnceLock;
use std::time::Duration;
use x509_cert::Certificate;
use x509_cert::attr::Attribute;
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::time::{Time, Validity};

const DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");
const SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
const CONTENT_TYPE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.3");
const MESSAGE_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4");
const SHA1_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.14.3.2.26");
const SHA256_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");
const RSA_ENCRYPTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");

const SIGNER: &str = "Apple iPhone OS Provisioning Profile Signing";
const ISSUER: &str = "Apple iPhone Certification Authority";

struct Issued {
    certificate: Certificate,
    key: RsaPrivateKey,
}

struct Chain {
    root: Issued,
    intermediate: Issued,
    leaf: Issued,
    expired_leaf: Issued,
    leaf_as_issuer: Issued,
    other_root: Issued,
}

fn validity(from: &str, to: &str) -> Validity {
    let seconds = |text: &str| {
        let time: chrono::DateTime<chrono::Utc> = text.parse().expect("fixture time");

        Duration::from_secs(u64::try_from(time.timestamp()).expect("positive time"))
    };

    Validity {
        not_before: Time::UtcTime(UtcTime::from_unix_duration(seconds(from)).expect("not before")),
        not_after: Time::UtcTime(UtcTime::from_unix_duration(seconds(to)).expect("not after")),
    }
}

fn issue(subject: &str, profile: Profile, serial: u32, period: Validity, issuer: Option<&Issued>) -> Issued {
    let key = sl_codesign::generate_signing_key().expect("RSA key");

    let public_key = rsa::pkcs8::EncodePublicKey::to_public_key_der(&key.to_public_key()).expect("SPKI");
    let public_key = SubjectPublicKeyInfoOwned::from_der(public_key.as_bytes()).expect("SPKI");
    let subject: Name = subject.parse().expect("subject");

    let signing_key = SigningKey::<Sha256>::new(issuer.map_or(&key, |issuer| &issuer.key).clone());
    let builder =
        CertificateBuilder::new(profile, SerialNumber::from(serial), period, subject, public_key, &signing_key)
            .expect("certificate builder");
    let certificate = builder.build::<rsa::pkcs1v15::Signature>().expect("certificate");

    Issued { certificate, key }
}

fn chain() -> &'static Chain {
    static CHAIN: OnceLock<Chain> = OnceLock::new();

    CHAIN.get_or_init(|| {
        let period = || validity("2020-01-01T00:00:00Z", "2040-01-01T00:00:00Z");

        let root = issue("CN=Fixture Root CA,O=Sideport,C=US", Profile::Root, 1, period(), None);
        let other_root = issue("CN=Fixture Root CA,O=Sideport,C=US", Profile::Root, 2, period(), None);

        let root_name = root.certificate.tbs_certificate.subject.clone();
        let intermediate_profile = Profile::SubCA { issuer: root_name.clone(), path_len_constraint: Some(0) };
        let intermediate_subject = format!("CN={ISSUER},O=Sideport,C=US");
        let intermediate = issue(&intermediate_subject, intermediate_profile, 3, period(), Some(&root));

        let intermediate_name = intermediate.certificate.tbs_certificate.subject.clone();
        let leaf_profile = || Profile::Leaf {
            issuer: intermediate_name.clone(),
            enable_key_agreement: false,
            enable_key_encipherment: false,
        };
        let leaf_subject = format!("CN={SIGNER},O=Sideport,C=US");
        let leaf = issue(&leaf_subject, leaf_profile(), 4, period(), Some(&intermediate));

        let expired_period = validity("2020-01-01T00:00:00Z", "2021-01-01T00:00:00Z");
        let expired_leaf = issue(&leaf_subject, leaf_profile(), 5, expired_period, Some(&intermediate));

        let non_ca_profile =
            Profile::Leaf { issuer: root_name, enable_key_agreement: false, enable_key_encipherment: false };
        let leaf_as_issuer = issue(&intermediate_subject, non_ca_profile, 6, period(), Some(&root));

        Chain { root, intermediate, leaf, expired_leaf, leaf_as_issuer, other_root }
    })
}

fn payload() -> Vec<u8> {
    let creation_date = plist::Date::from_xml_format("2026-09-01T00:00:00Z").expect("date");
    let expiration_date = plist::Date::from_xml_format("2026-09-08T00:00:00Z").expect("date");

    let mut entitlements = plist::Dictionary::new();
    entitlements.insert("application-identifier".into(), "TEAM123456.com.example.demo".into());

    let mut profile = plist::Dictionary::new();
    profile.insert("Name".into(), "Signed Fixture".into());
    profile.insert("UUID".into(), "SIGNED-UUID".into());
    profile.insert("TeamIdentifier".into(), plist::Value::Array(vec!["TEAM123456".into()]));
    profile.insert("Entitlements".into(), plist::Value::Dictionary(entitlements));
    profile.insert("CreationDate".into(), plist::Value::Date(creation_date));
    profile.insert("ExpirationDate".into(), plist::Value::Date(expiration_date));
    profile.insert("DeveloperCertificates".into(), plist::Value::Array(Vec::new()));

    sl_codesign::entitlements::to_xml(&profile).expect("plist")
}

#[derive(Clone, Copy)]
enum DigestKind {
    Sha1,
    Sha256,
}

fn attribute(oid: ObjectIdentifier, value: Any) -> Attribute {
    Attribute { oid, values: SetOfVec::try_from(vec![value]).expect("attribute") }
}

fn sign(content: &[u8], signer: &Issued, embedded: &[&Certificate], digest: DigestKind) -> Vec<u8> {
    let (digest_oid, content_digest) = match digest {
        DigestKind::Sha1 => (SHA1_DIGEST, Sha1::digest(content).to_vec()),
        DigestKind::Sha256 => (SHA256_DIGEST, Sha256::digest(content).to_vec()),
    };

    let content_type = attribute(CONTENT_TYPE, Any::encode_from(&DATA).expect("content type"));
    let message_digest = OctetString::new(content_digest).expect("digest");
    let message_digest = attribute(MESSAGE_DIGEST, Any::encode_from(&message_digest).expect("digest"));
    let attributes = SetOfVec::try_from(vec![content_type, message_digest]).expect("attributes");

    let signed_bytes = attributes.to_der().expect("attributes DER");
    let signature = match digest {
        DigestKind::Sha1 => SigningKey::<Sha1>::new(signer.key.clone()).sign(&signed_bytes).to_vec(),
        DigestKind::Sha256 => SigningKey::<Sha256>::new(signer.key.clone()).sign(&signed_bytes).to_vec(),
    };

    let leaf = &signer.certificate.tbs_certificate;
    let identifier = IssuerAndSerialNumber { issuer: leaf.issuer.clone(), serial_number: leaf.serial_number.clone() };
    let algorithm = |oid, parameters| AlgorithmIdentifierOwned { oid, parameters };

    let signer_info = SignerInfo {
        version: CmsVersion::V1,
        sid: SignerIdentifier::IssuerAndSerialNumber(identifier),
        digest_alg: algorithm(digest_oid, None),
        signed_attrs: Some(attributes),
        signature_algorithm: algorithm(RSA_ENCRYPTION, Some(Any::null())),
        signature: OctetString::new(signature).expect("signature"),
        unsigned_attrs: None,
    };

    let certificates = embedded.iter().map(|certificate| CertificateChoices::Certificate((*certificate).clone()));
    let certificates = SetOfVec::try_from(certificates.collect::<Vec<_>>()).expect("certificates");
    let econtent = Any::encode_from(&OctetString::new(content).expect("content")).expect("content");

    let signed = SignedData {
        version: CmsVersion::V1,
        digest_algorithms: SetOfVec::try_from(vec![algorithm(digest_oid, None)]).expect("digests"),
        encap_content_info: EncapsulatedContentInfo { econtent_type: DATA, econtent: Some(econtent) },
        certificates: Some(CertificateSet(certificates)),
        crls: None,
        signer_infos: SignerInfos(SetOfVec::try_from(vec![signer_info]).expect("signers")),
    };

    let envelope = ContentInfo { content_type: SIGNED_DATA, content: Any::encode_from(&signed).expect("SignedData") };

    envelope.to_der().expect("CMS")
}

fn standard(digest: DigestKind) -> Vec<u8> {
    let chain = chain();

    sign(
        &payload(),
        &chain.leaf,
        &[&chain.leaf.certificate, &chain.intermediate.certificate, &chain.root.certificate],
        digest,
    )
}

fn trust_for(root: &Issued) -> ProfileTrust {
    let pem = root.certificate.to_pem(der::pem::LineEnding::LF).expect("anchor PEM");

    ProfileTrust::from_pem(&pem, SIGNER, ISSUER).expect("trust")
}

fn verify(raw: &[u8], trust: &ProfileTrust) -> sl_codesign::Result<()> {
    ProvisioningProfile::parse(raw)?.verify_trust(trust)
}

#[test]
fn sha1_and_sha256_profiles_chain_to_the_configured_anchor() {
    let trust = trust_for(&chain().root);

    for digest in [DigestKind::Sha1, DigestKind::Sha256] {
        verify(&standard(digest), &trust).expect("trusted profile");
    }
}

#[test]
fn a_payload_change_breaks_the_message_digest() {
    let mut raw = standard(DigestKind::Sha256);
    let position = raw.windows(14).position(|window| window == b"Signed Fixture").expect("payload name");
    raw[position] = b'X';

    let profile = ProvisioningProfile::parse(&raw).expect("tampered payload still decodes");
    assert_eq!(profile.name, "Xigned Fixture");

    let error = profile.verify_trust(&trust_for(&chain().root)).expect_err("tampered payload");
    assert!(error.to_string().contains("message digest"), "{error}");
}

#[test]
fn a_signature_change_is_rejected() {
    let raw = standard(DigestKind::Sha256);
    let chain = chain();

    let mut envelope = ContentInfo::from_der(&raw).expect("envelope");
    let mut signed: SignedData = envelope.content.decode_as().expect("SignedData");
    let mut signers = signed.signer_infos.0.into_vec();
    let mut signature = signers[0].signature.as_bytes().to_vec();
    signature[10] ^= 1;
    signers[0].signature = OctetString::new(signature).expect("signature");
    signed.signer_infos = SignerInfos(SetOfVec::try_from(signers).expect("signers"));
    envelope.content = Any::encode_from(&signed).expect("SignedData");

    let tampered = envelope.to_der().expect("CMS");
    let error = verify(&tampered, &trust_for(&chain.root)).expect_err("tampered signature");
    assert!(error.to_string().contains("signature"), "{error}");
}

#[test]
fn an_unsigned_or_self_rooted_profile_is_not_trusted() {
    let chain = chain();
    let trust = trust_for(&chain.root);

    let unsigned = sign(&payload(), &chain.leaf, &[], DigestKind::Sha256);
    assert!(verify(&unsigned, &trust).is_err(), "signer certificate is required");

    assert!(verify(&standard(DigestKind::Sha256), &trust_for(&chain.other_root)).is_err(), "same-name other root");

    let missing_intermediate =
        sign(&payload(), &chain.leaf, &[&chain.leaf.certificate, &chain.root.certificate], DigestKind::Sha256);
    assert!(verify(&missing_intermediate, &trust).is_err(), "intermediate is required");
}

#[test]
fn signer_names_ca_constraints_and_validity_follow_the_policy() {
    let chain = chain();
    let raw = standard(DigestKind::Sha256);

    let root_pem = chain.root.certificate.to_pem(der::pem::LineEnding::LF).expect("anchor PEM");
    let other_signer = ProfileTrust::from_pem(&root_pem, "Other Signer", ISSUER).expect("trust");
    let other_issuer = ProfileTrust::from_pem(&root_pem, SIGNER, "Other Issuer").expect("trust");

    assert!(verify(&raw, &other_signer).is_err(), "leaf name");
    assert!(verify(&raw, &other_issuer).is_err(), "issuer name");

    let trust = trust_for(&chain.root);
    let embedded = [&chain.expired_leaf.certificate, &chain.intermediate.certificate];
    let expired = sign(&payload(), &chain.expired_leaf, &embedded, DigestKind::Sha256);
    let error = verify(&expired, &trust).expect_err("expired leaf");
    assert!(error.to_string().contains("validity"), "{error}");

    let signer = issue_under(&chain.leaf_as_issuer);
    let embedded = [&signer.certificate, &chain.leaf_as_issuer.certificate];
    let non_ca = sign(&payload(), &signer, &embedded, DigestKind::Sha256);
    let error = verify(&non_ca, &trust).expect_err("non-CA issuer");
    assert!(error.to_string().contains("certificate authority"), "{error}");

    assert!(ProfileTrust::from_pem("", SIGNER, ISSUER).is_err());
    assert!(ProfileTrust::from_pem(&root_pem, "", ISSUER).is_err());
}

fn issue_under(issuer: &Issued) -> Issued {
    let profile = Profile::Leaf {
        issuer: issuer.certificate.tbs_certificate.subject.clone(),
        enable_key_agreement: false,
        enable_key_encipherment: false,
    };
    let period = validity("2020-01-01T00:00:00Z", "2040-01-01T00:00:00Z");

    issue(&format!("CN={SIGNER},O=Sideport,C=US"), profile, 7, period, Some(issuer))
}

#[test]
fn the_bundled_apple_policy_rejects_fixture_chains() {
    let apple = ProfileTrust::apple().expect("Apple trust");

    assert!(verify(&standard(DigestKind::Sha1), &apple).is_err());
}

/// Local real-sample probe: `SIDEPORT_REAL_PROFILE=/path/to/profile.mobileprovision cargo test
/// -p sl-codesign --test trust -- --ignored`. Profiles are user data and are not checked in.
#[test]
#[ignore = "requires a locally supplied Apple-issued profile"]
fn a_supplied_apple_profile_chains_to_apple_root() {
    let path = std::env::var_os("SIDEPORT_REAL_PROFILE").expect("SIDEPORT_REAL_PROFILE");
    let raw = std::fs::read(path).expect("profile");

    let apple = ProfileTrust::apple().expect("Apple trust");

    let profile = ProvisioningProfile::parse(&raw).expect("decoded profile");
    profile.verify_trust(&apple).expect("Apple-issued profile");

    // Decoded-field rules against the profile's own team, certificate, wildcard stem and devices.
    let pattern = profile.bundle_id().expect("application identifier");
    let bundle_id = pattern.strip_suffix('*').map_or_else(|| pattern.to_owned(), |stem| format!("{stem}probe"));
    let target = ProfileTarget {
        team_id: &profile.team_identifiers[0],
        bundle_id: &bundle_id,
        certificate_der: &profile.developer_certificates[0],
        device_udid: None,
        platform: profile.platforms.first().map(String::as_str),
        now: profile.creation_date,
    };

    profile.validate_for(target).expect("decoded fields");

    for device in &profile.provisioned_devices {
        let lowercase = device.to_ascii_lowercase();

        profile.validate_for(ProfileTarget { device_udid: Some(device), ..target }).expect("listed device");
        profile.validate_for(ProfileTarget { device_udid: Some(&lowercase), ..target }).expect("casefolded device");
    }

    let mut tampered = raw;
    let position = tampered.windows(8).position(|window| window == b"<string>").expect("payload string");
    tampered[position + 8] ^= 1;

    assert!(verify(&tampered, &apple).is_err());
}
