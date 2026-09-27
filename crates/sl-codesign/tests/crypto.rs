mod common;

use der::asn1::OctetString;
use der::{Decode, DecodePem, Encode};
use rsa::traits::PublicKeyParts;
use sha2::{Digest, Sha256};
use signature::Verifier;
use sl_codesign::{SigningIdentity, build_csr_pem, cms, generate_signing_key};

#[test]
fn csr_is_signed_correctly_and_subject_metacharacters_are_literal() {
    let (_, key) = common::signing_material();

    assert_eq!(key.n().bits(), 2048);
    assert_eq!(key.e().to_string(), "65537");

    let pem = build_csr_pem(key, "Name,OU=INJECTED", "Research+CN=Injected").expect("CSR");
    let request = x509_cert::request::CertReq::from_pem(&pem).expect("PKCS10");

    assert_eq!(request.info.subject.0.len(), 3);

    let signature =
        rsa::pkcs1v15::Signature::try_from(request.signature.as_bytes().expect("aligned bits")).expect("signature");

    rsa::pkcs1v15::VerifyingKey::<Sha256>::new(key.to_public_key())
        .verify(&request.info.to_der().expect("info"), &signature)
        .expect("RSA verify");
    assert_eq!(request.algorithm.oid.to_string(), "1.2.840.113549.1.1.11");
}

#[test]
fn identity_matches_key_and_redacts_private_components() {
    let (identity, key) = common::signing_material();

    assert_eq!(identity.team_id(), "TEAM123456");
    assert_eq!(identity.serial_hex(), "2A");
    assert!(identity.expires() > chrono::Utc::now());

    let debug = format!("{identity:?}");

    assert!(!debug.contains(&key.n().to_string()));
    assert!(!debug.contains("PRIVATE KEY"));

    let other = generate_signing_key().expect("other key");

    assert!(SigningIdentity::with_chain(identity.certificate_der(), other, "").is_err());

    // The bundled Apple chain must also decode successfully.
    SigningIdentity::new(identity.certificate_der(), key.clone()).expect("bundled chain parses");
}

#[test]
fn cms_attributes_and_rsa_signature_are_independently_verified() {
    let (identity, key) = common::signing_material();

    let encoded = cms::sign(b"primary CodeDirectory", b"alternate CodeDirectory", identity).expect("CMS");
    let envelope = ::cms::content_info::ContentInfo::from_der(&encoded).expect("ContentInfo");
    let signed: ::cms::signed_data::SignedData = envelope.content.decode_as().expect("SignedData");

    assert!(signed.encap_content_info.econtent.is_none());

    let signer = &signed.signer_infos.0.as_slice()[0];
    let attributes = signer.signed_attrs.as_ref().expect("attrs");
    let signature = rsa::pkcs1v15::Signature::try_from(signer.signature.as_bytes()).expect("signature");
    let verifier = rsa::pkcs1v15::VerifyingKey::<Sha256>::new(key.to_public_key());

    verifier.verify(&attributes.to_der().expect("DER SET"), &signature).expect("RSA verify");
    assert_eq!(attributes.len(), 4);

    let digest =
        attributes.iter().find(|attribute| attribute.oid.to_string() == "1.2.840.113549.1.9.4").expect("digest");
    let octets: OctetString = digest.values.as_slice()[0].decode_as().expect("digest octets");

    assert_eq!(octets.as_bytes(), &Sha256::digest(b"primary CodeDirectory")[..]);

    let hashes = attributes
        .iter()
        .find(|attribute| attribute.oid.to_string() == "1.2.840.113635.100.9.2")
        .expect("hash attribute");

    assert_eq!(hashes.values.len(), 2);

    let mut changed = attributes.to_der().expect("DER");
    let last = changed.len() - 1;
    changed[last] ^= 1;

    assert!(verifier.verify(&changed, &signature).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn openssl_verifies_detached_cms_and_rejects_tampering() {
    use std::process::Command;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (identity, key) = common::signing_material();
    let encoded = cms::sign(b"primary CodeDirectory", b"alternate CodeDirectory", identity).expect("CMS");

    std::fs::write(root.join("signature.der"), encoded).expect("CMS file");
    std::fs::write(root.join("content"), b"primary CodeDirectory").expect("content");

    let verify = |command: &mut Command| {
        command
            .args(["cms", "-verify", "-noverify", "-binary", "-inform", "DER", "-in"])
            .arg(root.join("signature.der"))
            .arg("-content")
            .arg(root.join("content"))
            .arg("-out")
            .arg(root.join("verified"));
    };

    let mut command = Command::new("openssl");
    verify(&mut command);
    common::run(&mut command);

    assert_eq!(std::fs::read(root.join("verified")).expect("verified"), b"primary CodeDirectory");

    std::fs::write(root.join("content"), b"tampered CodeDirectory").expect("tamper");

    let mut command = Command::new("openssl");
    verify(&mut command);

    assert!(!command.output().expect("openssl").status.success());

    let csr = build_csr_pem(key, "Sideport", "Testing").expect("CSR");
    std::fs::write(root.join("request.pem"), csr).expect("CSR file");

    common::run(Command::new("openssl").args(["req", "-in"]).arg(root.join("request.pem")).args(["-verify", "-noout"]));
}
