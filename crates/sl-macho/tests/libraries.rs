use sl_macho::{Endian, LC_ID_DYLIB, LC_LOAD_DYLIB, LC_RPATH, MachO};
use std::collections::BTreeMap;

fn image(endian: Endian, is_64: bool) -> Vec<u8> {
    let mut bytes = vec![0; 1024];

    endian.put_u32(&mut bytes, 0, if is_64 { 0xfeedfacf } else { 0xfeedface }).expect("magic");
    endian.put_u32(&mut bytes, 4, if is_64 { sl_macho::CPU_TYPE_ARM64 } else { 12 }).expect("CPU");
    endian.put_u32(&mut bytes, 12, 6).expect("file type");
    bytes[512..].fill(0x5a);

    bytes
}

fn command(endian: Endian, is_64: bool, kind: u32, path: &str) -> Vec<u8> {
    let size = sl_macho::align_up(24 + path.len() + 1, if is_64 { 8 } else { 4 }).expect("alignment");
    let mut bytes = vec![0; size];

    endian.put_u32(&mut bytes, 0, kind).expect("kind");
    endian.put_u32(&mut bytes, 4, size as u32).expect("size");
    endian.put_u32(&mut bytes, 8, 24).expect("name offset");

    endian.put_u32(&mut bytes, 12, 7).expect("timestamp");
    endian.put_u32(&mut bytes, 16, 0x12345).expect("current version");
    endian.put_u32(&mut bytes, 20, 0x23456).expect("compatibility");
    bytes[24..24 + path.len()].copy_from_slice(path.as_bytes());

    bytes
}

fn path(image: &MachO<'_>, bytes: &[u8]) -> String {
    let offset = image.endian.u32(bytes, 8).expect("offset") as usize;
    let end = bytes[offset..].iter().position(|byte| *byte == 0).expect("terminator");

    String::from_utf8(bytes[offset..offset + end].to_vec()).expect("path")
}

#[test]
fn library_ids_dependencies_versions_unknown_commands_and_rpaths_survive_all_formats() {
    for endian in [Endian::Little, Endian::Big] {
        for is_64 in [false, true] {
            let original = image(endian, is_64);
            let parsed = MachO::parse(&original).expect("Mach-O");
            let mut unknown = vec![0x55; 16];
            endian.put_u32(&mut unknown, 0, 0x42).expect("unknown kind");
            endian.put_u32(&mut unknown, 4, 16).expect("unknown size");

            let commands = [
                command(endian, is_64, LC_ID_DYLIB, "/old/libMine.dylib"),
                command(endian, is_64, LC_LOAD_DYLIB, "/Library/Frameworks/Kit.framework/Versions/A/Kit"),
                unknown.clone(),
            ];
            let original = parsed.rewrite_commands(&commands).expect("commands");
            let parsed = MachO::parse(&original).expect("Mach-O");
            let replacements = BTreeMap::from([
                ("libMine.dylib".into(), "@executable_path/Frameworks/libMine.dylib".into()),
                ("Kit.framework/Kit".into(), "@executable_path/Frameworks/Kit.framework/Kit".into()),
            ]);
            let additions = vec!["@executable_path/Frameworks/libInjected.dylib".into()];

            let edited = parsed.edit_libraries(&replacements, &additions).expect("edit");
            let parsed = MachO::parse(&edited).expect("edited Mach-O");

            assert_eq!(parsed.commands.len(), 5);
            assert_eq!(parsed.commands[2].bytes, unknown);
            assert_eq!(path(&parsed, parsed.commands[0].bytes), "@executable_path/Frameworks/libMine.dylib");
            assert_eq!(path(&parsed, parsed.commands[1].bytes), "@executable_path/Frameworks/Kit.framework/Kit");
            assert_eq!(parsed.endian.u32(parsed.commands[1].bytes, 12).expect("timestamp"), 7);
            assert_eq!(parsed.endian.u32(parsed.commands[1].bytes, 16).expect("version"), 0x12345);
            assert_eq!(parsed.endian.u32(parsed.commands[1].bytes, 20).expect("compatibility"), 0x23456);
            assert_eq!(parsed.commands[4].kind, LC_RPATH);
            assert_eq!(&edited[512..], &original[512..]);
            assert_eq!(parsed.edit_libraries(&replacements, &additions).expect("idempotent"), edited);
        }
    }
}

#[test]
fn malformed_rpath_and_insufficient_header_padding_are_errors() {
    let original = image(Endian::Little, true);
    let parsed = MachO::parse(&original).expect("Mach-O");
    let mut rpath = vec![0; 16];

    Endian::Little.put_u32(&mut rpath, 0, LC_RPATH).expect("kind");
    Endian::Little.put_u32(&mut rpath, 4, 16).expect("size");
    Endian::Little.put_u32(&mut rpath, 8, 16).expect("bad offset");

    let edited = parsed.rewrite_commands(&[rpath]).expect("replacement bytes");

    assert!(MachO::parse(&edited).is_err());

    let additions = vec![format!("@executable_path/{}", "x".repeat(1000))];

    assert!(parsed.edit_libraries(&BTreeMap::new(), &additions).is_err());
    assert!(parsed.edit_libraries(&BTreeMap::new(), &["bad\0path".into()]).is_err());
}
