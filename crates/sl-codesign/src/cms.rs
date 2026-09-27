//! Detached CMS signature over CodeDirectories.

use crate::{Error, Result, SigningIdentity};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{
    CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos,
};
use const_oid::ObjectIdentifier;
use der::asn1::{OctetString, SetOfVec};
use der::{Any, Encode, Sequence, ValueOrd};
use rsa::traits::PublicKeyParts;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use signature::{SignatureEncoding, Signer as _};
use spki::AlgorithmIdentifierOwned;
use x509_cert::attr::Attribute;

const DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");
const SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
const SHA1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.14.3.2.26");
const SHA256: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");
const RSA_ENCRYPTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");

const CONTENT_TYPE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.3");
const MESSAGE_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4");
const APPLE_CDHASH_PLIST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113635.100.9.1");
const APPLE_CDHASH_DER: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113635.100.9.2");

#[derive(Debug, Clone, Eq, PartialEq, Sequence, ValueOrd)]
struct HashInfo {
    algorithm: ObjectIdentifier,
    digest: OctetString,
}

#[derive(Debug, Clone, Copy)]
enum SignatureMode {
    Sign,
    SizeOnly,
}

/// RFC 5652 SignedData, SHA-256/RSA PKCS#1 v1.5, with Apple's full-digest and
/// 20-byte CDHash attributes. The detached content is the primary CodeDirectory.
pub fn sign(primary: &[u8], alternate: &[u8], identity: &SigningIdentity) -> Result<Vec<u8>> {
    build(primary, alternate, identity, SignatureMode::Sign)
}

pub(crate) fn encoded_size(primary: &[u8], alternate: &[u8], identity: &SigningIdentity) -> Result<usize> {
    Ok(build(primary, alternate, identity, SignatureMode::SizeOnly)?.len())
}

fn build(primary: &[u8], alternate: &[u8], identity: &SigningIdentity, mode: SignatureMode) -> Result<Vec<u8>> {
    let attributes = signed_attributes(primary, alternate)?;

    let signature = match mode {
        SignatureMode::Sign => {
            // RFC 5652 §5.4: sign the universal SET OF tag, not the [0] IMPLICIT tag.
            let encoded_attributes = attributes.to_der().map_err(cms_error)?;
            let signing_key = rsa::pkcs1v15::SigningKey::<Sha256>::new(identity.key.clone());

            signing_key.try_sign(&encoded_attributes).map_err(cms_error)?.to_vec()
        }

        SignatureMode::SizeOnly => vec![0; identity.key.size()],
    };

    let certificate = &identity.certificate.tbs_certificate;

    let signer = SignerInfo {
        version: CmsVersion::V1,
        sid: SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
            issuer: certificate.issuer.clone(),
            serial_number: certificate.serial_number.clone(),
        }),
        digest_alg: AlgorithmIdentifierOwned { oid: SHA256, parameters: None },
        signed_attrs: Some(attributes),
        signature_algorithm: AlgorithmIdentifierOwned { oid: RSA_ENCRYPTION, parameters: Some(Any::null()) },
        signature: OctetString::new(signature).map_err(cms_error)?,
        unsigned_attrs: None,
    };

    let certificates = certificate_set(identity)?;
    let digest_algorithms =
        SetOfVec::try_from(vec![AlgorithmIdentifierOwned { oid: SHA256, parameters: None }]).map_err(cms_error)?;
    let signer_infos = SignerInfos(SetOfVec::try_from(vec![signer]).map_err(cms_error)?);

    let signed = SignedData {
        version: CmsVersion::V1,
        digest_algorithms,
        encap_content_info: EncapsulatedContentInfo { econtent_type: DATA, econtent: None },
        certificates: Some(certificates),
        crls: None,
        signer_infos,
    };

    let envelope = ContentInfo { content_type: SIGNED_DATA, content: Any::encode_from(&signed).map_err(cms_error)? };

    envelope.to_der().map_err(cms_error)
}

fn certificate_set(identity: &SigningIdentity) -> Result<CertificateSet> {
    let mut certificates = SetOfVec::new();
    certificates.insert(CertificateChoices::Certificate(identity.certificate.clone())).map_err(cms_error)?;

    for certificate in &identity.chain {
        let choice = CertificateChoices::Certificate(certificate.clone());

        if !certificates.iter().any(|existing| existing == &choice) {
            certificates.insert(choice).map_err(cms_error)?;
        }
    }

    Ok(CertificateSet(certificates))
}

fn signed_attributes(primary: &[u8], alternate: &[u8]) -> Result<SetOfVec<Attribute>> {
    let primary_digest = Sha256::digest(primary).to_vec();
    let primary_cdhash = Sha1::digest(primary).to_vec();
    let alternate_digest = Sha256::digest(alternate).to_vec();

    let content_type = attribute(CONTENT_TYPE, vec![Any::encode_from(&DATA).map_err(cms_error)?])?;
    let message_digest = attribute(MESSAGE_DIGEST, vec![octet_value(primary_digest)?])?;

    let mut cdhash_plist = plist::Dictionary::new();
    cdhash_plist.insert(
        "cdhashes".into(),
        plist::Value::Array(vec![
            plist::Value::Data(primary_cdhash.clone()),
            plist::Value::Data(alternate_digest[..20].to_vec()),
        ]),
    );

    let encoded_plist = crate::entitlements::to_xml(&cdhash_plist)?;
    let hash_plist = attribute(APPLE_CDHASH_PLIST, vec![octet_value(encoded_plist)?])?;

    let hash_values = vec![hash_value(SHA1, primary_cdhash)?, hash_value(SHA256, alternate_digest)?];

    let hash_der = attribute(APPLE_CDHASH_DER, hash_values)?;

    SetOfVec::try_from(vec![content_type, message_digest, hash_plist, hash_der]).map_err(cms_error)
}

fn attribute(oid: ObjectIdentifier, values: Vec<Any>) -> Result<Attribute> {
    let values = SetOfVec::try_from(values).map_err(cms_error)?;

    Ok(Attribute { oid, values })
}

fn octet_value(bytes: Vec<u8>) -> Result<Any> {
    let octets = OctetString::new(bytes).map_err(cms_error)?;

    Any::encode_from(&octets).map_err(cms_error)
}

fn hash_value(algorithm: ObjectIdentifier, digest: Vec<u8>) -> Result<Any> {
    let digest = OctetString::new(digest).map_err(cms_error)?;

    Any::encode_from(&HashInfo { algorithm, digest }).map_err(cms_error)
}

fn cms_error(error: impl std::fmt::Display) -> Error {
    Error::Cms(error.to_string())
}
