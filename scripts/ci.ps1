#requires -Version 7.0
<#
.SYNOPSIS
Applies supported fixes, then runs strict checks and reports every failure.
.DESCRIPTION
Requires Rust with Clippy and rustfmt. Full app coverage requires macOS and
Xcode 16 or newer (including swift-format). Logs and a JSON summary are written
under target/ci. Fixes remain in the working tree, even when validation fails.
.PARAMETER CheckOnly
Runs validation without changing source files.
.PARAMETER RequireMacOS
Fails on other platforms rather than accepting Rust-only coverage.
#>
[CmdletBinding()]
param(
    [switch] $CheckOnly,
    [switch] $RequireMacOS
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# Native failures belong in the summary; they must not stop later checks.
$PSNativeCommandUseErrorActionPreference = $false

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$reportDirectory = Join-Path $repositoryRoot ('target/ci/' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fff'))
$null = New-Item -ItemType Directory -Path $reportDirectory -Force
$results = [System.Collections.Generic.List[object]]::new()
$savedEnvironment = @{}
foreach ($name in @('PATH', 'RUSTFLAGS', 'RUSTDOCFLAGS', 'CARGO_BUILD_WARNINGS',
        'CARGO_ENCODED_RUSTFLAGS', 'CARGO_ENCODED_RUSTDOCFLAGS')) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

# Encoded flags take precedence in Cargo. Preserve them and append our policy
# there, so inherited flags cannot silently bypass validation.
function Get-BaseFlags {
    param([string] $EncodedName, [string] $PlainName)
    if ($savedEnvironment[$EncodedName]) {
        return $savedEnvironment[$EncodedName].Split([char]31)
    }
    return @([regex]::Split(($savedEnvironment[$PlainName] + '').Trim(), '\s+') | Where-Object { $_ })
}
$baseRustFlags = @(Get-BaseFlags -EncodedName 'CARGO_ENCODED_RUSTFLAGS' -PlainName 'RUSTFLAGS')
$baseRustdocFlags = @(Get-BaseFlags -EncodedName 'CARGO_ENCODED_RUSTDOCFLAGS' -PlainName 'RUSTDOCFLAGS')

function Set-CiRustFlags {
    param([string[]] $Flags)
    $env:CARGO_ENCODED_RUSTFLAGS = (@($baseRustFlags) + $Flags) -join [char]31
}

function Add-CiResult {
    param(
        [string] $Name,
        [string] $Phase,
        [string] $Status,
        [int] $ExitCode,
        [string] $Log,
        [string] $Detail
    )
    $results.Add([pscustomobject]@{
        Name = $Name
        Phase = $Phase
        Status = $Status
        ExitCode = $ExitCode
        Log = $Log
        Detail = $Detail
    })
}

function Invoke-CiCommand {
    param(
        [string] $Name,
        [string] $Phase,
        [string] $FilePath,
        [string[]] $Arguments
    )
    Write-Host "`n[$Phase] $Name"
    $log = Join-Path $reportDirectory ($Name + '.log')
    $code = 1
    $detail = ''
    try {
        & $FilePath @Arguments 2>&1 | Tee-Object -FilePath $log | Out-Host
        $code = $LASTEXITCODE
    }
    catch {
        $detail = $_.Exception.Message
        $detail | Tee-Object -FilePath $log -Append | Out-Host
    }
    $status = if ($code -eq 0) { 'Passed' } else { 'Failed' }
    Add-CiResult -Name $Name -Phase $Phase -Status $status -ExitCode $code -Log $log -Detail $detail
}

function Find-Cargo {
    $command = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }
    $cargoHome = $env:CARGO_HOME
    if (-not $cargoHome) {
        $cargoHome = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.cargo'
    }
    $executable = if ($IsWindows) { 'cargo.exe' } else { 'cargo' }
    $candidate = Join-Path $cargoHome "bin/$executable"
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
        return $candidate
    }
    return $null
}

