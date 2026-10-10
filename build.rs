use std::{
    env,
    error::Error,
    path::{Path, PathBuf},
    process::Command,
};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=native/App.swift");
    println!("cargo:rerun-if-changed=native/Media.swift");
    let target_os = env::var("CARGO_CFG_TARGET_OS")
        .map_err(|error| format!("Could not read the Cargo target OS: {error}"))?;
    if target_os != "macos" {
        return Ok(());
    }
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH")
        .map_err(|error| format!("Could not read the Cargo target architecture: {error}"))?;
    let arch = match target_arch.as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => return Err(format!("Unsupported macOS architecture: {other}").into()),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("Cargo did not set OUT_DIR")?);
    let compiler = PathBuf::from(xcrun_stdout(
        &["--find", "swiftc"],
        "locate swiftc (install Xcode Command Line Tools with xcode-select --install)",
    )?);
    let swift_libraries = compiler
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("Invalid Swift compiler path: {}", compiler.display()))?
        .join("lib/swift/macosx");
    let target = format!("{arch}-apple-macosx13.0");
    let sdk = xcrun_stdout(&["--show-sdk-path"], "locate the macOS SDK")?;
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
            &sdk,
            "-O",
            "native/App.swift",
            "native/Media.swift",
            "-o",
        ])
        .arg(out.join("liboptic_native.a"))
        .status()
        .map_err(|error| format!("Could not run {}: {error}", compiler.display()))?;
    if !status.success() {
        return Err(format!("The macOS native bridge failed to compile: {status}").into());
    }
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
    Ok(())
}

fn xcrun_stdout(arguments: &[&str], purpose: &str) -> Result<String, Box<dyn Error>> {
    let output = Command::new("xcrun")
        .args(arguments)
        .output()
        .map_err(|error| format!("Could not {purpose}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Could not {purpose}: {}; {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("Could not decode xcrun output to {purpose}: {error}"))?;
    let stdout = stdout.trim();
    if stdout.is_empty() {
        return Err(format!("Could not {purpose}: xcrun returned an empty path").into());
    }
    Ok(stdout.to_owned())
}
