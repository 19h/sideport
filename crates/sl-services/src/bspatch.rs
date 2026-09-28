//! Apply a BSDIFF40 binary patch, the format the recovered updater consumes.
//!
//! Recovered from `sideloadlysid/binarydist.Patch` (a fork of Colin Percival's bsdiff via
//! `kr/binarydist`). The 32-byte header is the magic `BSDIFF40` and three signed-magnitude
//! little-endian int64 lengths: the bzip2-compressed control block, the bzip2-compressed diff
//! block and the size of the reconstructed file. The remainder of the patch is the bzip2-compressed
//! extra block. The control block is a run of `(add, copy, seek)` triples, each a signed-magnitude
//! little-endian int64.
//!
//! Each triple copies `add` diff bytes added byte-wise to the old file, then `copy` literal extra
//! bytes, then advances the old-file cursor by `seek`. The recovered decoder's bounds checks are
//! reproduced: negative lengths, an old-file cursor that leaves the old file, and a write past the
//! declared new size are all rejected rather than trusted.

use crate::{Error, Result};
use bzip2::read::BzDecoder;
use std::io::Read;

const MAGIC: &[u8; 8] = b"BSDIFF40";
const HEADER_LEN: usize = 32;
const TRIPLE_LEN: usize = 24;

/// Reconstruct the new file from `old` and a BSDIFF40 `patch`.
pub fn bspatch(old: &[u8], patch: &[u8]) -> Result<Vec<u8>> {
    if patch.len() < HEADER_LEN || &patch[..8] != MAGIC {
        return Err(Error::Patch("not a BSDIFF40 patch"));
    }

    let control_len = read_offset(&patch[8..16])?;
    let diff_len = read_offset(&patch[16..24])?;
    let new_len = read_offset(&patch[24..32])?;

    let control_end = HEADER_LEN.checked_add(control_len).ok_or(Error::Patch("control length overflow"))?;
    let diff_end = control_end.checked_add(diff_len).ok_or(Error::Patch("diff length overflow"))?;

    if diff_end > patch.len() {
        return Err(Error::Patch("patch blocks exceed the patch"));
    }

    let mut control = BzDecoder::new(&patch[HEADER_LEN..control_end]);
    let mut diff = BzDecoder::new(&patch[control_end..diff_end]);
    let mut extra = BzDecoder::new(&patch[diff_end..]);

    let mut new = vec![0u8; new_len];
    let mut new_pos = 0usize;
    let mut old_pos = 0i64;

    while new_pos < new_len {
        let (add, copy, seek) = read_triple(&mut control)?;

        new_pos = apply_diff(&mut diff, old, &mut new, new_pos, &mut old_pos, add)?;
        new_pos = apply_extra(&mut extra, &mut new, new_pos, copy)?;

        old_pos = old_pos.checked_add(seek).ok_or(Error::Patch("old cursor overflow"))?;
    }

    Ok(new)
}

/// Copy `add` diff bytes into `new`, adding each to the aligned old byte, then advance both cursors.
fn apply_diff(
    diff: &mut impl Read,
    old: &[u8],
    new: &mut [u8],
    new_pos: usize,
    old_pos: &mut i64,
    add: i64,
) -> Result<usize> {
    let add = length(add)?;
    let end =
        new_pos.checked_add(add).filter(|end| *end <= new.len()).ok_or(Error::Patch("diff write past new size"))?;

    read_exact(diff, &mut new[new_pos..end])?;

    for offset in 0..add {
        let source = old_pos.wrapping_add(offset as i64);

        if (0..old.len() as i64).contains(&source) {
            new[new_pos + offset] = new[new_pos + offset].wrapping_add(old[source as usize]);
        }
    }

    *old_pos = old_pos.checked_add(add as i64).ok_or(Error::Patch("old cursor overflow"))?;

    Ok(end)
}

/// Copy `copy` literal extra bytes into `new` and advance the new cursor.
fn apply_extra(extra: &mut impl Read, new: &mut [u8], new_pos: usize, copy: i64) -> Result<usize> {
    let copy = length(copy)?;
    let end =
        new_pos.checked_add(copy).filter(|end| *end <= new.len()).ok_or(Error::Patch("copy write past new size"))?;

    read_exact(extra, &mut new[new_pos..end])?;

    Ok(end)
}

/// Read one `(add, copy, seek)` control triple; a short read ends the stream cleanly only when it
/// is exactly empty.
fn read_triple(control: &mut impl Read) -> Result<(i64, i64, i64)> {
    let mut bytes = [0u8; TRIPLE_LEN];

    read_exact(control, &mut bytes)?;

    let add = read_offset_signed(&bytes[0..8]);
    let copy = read_offset_signed(&bytes[8..16]);
    let seek = read_offset_signed(&bytes[16..24]);

    Ok((add, copy, seek))
}

/// Signed-magnitude little-endian int64, as `binarydist.signMagLittleEndian`.
fn read_offset_signed(bytes: &[u8]) -> i64 {
    let raw = u64::from_le_bytes(bytes.try_into().expect("eight bytes"));
    let magnitude = (raw & !(1 << 63)) as i64;

    if raw & (1 << 63) != 0 { -magnitude } else { magnitude }
}

/// A header length: signed-magnitude, and rejected when negative, as the recovered decoder does.
fn read_offset(bytes: &[u8]) -> Result<usize> {
    let value = read_offset_signed(bytes);

    usize::try_from(value).map_err(|_| Error::Patch("negative length in header"))
}

/// A control length: rejected when negative before it indexes a buffer.
fn length(value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| Error::Patch("negative control length"))
}

fn read_exact(reader: &mut impl Read, buffer: &mut [u8]) -> Result<()> {
    reader.read_exact(buffer).map_err(|_| Error::Patch("truncated or corrupt patch block"))
}

#[cfg(test)]
mod tests;
