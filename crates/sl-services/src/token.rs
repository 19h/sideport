//! A generic feature-token verifier.
//!
//! The recovered client unlocks paid options with an RS256 JWT signed by a backend key and gates
//! features on its claims. The signing key and the recovered claim names are obfuscated in the
//! binary; this crate deliberately extracts neither. Instead it verifies an RS256 JWT against a
//! **caller-supplied** RSA public key and reads a Sideport-defined claim schema documented in
//! `docs/SERVICES.md`. The verifier is a mechanism; which key and issuer to trust is a deployment
//! decision left to the operator.

use crate::{Error, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeZone, Utc};
use rsa::RsaPublicKey;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier;
use serde::Deserialize;
use sha2::Sha256;
use std::fmt;

/// The unlocked capabilities a valid token grants. These names are Sideport's own, not recovered
/// from the binary; they cover the option groups the recovered client gated.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Features {
    /// Custom auto-refresh threshold in hours; `None` leaves the built-in default in force.
    pub refresh_interval_hours: Option<u32>,
    /// A private/remote anisette provider may be selected.
    pub remote_anisette: bool,
    /// Alternate entitlements may be merged during signing.
    pub custom_entitlements: bool,
    /// A replacement app icon may be supplied.
    pub custom_icon: bool,
    /// Ten or more custom Info.plist properties may be set.
    pub custom_info_props: bool,
    /// A custom AFC upload chunk size may be used.
    pub custom_upload_chunk: bool,
}

/// A verified token: its subject, its lifetime and the features it grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureToken {
    pub subject: Option<String>,
    pub expires: Option<DateTime<Utc>>,
    pub features: Features,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
}

#[derive(Deserialize)]
struct Claims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    features: Features,
}

/// Verifies RS256 feature tokens against one configured RSA public key.
#[derive(Clone)]
pub struct TokenVerifier {
    key: RsaPublicKey,
}

impl fmt::Debug for TokenVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("TokenVerifier").finish_non_exhaustive()
    }
}

impl TokenVerifier {
    /// A verifier for a PEM `SubjectPublicKeyInfo` (`-----BEGIN PUBLIC KEY-----`).
    pub fn from_public_key_pem(pem: &str) -> Result<Self> {
        let key = RsaPublicKey::from_public_key_pem(pem).map_err(|_| Error::Invalid("public key PEM"))?;

        Ok(Self { key })
    }

    /// A verifier for a DER `SubjectPublicKeyInfo`.
    pub fn from_public_key_der(der: &[u8]) -> Result<Self> {
        let key = RsaPublicKey::from_public_key_der(der).map_err(|_| Error::Invalid("public key DER"))?;

        Ok(Self { key })
    }

    /// Verify `token` at wall-clock `now`, returning its features when the signature and lifetime
    /// hold. Only RS256 is accepted; every other algorithm is rejected without inspecting claims.
    pub fn verify(&self, token: &str, now: DateTime<Utc>) -> Result<FeatureToken> {
        let mut parts = token.split('.');
        let header_b64 = parts.next().ok_or(Error::Token("not a JWT"))?;
        let claims_b64 = parts.next().ok_or(Error::Token("not a JWT"))?;
        let signature_b64 = parts.next().ok_or(Error::Token("not a JWT"))?;

        if parts.next().is_some() {
            return Err(Error::Token("not a JWT"));
        }

        let header: Header = decode_segment(header_b64).map_err(|_| Error::Token("malformed header"))?;

        if !header.alg.eq_ignore_ascii_case("RS256") {
            return Err(Error::Token("unsupported algorithm"));
        }

        let signing_input = &token[..header_b64.len() + 1 + claims_b64.len()];
        let signature_bytes = URL_SAFE_NO_PAD.decode(signature_b64).map_err(|_| Error::Token("malformed signature"))?;

        self.verify_signature(signing_input, &signature_bytes)?;

        let claims: Claims = decode_segment(claims_b64).map_err(|_| Error::Token("malformed claims"))?;
        let expires = convert_time(claims.exp)?;
        let not_before = convert_time(claims.nbf)?;

        if not_before.is_some_and(|start| now < start) {
            return Err(Error::Token("not yet valid"));
        }

        if expires.is_some_and(|end| now >= end) {
            return Err(Error::Token("expired"));
        }

        Ok(FeatureToken { subject: claims.sub, expires, features: claims.features })
    }

    fn verify_signature(&self, signing_input: &str, signature_bytes: &[u8]) -> Result<()> {
        let verifying_key = VerifyingKey::<Sha256>::new(self.key.clone());
        let signature = Signature::try_from(signature_bytes).map_err(|_| Error::Token("malformed signature"))?;

        verifying_key.verify(signing_input.as_bytes(), &signature).map_err(|_| Error::Token("bad signature"))
    }
}

fn decode_segment<T: serde::de::DeserializeOwned>(segment: &str) -> Result<T> {
    let bytes = URL_SAFE_NO_PAD.decode(segment).map_err(|_| Error::Token("malformed segment"))?;

    serde_json::from_slice(&bytes).map_err(|_| Error::Token("malformed segment"))
}

fn convert_time(seconds: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    match seconds {
        None => Ok(None),
        Some(seconds) => match Utc.timestamp_opt(seconds, 0).single() {
            Some(time) => Ok(Some(time)),
            None => Err(Error::Token("out-of-range timestamp")),
        },
    }
}

#[cfg(test)]
mod tests;
