//! Test-only construction of BSDIFF40 patches and gzip payloads.
//!
//! There is no bsdiff *encoder* on typical systems (macOS ships only `/usr/bin/bspatch`), so the
//! tests assemble valid BSDIFF40 patches directly from an explicit control stream. Any patch built
//! here is a genuine BSDIFF40 file and is applied both by [`crate::bspatch`] and, where available,
//! by the system `bspatch` for cross-checking.

use bzip2::Compression;
use bzip2::write::BzEncoder;
use std::io::Write;

/// One control triple plus the diff and extra bytes it consumes.
pub(crate) struct Segment {
    pub add: Vec<u8>,
    pub copy: Vec<u8>,
    pub seek: i64,
}

/// Signed-magnitude little-endian int64, matching `binarydist.signMagLittleEndian`.
fn put_offset(value: i64) -> [u8; 8] {
    let magnitude = value.unsigned_abs();
    let mut bytes = magnitude.to_le_bytes();

    if value < 0 {
        bytes[7] |= 0x80;
    }

    bytes
}

fn bzip2(data: &[u8]) -> Vec<u8> {
    let mut encoder = BzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("bzip2 write");

    encoder.finish().expect("bzip2 finish")
}

/// Assemble a BSDIFF40 patch from control segments; `new_len` is written into the header.
pub(crate) fn assemble(new_len: usize, segments: &[Segment]) -> Vec<u8> {
    let mut control = Vec::new();
    let mut diff = Vec::new();
    let mut extra = Vec::new();

    for segment in segments {
        control.extend_from_slice(&put_offset(segment.add.len() as i64));
        control.extend_from_slice(&put_offset(segment.copy.len() as i64));
        control.extend_from_slice(&put_offset(segment.seek));

        diff.extend_from_slice(&segment.add);
        extra.extend_from_slice(&segment.copy);
    }

    let control = bzip2(&control);
    let diff = bzip2(&diff);
    let extra = bzip2(&extra);

    let mut patch = Vec::with_capacity(32 + control.len() + diff.len() + extra.len());
    patch.extend_from_slice(b"BSDIFF40");
    patch.extend_from_slice(&put_offset(control.len() as i64));
    patch.extend_from_slice(&put_offset(diff.len() as i64));
    patch.extend_from_slice(&put_offset(new_len as i64));

    patch.extend_from_slice(&control);
    patch.extend_from_slice(&diff);
    patch.extend_from_slice(&extra);

    patch
}

/// The whole new file as one diff triple added over the old-file prefix.
pub(crate) fn patch_all_diff(old: &[u8], new: &[u8]) -> Vec<u8> {
    let add = new.iter().enumerate().map(|(index, byte)| byte.wrapping_sub(old.get(index).copied().unwrap_or(0)));
    let segment = Segment { add: add.collect(), copy: Vec::new(), seek: 0 };

    assemble(new.len(), &[segment])
}

/// The whole new file as literal extra bytes (the old file is not read).
pub(crate) fn patch_all_copy(new: &[u8]) -> Vec<u8> {
    let segment = Segment { add: Vec::new(), copy: new.to_vec(), seek: 0 };

    assemble(new.len(), &[segment])
}

/// A half-diff, half-copy patch that also exercises a nonzero seek between two diff runs.
pub(crate) fn patch_mixed(old: &[u8], new: &[u8]) -> Vec<u8> {
    let split = new.len() / 2;
    let seek = 1i64;

    let head =
        new[..split].iter().enumerate().map(|(index, byte)| byte.wrapping_sub(old.get(index).copied().unwrap_or(0)));
    let first = Segment { add: head.collect(), copy: Vec::new(), seek };

    let tail_start = split as i64 + seek;
    let tail = new[split..]
        .iter()
        .enumerate()
        .map(|(index, byte)| byte.wrapping_sub(old.get((tail_start + index as i64) as usize).copied().unwrap_or(0)));
    let second = Segment { add: tail.collect(), copy: Vec::new(), seek: 0 };

    assemble(new.len(), &[first, second])
}

/// Gzip a payload, as the full-binary update endpoint serves it.
pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
    use flate2::Compression as GzCompression;
    use flate2::write::GzEncoder;

    let mut encoder = GzEncoder::new(Vec::new(), GzCompression::default());
    encoder.write_all(data).expect("gzip write");

    encoder.finish().expect("gzip finish")
}
