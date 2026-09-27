use sl_bundle::{ArchiveLimits, BundleArchive, Control, Error, OutputLayout, PackOptions, PatchOptions, Replacement};
use std::{
    fs,
    io::{Cursor, Read, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

const INFO: &str = r#"<?xml version="1.0"?><plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.example.test</string>
<key>CFBundleExecutable</key><string>Test</string>
<key>CFBundleName</key><string>Test</string>
</dict></plist>"#;

fn fixture(path: &Path, prefix: &str, extras: &[(&str, &[u8])], links: &[(&str, &str)]) {
    let file = fs::File::create(path).expect("ZIP file");
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file(format!("{prefix}/Info.plist"), options).expect("plist entry");
    zip.write_all(INFO.as_bytes()).expect("plist");
    zip.start_file(format!("{prefix}/Test"), options.unix_permissions(0o755)).expect("executable");
    zip.write_all(&[0x55; 512]).expect("binary");

    for (name, bytes) in extras {
        zip.start_file(format!("{prefix}/{name}"), options).expect("resource entry");
        zip.write_all(bytes).expect("resource");
    }

    for (name, target) in links {
        zip.add_symlink(format!("{prefix}/{name}"), target, options).expect("symlink");
    }

    zip.finish().expect("finish ZIP");
}

#[test]
fn ipa_appzip_bare_app_and_flipped_inputs_preserve_content() {
    let temporary = tempfile::tempdir().expect("tempdir");

    for prefix in ["Payload/Test.app", "Test.app"] {
        let path = temporary.path().join("input.zip");
        fixture(&path, prefix, &[("日本語.txt", b"Unicode resource")], &[]);

        let archive = BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).expect("unpack");

        assert_eq!(fs::read(archive.bundle_path().join("日本語.txt")).expect("resource"), b"Unicode resource");
        assert_eq!(fs::read_to_string(archive.bundle_path().join("Info.plist")).expect("plist"), INFO);

        let bytes = fs::read(&path).expect("ZIP").into_iter().map(|byte| byte ^ 0xaa).collect::<Vec<_>>();
        fs::write(&path, bytes).expect("flipped ZIP");

        let flipped = BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).expect("unflip");

        assert_eq!(fs::read(flipped.bundle_path().join("Test")).expect("binary"), [0x55; 512]);
    }

    let path = temporary.path().join("Directory.app");
    fs::create_dir(&path).expect("app");
    fs::write(path.join("Info.plist"), INFO).expect("plist");
    fs::write(path.join("resource"), b"directory input").expect("resource");

    let archive = BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).expect("copy");

    assert_eq!(fs::read(archive.bundle_path().join("resource")).expect("resource"), b"directory input");
}

#[test]
fn forward_only_output_is_deterministic_and_zip64_roundtrips() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let path = temporary.path().join("input.ipa");
    fixture(&path, "Payload/Test.app", &[("resource", b"RESOURCE PAYLOAD")], &[("alias", "resource")]);

    let archive = BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).expect("unpack");

    for force_zip64 in [false, true] {
        let options = PackOptions { force_zip64, ..PackOptions::default() };
        let mut first = Vec::new();
        let mut second = Vec::new();

        let size = archive.write_to(&mut first, OutputLayout::Ipa, options, Control::default()).expect("pack");
        archive.write_to(&mut second, OutputLayout::Ipa, options, Control::default()).expect("repeat");

        assert_eq!(first, second);
        assert_eq!(size, first.len() as u64);

        let mut zip = ZipArchive::new(Cursor::new(&first)).expect("independent ZIP reader");
        let mut resource = Vec::new();
        zip.by_name("Payload/Test.app/resource").expect("entry").read_to_end(&mut resource).expect("CRC");

        assert_eq!(resource, b"RESOURCE PAYLOAD");
        assert_eq!(
            zip.by_name("Payload/Test.app/alias").expect("link").unix_mode().expect("mode") & 0o170000,
            0o120000
        );
        assert_eq!(zip.by_name("Payload/Test.app/Test").expect("binary").unix_mode().expect("mode") & 0o777, 0o755);

        let output = temporary.path().join("packed.ipa");
        fs::write(&output, &first).expect("output");

        let unpacked = BundleArchive::unpack(&output, ArchiveLimits::default(), Control::default()).expect("roundtrip");

        assert_eq!(fs::read(unpacked.bundle_path().join("alias")).expect("symlink"), b"RESOURCE PAYLOAD");

        #[cfg(unix)]
        {
            let status = std::process::Command::new("python3")
                .args(["-c", "import sys,zipfile; z=zipfile.ZipFile(sys.argv[1]); assert z.testzip() is None; assert z.read('Payload/Test.app/resource') == b'RESOURCE PAYLOAD'"])
                .arg(&output).status().expect("Python ZIP reader");

            assert!(status.success());

            let output = std::process::Command::new("unzip").arg("-t").arg(&output).output().expect("Info-ZIP");

            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
        }
    }
}

