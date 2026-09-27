//! Code requirement language (binary form).

use crate::{Error, Result, Signer, blob};
use sl_macho::Endian;
use std::collections::BTreeMap;

/// Designated Apple development requirement and optional library requirements.
/// Library expressions from LC_DYLIB_CODE_SIGN_DRS are preserved, joined with OR.
#[rustfmt::skip]
pub fn build(identifier: &str, signer: &Signer, library_drs: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut entries = BTreeMap::new();

    let Signer::Identity(identity) =
        signer else {
            return blob::superblob(blob::REQUIREMENTS, &entries);
        };

    let mut expr = Vec::new();

    word(&mut expr, 6); // identifier AND (anchor AND (CN AND WWDR))
    word(&mut expr, 2); data(&mut expr, identifier.as_bytes())?;
    word(&mut expr, 6); word(&mut expr, 15); // apple generic anchor
    word(&mut expr, 6); word(&mut expr, 11); word(&mut expr, 0); // leaf cert field
    data(&mut expr, b"subject.CN")?; word(&mut expr, 1); data(&mut expr, identity.common_name().as_bytes())?;
    word(&mut expr, 14); word(&mut expr, 1);
    data(&mut expr, &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 6, 2, 1])?;
    word(&mut expr, 0); // matchExists

    entries.insert(3, requirement(&expr)?);

    if let Some(drs) = library_drs {
        // DRS is a 0xfade0c05 superblob whose children are expression-form requirements.
        let children = blob::parse_superblob(drs, 0xfade_0c05)?;

        let mut library = Vec::new();
        let count = children.len();

        for (i, bytes) in children.values().enumerate() {
            if bytes.len() < 16
            || Endian::Big.u32(bytes, 0)? != blob::REQUIREMENT
            || Endian::Big.u32(bytes, 8)? != 1 {
                return Err(Error::Other("invalid expression-form library requirement".into()));
            }

            if i + 1 < count {
                word(&mut library, 7);
            }

            library.extend_from_slice(&bytes[12..]);
        }

        if count != 0 {
            entries.insert(4, requirement(&library)?);
        }
    }

    blob::superblob(blob::REQUIREMENTS, &entries)
}

fn requirement(expr: &[u8]) -> Result<Vec<u8>> {
    let mut payload = 1u32.to_be_bytes().to_vec();
    payload.extend_from_slice(expr);

    blob::wrap(blob::REQUIREMENT, &payload)
}

fn word(output: &mut Vec<u8>, value: u32) {
    output.extend(value.to_be_bytes());
}

fn data(output: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    word(output, sl_macho::to_u32(value.len(), "requirement data length")?);
    output.extend_from_slice(value);
    output.resize(sl_macho::align_up(output.len(), 4)?, 0);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adhoc_empty_vector() {
        assert_eq!(
            build("example", &Signer::AdHoc, None).expect("requirements"),
            [0xfa, 0xde, 0x0c, 1, 0, 0, 0, 12, 0, 0, 0, 0]
        );
    }
}