# Restriction contains mutually conflicting policies, so enable specific safety
# rules instead of the whole group. Use identical lint policies for fix/check.
$clippyLints = @(
    'clippy::all',
    'clippy::pedantic',
    'clippy::nursery',
    'clippy::cargo',
    'clippy::allow_attributes_without_reason',
    'clippy::as_conversions',
    'clippy::dbg_macro',
    'clippy::expect_used',
    'clippy::panic',
    'clippy::todo',
    'clippy::undocumented_unsafe_blocks',
    'clippy::unimplemented',
    'clippy::unreachable',
    'clippy::unwrap_used'
)
$fixLints = @($clippyLints | ForEach-Object { '-W'; $_ })
$checkLints = @($clippyLints | ForEach-Object { '-D'; $_ }) + @('-D', 'warnings')
$cargoScope = @('--workspace', '--all-targets', '--all-features', '--locked')
$compilerLints = @('unused', 'future_incompatible', 'rust_2018_idioms', 'rust_2024_compatibility', 'unsafe_op_in_unsafe_fn')
$compilerFixFlags = @('-W', 'warnings') + @($compilerLints | ForEach-Object { '-W'; $_ })
$compilerCheckFlags = @($compilerLints | ForEach-Object { '-D'; $_ }) + @('-D', 'warnings')

