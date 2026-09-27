#![allow(dead_code)]

use der::{Decode, Encode};
use rsa::{RsaPrivateKey, pkcs1v15::SigningKey};
use sha2::Sha256;
use sl_codesign::{SigningIdentity, generate_signing_key};
use std::{
    path::Path,
    process::{Command, Output},
    sync::{Arc, OnceLock},
};
use x509_cert::{
    builder::{Builder, CertificateBuilder, Profile},
    name::Name,
    serial_number::SerialNumber,
    time::Validity,
};

pub fn signing_material() -> &'static (Arc<SigningIdentity>, RsaPrivateKey) {
    static MATERIAL: OnceLock<(Arc<SigningIdentity>, RsaPrivateKey)> = OnceLock::new();

    MATERIAL.get_or_init(|| {
        let key = generate_signing_key().expect("RSA key");
        let signer = SigningKey::<Sha256>::new(key.clone());

        let public_key_der = rsa::pkcs8::EncodePublicKey::to_public_key_der(&key.to_public_key()).expect("SPKI");
        let public_key_info = spki::SubjectPublicKeyInfoOwned::from_der(public_key_der.as_bytes()).expect("SPKI");
        let subject: Name =
            "CN=Apple Development: Sideport Test,OU=TEAM123456,O=Sideport,C=US".parse().expect("subject");
        let validity = Validity::from_now(std::time::Duration::from_secs(3600)).expect("validity");

        let cert = CertificateBuilder::new(
            Profile::Root,
            SerialNumber::from(42u32),
            validity,
            subject,
            public_key_info,
            &signer,
        )
        .expect("builder")
        .build::<rsa::pkcs1v15::Signature>()
        .expect("certificate");

        let certificate_der = cert.to_der().expect("DER");
        let identity = SigningIdentity::with_chain(&certificate_der, key.clone(), "").expect("identity");

        (Arc::new(identity), key)
    })
}

pub fn run(command: &mut Command) -> Output {
    let output = command.output().unwrap_or_else(|e| panic!("{command:?}: {e}"));

    assert!(
        output.status.success(),
        "{command:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    output
}

#[cfg(target_os = "macos")]
pub fn compile(path: &Path, universal: bool) -> Vec<u8> {
    let source = path.with_extension("c");

    std::fs::write(&source, "int main(void) { return 0; }\n").expect("source");

    let mut command = Command::new("xcrun");
    command.args(["clang", "-Wl,-no_adhoc_codesign"]);

    if universal {
        command.args(["-arch", "arm64", "-arch", "x86_64"]);
    }

    command.arg(&source).arg("-o").arg(path);
    run(&mut command);

    std::fs::read(path).expect("binary")
}
