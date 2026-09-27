//! GrandSlam's SHA-256 / RFC 5054 2048-bit SRP exchange.
//!
//! `no_username_in_x` retains the colon: x = H(s || H(":" || derived_password)).
//! Only k and u use N-width padding; K, M1 and M2 use minimal integer encodings.

mod session;

pub use session::{Continuation, SessionData, VerifiedSession, XCODE_APP};

use crate::{Error, Result};
use crypto_bigint::modular::runtime_mod::{DynResidue, DynResidueParams};
use crypto_bigint::{Encoding, U256, U512, U2048};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use std::fmt;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

const MODULUS: U2048 = U2048::from_be_hex(concat!(
    "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050A",
    "37329CBB4A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50E8",
    "083969EDB767B0CF6095179A163AB3661A05FBD5FAAAE82918A9962F0B93B855F",
    "97993EC975EEAA80D740ADBF4FF747359D041D5C33EA71D281E446B14773BCA97",
    "B43A23FB801676BD207A436C6481F1D2B9078717461A5B9D32E688F8774854452",
    "3B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB3786160279004E57AE6AF874E",
    "7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DBFBB694B5C8",
    "03D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73",
));
const PARAMETERS: DynResidueParams<{ U2048::LIMBS }> = DynResidueParams::new(&MODULUS);
const GENERATOR: U2048 = U2048::from_u8(2);

pub const MAX_ITERATIONS: u32 = 1_000_000;
pub const MAX_SALT_BYTES: usize = 1024;
pub const MAX_PASSWORD_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordProtocol {
    S2k,
    S2kFo,
}

impl PasswordProtocol {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "s2k" => Ok(Self::S2k),
            "s2k_fo" => Ok(Self::S2kFo),
            _ => Err(Error::Invalid("unsupported password protocol")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::S2k => "s2k",
            Self::S2kFo => "s2k_fo",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Challenge<'a> {
    pub protocol: PasswordProtocol,
    pub iterations: u32,
    pub salt: &'a [u8],
    pub server_public: &'a [u8],
}

impl fmt::Debug for Challenge<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Challenge")
            .field("protocol", &self.protocol)
            .field("iterations", &self.iterations)
            .finish_non_exhaustive()
    }
}

/// One exchange. Consuming transitions prevent reuse of the secret ephemeral.
pub struct SrpClient {
    username: String,
    ephemeral: Zeroizing<U256>,
    public: U2048,
}

impl fmt::Debug for SrpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SrpClient").finish_non_exhaustive()
    }
}

impl SrpClient {
    pub fn new(username: String) -> Result<Self> {
        let mut ephemeral = Zeroizing::new([0; 32]);
        OsRng.try_fill_bytes(ephemeral.as_mut()).map_err(|_| Error::Entropy)?;
        ephemeral[0] |= 0x80;

        Self::with_ephemeral(username, *ephemeral)
    }

    pub(crate) fn with_ephemeral(username: String, ephemeral: [u8; 32]) -> Result<Self> {
        let ephemeral = Zeroizing::new(ephemeral);

        if username.is_empty() || username.len() > 1024 || username.contains('\0') {
            return Err(Error::Invalid("username"));
        }

        let ephemeral = Zeroizing::new(U256::from_be_bytes(*ephemeral));

        if bool::from(ephemeral.ct_eq(&U256::ZERO)) {
            return Err(Error::Invalid("zero secret ephemeral"));
        }

        let public = DynResidue::new(&GENERATOR, PARAMETERS).pow(&ephemeral).retrieve();

        Ok(Self { username, ephemeral, public })
    }

    pub fn public_key(&self) -> Vec<u8> {
        minimal(&self.public.to_be_bytes()).to_vec()
    }

