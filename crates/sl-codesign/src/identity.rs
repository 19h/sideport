//! Signing identities (certificate + key + Apple chain), key generation and CSRs. CONTRACT.

use crate::Result;

/// A development certificate, its RSA private key and the Apple intermediate/root certificates
/// embedded in every CMS signature.
#[derive(Debug)]
pub struct SigningIdentity {
    _private: (),
}

impl SigningIdentity {
    /// Build from a DER certificate (as returned by the developer portal) and a private key.
    pub fn new(certificate_der: &[u8], key: rsa::RsaPrivateKey) -> Result<Self> {
        let _ = (certificate_der, key);
        unimplemented!("sl-codesign: SigningIdentity::new")
    }

    /// Build from PEM strings (certificate `CERTIFICATE`, key `RSA PRIVATE KEY` or `PRIVATE KEY`).
    pub fn from_pem(certificate_pem: &str, key_pem: &str) -> Result<Self> {
        let _ = (certificate_pem, key_pem);
        unimplemented!("sl-codesign: SigningIdentity::from_pem")
    }

    /// Subject OU of the certificate (Apple team identifier).
    pub fn team_id(&self) -> &str {
        unimplemented!()
    }

    /// Subject CN of the certificate, e.g. `Apple Development: Jane Doe (ABCDE12345)`.
    pub fn common_name(&self) -> &str {
        unimplemented!()
    }

    pub fn certificate_der(&self) -> &[u8] {
        unimplemented!()
    }

    /// Certificate serial number as upper-case hex without leading zeros (portal format).
    pub fn serial_hex(&self) -> String {
        unimplemented!()
    }

    /// `notAfter` of the certificate.
    pub fn expires(&self) -> chrono::DateTime<chrono::Utc> {
        unimplemented!()
    }
}

/// Generate a fresh RSA-2048 (e = 65537) signing key.
pub fn generate_signing_key() -> Result<rsa::RsaPrivateKey> {
    unimplemented!()
}

/// PKCS#10 CSR (SHA-256 with RSA) for the given key, PEM encoded, subject
/// `C=US, ST=, L=, O=<organization>, CN=<common_name>`.
pub fn build_csr_pem(
    key: &rsa::RsaPrivateKey,
    common_name: &str,
    organization: &str,
) -> Result<String> {
    let _ = (key, common_name, organization);
    unimplemented!()
}
