use super::*;
use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use hmac::{Hmac, Mac};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixtures {
    vectors: Vec<Vector>,
}

#[derive(Deserialize)]
struct Vector {
    name: String,
    protocol: String,
    username: String,
    password: String,
    iterations: u32,
    ephemeral: String,
    salt: String,
    public: String,
    server_public: String,
    client_proof: String,
    server_proof: String,
    key: String,
    encrypted_session: String,
    context: String,
    negotiation: String,
    token: String,
    checksum: String,
}

fn fixtures() -> Vec<Vector> {
    let fixtures: Fixtures = serde_json::from_str(include_str!("../../tests/fixtures/grandslam.json"))
        .expect("valid independently generated fixtures");

    fixtures.vectors
}

fn bytes(value: &str) -> Vec<u8> {
    hex::decode(value).expect("fixture hexadecimal")
}

fn proof(vector: &Vector) -> SrpProof {
    let ephemeral = bytes(&vector.ephemeral).try_into().expect("32-byte fixture ephemeral");
    let client = SrpClient::with_ephemeral(vector.username.clone(), ephemeral).expect("fixture client");
    assert_eq!(client.public_key(), bytes(&vector.public), "{} public key", vector.name);

    let salt = bytes(&vector.salt);
    let server_public = bytes(&vector.server_public);
    let challenge = Challenge {
        protocol: PasswordProtocol::parse(&vector.protocol).expect("fixture protocol"),
        iterations: vector.iterations,
        salt: &salt,
        server_public: &server_public,
    };
    let proof = client.process(&vector.password, challenge).expect("fixture challenge");

    assert_eq!(proof.client_proof(), bytes(&vector.client_proof).as_slice(), "{} M1", vector.name);
    assert_eq!(proof.key.as_slice(), bytes(&vector.key), "{} K", vector.name);

    proof
}

#[test]
fn independent_srp_and_encrypted_token_vectors() {
    for vector in fixtures() {
        let proof = proof(&vector);
        let session = proof.verify(&bytes(&vector.server_proof)).expect("independent M2");
        let data = session
            .decrypt_session_data(
                &bytes(&vector.encrypted_session),
                &bytes(&vector.context),
                &bytes(&vector.negotiation),
            )
            .expect("independent negotiation and CBC");

        assert_eq!(data.dsid(), "123456789");
        assert_eq!(data.idms_token(), "fixture-idms-token");
        assert_eq!(data.app_token_checksum().as_slice(), bytes(&vector.checksum));
        assert_eq!(data.additional_data("canHaveCustodian").and_then(plist::Value::as_boolean), Some(true));
        assert_eq!(&*data.decrypt_app_token(&bytes(&vector.token)).expect("independent GCM"), "fixture-xcode-token");

        match data.continuation() {
            Continuation::Data(value) => assert_eq!(value.as_slice(), b"fixture-continuation"),
            _ => panic!("fixture continuation type"),
        }

        let debug = format!("{session:?} {data:?} {:?}", data.continuation());
        assert!(!debug.contains("fixture-idms-token"));
        assert!(!debug.contains("fixture-continuation"));
        assert!(!debug.contains(&vector.key));
    }
}

#[test]
fn server_proof_tampering_and_truncation_reject_session_access() {
    let vector = fixtures().remove(0);
    let mut tampered = bytes(&vector.server_proof);
    tampered[0] ^= 1;

    for server_proof in [&tampered[..], &tampered[..31], &[]] {
        assert!(matches!(proof(&vector).verify(server_proof), Err(Error::Verification("server SRP proof"))));
    }
}

#[test]
fn negotiation_protects_protocol_ciphertext_and_context() {
    let vector = fixtures().remove(0);
    let session = proof(&vector).verify(&bytes(&vector.server_proof)).expect("session");
    let ciphertext = bytes(&vector.encrypted_session);
    let context = bytes(&vector.context);
    let negotiation = bytes(&vector.negotiation);
    let mut tampered = ciphertext.clone();
    tampered[0] ^= 1;

    for (ciphertext, context, proof) in [
        (&tampered[..], &context[..], &negotiation[..]),
        (&ciphertext[..], b"changed".as_slice(), &negotiation[..]),
        (&ciphertext[..], &context[..], &negotiation[..31]),
    ] {
        assert!(matches!(
            session.decrypt_session_data(ciphertext, context, proof),
            Err(Error::Verification("negotiation proof"))
        ));
    }

    let switched = VerifiedSession { key: session.key, protocol: PasswordProtocol::S2kFo };
    assert!(switched.decrypt_session_data(&ciphertext, &context, &negotiation).is_err());
}