    /// CPU-bound: front ends must execute this outside their UI/async executor.
    pub fn process(self, password: &str, challenge: Challenge<'_>) -> Result<SrpProof> {
        let server_public = decode_public(challenge.server_public)?;
        let derived = derive_password(password, challenge)?;

        let private_hash = Zeroizing::new(hash(&[b":", derived.as_slice()]));
        let private = Zeroizing::new(U256::from_be_bytes(hash(&[challenge.salt, private_hash.as_slice()])));
        let scramble = U256::from_be_bytes(hash(&[&self.public.to_be_bytes(), &server_public.to_be_bytes()]));

        if bool::from(scramble.ct_eq(&U256::ZERO)) {
            return Err(Error::Invalid("zero scrambling parameter"));
        }

        let multiplier: U2048 = U256::from_be_bytes(hash(&[&MODULUS.to_be_bytes(), &GENERATOR.to_be_bytes()])).resize();
        let verifier = DynResidue::new(&GENERATOR, PARAMETERS).pow(&private);
        let base = DynResidue::new(&server_public, PARAMETERS) - DynResidue::new(&multiplier, PARAMETERS) * verifier;

        // u and x are at most 256 bits. Their product plus a fits 512 bits.
        let product = Zeroizing::new(U512::from(scramble.mul_wide(&private)));
        let exponent = Zeroizing::new(product.wrapping_add(&self.ephemeral.resize()));
        let shared = Zeroizing::new(base.pow(&exponent).retrieve());

        if bool::from(shared.ct_eq(&U2048::ZERO)) {
            return Err(Error::Invalid("zero shared secret"));
        }

        let shared_bytes = Zeroizing::new(shared.to_be_bytes());
        let key = Zeroizing::new(hash(&[minimal(shared_bytes.as_slice())]));
        let modulus_hash = hash(&[&MODULUS.to_be_bytes()]);
        let generator_hash = hash(&[&GENERATOR.to_be_bytes()]);
        let group_hash: [u8; 32] = std::array::from_fn(|index| modulus_hash[index] ^ generator_hash[index]);

        let public_bytes = self.public.to_be_bytes();
        let server_bytes = server_public.to_be_bytes();
        let client_proof = hash(&[
            &group_hash,
            &hash(&[self.username.as_bytes()]),
            challenge.salt,
            minimal(&public_bytes),
            minimal(&server_bytes),
            key.as_slice(),
        ]);
        let server_proof = Zeroizing::new(hash(&[minimal(&public_bytes), &client_proof, key.as_slice()]));

        Ok(SrpProof { client_proof, server_proof, key, protocol: challenge.protocol })
    }
}

pub struct SrpProof {
    client_proof: [u8; 32],
    server_proof: Zeroizing<[u8; 32]>,
    key: Zeroizing<[u8; 32]>,
    protocol: PasswordProtocol,
}

impl fmt::Debug for SrpProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SrpProof").finish_non_exhaustive()
    }
}

impl SrpProof {
    pub fn client_proof(&self) -> &[u8; 32] {
        &self.client_proof
    }

    /// Session decryption becomes available only after authenticating M2.
    pub fn verify(self, server_proof: &[u8]) -> Result<VerifiedSession> {
        if !bool::from(self.server_proof.as_slice().ct_eq(server_proof)) {
            return Err(Error::Verification("server SRP proof"));
        }

        Ok(VerifiedSession { key: self.key, protocol: self.protocol })
    }
}

fn derive_password(password: &str, challenge: Challenge<'_>) -> Result<Zeroizing<[u8; 32]>> {
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(Error::Invalid("password length"));
    }

    if !(1..=MAX_ITERATIONS).contains(&challenge.iterations) {
        return Err(Error::Invalid("PBKDF2 iteration count"));
    }

    if challenge.salt.is_empty() || challenge.salt.len() > MAX_SALT_BYTES {
        return Err(Error::Invalid("salt length"));
    }

    let password_hash = Zeroizing::new(hash(&[password.as_bytes()]));
    let hex_hash = Zeroizing::new(hex::encode(password_hash.as_slice()));
    let input = match challenge.protocol {
        PasswordProtocol::S2k => password_hash.as_slice(),
        PasswordProtocol::S2kFo => hex_hash.as_bytes(),
    };
    let mut derived = Zeroizing::new([0; 32]);
    pbkdf2::pbkdf2_hmac::<Sha256>(input, challenge.salt, challenge.iterations, derived.as_mut());

    Ok(derived)
}

fn decode_public(bytes: &[u8]) -> Result<U2048> {
    if bytes.is_empty() || bytes.len() > 256 {
        return Err(Error::Invalid("server public key length"));
    }

    let mut padded = [0; 256];
    padded[256 - bytes.len()..].copy_from_slice(bytes);
    let public = U2048::from_be_bytes(padded);

    if public == U2048::ZERO || public >= MODULUS {
        return Err(Error::Invalid("server public key range"));
    }

    Ok(public)
}

fn minimal(bytes: &[u8]) -> &[u8] {
    let first = bytes.iter().position(|byte| *byte != 0).unwrap_or(bytes.len());

    &bytes[first..]
}

pub(super) fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();

    for part in parts {
        hash.update(part);
    }

    hash.finalize().into()
}

#[cfg(test)]
mod tests;
