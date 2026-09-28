use super::PasswordProtocol;
use crate::{Error, Result, wire};
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use aes_gcm::aead::{AeadInPlace, KeyInit, generic_array::typenum::U16};
use aes_gcm::{AesGcm, Nonce, Tag};
use hmac::{Hmac, Mac};
use plist::{Dictionary, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use zeroize::Zeroizing;

pub const XCODE_APP: &str = "com.apple.gs.xcode.auth";
pub const MAX_SESSION_BYTES: usize = 64 * 1024;

pub struct VerifiedSession {
    pub(super) key: Zeroizing<[u8; 32]>,
    pub(super) protocol: PasswordProtocol,
}

impl fmt::Debug for VerifiedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("VerifiedSession").finish_non_exhaustive()
    }
}

impl VerifiedSession {
    /// Verify negotiation over the ciphertext before attempting CBC decryption.
    pub fn decrypt_session_data(&self, ciphertext: &[u8], context: Option<&[u8]>, proof: &[u8]) -> Result<SessionData> {
        if ciphertext.is_empty() || ciphertext.len() > MAX_SESSION_BYTES || !ciphertext.len().is_multiple_of(16) {
            return Err(Error::Invalid("encrypted session data length"));
        }

        if context.is_some_and(|value| value.len() > MAX_SESSION_BYTES) {
            return Err(Error::Invalid("negotiation context length"));
        }

        let negotiation_key = Zeroizing::new(hmac(&self.key[..], &[b"HMAC key:"]));
        let mut verified = false;

        for length_prefixed in [false, true] {
            let transcript = negotiation_transcript(self.protocol, ciphertext, context, length_prefixed);
            let mut negotiation = new_hmac(negotiation_key.as_slice());

            negotiation.update(&transcript);
            verified |= negotiation.verify_slice(proof).is_ok();
        }

        if !verified {
            return Err(Error::Verification("negotiation proof"));
        }

        let data_key = Zeroizing::new(hmac(&self.key[..], &[b"extra data key:"]));
        let iv = Zeroizing::new(hmac(&self.key[..], &[b"extra data iv:"]));
        let mut plaintext = Zeroizing::new(ciphertext.to_vec());
        let cipher = cbc::Decryptor::<aes::Aes256>::new_from_slices(data_key.as_slice(), &iv[..16])
            .map_err(|_| Error::Invalid("session cipher parameters"))?;
        let fragment = cipher
            .decrypt_padded_mut::<Pkcs7>(&mut plaintext)
            .map_err(|_| Error::Verification("session data padding"))?;
        let mut plist = parse_fragment(fragment)?;
        let dictionary = plist.0.as_dictionary_mut().ok_or(Error::Invalid("session dictionary"))?;

        let dsid = take_string(dictionary, "adsid")?;
        let idms_token = take_string(dictionary, "GsIdmsToken")?;
        let key = match dictionary.remove("sk") {
            Some(Value::Data(key)) => Zeroizing::new(key),
            _ => return Err(Error::Invalid("app token key")),
        };

        if !matches!(key.len(), 16 | 24 | 32) {
            return Err(Error::Invalid("app token key length"));
        }

        let continuation = match dictionary.remove("c") {
            Some(Value::String(value)) if !value.is_empty() => Continuation::Text(Zeroizing::new(value)),
            Some(Value::Data(value)) if !value.is_empty() => Continuation::Data(Zeroizing::new(value)),
            _ => return Err(Error::Invalid("session continuation")),
        };

        Ok(SessionData { dsid, idms_token, key, continuation, additional: plist })
    }
}

fn negotiation_transcript(
    protocol: PasswordProtocol,
    ciphertext: &[u8],
    context: Option<&[u8]>,
    length_prefixed: bool,
) -> [u8; 32] {
    let mut digest = Sha256::new();

    digest.update(b"s2k,s2k_fo||");
    digest.update(protocol.as_str().as_bytes());
    digest.update(b"|");

    if length_prefixed {
        digest.update((ciphertext.len() as u32).to_le_bytes());
    }

    digest.update(ciphertext);
    digest.update(b"|");

    if let Some(context) = context {
        if length_prefixed {
            digest.update((context.len() as u32).to_le_bytes());
        }

        digest.update(context);
    }

    digest.update(b"|");

    digest.finalize().into()
}

