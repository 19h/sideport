use super::bspatch;
use crate::Error;
use crate::testsupport::{Segment, assemble, patch_all_copy, patch_all_diff, patch_mixed};
use std::process::Command;

fn old_and_new() -> (Vec<u8>, Vec<u8>) {
    let old: Vec<u8> = (0u8..=200).cycle().take(4096).collect();

    let mut new = old.clone();
    new.iter_mut().step_by(7).for_each(|byte| *byte = byte.wrapping_add(0x5a));
    new.extend_from_slice(b"appended tail that is not present in the old file at all");

    (old, new)
}

#[test]
fn diff_copy_and_mixed_patches_all_reconstruct_the_new_file() {
    let (old, new) = old_and_new();

    assert_eq!(bspatch(&old, &patch_all_diff(&old, &new)).expect("diff"), new);
    assert_eq!(bspatch(&old, &patch_all_copy(&new)).expect("copy"), new);
    assert_eq!(bspatch(&old, &patch_mixed(&old, &new)).expect("mixed"), new);
}

#[test]
fn an_all_copy_patch_ignores_the_old_file() {
    let (_, new) = old_and_new();
    let patch = patch_all_copy(&new);

    assert_eq!(bspatch(b"", &patch).expect("empty old"), new);
    assert_eq!(bspatch(b"unrelated old bytes", &patch).expect("other old"), new);
}

#[test]
fn a_wrong_magic_a_short_header_and_a_corrupt_block_are_rejected() {
    let (old, new) = old_and_new();

    assert!(matches!(bspatch(&old, b"not a patch at all___").expect_err("error expected"), Error::Patch(_)));

    let copy = patch_all_copy(&new);
    assert!(matches!(bspatch(&old, &copy[..8]).expect_err("error expected"), Error::Patch(_)));

    // A control length inflated past the patch overruns the declared blocks.
    let mut overrun = copy.clone();
    overrun[8..16].copy_from_slice(&(i64::MAX as u64).to_le_bytes());
    assert!(matches!(bspatch(&old, &overrun).expect_err("error expected"), Error::Patch(_)));

    // A byte flipped inside the compressed control block breaks its bzip2 stream.
    let mut corrupt = copy;
    corrupt[34] ^= 0xff;
    assert!(matches!(bspatch(&old, &corrupt).expect_err("error expected"), Error::Patch(_)));
}

#[test]
fn a_control_write_past_the_declared_new_size_is_rejected() {
    // Declare a new size of one byte but instruct the decoder to copy sixteen.
    let overrun = assemble(1, &[Segment { add: Vec::new(), copy: vec![0u8; 16], seek: 0 }]);

    assert!(matches!(bspatch(b"", &overrun).expect_err("error expected"), Error::Patch(_)));
}

/// Cross-check against the platform `bspatch` when it is installed; skip otherwise. This proves the
/// generated fixtures are genuine BSDIFF40 files and that the decoder agrees with the reference.
#[test]
fn the_system_bspatch_reconstructs_the_same_bytes() {
    let tool = "/usr/bin/bspatch";

    if !std::path::Path::new(tool).exists() {
        eprintln!("skipping: {tool} is not installed");

        return;
    }

    let (old, new) = old_and_new();
    let directory = tempfile::tempdir().expect("temp dir");
    let old_path = directory.path().join("old.bin");
    let new_path = directory.path().join("new.bin");
    let patch_path = directory.path().join("patch.bsdiff");

    for patch in [patch_all_diff(&old, &new), patch_all_copy(&new), patch_mixed(&old, &new)] {
        std::fs::write(&old_path, &old).expect("write old");
        std::fs::write(&patch_path, &patch).expect("write patch");
        let _ = std::fs::remove_file(&new_path);

        let status = Command::new(tool).args([&old_path, &new_path, &patch_path]).status().expect("run system bspatch");

        assert!(status.success(), "system bspatch failed");
        let reference = std::fs::read(&new_path).expect("read system output");

        assert_eq!(reference, new, "system bspatch output differs");
        assert_eq!(bspatch(&old, &patch).expect("our bspatch"), reference);
    }
}