#[test]
fn authenticated_invalid_pkcs7_padding_is_rejected() {
    let vector = fixtures().remove(0);
    let session = proof(&vector).verify(&bytes(&vector.server_proof)).expect("session");
    let mut ciphertext = bytes(&vector.encrypted_session);
    let length = ciphertext.len();
    ciphertext[length - 17] ^= 1;

    let context = bytes(&vector.context);
    let transcript = hash(&[b"s2k,s2k_fo", b"s2k", b"|", &ciphertext, b"|", &context]);
    let negotiation_key = test_mac(&session.key[..], b"HMAC key:");
    let negotiation = test_mac(&negotiation_key, &transcript);

    assert!(matches!(
        session.decrypt_session_data(&ciphertext, &context, &negotiation),
        Err(Error::Verification("session data padding"))
    ));
}

#[test]
fn token_tampering_and_invalid_envelopes_are_rejected() {
    let vector = fixtures().remove(0);
    let session = proof(&vector).verify(&bytes(&vector.server_proof)).expect("session");
    let data = session
        .decrypt_session_data(&bytes(&vector.encrypted_session), &bytes(&vector.context), &bytes(&vector.negotiation))
        .expect("session data");
    let token = bytes(&vector.token);

    for index in [3, 19, token.len() - 1] {
        let mut tampered = token.clone();
        tampered[index] ^= 1;

        assert!(matches!(data.decrypt_app_token(&tampered), Err(Error::Verification("app token authentication"))));
    }

    let mut wrong_version = token;
    wrong_version[0] = b'Z';
    assert!(matches!(data.decrypt_app_token(&wrong_version), Err(Error::Invalid("encrypted token version"))));
    assert!(data.decrypt_app_token(&[0; 34]).is_err());
    assert!(data.decrypt_app_token(&vec![0; session::MAX_SESSION_BYTES + 1]).is_err());
}

#[test]
fn challenge_limits_and_public_key_range_are_checked_before_work() {
    for public in [vec![], vec![0], MODULUS.to_be_bytes().to_vec(), vec![0xff; 256], vec![1; 257]] {
        let challenge =
            Challenge { protocol: PasswordProtocol::S2k, iterations: 1, salt: b"salt", server_public: &public };
        let client = SrpClient::with_ephemeral("fixture@example.test".into(), [1; 32]).expect("client");

        assert!(client.process("password", challenge).is_err());
    }

    let oversized_salt = vec![1; MAX_SALT_BYTES + 1];
    let oversized_password = "a".repeat(MAX_PASSWORD_BYTES + 1);

    for (iterations, salt, password) in [
        (0, b"salt".as_slice(), "password"),
        (MAX_ITERATIONS + 1, b"salt".as_slice(), "password"),
        (1, b"".as_slice(), "password"),
        (1, oversized_salt.as_slice(), "password"),
        (1, b"salt".as_slice(), oversized_password.as_str()),
    ] {
        let challenge = Challenge { protocol: PasswordProtocol::S2k, iterations, salt, server_public: &[2] };
        let client = SrpClient::with_ephemeral("fixture@example.test".into(), [1; 32]).expect("client");

        assert!(client.process(password, challenge).is_err());
    }

    assert!(PasswordProtocol::parse("unsupported").is_err());
    assert!(SrpClient::with_ephemeral("".into(), [1; 32]).is_err());
    assert!(SrpClient::with_ephemeral("fixture@example.test".into(), [0; 32]).is_err());
}

fn test_mac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key");
    mac.update(message);

    mac.finalize().into_bytes().into()
}

#[test]
fn authenticated_invalid_session_schema_and_xml_are_rejected() {
    let vector = fixtures().remove(0);
    let session = proof(&vector).verify(&bytes(&vector.server_proof)).expect("session");
    let data_key = test_mac(&session.key[..], b"extra data key:");
    let iv = test_mac(&session.key[..], b"extra data iv:");
    let negotiation_key = test_mac(&session.key[..], b"HMAC key:");
    let deep = format!("{}<string>deep</string>{}", "<array>".repeat(40), "</array>".repeat(40));
    let invalid_key = "<dict><key>adsid</key><string>1</string><key>GsIdmsToken</key><string>token</string><key>sk</key><data>AQ==</data></dict>";

    for fragment in [
        "<string>wrong root</string>",
        "<dict/>",
        "<dict><key>adsid</key><integer>1</integer></dict>",
        "<!DOCTYPE plist [<!ENTITY secret SYSTEM 'file:///etc/passwd'>]><dict/>",
        "<?xml version='1.0'?><dict/>",
        "<dict>",
        deep.as_str(),
        invalid_key,
    ] {
        let cipher = cbc::Encryptor::<aes::Aes256>::new_from_slices(&data_key, &iv[..16]).expect("cipher parameters");
        let ciphertext = cipher.encrypt_padded_vec_mut::<Pkcs7>(fragment.as_bytes());
        let transcript = hash(&[b"s2k,s2k_fo", b"s2k", b"|", &ciphertext, b"|"]);
        let negotiation = test_mac(&negotiation_key, &transcript);

        assert!(session.decrypt_session_data(&ciphertext, &[], &negotiation).is_err(), "{fragment}");
    }
}