pub enum Continuation {
    Text(Zeroizing<String>),
    Data(Zeroizing<Vec<u8>>),
}

impl fmt::Debug for Continuation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Continuation([redacted])")
    }
}

pub struct SessionData {
    dsid: Zeroizing<String>,
    idms_token: Zeroizing<String>,
    key: Zeroizing<Vec<u8>>,
    continuation: Continuation,
    additional: wire::SecretValue,
}

impl fmt::Debug for SessionData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SessionData").finish_non_exhaustive()
    }
}

impl SessionData {
    pub fn dsid(&self) -> &str {
        &self.dsid
    }

    pub fn idms_token(&self) -> &str {
        &self.idms_token
    }

    pub fn continuation(&self) -> &Continuation {
        &self.continuation
    }

    /// Retain additional authenticated fields needed for trusted-device/SMS flows.
    pub fn additional_data(&self, name: &str) -> Option<&Value> {
        self.additional.0.as_dictionary()?.get(name)
    }

    pub fn app_token_checksum(&self) -> [u8; 32] {
        hmac(&self.key, &[b"apptokens", self.dsid.as_bytes(), XCODE_APP.as_bytes()])
    }

    /// Apple's envelope uses a 128-bit nonce, rather than GCM's common 96-bit nonce.
    pub fn decrypt_app_token(&self, envelope: &[u8]) -> Result<Zeroizing<String>> {
        if envelope.len() < 35 || envelope.len() > MAX_SESSION_BYTES {
            return Err(Error::Invalid("encrypted token length"));
        }

        if &envelope[..3] != b"XYZ" {
            return Err(Error::Invalid("encrypted token version"));
        }

        let nonce = &envelope[3..19];
        let tag = &envelope[envelope.len() - 16..];
        let mut plaintext = Zeroizing::new(envelope[19..envelope.len() - 16].to_vec());

        match self.key.len() {
            16 => decrypt_gcm::<AesGcm<aes::Aes128, U16>>(&self.key, nonce, tag, &mut plaintext)?,
            24 => decrypt_gcm::<AesGcm<aes::Aes192, U16>>(&self.key, nonce, tag, &mut plaintext)?,
            32 => decrypt_gcm::<AesGcm<aes::Aes256, U16>>(&self.key, nonce, tag, &mut plaintext)?,
            _ => return Err(Error::Invalid("app token key length")),
        }

        let mut plist = parse_fragment(&plaintext)?;
        let token = plist
            .0
            .as_dictionary_mut()
            .and_then(|root| root.get_mut("t"))
            .and_then(Value::as_dictionary_mut)
            .and_then(|tokens| tokens.get_mut(XCODE_APP))
            .and_then(Value::as_dictionary_mut)
            .ok_or(Error::Invalid("app token dictionary"))?;

        take_string(token, "token")
    }
}

fn decrypt_gcm<Cipher>(key: &[u8], nonce: &[u8], tag: &[u8], ciphertext: &mut [u8]) -> Result<()>
where
    Cipher: KeyInit + AeadInPlace<NonceSize = U16, TagSize = U16>,
{
    let cipher = Cipher::new_from_slice(key).map_err(|_| Error::Invalid("app token key length"))?;

    cipher
        .decrypt_in_place_detached(Nonce::<U16>::from_slice(nonce), b"XYZ", ciphertext, Tag::from_slice(tag))
        .map_err(|_| Error::Verification("app token authentication"))
}

fn new_hmac(key: &[u8]) -> Hmac<Sha256> {
    // HMAC accepts every key length; this branch is unreachable for this implementation.
    <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC supports every key length")
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = new_hmac(key);

    for part in parts {
        mac.update(part);
    }

    mac.finalize().into_bytes().into()
}

fn take_string(dictionary: &mut Dictionary, key: &'static str) -> Result<Zeroizing<String>> {
    let value = match dictionary.remove(key) {
        Some(Value::String(value)) => Zeroizing::new(value),
        _ => return Err(Error::Invalid(key)),
    };

    if value.is_empty() || value.contains('\0') {
        return Err(Error::Invalid(key));
    }

    Ok(value)
}

fn parse_fragment(fragment: &[u8]) -> Result<wire::SecretValue> {
    wire::decode(fragment, true)
}
