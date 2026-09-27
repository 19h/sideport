//! Checked Apple code-signature blob containers (big-endian).

use crate::{Error, Result};
use sl_macho::{Endian, to_u32};
use std::collections::BTreeMap;

pub const EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
pub const CODE_DIRECTORY: u32 = 0xfade_0c02;
pub const REQUIREMENTS: u32 = 0xfade_0c01;
pub const REQUIREMENT: u32 = 0xfade_0c00;
pub const ENTITLEMENTS: u32 = 0xfade_7171;
pub const DER_ENTITLEMENTS: u32 = 0xfade_7172;
pub const BLOB_WRAPPER: u32 = 0xfade_0b01;

pub fn wrap(magic: u32, contents: &[u8]) -> Result<Vec<u8>> {
    let size = contents.len().checked_add(8).ok_or_else(|| invalid("blob size overflow"))?;

    let mut bytes = Vec::with_capacity(size);
    bytes.extend(magic.to_be_bytes());
    bytes.extend(to_u32(size, "blob size")?.to_be_bytes());
    bytes.extend_from_slice(contents);

    Ok(bytes)
}

pub fn superblob(magic: u32, blobs: &BTreeMap<u32, Vec<u8>>) -> Result<Vec<u8>> {
    let header =
        blobs.len().checked_mul(8).and_then(|n| n.checked_add(12)).ok_or_else(|| invalid("blob index overflow"))?;

    let total = blobs
        .values()
        .try_fold(header, |size, blob| size.checked_add(blob.len()).ok_or_else(|| invalid("blob length overflow")))?;

    let mut output = vec![0; header];
    Endian::Big.put_u32(&mut output, 0, magic)?;
    Endian::Big.put_u32(&mut output, 4, to_u32(total, "superblob length")?)?;
    Endian::Big.put_u32(&mut output, 8, to_u32(blobs.len(), "superblob count")?)?;

    for (i, (slot, blob)) in blobs.iter().enumerate() {
        if blob.len() < 8 || Endian::Big.u32(blob, 4)? as usize != blob.len() {
            return Err(invalid("invalid child blob length"));
        }

        let offset = to_u32(output.len(), "blob offset")?;
        Endian::Big.put_u32(&mut output, 12 + i * 8, *slot)?;
        Endian::Big.put_u32(&mut output, 16 + i * 8, offset)?;
        output.extend_from_slice(blob);
    }

    Ok(output)
}

/// Return checked child blobs. Padding after the declared container length is not included.
pub fn parse_superblob(bytes: &[u8], magic: u32) -> Result<BTreeMap<u32, &[u8]>> {
    if Endian::Big.u32(bytes, 0)? != magic {
        return Err(invalid("unexpected superblob magic"));
    }

    let length = Endian::Big.u32(bytes, 4)? as usize;
    let count = Endian::Big.u32(bytes, 8)? as usize;
    let header = count.checked_mul(8).and_then(|n| n.checked_add(12)).ok_or_else(|| invalid("blob index overflow"))?;

    if length > bytes.len() || header > length {
        return Err(invalid("truncated superblob index"));
    }

    let mut blobs = BTreeMap::new();
    let mut ranges = Vec::with_capacity(count);

    for i in 0..count {
        let slot = Endian::Big.u32(bytes, 12 + i * 8)?;
        let offset = Endian::Big.u32(bytes, 16 + i * 8)? as usize;

        if offset < header || offset.checked_add(8).is_none_or(|end| end > length) {
            return Err(invalid("child blob overlaps index or exceeds container"));
        }

        let size = Endian::Big.u32(bytes, offset + 4)? as usize;
        let end = offset.checked_add(size).ok_or_else(|| invalid("child blob end overflow"))?;

        if size < 8 || end > length || blobs.insert(slot, &bytes[offset..end]).is_some() {
            return Err(invalid("duplicate slot or invalid child blob size"));
        }

        ranges.push(offset..end);
    }

    ranges.sort_unstable_by_key(|range| range.start);

    if ranges.windows(2).any(|pair| pair[0].end > pair[1].start) {
        return Err(invalid("overlapping child blobs"));
    }

    Ok(blobs)
}

fn invalid(message: &str) -> Error {
    Error::Other(format!("invalid code-signature blob: {message}"))
}
