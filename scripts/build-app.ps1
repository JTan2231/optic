#requires -Version 7.0
[CmdletBinding()]
param()
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Use build-app.sh on macOS. This script builds the Windows x64 app.' }
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$cargo = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue
$cargoPath = if ($cargo) { $cargo.Source } else {
    Join-Path ([Environment]::GetFolderPath('UserProfile')) '.cargo/bin/cargo.exe'
}
if (-not (Test-Path -LiteralPath $cargoPath -PathType Leaf)) { throw 'Install Rust from https://rustup.rs.' }
$savedPath = $env:PATH
$savedFlags = $env:CARGO_ENCODED_RUSTFLAGS
# Statically link the MSVC C runtime in the Rust executable.
$baseFlags = if ($savedFlags) { @($savedFlags.Split([char]31)) } else {
    @([regex]::Split(($env:RUSTFLAGS + '').Trim(), '\s+') | Where-Object { $_ })
}
$env:CARGO_ENCODED_RUSTFLAGS = ($baseFlags + @('-C', 'target-feature=+crt-static')) -join [char]31
$env:PATH = (Split-Path -Parent $cargoPath) + [IO.Path]::PathSeparator + $savedPath
Push-Location $repositoryRoot
try {
    & $cargoPath build --release --locked --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw "Windows build failed with exit code $LASTEXITCODE." }
    $destination = Join-Path $repositoryRoot 'dist'
    $null = New-Item -ItemType Directory -Path $destination -Force
    $executable = Join-Path $destination 'Optic.exe'
    Copy-Item -LiteralPath 'target/x86_64-pc-windows-msvc/release/optic.exe' -Destination $executable
    Write-Host "Built $executable"
    Write-Host "Launch with: & '$executable'"
}
finally {
    Pop-Location
    $env:PATH = $savedPath
    $env:CARGO_ENCODED_RUSTFLAGS = $savedFlags
}
