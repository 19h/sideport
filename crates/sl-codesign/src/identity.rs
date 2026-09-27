//! Signing identities (certificate + key + Apple chain), key generation and CSRs. CONTRACT.

use crate::{Error, Result};
use const_oid::ObjectIdentifier;
use der::asn1::{Any, PrintableStringRef, SetOfVec, Utf8StringRef};
use der::{Decode, DecodePem, Encode, EncodePem, Tagged};
use rsa::{RsaPrivateKey, RsaPublicKey, pkcs1::DecodeRsaPrivateKey, pkcs8::DecodePrivateKey};
use sha2::Sha256;
use spki::DecodePublicKey;
use x509_cert::builder::{Builder, RequestBuilder};
use x509_cert::{
    Certificate,
    attr::AttributeTypeAndValue,
    name::{RdnSequence, RelativeDistinguishedName},
};

const COMMON_NAME: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.3");
const COUNTRY_NAME: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.6");
const ORGANIZATION_NAME: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.10");
const ORGANIZATIONAL_UNIT: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.11");

/// A development certificate, its RSA private key and the Apple intermediate/root certificates
/// embedded in every CMS signature.
pub struct SigningIdentity {
    pub(crate) certificate: Certificate,
    pub(crate) chain: Vec<Certificate>,
    pub(crate) key: RsaPrivateKey,
    certificate_der: Vec<u8>,
    team_id: String,
    common_name: String,
    expires: chrono::DateTime<chrono::Utc>,
}

// Neither private key components nor the PEM are included in diagnostics.
impl std::fmt::Debug for SigningIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningIdentity")
            .field("team_id", &self.team_id)
            .field("common_name", &self.common_name)
            .field("serial", &self.serial_hex())
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

impl SigningIdentity {
    /// Build from a DER certificate (as returned by the developer portal) and a private key.
    pub fn new(certificate_der: &[u8], key: RsaPrivateKey) -> Result<Self> {
        Self::with_chain(certificate_der, key, include_str!("../resources/apple-chain.pem"))
    }

    /// Decode signing material with an explicit PEM chain. This validates the key/certificate
    /// match, not PKI trust or developer-portal authorization. An empty chain is permitted.
    pub fn with_chain(certificate_der: &[u8], key: RsaPrivateKey, chain_pem: &str) -> Result<Self> {
        key.validate().map_err(|error| Error::Key(error.to_string()))?;

        let certificate = Certificate::from_der(certificate_der).map_err(cert_error)?;
        let public_key_der = certificate.tbs_certificate.subject_public_key_info.to_der().map_err(cert_error)?;
        let public_key = RsaPublicKey::from_public_key_der(&public_key_der).map_err(cert_error)?;

        if public_key != key.to_public_key() {
            return Err(Error::Key("certificate and private key do not match".into()));
        }

        let team_id = subject_string(&certificate, ORGANIZATIONAL_UNIT)?;
        let common_name = subject_string(&certificate, COMMON_NAME)?;

        let expiration_seconds = certificate.tbs_certificate.validity.not_after.to_unix_duration().as_secs();
        let expires = i64::try_from(expiration_seconds)
            .ok()
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
            .ok_or_else(|| Error::Certificate("certificate expiration is out of range".into()))?;

        let mut chain = Vec::new();

        for certificate_pem in chain_pem.split_inclusive("-----END CERTIFICATE-----") {
            if certificate_pem.trim().is_empty() {
                continue;
            }

            chain.push(Certificate::from_pem(certificate_pem.trim()).map_err(cert_error)?);
        }

        Ok(Self { certificate, chain, key, certificate_der: certificate_der.to_vec(), team_id, common_name, expires })
    }

    /// Build from PEM strings (certificate CERTIFICATE, key RSA PRIVATE KEY or PRIVATE KEY).
    pub fn from_pem(certificate_pem: &str, key_pem: &str) -> Result<Self> {
        let certificate = Certificate::from_pem(certificate_pem).map_err(cert_error)?;

        let key = if key_pem.contains("-----BEGIN RSA PRIVATE KEY-----") {
            RsaPrivateKey::from_pkcs1_pem(key_pem).map_err(|error| Error::Key(error.to_string()))?
        } else {
            RsaPrivateKey::from_pkcs8_pem(key_pem).map_err(|error| Error::Key(error.to_string()))?
        };

        Self::new(&certificate.to_der().map_err(cert_error)?, key)
    }