#[test]
fn rejects_escaping_colliding_and_symlink_traversing_names() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let path = temporary.path().join("input.ipa");

    for names in [
        vec![("../outside", &b"x"[..])],
        vec![("Folder/a", &b"a"[..]), ("folder/b", &b"b"[..])],
        vec![("é", &b"a"[..]), ("e\u{301}", &b"b"[..])],
        vec![("file", &b"a"[..]), ("file/child", &b"b"[..])],
    ] {
        fixture(&path, "Payload/Test.app", &names, &[]);

        assert!(BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).is_err());
    }

    fixture(&path, "Payload/Test.app", &[], &[("link", "../../../outside")]);

    assert!(BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).is_err());

    fixture(&path, "Payload/Test.app", &[("link/child", b"x")], &[("link", "somewhere")]);

    assert!(BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).is_err());
}

#[test]
fn duplicate_central_names_crc_damage_and_resource_limits_are_rejected() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let path = temporary.path().join("input.ipa");
    fixture(&path, "Payload/Test.app", &[("resourceA", b"RESOURCE PAYLOAD"), ("resourceB", b"b")], &[]);

    let original = fs::read(&path).expect("ZIP");
    let mut duplicate = original.clone();
    let name = b"Payload/Test.app/resourceB";

    for start in 0..duplicate.len() - name.len() + 1 {
        if &duplicate[start..start + name.len()] == name {
            duplicate[start + name.len() - 1] = b'A';
        }
    }

    fs::write(&path, duplicate).expect("duplicate");

    assert!(BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).is_err());

    let mut damaged = original.clone();
    let position = damaged
        .windows(b"RESOURCE PAYLOAD".len())
        .position(|bytes| bytes == b"RESOURCE PAYLOAD")
        .expect("stored payload");
    damaged[position] ^= 1;
    fs::write(&path, damaged).expect("CRC damage");

    assert!(BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).is_err());

    fs::write(&path, &original).expect("restore");
    let limits = ArchiveLimits { max_entries: 1, ..ArchiveLimits::default() };

    assert!(matches!(BundleArchive::unpack(&path, limits, Control::default()), Err(Error::Limit(_))));

    let limits = ArchiveLimits { max_total_bytes: 1, ..ArchiveLimits::default() };

    assert!(matches!(BundleArchive::unpack(&path, limits, Control::default()), Err(Error::Limit(_))));

    let limits = ArchiveLimits { max_directory_bytes: 1, ..ArchiveLimits::default() };

    assert!(matches!(BundleArchive::unpack(&path, limits, Control::default()), Err(Error::Limit(_))));

    for limits in [
        ArchiveLimits { max_path_depth: 2, ..ArchiveLimits::default() },
        ArchiveLimits { max_path_bytes: 5, ..ArchiveLimits::default() },
        ArchiveLimits { max_index_path_bytes: 1, ..ArchiveLimits::default() },
    ] {
        assert!(matches!(BundleArchive::unpack(&path, limits, Control::default()), Err(Error::Limit(_))));
    }

    let mut understated = original;
    let directory_record = understated.len() - 22;
    understated[directory_record + 12..directory_record + 16].copy_from_slice(&1u32.to_le_bytes());
    fs::write(&path, understated).expect("understated directory size");

    assert!(BundleArchive::unpack(&path, ArchiveLimits::default(), Control::default()).is_err());
}

#[test]
fn cancellation_and_failed_packing_leave_source_and_existing_output_intact() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let input = temporary.path().join("input.ipa");
    let output = temporary.path().join("output.ipa");
    fixture(&input, "Payload/Test.app", &[("large", &vec![0x5a; 400_000])], &[]);
    let original = fs::read(&input).expect("source");

    let cancelled = AtomicBool::new(false);
    let is_cancelled = || cancelled.load(Ordering::Relaxed);
    let progress = |event: sl_bundle::Progress| {
        if event.completed != 0 {
            cancelled.store(true, Ordering::Relaxed);
        }
    };
    let control = Control { is_cancelled: Some(&is_cancelled), on_progress: Some(&progress) };

    assert!(matches!(BundleArchive::unpack(&input, ArchiveLimits::default(), control), Err(Error::Cancelled)));
    assert_eq!(fs::read(&input).expect("unchanged source"), original);

    let archive = BundleArchive::unpack(&input, ArchiveLimits::default(), Control::default()).expect("unpack");
    fs::write(&output, b"previous output").expect("old output");
    cancelled.store(false, Ordering::Relaxed);

    assert!(matches!(archive.save(&output, OutputLayout::Ipa, PackOptions::default(), control), Err(Error::Cancelled)));
    assert_eq!(fs::read(&output).expect("old output"), b"previous output");

    archive.save(&output, OutputLayout::Ipa, PackOptions::default(), Control::default()).expect("save");

    assert!(ZipArchive::new(fs::File::open(&output).expect("output")).is_ok());
}

