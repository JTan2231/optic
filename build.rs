use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=native/App.swift");
    println!("cargo:rerun-if-changed=native/Media.swift");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => panic!("Unsupported macOS architecture: {other}"),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let compiler = Command::new("xcrun")
        .args(["--find", "swiftc"])
        .output()
        .expect("Xcode Command Line Tools are required (xcode-select --install)");
    assert!(compiler.status.success(), "Could not locate swiftc");
    let compiler = PathBuf::from(String::from_utf8(compiler.stdout).unwrap().trim());
    let swift_libraries = compiler
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("lib/swift/macosx");
    let target = format!("{arch}-apple-macosx13.0");
    let sdk = Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .expect("Could not locate the macOS SDK");
    assert!(sdk.status.success(), "Could not locate the macOS SDK");
    let sdk = String::from_utf8(sdk.stdout).unwrap();
    let status = Command::new(&compiler)
        .args([
            "-emit-library",
            "-static",
            "-parse-as-library",
            "-swift-version",
            "5",
            "-module-name",
            "OpticNative",
            "-target",
            &target,
            "-sdk",
            sdk.trim(),
            "-O",
            "native/App.swift",
            "native/Media.swift",
            "-o",
        ])
        .arg(out.join("liboptic_native.a"))
        .status()
        .expect("Could not run swiftc");
    assert!(
        status.success(),
        "The macOS native bridge failed to compile"
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!(
        "cargo:rustc-link-search=native={}",
        swift_libraries.display()
    );
    println!("cargo:rustc-link-lib=static=optic_native");
    for framework in [
        "AppKit",
        "Carbon",
        "ScreenCaptureKit",
        "VideoToolbox",
        "AVFoundation",
        "CoreMedia",
        "CoreVideo",
        "Foundation",
        "CoreGraphics",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rustc-link-arg=-mmacosx-version-min=13.0");
}
