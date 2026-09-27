use proptest::prelude::*;
use sl_macho::{Binary, Endian, Format, LC_LOAD_DYLIB, LC_LOAD_WEAK_DYLIB, MachO};

fn image(endian: Endian, is_64: bool) -> Vec<u8> {
    let mut bytes = vec![0; 512];

    endian.put_u32(&mut bytes, 0, if is_64 { 0xfeed_facf } else { 0xfeed_face }).expect("magic");
    endian.put_u32(&mut bytes, 4, if is_64 { sl_macho::CPU_TYPE_ARM64 } else { 12 }).expect("cpu");
    endian.put_u32(&mut bytes, 12, 2).expect("file type");

    bytes[256..].fill(0x55);

    bytes
}

#[test]
fn both_widths_and_endians_preserve_unknown_commands_and_edit_dylibs() {
    for endian in [Endian::Little, Endian::Big] {
        for is_64 in [false, true] {
            let bytes = image(endian, is_64);
            let parsed = MachO::parse(&bytes).expect("parse");

            let mut unknown = vec![0; 16];
            endian.put_u32(&mut unknown, 0, 0x1234).expect("command");
            endian.put_u32(&mut unknown, 4, 16).expect("size");
            unknown[8..].fill(0x78);

            let bytes = parsed.rewrite_commands(&[unknown.clone()]).expect("rewrite");
            let parsed = MachO::parse(&bytes).expect("parse");
            let added = parsed.add_dylib("@executable_path/Frameworks/test.dylib", false).expect("add");
            let parsed = MachO::parse(&added).expect("parse");

            assert_eq!(parsed.commands[0].bytes, unknown);
            assert_eq!(parsed.commands[1].kind, LC_LOAD_DYLIB);
            assert_eq!(parsed.add_dylib("@executable_path/Frameworks/test.dylib", false).expect("idempotent"), added);

            let weak = parsed.add_dylib("@rpath/other.dylib", true).expect("weak");

            assert_eq!(MachO::parse(&weak).expect("parse").commands[2].kind, LC_LOAD_WEAK_DYLIB);
            assert_eq!(&weak[256..], &bytes[256..]);
        }
    }
}

#[test]
fn rejects_no_padding_bad_command_count_and_overflow() {
    let mut bytes = image(Endian::Little, true);
    bytes[32] = 1;

    assert!(MachO::parse(&bytes).expect("parse").add_dylib("x", false).is_err());

    Endian::Little.put_u32(&mut bytes, 16, u32::MAX).expect("count");

    assert!(MachO::parse(&bytes).is_err());
    assert!(sl_macho::checked_range(u64::MAX, 4, 32).is_err());
    assert!(sl_macho::align_up(usize::MAX, 16).is_err());
    assert!(sl_macho::align_up(5, 3).is_err());
}

#[test]
fn fat_variants_rebuild_and_reject_overlap_misalignment_or_cpu_mismatch() {
    for endian in [Endian::Little, Endian::Big] {
        for is_64 in [false, true] {
            let first = image(Endian::Little, true);
            let second = image(Endian::Big, false);
            let first_binary = Binary::parse(&first).expect("thin");
            let second_binary = Binary::parse(&second).expect("thin");

            let descriptor = Binary {
                format: Format::Fat { endian, is_64 },
                slices: vec![first_binary.slices[0].clone(), second_binary.slices[0].clone()],
            };
            let fat = descriptor.rebuild(&[first.clone(), second.clone()], 14).expect("fat");
            let parsed = Binary::parse(&fat).expect("parse");

            assert_eq!(parsed.slices.len(), 2);
            assert_eq!(parsed.slices[0].offset, 16384);
            assert_eq!(parsed.slices[1].offset, 32768);
            assert_eq!(parsed.rebuild(&[first.clone(), second.clone()], 14).expect("rebuild"), fat);

            let mut bad_cpu = fat.clone();
            endian.put_u32(&mut bad_cpu, 8, 7).expect("cpu");

            assert!(Binary::parse(&bad_cpu).is_err());

            let mut overlapping = fat.clone();
            let second_arch_offset = 8 + if is_64 { 32 } else { 20 };

            if is_64 {
                endian.put_u64(&mut overlapping, second_arch_offset + 8, 16384).expect("offset");
            } else {
                endian.put_u32(&mut overlapping, second_arch_offset + 8, 16384).expect("offset");
            }

            assert!(Binary::parse(&overlapping).is_err());

            let mut misaligned = fat;

            if is_64 {
                endian.put_u64(&mut misaligned, 16, 16385).expect("offset");
            } else {
                endian.put_u32(&mut misaligned, 16, 16385).expect("offset");
            }

            assert!(Binary::parse(&misaligned).is_err());
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_input_never_panics(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let _ = Binary::parse(&data);
    }

    #[test]
    fn structured_header_mutations_never_panic(
        data in prop::collection::vec(any::<u8>(), 4..4096),
        magic in prop::sample::select(vec![0xfeedfaceu32, 0xfeedfacf, 0xcafebabe, 0xcafebabf]),
        big in any::<bool>(),
    ) {
        let mut data = data;
        let endian = if big { Endian::Big } else { Endian::Little };

        endian.put_u32(&mut data, 0, magic).expect("magic");

        let _ = Binary::parse(&data);
    }
}