#[test]
fn readonly_directory_modes_survive_editing_and_repacking() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let input = temporary.path().join("readonly.ipa");
    let mut zip = ZipWriter::new(fs::File::create(&input).expect("ZIP"));
    let options = SimpleFileOptions::default();

    zip.start_file("Payload/Test.app/Info.plist", options).expect("info entry");
    zip.write_all(INFO.as_bytes()).expect("info");
    zip.start_file("Payload/Test.app/Test", options.unix_permissions(0o755)).expect("binary entry");
    zip.write_all(&[0x55; 512]).expect("binary");
    zip.add_directory("Payload/Test.app/ReadOnly/", options.unix_permissions(0o555)).expect("directory");
    zip.start_file("Payload/Test.app/ReadOnly/resource", options.unix_permissions(0o444)).expect("resource entry");
    zip.write_all(b"original").expect("resource");
    zip.finish().expect("ZIP finish");

    let source = temporary.path().join("replacement");
    fs::write(&source, b"edited").expect("replacement");
    let mut archive = BundleArchive::unpack(&input, ArchiveLimits::default(), Control::default()).expect("unpack");
    let options = PatchOptions {
        replacements: vec![Replacement { target: "ReadOnly/resource".into(), source: Some(source) }],
        ..PatchOptions::default()
    };

    archive.patch(&options, Control::default()).expect("edit inside readonly directory");
    let mut bytes = Vec::new();
    archive.write_to(&mut bytes, OutputLayout::Original, PackOptions::default(), Control::default()).expect("pack");
    let mut zip = ZipArchive::new(Cursor::new(bytes)).expect("ZIP reader");
    let mode = zip.by_name("Payload/Test.app/ReadOnly/").expect("directory").unix_mode().expect("mode");
    let mut resource = String::new();
    zip.by_name("Payload/Test.app/ReadOnly/resource")
        .expect("resource")
        .read_to_string(&mut resource)
        .expect("content");

    assert_eq!(mode & 0o777, 0o555);
    assert_eq!(resource, "edited");
}
#[test]
fn cancellation_interrupts_deflate_streams_that_produce_no_payload_bytes() {
    use std::sync::atomic::AtomicUsize;

    let temporary = tempfile::tempdir().expect("tempdir");
    let path = temporary.path().join("empty-blocks.ipa");
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    writer.start_file("Payload/Test.app/Info.plist", options).expect("info entry");
    writer.write_all(INFO.as_bytes()).expect("info");
    writer.start_file("Payload/Test.app/Test", options).expect("binary entry");
    writer.write_all(&[0x55; 512]).expect("binary");
    writer.start_file("Payload/Test.app/NoOp", options).expect("empty entry");

    let original = writer.finish().expect("finish").into_inner();
    let mut index = ZipArchive::new(Cursor::new(&original)).expect("ZIP index");
    let entry = index.by_name("Payload/Test.app/NoOp").expect("empty member");
    let local_offset = entry.header_start() as usize;
    let data_offset = entry.data_start() as usize;
    let old_size = entry.compressed_size() as usize;
    let central_offset = entry.central_header_start() as usize;
    drop(entry);

    // Independent raw DEFLATE fixture: non-final empty stored blocks, followed
    // by one final empty block. It is valid but emits no uncompressed bytes.
    let mut deflate = [0, 0, 0, 0xff, 0xff].repeat(2_000_000);
    deflate.extend([1, 0, 0, 0xff, 0xff]);
    let delta = deflate.len() - old_size;
    let mut bytes = original.clone();
    bytes.splice(data_offset..data_offset + old_size, deflate.iter().copied());
    bytes[local_offset + 18..local_offset + 22].copy_from_slice(&(deflate.len() as u32).to_le_bytes());
    let central_offset = central_offset + delta;
    bytes[central_offset + 20..central_offset + 24].copy_from_slice(&(deflate.len() as u32).to_le_bytes());

    let end = bytes.len() - 22;
    let old_directory =
        u32::from_le_bytes(original[original.len() - 6..original.len() - 2].try_into().expect("EOCD offset"));
    bytes[end + 16..end + 20].copy_from_slice(&(old_directory + delta as u32).to_le_bytes());
    fs::write(&path, &bytes).expect("fixture");

    let mut independent = ZipArchive::new(Cursor::new(&bytes)).expect("independent index");
    let mut decoded = Vec::new();
    independent
        .by_name("Payload/Test.app/NoOp")
        .expect("NoOp")
        .read_to_end(&mut decoded)
        .expect("valid DEFLATE and CRC");

    assert!(decoded.is_empty());

    let armed = AtomicBool::new(false);
    let checks = AtomicUsize::new(0);
    let cancelled = || armed.load(Ordering::Relaxed) && checks.fetch_add(1, Ordering::Relaxed) >= 128;
    let progress = |event: sl_bundle::Progress| {
        if event.phase == sl_bundle::Phase::Extract {
            armed.store(true, Ordering::Relaxed);
        }
    };
    let control = Control { is_cancelled: Some(&cancelled), on_progress: Some(&progress) };

    assert!(matches!(BundleArchive::unpack(&path, ArchiveLimits::default(), control), Err(Error::Cancelled)));
}
