use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=anisette-bridge/Package.swift");
    println!("cargo:rerun-if-changed=anisette-bridge/Package.resolved");
    println!("cargo:rerun-if-changed=anisette-bridge/Sources/SideportAnisetteBridge/Bridge.swift");

    if !env::var("TARGET").is_ok_and(|target| target.contains("apple-darwin")) {
        return;
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("build output directory"));
    let package = manifest.join("anisette-bridge");
    let scratch = output.join("swift");

    let status = Command::new("swift")
        .args(["build", "-c", "release", "--product", "SideportAnisetteBridge", "--package-path"])
        .arg(&package)
        .arg("--scratch-path")
        .arg(&scratch)
        .status()
        .expect("Swift is required to build local anisette");

    assert!(status.success(), "failed to build the in-process local anisette bridge");

    let dylib = scratch.join("release/libSideportAnisetteBridge.dylib");
    assert!(dylib.is_file(), "Swift did not produce the local anisette bridge");

    println!("cargo:rustc-env=SIDEPORT_ANISETTE_BRIDGE={}", dylib.display());
}
