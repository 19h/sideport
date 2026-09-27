//! Signature blob construction and Mach-O rewriting.
use crate::{Result, SignOptions, Signer};

pub(crate) fn sign_file(input: &[u8], signer: &Signer, opts: &SignOptions<'_>) -> Result<Vec<u8>> {
    let _ = (input, signer, opts);
    unimplemented!()
}
pub(crate) fn strip_file(input: &[u8]) -> Result<Vec<u8>> {
    let _ = input;
    unimplemented!()
}
pub(crate) fn is_arm64_macho(data: &[u8]) -> bool {
    let _ = data;
    unimplemented!()
}
pub(crate) fn is_encrypted(data: &[u8]) -> Result<bool> {
    let _ = data;
    unimplemented!()
}
