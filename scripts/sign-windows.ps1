#requires -Version 7.0
<# Signs and verifies one artifact. The private key stays in CurrentUser\My. #>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $FilePath,
    [Parameter(Mandatory)] [ValidatePattern('^[A-Fa-f0-9]{40}$')] [string] $CertificateThumbprint,
    [Parameter(Mandatory)] [string] $TimestampUrl,
    [string] $SignToolPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Windows Authenticode signing requires Windows.' }
$timestamp = [uri] $TimestampUrl
if (-not $timestamp.IsAbsoluteUri -or $timestamp.Scheme -notin @('http', 'https')) {
    throw 'TimestampUrl must be an absolute HTTP or HTTPS RFC 3161 timestamp URL.'
}
$certificate = Get-Item -LiteralPath "Cert:/CurrentUser/My/$CertificateThumbprint"
if (-not $certificate.HasPrivateKey -or $certificate.NotBefore -gt (Get-Date) -or $certificate.NotAfter -le (Get-Date)) {
    throw 'The selected certificate must have a private key and be currently valid.'
}
if ('1.3.6.1.5.5.7.3.3' -notin @($certificate.EnhancedKeyUsageList | ForEach-Object { $_.ObjectId })) {
    throw 'The selected certificate must permit code signing.'
}
if (-not $SignToolPath) {
    $command = Get-Command signtool.exe -CommandType Application -ErrorAction SilentlyContinue
    if ($command) { $SignToolPath = $command.Source }
    else {
        $sdk = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits/10/bin'
        $SignToolPath = Get-ChildItem -LiteralPath $sdk -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^10\.\d+\.\d+\.\d+$' } |
            Sort-Object { [version] $_.Name } -Descending |
            ForEach-Object { Join-Path $_.FullName 'x64/signtool.exe' } |
            Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
            Select-Object -First 1
    }
}
if (-not $SignToolPath -or -not (Test-Path -LiteralPath $SignToolPath -PathType Leaf)) {
    throw 'Install the Windows SDK signing tools, or supply -SignToolPath.'
}
$artifact = (Resolve-Path -LiteralPath $FilePath).Path
& $SignToolPath sign /sha1 $CertificateThumbprint /s My /fd SHA256 /tr $TimestampUrl /td SHA256 $artifact
if ($LASTEXITCODE -ne 0) { throw "Signing failed with exit code $LASTEXITCODE." }
& $SignToolPath verify /pa /all /tw $artifact
if ($LASTEXITCODE -ne 0) { throw "Signature verification failed with exit code $LASTEXITCODE." }