Push-Location $repositoryRoot
try {
    $cargo = Find-Cargo
    if (-not $cargo) {
        Add-CiResult -Name 'rust-toolchain' -Phase 'Prerequisite' -Status 'Failed' -ExitCode 1 -Log '' `
            -Detail 'Install Rust with the clippy and rustfmt components (https://rustup.rs).'
    }
    else {
        # Cargo subcommands and rustup proxies also need the tool directory.
        $env:PATH = (Split-Path -Parent $cargo) + [IO.Path]::PathSeparator + $savedEnvironment['PATH']
        Invoke-CiCommand -Name 'cargo-version' -Phase 'Prerequisite' -FilePath $cargo -Arguments @('--version')
        Invoke-CiCommand -Name 'clippy-version' -Phase 'Prerequisite' -FilePath $cargo -Arguments @('clippy', '--version')
        Invoke-CiCommand -Name 'rustfmt-version' -Phase 'Prerequisite' -FilePath $cargo -Arguments @('fmt', '--version')
        Set-CiRustFlags -Flags $compilerFixFlags
        $env:CARGO_BUILD_WARNINGS = 'warn'
        if (-not $CheckOnly) {
            Invoke-CiCommand -Name 'rust-format-fix' -Phase 'Fix' -FilePath $cargo -Arguments @('fmt', '--all')
            Invoke-CiCommand -Name 'rust-compiler-fix' -Phase 'Fix' -FilePath $cargo `
                -Arguments (@('fix') + $cargoScope + @('--allow-dirty', '--allow-staged'))
            # RUSTFLAGS also covers build.rs; trailing cargo args cover targets.
            Set-CiRustFlags -Flags ($compilerFixFlags + $fixLints)
            Invoke-CiCommand -Name 'rust-clippy-fix' -Phase 'Fix' -FilePath $cargo `
                -Arguments (@('clippy', '--fix') + $cargoScope + @('--allow-dirty', '--allow-staged', '--') + $fixLints)
            Invoke-CiCommand -Name 'rust-format-after-fix' -Phase 'Fix' -FilePath $cargo -Arguments @('fmt', '--all')
        }

        Set-CiRustFlags -Flags $compilerCheckFlags
        $env:CARGO_ENCODED_RUSTDOCFLAGS = ($baseRustdocFlags + @('-D', 'warnings', '-D', 'rustdoc::all')) -join [char]31
        $env:CARGO_BUILD_WARNINGS = 'deny'
        Invoke-CiCommand -Name 'rust-format-check' -Phase 'Check' -FilePath $cargo -Arguments @('fmt', '--all', '--', '--check')
        Invoke-CiCommand -Name 'rust-compiler-check' -Phase 'Check' -FilePath $cargo -Arguments (@('check') + $cargoScope)
        Set-CiRustFlags -Flags ($compilerCheckFlags + $checkLints)
        Invoke-CiCommand -Name 'rust-clippy-check' -Phase 'Check' -FilePath $cargo `
            -Arguments (@('clippy') + $cargoScope + @('--') + $checkLints)
        # Tests and docs need compiler strictness, without Clippy-only policies.
        Set-CiRustFlags -Flags $compilerCheckFlags
        Invoke-CiCommand -Name 'rust-tests-debug' -Phase 'Check' -FilePath $cargo -Arguments (@('test') + $cargoScope)
        Invoke-CiCommand -Name 'rust-tests-release' -Phase 'Check' -FilePath $cargo -Arguments (@('test', '--release') + $cargoScope)
        Invoke-CiCommand -Name 'rust-docs' -Phase 'Check' -FilePath $cargo `
            -Arguments @('doc', '--workspace', '--all-features', '--locked', '--no-deps', '--document-private-items')
    }

    if ($IsMacOS) {
        # Swift 6 / Xcode 16 includes swift-format. Missing tools fail these steps.
        if (-not $CheckOnly) {
            Invoke-CiCommand -Name 'swift-format-fix' -Phase 'Fix' -FilePath 'xcrun' `
                -Arguments @('swift-format', 'format', '--in-place', '--recursive', 'native')
        }
        Invoke-CiCommand -Name 'swift-lint' -Phase 'Check' -FilePath 'xcrun' `
            -Arguments @('swift-format', 'lint', '--strict', '--recursive', 'native')
        $architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        $swiftArchitecture = switch ($architecture) {
            'Arm64' { 'arm64' }
            'X64' { 'x86_64' }
            default { $null }
        }
        if ($swiftArchitecture) {
            Invoke-CiCommand -Name 'swift-compiler-check' -Phase 'Check' -FilePath 'xcrun' `
                -Arguments @('swiftc', '-typecheck', '-parse-as-library', '-swift-version', '5',
                    '-module-name', 'OpticNative', '-target', "$swiftArchitecture-apple-macosx13.0",
                    '-warnings-as-errors', '-strict-concurrency=complete', 'native/App.swift', 'native/Media.swift')
        }
        else {
            Add-CiResult -Name 'swift-compiler-check' -Phase 'Check' -Status 'Failed' -ExitCode 1 -Log '' `
                -Detail "Unsupported macOS architecture: $architecture"
        }
        Invoke-CiCommand -Name 'native-plist' -Phase 'Check' -FilePath 'plutil' -Arguments @('-lint', 'native/Info.plist')
        if ($cargo) {
            Invoke-CiCommand -Name 'media-self-test' -Phase 'Check' -FilePath $cargo `
                -Arguments @('run', '--locked', '--all-features', '--', '--self-test', (Join-Path $reportDirectory 'self-test.mp4'))
        }
    }
    else {
        $status = if ($RequireMacOS) { 'Failed' } else { 'Skipped' }
        $code = if ($RequireMacOS) { 1 } else { 0 }
        $detail = 'macOS app Rust, Swift, plist, and media checks require a macOS runner with Xcode 16+.'
        Add-CiResult -Name 'macos-app-checks' -Phase 'Coverage' -Status $status -ExitCode $code -Log '' -Detail $detail
        Write-Host "`n$status`: $detail"
    }
}
catch {
    Add-CiResult -Name 'ci-script' -Phase 'Infrastructure' -Status 'Failed' -ExitCode 1 -Log '' -Detail $_.Exception.Message
}
finally {
    Pop-Location
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
}

$summaryPath = Join-Path $reportDirectory 'summary.json'
ConvertTo-Json -InputObject @($results.ToArray()) -Depth 4 | Set-Content -LiteralPath $summaryPath -Encoding utf8
$results | Format-Table Name, Phase, Status, ExitCode -AutoSize | Out-Host
foreach ($result in $results) {
    if ($result.Detail) {
        Write-Host "$($result.Name): $($result.Detail)"
    }
}
Write-Host "Logs and summary: $summaryPath"
$failures = @($results | Where-Object Status -EQ 'Failed')
if ($failures.Count -gt 0) {
    Write-Host "CI failed: $($failures.Count) step(s). Applied fixes remain available for review."
    exit 1
}
Write-Host 'CI passed for the reported coverage.'
exit 0
