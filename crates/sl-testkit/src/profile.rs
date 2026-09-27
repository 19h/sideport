//! Provisioning-profile payloads and CMS SignedData envelopes.

use crate::pki::ProfileChain;
use chrono::{DateTime, Utc};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{
    CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos,
};
use const_oid::ObjectIdentifier;
use der::asn1::{OctetString, SetOfVec};
use der::{Any, Encode};
use rsa::pkcs1v15::SigningKey;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use signature::{SignatureEncoding, Signer as _};
use spki::AlgorithmIdentifierOwned;
use x509_cert::attr::Attribute;

const DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");
const SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
const CONTENT_TYPE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.3");
const MESSAGE_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4");
const SHA1_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.14.3.2.26");
const RSA_ENCRYPTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");

/// Fields of a development profile issued for one App ID.
#[derive(Debug, Clone)]
pub struct ProfileFields {
    pub team_id: String,
    /// App ID identifier without the team prefix (may end in `*`).
    pub identifier: String,
    pub name: String,
    pub uuid: String,
    pub created: DateTime<Utc>,
    pub expires: DateTime<Utc>,
    pub free: bool,
    pub platform: &'static str,
    pub devices: Vec<String>,
    pub certificates: Vec<Vec<u8>>,
}

impl ProfileFields {
    pub fn payload(&self) -> Vec<u8> {
        let mut entitlements = plist::Dictionary::new();
        entitlements.insert("application-identifier".into(), format!("{}.{}", self.team_id, self.identifier).into());
        entitlements.insert("com.apple.developer.team-identifier".into(), self.team_id.clone().into());
        entitlements.insert("get-task-allow".into(), true.into());
        entitlements
            .insert("keychain-access-groups".into(), plist::Value::Array(vec![format!("{}.*", self.team_id).into()]));

        let date = |time: DateTime<Utc>| plist::Value::Date(std::time::SystemTime::from(time).into());
        let strings = |values: &[String]| plist::Value::Array(values.iter().cloned().map(Into::into).collect());

        let mut fields = plist::Dictionary::new();
        fields.insert("AppIDName".into(), self.name.clone().into());
        fields.insert("ApplicationIdentifierPrefix".into(), strings(std::slice::from_ref(&self.team_id)));
        fields.insert("CreationDate".into(), date(self.created));
        fields.insert("ExpirationDate".into(), date(self.expires));
        fields.insert("Entitlements".into(), plist::Value::Dictionary(entitlements));
        fields.insert("Name".into(), format!("iOS Team Provisioning Profile: {}", self.identifier).into());
        fields.insert("Platform".into(), plist::Value::Array(vec![self.platform.into()]));
        fields.insert("ProvisionedDevices".into(), strings(&self.devices));
        fields.insert("TeamIdentifier".into(), strings(std::slice::from_ref(&self.team_id)));
        fields.insert("TeamName".into(), "Fixture Team".into());
        fields.insert("TimeToLive".into(), ((self.expires - self.created).num_days().max(0) as u64).into());
        fields.insert("UUID".into(), self.uuid.clone().into());
        fields.insert("Version".into(), 1u64.into());

        let certificates = self.certificates.iter().cloned().map(plist::Value::Data).collect();
        fields.insert("DeveloperCertificates".into(), plist::Value::Array(certificates));

        if self.free {
            fields.insert("LocalProvision".into(), true.into());
        }

        let mut bytes = Vec::new();
        plist::Value::Dictionary(fields).to_writer_xml(&mut bytes).expect("profile plist");

        bytes
    }

    /// The payload in a SHA-1 SignedData envelope, as inspected Apple profiles use.
    pub fn signed(&self, chain: &ProfileChain) -> Vec<u8> {
        sign(&self.payload(), chain)
    }
}

/// CMS SignedData over `content` by the chain's profile signer, embedding signer and issuer.
pub fn sign(content: &[u8], chain: &ProfileChain) -> Vec<u8> {
    let content_type = attribute(CONTENT_TYPE, Any::encode_from(&DATA).expect("content type"));
    let digest = OctetString::new(Sha1::digest(content).to_vec()).expect("digest");
    let message_digest = attribute(MESSAGE_DIGEST, Any::encode_from(&digest).expect("digest"));
    let attributes = SetOfVec::try_from(vec![content_type, message_digest]).expect("attributes");

    let signed_bytes = attributes.to_der().expect("attributes DER");
    let signature = SigningKey::<Sha1>::new(chain.signer.key.clone()).sign(&signed_bytes).to_vec();

    let leaf = &chain.signer.certificate.tbs_certificate;
    let identifier = IssuerAndSerialNumber { issuer: leaf.issuer.clone(), serial_number: leaf.serial_number.clone() };
    let algorithm = |oid, parameters| AlgorithmIdentifierOwned { oid, parameters };

    let signer = SignerInfo {
        version: CmsVersion::V1,
        sid: SignerIdentifier::IssuerAndSerialNumber(identifier),
        digest_alg: algorithm(SHA1_DIGEST, None),
        signed_attrs: Some(attributes),
        signature_algorithm: algorithm(RSA_ENCRYPTION, Some(Any::null())),
        signature: OctetString::new(signature).expect("signature"),
        unsigned_attrs: None,
    };

    let embedded = [&chain.signer.certificate, &chain.intermediate.certificate, &chain.root.certificate];
    let certificates = embedded.map(|certificate| CertificateChoices::Certificate(certificate.clone()));
    let certificates = SetOfVec::try_from(certificates.to_vec()).expect("certificates");
    let econtent = Any::encode_from(&OctetString::new(content).expect("content")).expect("content");

    let signed = SignedData {
        version: CmsVersion::V1,
        digest_algorithms: SetOfVec::try_from(vec![algorithm(SHA1_DIGEST, None)]).expect("digests"),
        encap_content_info: EncapsulatedContentInfo { econtent_type: DATA, econtent: Some(econtent) },
        certificates: Some(CertificateSet(certificates)),
        crls: None,
        signer_infos: SignerInfos(SetOfVec::try_from(vec![signer]).expect("signers")),
    };

    let envelope = ContentInfo { content_type: SIGNED_DATA, content: Any::encode_from(&signed).expect("SignedData") };

    envelope.to_der().expect("CMS")
}

fn attribute(oid: ObjectIdentifier, value: Any) -> Attribute {
    Attribute { oid, values: SetOfVec::try_from(vec![value]).expect("attribute") }
}

/// SHA-256 digest helper for fixtures that record content hashes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}
