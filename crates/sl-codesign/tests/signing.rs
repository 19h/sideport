use proptest::prelude::*;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use sl_codesign::{CodeKind, SignOptions, Signer, blob, sign_macho, strip_signature};
use sl_macho::{Binary, Endian, MachO};

fn options() -> SignOptions<'static> {
    SignOptions {
        identifier: "com.example.test",
        kind: CodeKind::MainExecutable,
        entitlements: None,
        info_plist: None,
        code_resources: None,
    }
}

fn image(length: usize, endian: Endian, is_64: bool) -> Vec<u8> {
    let mut input = vec![0; length.max(256)];

    endian.put_u32(&mut input, 0, if is_64 { 0xfeedfacf } else { 0xfeedface }).expect("magic");
    endian.put_u32(&mut input, 4, if is_64 { sl_macho::CPU_TYPE_ARM64 } else { 12 }).expect("cpu");
    endian.put_u32(&mut input, 12, 2).expect("type");

    input[256..].fill(0x5a);

    input
}

#[test]
fn independent_page_hashes_special_slots_and_partial_final_page() {
    for endian in [Endian::Little, Endian::Big] {
        for is_64 in [false, true] {
            let input = image(5003, endian, is_64);
            let opts = SignOptions { info_plist: Some(b"info"), code_resources: Some(b"resources"), ..options() };

            let signed = sign_macho(&input, &Signer::AdHoc, &opts).expect("sign");
            let parsed = MachO::parse(&signed).expect("Mach-O");
            let range = parsed.signature.expect("signature");

            assert_eq!(range.start, 5008);
            assert_eq!(&signed[256..5003], &input[256..]);

            let blobs = blob::parse_superblob(&signed[range.clone()], blob::EMBEDDED_SIGNATURE).expect("superblob");

            assert_eq!(blobs.len(), 3);
            assert!(!blobs.contains_key(&0x10000));

            for slot in [0, 0x1000] {
                let directory = blobs[&slot];
                let hash_offset = Endian::Big.u32(directory, 16).expect("hash offset") as usize;
                let page_count = Endian::Big.u32(directory, 28).expect("count") as usize;
                let hash_size = directory[36] as usize;

                assert_eq!(page_count, 2);
                assert_eq!(Endian::Big.u32(directory, 24).expect("special count"), 3);
                assert_eq!(Endian::Big.u32(directory, 12).expect("flags"), 2);
                assert_eq!(Endian::Big.u64(directory, 80).expect("exec flags"), 1);

                for (index, page) in signed[..range.start].chunks(4096).enumerate() {
                    let expected = if slot == 0 { Sha1::digest(page).to_vec() } else { Sha256::digest(page).to_vec() };
                    let start = hash_offset + index * hash_size;

                    assert_eq!(&directory[start..start + hash_size], expected);
                }

                for (special_slot, data) in [(1, &b"info"[..]), (2, blobs[&2]), (3, &b"resources"[..])] {
                    let expected = if slot == 0 { Sha1::digest(data).to_vec() } else { Sha256::digest(data).to_vec() };
                    let start = hash_offset - special_slot * hash_size;

                    assert_eq!(&directory[start..start + hash_size], expected);
                }
            }

            assert_eq!(signed, sign_macho(&signed, &Signer::AdHoc, &opts).expect("resign"));

            let stripped = strip_signature(&signed).expect("strip");

            assert!(MachO::parse(&stripped).expect("parse").signature.is_none());
            assert_eq!(&stripped[..input.len()], &input);
            assert_eq!(strip_signature(&stripped).expect("idempotent"), stripped);
        }
    }
}

#[test]
fn universal_signing_and_stripping_preserve_architectures() {
    let first = image(1000, Endian::Little, true);
    let second = image(2000, Endian::Big, false);
    let first_binary = Binary::parse(&first).expect("thin");
    let second_binary = Binary::parse(&second).expect("thin");

    let descriptor = Binary {
        format: sl_macho::Format::Fat { endian: Endian::Big, is_64: true },
        slices: vec![first_binary.slices[0].clone(), second_binary.slices[0].clone()],
    };
    let original = descriptor.rebuild(&[first.clone(), second.clone()], 14).expect("fat");

    let signed = sign_macho(&original, &Signer::AdHoc, &options()).expect("sign");
    let parsed = Binary::parse(&signed).expect("fat");

    assert!(parsed.slices.iter().all(|slice| slice.image.signature.is_some()));
    assert!(sl_codesign::is_arm64_macho(&signed));
    assert!(!sl_codesign::is_encrypted(&signed).expect("encrypted"));

    let stripped = strip_signature(&signed).expect("strip");
    let parsed = Binary::parse(&stripped).expect("fat");

    assert!(parsed.slices.iter().all(|slice| slice.image.signature.is_none()));
}

#[test]
fn malformed_signatures_are_errors_instead_of_panics_or_data_loss() {
    let signed = sign_macho(&image(600, Endian::Little, true), &Signer::AdHoc, &options()).expect("sign");

    let mut suffix = signed.clone();
    suffix.push(0x55);

    assert!(strip_signature(&suffix).is_err());

    let parsed = MachO::parse(&signed).expect("parse");
    let command = parsed.commands[0].bytes.to_vec();
    let duplicate = parsed.rewrite_commands(&[command.clone(), command]).expect("rewrite");

    assert!(MachO::parse(&duplicate).is_err());

    let range = parsed.signature.expect("sig");
    let mut bad = signed[range].to_vec();
    Endian::Big.put_u32(&mut bad, 16, 12).expect("index overlap");

    assert!(blob::parse_superblob(&bad, blob::EMBEDDED_SIGNATURE).is_err());
    assert!(sign_macho(&signed, &Signer::AdHoc, &SignOptions { identifier: "x\0y", ..options() }).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn arbitrary_inputs_fail_without_panicking(data in prop::collection::vec(any::<u8>(), 0..2048)) {
        let _ = sign_macho(&data, &Signer::AdHoc, &options());
        let _ = strip_signature(&data);

        let _ = sl_codesign::ProvisioningProfile::parse(&data);
        let _ = blob::parse_superblob(&data, blob::EMBEDDED_SIGNATURE);
    }
}
