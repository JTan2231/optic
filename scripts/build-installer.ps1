#requires -Version 7.0
<#
.SYNOPSIS
Builds the Windows app and dist/OpticSetup.exe with Inno Setup 6.4 or newer.
.DESCRIPTION
For a signed release supply a CurrentUser\My code-signing certificate thumbprint
and RFC 3161 timestamp URL. App, setup, and uninstaller are signed and verified.
Use -RequireSigning in release jobs to fail rather than emit an unsigned build.
#>
[CmdletBinding()]
param(
    [string] $IsccPath,
    [switch] $SkipAppBuild,
    [ValidatePattern('^[A-Fa-f0-9]{40}$')] [string] $CertificateThumbprint,
    [string] $TimestampUrl,
    [string] $SignToolPath,
    [switch] $RequireSigning
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'The Optic installer requires Windows.' }
if ($RequireSigning -and -not $CertificateThumbprint) {
    throw 'A signed release requires -CertificateThumbprint and -TimestampUrl.'
}
if ($CertificateThumbprint -and -not $TimestampUrl) { throw 'Signing requires -TimestampUrl.' }
if (-not $CertificateThumbprint -and ($TimestampUrl -or $SignToolPath)) {
    throw 'Specify -CertificateThumbprint to enable signing.'
}
$repositoryRoot = Split-Path -Parent $PSScriptRoot
if (-not $IsccPath) { $IsccPath = $env:OPTIC_ISCC }
if (-not $IsccPath) {
    $command = Get-Command ISCC.exe -CommandType Application -ErrorAction SilentlyContinue
    if ($command) { $IsccPath = $command.Source }
    else {
        $candidates = @(
            (Join-Path $repositoryRoot 'target/tools/inno-setup/ISCC.exe'),
            (Join-Path $env:LOCALAPPDATA 'Programs/Inno Setup 7/ISCC.exe'),
            (Join-Path $env:LOCALAPPDATA 'Programs/Inno Setup 6/ISCC.exe'),
            (Join-Path $env:ProgramFiles 'Inno Setup 7/ISCC.exe'),
            (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 7/ISCC.exe'),
            (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6/ISCC.exe')
        )
        $IsccPath = $candidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
    }
}
if (-not $IsccPath -or -not (Test-Path -LiteralPath $IsccPath -PathType Leaf)) {
    throw 'Install Inno Setup 6.4+ from https://jrsoftware.org/isdl.php, or supply -IsccPath / OPTIC_ISCC.'
}
$IsccPath = (Resolve-Path -LiteralPath $IsccPath).Path
# The script checks the compiler's VER preprocessor constant. ISCC's Windows
# file version is 0.0.0.0 in official releases, so it is not a version check.
if (-not $SkipAppBuild) {
    & (Join-Path $PSScriptRoot 'build-app.ps1')
}
$executable = Join-Path $repositoryRoot 'dist/Optic.exe'
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw 'Build dist/Optic.exe first, or omit -SkipAppBuild.'
}
$manifest = Get-Content -LiteralPath (Join-Path $repositoryRoot 'Cargo.toml') -Raw
$versionMatch = [regex]::Match($manifest, '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"\s*$')
if (-not $versionMatch.Success) { throw 'Cargo.toml must contain a numeric major.minor.patch version.' }
$appVersion = $versionMatch.Groups[1].Value
if ((Get-Item -LiteralPath $executable).VersionInfo.ProductVersion -ne $appVersion) {
    throw 'Optic.exe and Cargo.toml versions differ. Update the Windows resources and rebuild.'
}
$compilerArguments = @(
    "/DAppVersion=$appVersion",
    "/DSourceExe=$executable",
    "/DOutputDir=$(Join-Path $repositoryRoot 'dist')"
)
if ($CertificateThumbprint) {
    $signScript = Join-Path $PSScriptRoot 'sign-windows.ps1'
    $signParameters = @{
        CertificateThumbprint = $CertificateThumbprint
        TimestampUrl = $TimestampUrl
    }
    if ($SignToolPath) { $signParameters.SignToolPath = $SignToolPath }
    & $signScript -FilePath $executable @signParameters
    # Inno substitutes $q for quotes and $f for the quoted artifact filename.
    # Escape literal $ before passing user paths through Inno's substitution.
    function Quote-InnoArgument([string] $Value) {
        if ($Value.Contains('"') -or $Value.Contains("`r") -or $Value.Contains("`n")) {
            throw 'Signing arguments cannot contain quotes or line breaks.'
        }
        return '$q' + $Value.Replace('$', '$$') + '$q'
    }
    $shell = (Get-Process -Id $PID).Path
    $signCommand = (Quote-InnoArgument $shell) + ' -NoProfile -File ' +
        (Quote-InnoArgument $signScript) + ' -FilePath $f -CertificateThumbprint ' +
        $CertificateThumbprint + ' -TimestampUrl ' + (Quote-InnoArgument $TimestampUrl)
    if ($SignToolPath) { $signCommand += ' -SignToolPath ' + (Quote-InnoArgument $SignToolPath) }
    $compilerArguments += @('/DSignTool=optic', "/Soptic=$signCommand")
}
else { Write-Host 'Building an unsigned installer for local testing.' }
$compilerArguments += Join-Path $repositoryRoot 'installer/Optic.iss'
& $IsccPath @compilerArguments
if ($LASTEXITCODE -ne 0) { throw "Installer compilation failed with exit code $LASTEXITCODE." }
$setup = Join-Path $repositoryRoot 'dist/OpticSetup.exe'
if ($CertificateThumbprint) {
    # The Inno signing hook verifies each embedded uninstaller and the setup.
    $signature = Get-AuthenticodeSignature -LiteralPath $setup
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Thumbprint -ne $CertificateThumbprint) {
        throw 'The final installer does not have the expected valid publisher signature.'
    }
}
Get-FileHash -LiteralPath $executable, $setup -Algorithm SHA256 | Format-Table -AutoSize
Write-Host "Built $setup"
