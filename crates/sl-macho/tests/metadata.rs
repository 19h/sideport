use sl_macho::{Binary, BinaryMetadata, Endian, Format, LC_ENCRYPTION_INFO_64};
use std::io::Cursor;

fn image(endian: Endian, encrypted: bool, is_64: bool) -> Vec<u8> {
    let mut bytes = vec![0; 512];
    let header_size = if is_64 { 32 } else { 28 };
    let command_size = if is_64 { 24 } else { 20 };
    let command = if is_64 { LC_ENCRYPTION_INFO_64 } else { sl_macho::LC_ENCRYPTION_INFO };

    endian.put_u32(&mut bytes, 0, if is_64 { 0xfeed_facf } else { 0xfeed_face }).expect("magic");
    endian.put_u32(&mut bytes, 4, if is_64 { sl_macho::CPU_TYPE_ARM64 } else { 12 }).expect("cpu");
    endian.put_u32(&mut bytes, 12, 2).expect("file type");
    endian.put_u32(&mut bytes, 16, 1).expect("count");
    endian.put_u32(&mut bytes, 20, command_size).expect("command bytes");
    endian.put_u32(&mut bytes, header_size, command).expect("command");
    endian.put_u32(&mut bytes, header_size + 4, command_size).expect("size");
    endian.put_u32(&mut bytes, header_size + 8, 256).expect("crypt offset");
    endian.put_u32(&mut bytes, header_size + 12, 256).expect("crypt size");
    endian.put_u32(&mut bytes, header_size + 16, u32::from(encrypted)).expect("crypt id");

    bytes
}

#[test]
fn reads_only_headers_and_detects_encryption_in_both_byte_orders() {
    for endian in [Endian::Little, Endian::Big] {
        for encrypted in [false, true] {
            for is_64 in [false, true] {
                let bytes = image(endian, encrypted, is_64);
                let mut reader = Cursor::new(&bytes);
                let metadata = BinaryMetadata::read(&mut reader, bytes.len() as u64).expect("metadata");

                assert_eq!(metadata.encrypted(), encrypted);
                assert_eq!(reader.position(), if is_64 { 56 } else { 48 });
                assert_eq!(metadata.architectures[0].cpu_type, if is_64 { sl_macho::CPU_TYPE_ARM64 } else { 12 });
            }
        }
    }
}

#[test]
fn reads_fat32_and_fat64_in_both_byte_orders_without_reading_the_last_payload() {
    for endian in [Endian::Little, Endian::Big] {
        for is_64 in [false, true] {
            let first = image(Endian::Little, false, true);
            let mut second = image(Endian::Big, true, true);
            Endian::Big.put_u32(&mut second, 4, 0x0100_0007).expect("x86_64");
            let descriptor = Binary {
                format: Format::Fat { endian, is_64 },
                slices: vec![
                    Binary::parse(&first).expect("first").slices[0].clone(),
                    Binary::parse(&second).expect("second").slices[0].clone(),
                ],
            };
            let fat = descriptor.rebuild(&[first.clone(), second.clone()], 14).expect("fat");
            let mut reader = Cursor::new(&fat);
            let metadata = BinaryMetadata::read(&mut reader, fat.len() as u64).expect("metadata");

            assert_eq!(metadata.architectures.len(), 2);
            assert!(metadata.encrypted());
            assert_eq!(reader.position(), 32768 + 56);
        }
    }
}

#[test]
fn rejects_inconsistent_commands_extents_and_oversized_tables() {
    let original = image(Endian::Little, false, true);

    for (offset, value) in [(16, 2), (20, 16 * 1024 * 1024 + 1), (36, 16), (44, 513)] {
        let mut bytes = original.clone();
        Endian::Little.put_u32(&mut bytes, offset, value).expect("mutate");

        assert!(BinaryMetadata::read(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
    }

    let fat = [0xca, 0xfe, 0xba, 0xbe, 0xff, 0xff, 0xff, 0xff];
    assert!(BinaryMetadata::read(&mut Cursor::new(fat), u64::MAX).is_err());
}
