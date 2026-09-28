use super::TokenVerifier;
use crate::Error;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{TimeZone, Utc};
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{EncodePublicKey, LineEnding};
use rsa::signature::{SignatureEncoding, Signer};
use sha2::Sha256;

/// A signer plus the PEM public key a verifier is built from.
struct Issuer {
    signing_key: SigningKey<Sha256>,
    public_pem: String,
}

impl Issuer {
    fn new() -> Self {
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("generate key");
        let public_pem = private.to_public_key().to_public_key_pem(LineEnding::LF).expect("public pem");

        Self { signing_key: SigningKey::<Sha256>::new(private), public_pem }
    }

    fn verifier(&self) -> TokenVerifier {
        TokenVerifier::from_public_key_pem(&self.public_pem).expect("verifier")
    }

    fn mint(&self, header: &str, claims: &str) -> String {
        let signing_input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(claims));
        let signature = self.signing_key.sign(signing_input.as_bytes());

        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }
}

fn now() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000, 0).single().expect("time")
}

#[test]
fn a_valid_token_yields_its_subject_expiry_and_features() {
    let issuer = Issuer::new();
    let claims = r#"{
        "sub": "patron-42",
        "exp": 1700003600,
        "nbf": 1699996400,
        "features": { "refresh_interval_hours": 6, "remote_anisette": true, "custom_entitlements": true }
    }"#;
    let token = issuer.mint(r#"{"alg":"RS256","typ":"JWT"}"#, claims);

    let verified = issuer.verifier().verify(&token, now()).expect("verify");

    assert_eq!(verified.subject.as_deref(), Some("patron-42"));
    assert_eq!(verified.expires, Utc.timestamp_opt(1_700_003_600, 0).single());
    assert_eq!(verified.features.refresh_interval_hours, Some(6));
    assert!(verified.features.remote_anisette);
    assert!(verified.features.custom_entitlements);
    assert!(!verified.features.custom_icon);
}

#[test]
fn an_expired_or_not_yet_valid_token_is_refused() {
    let issuer = Issuer::new();

    let expired = issuer.mint(r#"{"alg":"RS256"}"#, r#"{"exp": 1699999999}"#);
    assert!(matches!(issuer.verifier().verify(&expired, now()).expect_err("error expected"), Error::Token("expired")));

    let future = issuer.mint(r#"{"alg":"RS256"}"#, r#"{"nbf": 1700000001}"#);
    assert!(matches!(
        issuer.verifier().verify(&future, now()).expect_err("error expected"),
        Error::Token("not yet valid")
    ));
}

#[test]
fn a_foreign_key_a_tampered_body_and_a_wrong_algorithm_are_rejected() {
    let issuer = Issuer::new();
    let other = Issuer::new();
    let token = issuer.mint(r#"{"alg":"RS256"}"#, r#"{"features":{"remote_anisette":true}}"#);

    assert!(matches!(other.verifier().verify(&token, now()).expect_err("error expected"), Error::Token(_)));

    let mut segments: Vec<&str> = token.split('.').collect();
    let forged_claims = URL_SAFE_NO_PAD.encode(r#"{"features":{"remote_anisette":true,"custom_icon":true}}"#);
    segments[1] = &forged_claims;
    let tampered = segments.join(".");
    assert!(matches!(issuer.verifier().verify(&tampered, now()).expect_err("error expected"), Error::Token(_)));

    let none_alg = issuer.mint(r#"{"alg":"none"}"#, r#"{"features":{}}"#);
    assert!(matches!(
        issuer.verifier().verify(&none_alg, now()).expect_err("error expected"),
        Error::Token("unsupported algorithm")
    ));
}

#[test]
fn a_token_that_is_not_three_segments_is_rejected() {
    let issuer = Issuer::new();

    assert!(matches!(
        issuer.verifier().verify("only.two", now()).expect_err("error expected"),
        Error::Token("not a JWT")
    ));
    assert!(matches!(
        issuer.verifier().verify("a.b.c.d", now()).expect_err("error expected"),
        Error::Token("not a JWT")
    ));
}

#[test]
fn an_unknown_feature_claim_is_rejected_rather_than_silently_ignored() {
    let issuer = Issuer::new();
    let token = issuer.mint(r#"{"alg":"RS256"}"#, r#"{"features":{"unbounded_power": true}}"#);

    assert!(matches!(
        issuer.verifier().verify(&token, now()).expect_err("error expected"),
        Error::Token("malformed claims")
    ));
}
