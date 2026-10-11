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
    if target_os == "windows" {
        return build_windows();
    }
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
        "AudioToolbox",
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

fn build_windows() -> Result<(), Box<dyn Error>> {
    if env::var("CARGO_CFG_TARGET_ARCH")? != "x86_64" || env::var("CARGO_CFG_TARGET_ENV")? != "msvc"
    {
        return Err("Windows builds require the x86_64-pc-windows-msvc Rust target".into());
    }
    for file in ["Optic.rc", "Optic.manifest", "Optic.ico"] {
        println!("cargo:rerun-if-changed=native/windows/{file}");
    }
    println!("cargo:rerun-if-env-changed=OPTIC_VCVARSALL");
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("Cargo did not set OUT_DIR")?);
    let environment = windows_environment(&out)?;
    let resource = out.join("Optic.res");
    let mut resources = Command::new(windows_tool("rc.exe", &environment)?);
    resources.envs(environment.iter().map(|(name, value)| (name, value)));
    resources
        .args(["/nologo", "/I", "native/windows", "/fo"])
        .arg(&resource)
        .arg("native/windows/Optic.rc");
    run_native(&mut resources, "compile Windows application resources")?;
    // Include the icon/manifest in both the application and Rust UI test harness.
    println!("cargo:rustc-link-arg={}", resource.display());
    Ok(())
}
fn run_native(command: &mut Command, purpose: &str) -> Result<(), Box<dyn Error>> {
    let status = command
        .status()
        .map_err(|error| format!("Could not {purpose}: {error}"))?;
    if !status.success() {
        return Err(format!("Could not {purpose}: {status}").into());
    }
    Ok(())
}

fn windows_environment(out: &Path) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let vcvars = if let Some(path) = env::var_os("OPTIC_VCVARSALL") {
        PathBuf::from(path)
    } else {
        let program_files =
            env::var_os("ProgramFiles(x86)").ok_or("Could not locate Visual Studio Installer")?;
        let vswhere =
            PathBuf::from(program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
        let output = Command::new(vswhere)
            .args([
                "-all",
                "-latest",
                "-products",
                "*",
                "-property",
                "installationPath",
            ])
            .output()
            .map_err(|error| {
                format!("Install the Visual Studio 2022 MSVC linker and Windows SDK: {error}")
            })?;
        let installation = String::from_utf8(output.stdout)?;
        let installation = installation.trim();
        if !output.status.success() || installation.is_empty() {
            return Err("Install the Visual Studio 2022 MSVC linker and Windows SDK".into());
        }
        PathBuf::from(installation).join("VC/Auxiliary/Build/vcvarsall.bat")
    };
    if !vcvars.is_file() {
        return Err(format!("Could not locate {}", vcvars.display()).into());
    }
    let script = out.join("optic-toolchain.cmd");
    // The generated batch file only reads toolchain environment. Reject shell
    // metacharacters in the override before interpolating its quoted path.
    let vcvars = vcvars
        .to_str()
        .ok_or("Invalid Visual Studio toolchain path")?;
    if vcvars.contains(['"', '%', '\r', '\n']) {
        return Err("Invalid Visual Studio toolchain path".into());
    }
    std::fs::write(
        &script,
        format!("@echo off\r\nchcp 65001 >nul\r\ncall \"{vcvars}\" x64 >nul\r\nif errorlevel 1 exit /b 1\r\nset\r\n"),
    )?;
    let output = Command::new("cmd.exe")
        .args(["/d", "/c"])
        .arg(&script)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Could not initialize MSVC: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let environment = String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once('=')?;
            if name.is_empty() {
                None
            } else {
                Some((name.to_owned(), value.to_owned()))
            }
        })
        .collect();
    Ok(environment)
}

fn windows_tool(name: &str, environment: &[(String, String)]) -> Result<PathBuf, Box<dyn Error>> {
    let path = environment
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
        .ok_or("MSVC did not set PATH")?;
    env::split_paths(&path.1)
        .map(|folder| folder.join(name))
        .find(|tool| tool.is_file())
        .ok_or_else(|| {
            format!("Could not locate {name}; install the MSVC linker and Windows SDK").into()
        })
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
