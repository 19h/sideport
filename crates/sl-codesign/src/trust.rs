//! Provisioning-profile CMS signature, certificate-chain and signer-policy verification.
//!
//! Apple profiles are RFC 5652 SignedData with an encapsulated XML plist. Inspected profiles use
//! a SHA-1 or SHA-256 digest, signed attributes and RSA PKCS#1 v1.5. The signer is
//! "Apple iPhone OS Provisioning Profile Signing", issued by "Apple iPhone Certification Authority",
//! issued by Apple Root CA. The embedded root is never trusted; anchors come from [`ProfileTrust`].

use crate::identity::{pem_certificates, subject_string};
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use cms::cert::CertificateChoices;
use cms::content_info::ContentInfo;
use cms::signed_data::{SignedData, SignerIdentifier, SignerInfo};
use const_oid::{AssociatedOid, ObjectIdentifier};
use der::asn1::OctetString;
use der::{Any, Decode, Encode};
use rsa::RsaPublicKey;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use signature::Verifier;
use spki::DecodePublicKey;
use x509_cert::Certificate;
use x509_cert::ext::pkix::{BasicConstraints, KeyUsage, SubjectKeyIdentifier};

const DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");
const SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
const CONTENT_TYPE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.3");
const MESSAGE_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4");
const COMMON_NAME: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.3");

const SHA1_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.14.3.2.26");
const SHA256_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");
const SHA384_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.2");
const SHA512_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.3");

const RSA_ENCRYPTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
const SHA1_WITH_RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.5");
const SHA256_WITH_RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11");
const SHA384_WITH_RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.12");
const SHA512_WITH_RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.13");

const APPLE_ROOT: &str = "Apple Root CA";
const APPLE_PROFILE_SIGNER: &str = "Apple iPhone OS Provisioning Profile Signing";
const APPLE_PROFILE_ISSUER: &str = "Apple iPhone Certification Authority";

/// Trust anchors and the signer naming policy for provisioning-profile signatures. CONTRACT.
#[derive(Debug, Clone)]
pub struct ProfileTrust {
    anchors: Vec<Certificate>,
    signer_common_name: String,
    issuer_common_name: String,
}

impl ProfileTrust {
    /// Apple Root CA from the bundled Apple chain, with Apple's profile signer and issuer names.
    pub fn apple() -> Result<Self> {
        let certificates = pem_certificates(include_str!("../resources/apple-chain.pem"))?;

        let anchors: Vec<_> = certificates
            .into_iter()
            .filter(|certificate| {
                let self_issued = certificate.tbs_certificate.subject == certificate.tbs_certificate.issuer;
                let name = subject_string(certificate, COMMON_NAME).ok();

                self_issued && name.as_deref() == Some(APPLE_ROOT)
            })
            .collect();

        Self::with_anchors(anchors, APPLE_PROFILE_SIGNER, APPLE_PROFILE_ISSUER)
    }

    /// Anchors as PEM certificates plus the required leaf and intermediate subject common names.
    pub fn from_pem(anchors_pem: &str, signer_common_name: &str, issuer_common_name: &str) -> Result<Self> {
        Self::with_anchors(pem_certificates(anchors_pem)?, signer_common_name, issuer_common_name)
    }

    fn with_anchors(anchors: Vec<Certificate>, signer_common_name: &str, issuer_common_name: &str) -> Result<Self> {
        if anchors.is_empty() || signer_common_name.is_empty() || issuer_common_name.is_empty() {
            return Err(trust_error("a profile trust policy needs anchors and signer names"));
        }

        Ok(Self {
            anchors,
            signer_common_name: signer_common_name.into(),
            issuer_common_name: issuer_common_name.into(),
        })
    }
}

/// Decoded SignedData parts needed for verification.
pub(crate) struct SignedProfile {
    content: Vec<u8>,
    signer: SignerInfo,
    certificates: Vec<Certificate>,
}

impl SignedProfile {
    pub(crate) fn decode(raw: &[u8]) -> Result<Self> {
        let envelope = ContentInfo::from_der(raw).map_err(trust_error)?;

        if envelope.content_type != SIGNED_DATA {
            return Err(trust_error("CMS content type is not SignedData"));
        }

        let signed: SignedData = envelope.content.decode_as().map_err(trust_error)?;

        if signed.encap_content_info.econtent_type != DATA {
            return Err(trust_error("encapsulated CMS content type is not data"));
        }

        let content = signed.encap_content_info.econtent.ok_or_else(|| trust_error("detached profile payload"))?;
        let content = content.decode_as::<OctetString>().map_err(trust_error)?.as_bytes().to_vec();

        let signers = signed.signer_infos.0.into_vec();

        let [signer] = <[SignerInfo; 1]>::try_from(signers)
            .map_err(|signers| trust_error(format!("expected one CMS signer, found {}", signers.len())))?;

        let certificates = signed
            .certificates
            .map(|set| set.0.into_vec())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|choice| match choice {
                CertificateChoices::Certificate(certificate) => Some(certificate),
                CertificateChoices::Other(_) => None,
            })
            .collect();