    /// Subject OU of the certificate (Apple team identifier).
    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    /// Subject CN of the certificate, e.g. Apple Development: Jane Doe (ABCDE12345).
    pub fn common_name(&self) -> &str {
        &self.common_name
    }

    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate_der
    }

    /// Certificate serial number as upper-case hex without leading zeros (portal format).
    pub fn serial_hex(&self) -> String {
        let serial_bytes = self.certificate.tbs_certificate.serial_number.as_bytes();
        let hexadecimal: String = serial_bytes.iter().map(|byte| format!("{byte:02X}")).collect();

        let trimmed = hexadecimal.trim_start_matches('0');

        if trimmed.is_empty() { "0".into() } else { trimmed.into() }
    }

    /// notAfter of the certificate.
    pub fn expires(&self) -> chrono::DateTime<chrono::Utc> {
        self.expires
    }
}

/// Generate a fresh RSA-2048 (e = 65537) signing key.
pub fn generate_signing_key() -> Result<RsaPrivateKey> {
    RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).map_err(|error| Error::Key(error.to_string()))
}

/// PKCS#10 CSR (SHA-256 with RSA) for the given key, PEM encoded, subject
/// C=US, O=<organization>, CN=<common_name>. Names are encoded as typed attributes;
/// commas and other RFC 4514 metacharacters cannot inject extra subject attributes.
pub fn build_csr_pem(key: &RsaPrivateKey, common_name: &str, organization: &str) -> Result<String> {
    if common_name.is_empty() || organization.is_empty() {
        return Err(Error::Certificate("CSR names must be nonempty".into()));
    }

    key.validate().map_err(|error| Error::Key(error.to_string()))?;

    let country = PrintableStringRef::new("US").map_err(cert_error)?;
    let organization = Utf8StringRef::new(organization).map_err(cert_error)?;
    let common_name = Utf8StringRef::new(common_name).map_err(cert_error)?;

    let subject = RdnSequence(vec![
        subject_attribute(COUNTRY_NAME, Any::encode_from(&country).map_err(cert_error)?)?,
        subject_attribute(ORGANIZATION_NAME, Any::encode_from(&organization).map_err(cert_error)?)?,
        subject_attribute(COMMON_NAME, Any::encode_from(&common_name).map_err(cert_error)?)?,
    ]);

    let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(key.clone());
    let request = RequestBuilder::new(subject, &signer)
        .map_err(cert_error)?
        .build::<rsa::pkcs1v15::Signature>()
        .map_err(cert_error)?;

    request.to_pem(der::pem::LineEnding::LF).map_err(cert_error)
}

fn subject_attribute(oid: ObjectIdentifier, value: Any) -> Result<RelativeDistinguishedName> {
    let attributes = SetOfVec::try_from(vec![AttributeTypeAndValue { oid, value }]).map_err(cert_error)?;

    Ok(RelativeDistinguishedName(attributes))
}

fn subject_string(certificate: &Certificate, oid: ObjectIdentifier) -> Result<String> {
    let values: Vec<_> = certificate
        .tbs_certificate
        .subject
        .0
        .iter()
        .flat_map(|rdn| rdn.0.iter())
        .filter(|attribute| attribute.oid == oid)
        .collect();

    if values.len() != 1 {
        return Err(Error::Certificate(format!("expected one subject attribute {oid}, found {}", values.len())));
    }

    let value = &values[0].value;

    let text = match value.tag() {
        der::Tag::Utf8String | der::Tag::PrintableString | der::Tag::Ia5String => {
            std::str::from_utf8(value.value()).map_err(cert_error)?.to_owned()
        }

        _ => return Err(Error::Certificate(format!("unsupported subject string encoding for {oid}"))),
    };

    if text.is_empty() || text.as_bytes().contains(&0) {
        return Err(Error::Certificate(format!("empty or NUL-containing subject attribute {oid}")));
    }

    Ok(text)
}

fn cert_error(error: impl std::fmt::Display) -> Error {
    Error::Certificate(error.to_string())
}
