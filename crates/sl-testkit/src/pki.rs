//! Generated certificate chains mirroring the inspected Apple layouts.

use der::asn1::UtcTime;
use der::{Decode, EncodePem};
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use sha2::Sha256;
use sl_codesign::ProfileTrust;
use spki::SubjectPublicKeyInfoOwned;
use std::sync::OnceLock;
use std::time::Duration;
use x509_cert::Certificate;
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::time::{Time, Validity};

pub const PROFILE_SIGNER: &str = "Apple iPhone OS Provisioning Profile Signing";
pub const PROFILE_ISSUER: &str = "Apple iPhone Certification Authority";

/// A certificate and its private key.
#[derive(Debug, Clone)]
pub struct Issued {
    pub certificate: Certificate,
    pub key: RsaPrivateKey,
}

/// Validity from RFC 3339 instants.
pub fn validity(from: &str, to: &str) -> Validity {
    let seconds = |text: &str| {
        let time: chrono::DateTime<chrono::Utc> = text.parse().expect("fixture time");

        Duration::from_secs(u64::try_from(time.timestamp()).expect("positive fixture time"))
    };

    Validity {
        not_before: Time::UtcTime(UtcTime::from_unix_duration(seconds(from)).expect("not before")),
        not_after: Time::UtcTime(UtcTime::from_unix_duration(seconds(to)).expect("not after")),
    }
}

/// The default fixture validity, 2020 through 2040.
pub fn long_validity() -> Validity {
    validity("2020-01-01T00:00:00Z", "2040-01-01T00:00:00Z")
}

pub fn subject_public_key(key: &RsaPrivateKey) -> SubjectPublicKeyInfoOwned {
    let encoded = rsa::pkcs8::EncodePublicKey::to_public_key_der(&key.to_public_key()).expect("SPKI");

    SubjectPublicKeyInfoOwned::from_der(encoded.as_bytes()).expect("SPKI")
}

/// Issue a certificate for `public_key`, signed by `issuer` (self-signed when `None`).
pub fn issue_for(
    subject: &str,
    profile: Profile,
    serial: u32,
    period: Validity,
    public_key: SubjectPublicKeyInfoOwned,
    issuer_key: &RsaPrivateKey,
) -> Certificate {
    let subject: Name = subject.parse().expect("subject");
    let signer = SigningKey::<Sha256>::new(issuer_key.clone());

    let builder = CertificateBuilder::new(profile, SerialNumber::from(serial), period, subject, public_key, &signer)
        .expect("certificate builder");

    builder.build::<rsa::pkcs1v15::Signature>().expect("certificate")
}

/// Generate a key and issue its certificate.
pub fn issue(subject: &str, profile: Profile, serial: u32, period: Validity, issuer: Option<&Issued>) -> Issued {
    let key = sl_codesign::generate_signing_key().expect("RSA key");
    let issuer_key = issuer.map_or(&key, |issuer| &issuer.key);
    let certificate = issue_for(subject, profile, serial, period, subject_public_key(&key), issuer_key);

    Issued { certificate, key }
}

/// Root → intermediate → profile signer, with Apple's policy names on generated keys.
#[derive(Debug)]
pub struct ProfileChain {
    pub root: Issued,
    pub intermediate: Issued,
    pub signer: Issued,
}

impl ProfileChain {
    /// One chain per test process; RSA generation dominates fixture cost.
    pub fn shared() -> &'static Self {
        static CHAIN: OnceLock<ProfileChain> = OnceLock::new();

        CHAIN.get_or_init(Self::generate)
    }

    pub fn generate() -> Self {
        let root = issue("CN=Fixture Root CA,O=Sideport,C=US", Profile::Root, 1, long_validity(), None);

        let root_name = root.certificate.tbs_certificate.subject.clone();
        let intermediate_profile = Profile::SubCA { issuer: root_name, path_len_constraint: Some(0) };
        let intermediate_subject = format!("CN={PROFILE_ISSUER},O=Sideport,C=US");
        let intermediate = issue(&intermediate_subject, intermediate_profile, 2, long_validity(), Some(&root));

        let intermediate_name = intermediate.certificate.tbs_certificate.subject.clone();
        let signer_profile =
            Profile::Leaf { issuer: intermediate_name, enable_key_agreement: false, enable_key_encipherment: false };
        let signer_subject = format!("CN={PROFILE_SIGNER},O=Sideport,C=US");
        let signer = issue(&signer_subject, signer_profile, 3, long_validity(), Some(&intermediate));

        Self { root, intermediate, signer }
    }

    /// Trust policy anchored at this chain's root.
    pub fn trust(&self) -> ProfileTrust {
        let pem = self.root.certificate.to_pem(der::pem::LineEnding::LF).expect("anchor PEM");

        ProfileTrust::from_pem(&pem, PROFILE_SIGNER, PROFILE_ISSUER).expect("fixture trust")
    }
}

/// A development-certificate authority standing in for Apple WWDR.
#[derive(Debug)]
pub struct DevelopmentAuthority {
    pub authority: Issued,
}

impl DevelopmentAuthority {
    pub fn shared() -> &'static Self {
        static AUTHORITY: OnceLock<DevelopmentAuthority> = OnceLock::new();

        AUTHORITY.get_or_init(|| {
            let authority = issue("CN=Fixture WWDR,OU=G3,O=Sideport,C=US", Profile::Root, 10, long_validity(), None);

            Self { authority }
        })
    }

    /// Issue an Apple-style development certificate for a CSR's public key.
    pub fn issue_development(&self, public_key: SubjectPublicKeyInfoOwned, team_id: &str, serial: u32) -> Certificate {
        let subject = format!("CN=Apple Development: Fixture ({team_id}),OU={team_id},O=Fixture,C=US");
        let profile = Profile::Leaf {
            issuer: self.authority.certificate.tbs_certificate.subject.clone(),
            enable_key_agreement: false,
            enable_key_encipherment: false,
        };

        issue_for(&subject, profile, serial, long_validity(), public_key, &self.authority.key)
    }
}