        Ok(Self { content, signer, certificates })
    }

    /// RFC 5652 §5.4 and §5.6: the message digest covers the payload and the signature covers the
    /// DER SET OF signed attributes. Without signed attributes the signature covers the payload.
    pub(crate) fn verify_signature(&self) -> Result<()> {
        let digest = Hash::from_digest(&self.signer.digest_alg.oid)?;
        let signature_hash = Hash::from_signature(&self.signer.signature_algorithm.oid)?;

        if signature_hash.is_some_and(|hash| hash != digest) {
            return Err(trust_error("CMS signature and digest algorithms disagree"));
        }

        let message = match &self.signer.signed_attrs {
            None => self.content.clone(),

            Some(attributes) => {
                let content_type = single_attribute(attributes, CONTENT_TYPE)?.decode_as::<ObjectIdentifier>();
                let message_digest = single_attribute(attributes, MESSAGE_DIGEST)?.decode_as::<OctetString>();

                if content_type.map_err(trust_error)? != DATA {
                    return Err(trust_error("signed content type is not data"));
                }

                if message_digest.map_err(trust_error)?.as_bytes() != digest.digest(&self.content) {
                    return Err(trust_error("message digest does not match the profile payload"));
                }

                attributes.to_der().map_err(trust_error)?
            }
        };

        let signer = self.signer_certificate()?;
        let key = public_key(signer)?;

        digest.verify(&key, &message, self.signer.signature.as_bytes())
    }

    /// Leaf → embedded intermediate → configured anchor, each valid at `signed_at`.
    pub(crate) fn verify_chain(&self, trust: &ProfileTrust, signed_at: DateTime<Utc>) -> Result<()> {
        let leaf = self.signer_certificate()?;

        require_common_name(leaf, &trust.signer_common_name)?;
        require_digital_signature(leaf)?;
        require_valid_at(leaf, signed_at)?;

        let issuer = self
            .certificates
            .iter()
            .filter(|candidate| candidate.tbs_certificate.subject == leaf.tbs_certificate.issuer)
            .find(|candidate| verify_issued_by(leaf, candidate).is_ok())
            .ok_or_else(|| trust_error("the profile signer's issuer is missing or did not sign it"))?;

        require_common_name(issuer, &trust.issuer_common_name)?;
        require_certificate_authority(issuer)?;
        require_valid_at(issuer, signed_at)?;

        let anchor = trust
            .anchors
            .iter()
            .filter(|anchor| anchor.tbs_certificate.subject == issuer.tbs_certificate.issuer)
            .find(|anchor| verify_issued_by(issuer, anchor).is_ok())
            .ok_or_else(|| trust_error("the profile chain does not end at a trusted anchor"))?;

        require_valid_at(anchor, signed_at)
    }

    fn signer_certificate(&self) -> Result<&Certificate> {
        let mut matches = self.certificates.iter().filter(|certificate| {
            let certificate = &certificate.tbs_certificate;

            match &self.signer.sid {
                SignerIdentifier::IssuerAndSerialNumber(identifier) => {
                    certificate.issuer == identifier.issuer && certificate.serial_number == identifier.serial_number
                }

                SignerIdentifier::SubjectKeyIdentifier(identifier) => {
                    subject_key_identifier(certificate).as_ref() == Some(identifier)
                }
            }
        });

        let signer = matches.next().ok_or_else(|| trust_error("the CMS signer certificate is missing"))?;

        if matches.next().is_some() {
            return Err(trust_error("the CMS signer certificate is ambiguous"));
        }

        Ok(signer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hash {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    fn from_digest(oid: &ObjectIdentifier) -> Result<Self> {
        match *oid {
            SHA1_DIGEST => Ok(Self::Sha1),
            SHA256_DIGEST => Ok(Self::Sha256),
            SHA384_DIGEST => Ok(Self::Sha384),
            SHA512_DIGEST => Ok(Self::Sha512),
            _ => Err(trust_error(format!("unsupported digest algorithm {oid}"))),
        }
    }

    /// `None` for rsaEncryption, whose hash is the SignerInfo digest algorithm.
    fn from_signature(oid: &ObjectIdentifier) -> Result<Option<Self>> {
        match *oid {
            RSA_ENCRYPTION => Ok(None),
            SHA1_WITH_RSA => Ok(Some(Self::Sha1)),
            SHA256_WITH_RSA => Ok(Some(Self::Sha256)),
            SHA384_WITH_RSA => Ok(Some(Self::Sha384)),
            SHA512_WITH_RSA => Ok(Some(Self::Sha512)),
            _ => Err(trust_error(format!("unsupported signature algorithm {oid}"))),
        }
    }

    fn digest(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => Sha1::digest(bytes).to_vec(),
            Self::Sha256 => Sha256::digest(bytes).to_vec(),
            Self::Sha384 => Sha384::digest(bytes).to_vec(),
            Self::Sha512 => Sha512::digest(bytes).to_vec(),
        }
    }

    fn verify(self, key: &RsaPublicKey, message: &[u8], signature: &[u8]) -> Result<()> {
        let signature = Signature::try_from(signature).map_err(trust_error)?;

        let verified = match self {
            Self::Sha1 => VerifyingKey::<Sha1>::new(key.clone()).verify(message, &signature),
            Self::Sha256 => VerifyingKey::<Sha256>::new(key.clone()).verify(message, &signature),
            Self::Sha384 => VerifyingKey::<Sha384>::new(key.clone()).verify(message, &signature),
            Self::Sha512 => VerifyingKey::<Sha512>::new(key.clone()).verify(message, &signature),
        };

        verified.map_err(|_| trust_error("RSA signature verification failed"))
    }
}

fn single_attribute(attributes: &cms::signed_data::SignedAttributes, oid: ObjectIdentifier) -> Result<&Any> {
    let mut matches = attributes.iter().filter(|attribute| attribute.oid == oid);

    let attribute = matches.next().ok_or_else(|| trust_error(format!("missing signed attribute {oid}")))?;

    if matches.next().is_some() || attribute.values.len() != 1 {
        return Err(trust_error(format!("signed attribute {oid} must have one value")));
    }

    attribute.values.iter().next().ok_or_else(|| trust_error(format!("empty signed attribute {oid}")))
}

fn verify_issued_by(child: &Certificate, issuer: &Certificate) -> Result<()> {
    if child.signature_algorithm != child.tbs_certificate.signature {
        return Err(trust_error("certificate signature algorithms disagree"));
    }

    let hash = Hash::from_signature(&child.signature_algorithm.oid)?
        .ok_or_else(|| trust_error("certificate signature algorithm names no digest"))?;
    let signed = child.tbs_certificate.to_der().map_err(trust_error)?;
    let signature = child.signature.as_bytes().ok_or_else(|| trust_error("certificate signature has unused bits"))?;

    hash.verify(&public_key(issuer)?, &signed, signature)
}

fn public_key(certificate: &Certificate) -> Result<RsaPublicKey> {
    let encoded = certificate.tbs_certificate.subject_public_key_info.to_der().map_err(trust_error)?;

    RsaPublicKey::from_public_key_der(&encoded).map_err(trust_error)
}

fn require_common_name(certificate: &Certificate, expected: &str) -> Result<()> {
    let name = subject_string(certificate, COMMON_NAME).map_err(trust_error)?;

    if name != expected {
        return Err(trust_error(format!("unexpected profile certificate subject {name:?}")));
    }

    Ok(())
}

fn require_valid_at(certificate: &Certificate, at: DateTime<Utc>) -> Result<()> {
    let validity = &certificate.tbs_certificate.validity;
    let seconds = |time: x509_cert::time::Time| i64::try_from(time.to_unix_duration().as_secs()).unwrap_or(i64::MAX);

    let not_before = seconds(validity.not_before);
    let not_after = seconds(validity.not_after);

    if at.timestamp() < not_before || at.timestamp() > not_after {
        return Err(trust_error("a profile certificate is outside its validity period at signing time"));
    }

    Ok(())
}

fn require_certificate_authority(certificate: &Certificate) -> Result<()> {
    let constraints = extension::<BasicConstraints>(certificate)?;

    if !constraints.is_some_and(|constraints| constraints.ca) {
        return Err(trust_error("the profile signer's issuer is not a certificate authority"));
    }

    Ok(())
}

fn require_digital_signature(certificate: &Certificate) -> Result<()> {
    let usage = extension::<KeyUsage>(certificate)?;

    if usage.is_some_and(|usage| !usage.digital_signature()) {
        return Err(trust_error("the profile signer's key usage excludes digital signatures"));
    }

    Ok(())
}

fn subject_key_identifier(certificate: &x509_cert::TbsCertificate) -> Option<SubjectKeyIdentifier> {
    let extensions = certificate.extensions.as_ref()?;
    let extension = extensions.iter().find(|extension| extension.extn_id == SubjectKeyIdentifier::OID)?;

    SubjectKeyIdentifier::from_der(extension.extn_value.as_bytes()).ok()
}

fn extension<T: AssociatedOid + for<'a> Decode<'a>>(certificate: &Certificate) -> Result<Option<T>> {
    let Some(extensions) = &certificate.tbs_certificate.extensions else {
        return Ok(None);
    };

    let mut matches = extensions.iter().filter(|extension| extension.extn_id == T::OID);

    let Some(extension) = matches.next() else {
        return Ok(None);
    };

    if matches.next().is_some() {
        return Err(trust_error(format!("duplicate certificate extension {}", T::OID)));
    }

    T::from_der(extension.extn_value.as_bytes()).map(Some).map_err(trust_error)
}

fn trust_error(error: impl std::fmt::Display) -> Error {
    Error::ProfileTrust(error.to_string())
}
