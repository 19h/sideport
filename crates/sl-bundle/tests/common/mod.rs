#![allow(dead_code)]

use plist::{Dictionary, Value};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

pub fn info(identifier: &str, executable: &str, package: &str) -> Dictionary {
    let mut info = Dictionary::new();
    info.insert("CFBundleIdentifier".into(), identifier.into());
    info.insert("CFBundleExecutable".into(), executable.into());
    info.insert("CFBundleName".into(), executable.into());
    info.insert("CFBundlePackageType".into(), package.into());
    info.insert("CFBundleVersion".into(), "1".into());
    info.insert("CFBundleShortVersionString".into(), "1.0".into());

    info
}

pub fn write_info(root: &Path, info: Dictionary) {
    fs::create_dir_all(root).expect("bundle directory");
    Value::Dictionary(info).to_file_xml(root.join("Info.plist")).expect("plist");
}

pub fn macho() -> Vec<u8> {
    let mut bytes = vec![0; 2048];
    let endian = sl_macho::Endian::Little;

    endian.put_u32(&mut bytes, 0, 0xfeedfacf).expect("magic");
    endian.put_u32(&mut bytes, 4, sl_macho::CPU_TYPE_ARM64).expect("CPU");
    endian.put_u32(&mut bytes, 12, 2).expect("file type");
    bytes[512..].fill(0x5a);

    bytes
}

pub fn synthetic_bundle(root: &Path, identifier: &str, executable: &str, package: &str) {
    write_info(root, info(identifier, executable, package));
    fs::write(root.join(executable), macho()).expect("binary");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(root.join(executable), fs::Permissions::from_mode(0o755)).expect("mode");
    }
}

pub fn run(command: &mut Command) -> Output {
    let output = command.output().unwrap_or_else(|error| panic!("{command:?}: {error}"));

    assert!(
        output.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    output
}

#[cfg(target_os = "macos")]
pub fn compile(source: &Path, destination: &Path, dylib: bool) {
    let mut command = Command::new("xcrun");
    command.args(["clang", "-arch", "arm64", "-arch", "x86_64", "-Wl,-no_adhoc_codesign", "-Wl,-headerpad,0x4000"]);

    if dylib {
        command.arg("-dynamiclib").args(["-install_name", "/usr/lib/libOriginal.dylib"]);
    }

    command.arg(source).arg("-o").arg(destination);
    run(&mut command);
}
