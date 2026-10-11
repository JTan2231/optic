#requires -Version 7.0
<#
.SYNOPSIS
Runs opt-in Windows installer smoke checks without capturing the screen.
.DESCRIPTION
Temporarily creates real per-user Installed apps, Start menu, and startup entries.
Refuses to run if Optic is already installed, running, or has these entries.
Installs under target/installer-smoke, then uninstalls. Logs and a synthetic MP4
fixture remain there for review. The Videos recording folder is never touched.
#>
[CmdletBinding()]
param([string] $InstallerPath)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Installer smoke checks require Windows.' }
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
try {
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if ($identity.IsSystem -or $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run installer smoke checks in an unelevated user PowerShell session.'
    }
    $userSid = $identity.User.Value
}
finally { $identity.Dispose() }
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$uninstallKeyName = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{743C904A-A9C6-4BFD-A7C9-A9440E714ACD}_is1'
$runKeyName = 'Software\Microsoft\Windows\CurrentVersion\Run'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Optic.lnk'
$portableExe = Join-Path $repositoryRoot 'dist/Optic.exe'
if (-not $InstallerPath) { $InstallerPath = Join-Path $repositoryRoot 'dist/OpticSetup.exe' }
$InstallerPath = (Resolve-Path -LiteralPath $InstallerPath).Path
if (-not (Test-Path -LiteralPath $portableExe -PathType Leaf)) { throw 'Build dist/Optic.exe first.' }
$sourceHash = (Get-FileHash -LiteralPath $portableExe -Algorithm SHA256).Hash
$expectedVersion = (Get-Item -LiteralPath $portableExe).VersionInfo.ProductVersion
$registryView = [Microsoft.Win32.RegistryView]::Registry64

function Get-InstallRecord {
    param([Microsoft.Win32.RegistryView] $View = $registryView)
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $View)
    try {
        $key = $base.OpenSubKey($uninstallKeyName)
        if ($key) {
            try {
                return [pscustomobject]@{
                    InstallLocation = $key.GetValue('InstallLocation')
                    DisplayVersion = $key.GetValue('DisplayVersion')
                }
            }
            finally { $key.Dispose() }
        }
    }
    finally { $base.Dispose() }
}

function Get-StartupValue {
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::CurrentUser, $registryView)
    try {
        $key = $base.OpenSubKey($runKeyName)
        if ($key) {
            try {
                if ($key.GetValueNames() -contains 'Optic') { return $key.GetValue('Optic') }
            }
            finally { $key.Dispose() }
        }
    }
    finally { $base.Dispose() }
}

foreach ($view in @([Microsoft.Win32.RegistryView]::Registry64, [Microsoft.Win32.RegistryView]::Registry32)) {
    if (Get-InstallRecord -View $view) { throw 'Optic is already installed. Use a clean Windows user for these smoke checks.' }
}
if ($null -ne (Get-StartupValue) -or (Test-Path -LiteralPath $shortcut)) {
    throw 'An Optic startup value or Start menu shortcut already exists. Use a clean Windows user for these smoke checks.'
}
if (Get-Process -Name Optic, OpticMaintenance -ErrorAction SilentlyContinue) {
    throw 'Optic is running. Quit it before running these smoke checks.'
}
# Copies renamed by the user still share Optic's SID-scoped instance identity.
$instanceName = 'Local\Optic.Instance.' + $userSid
$existingInstance = $null
try {
    try { $existingInstance = [Threading.Mutex]::OpenExisting($instanceName) }
    catch [Threading.WaitHandleCannotBeOpenedException] { }
    if ($existingInstance) { throw 'An Optic instance already exists. Quit it before running these smoke checks.' }
}
finally { if ($existingInstance) { $existingInstance.Dispose() } }
if (-not ('Optic.InstallerSmoke.NativeMethods' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace Optic.InstallerSmoke {
    public static class NativeMethods {
        [DllImport("user32.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
        public static extern IntPtr FindWindowW(string className, string windowName);
    }
}
'@
}
foreach ($title in @($instanceName, 'Optic')) {
    if ([Optic.InstallerSmoke.NativeMethods]::FindWindowW('Optic.TrayWindow', $title) -ne [IntPtr]::Zero) {
        throw 'An Optic tray endpoint already exists. Quit it before running these smoke checks.'
    }
}

$smokeRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'target/installer-smoke'))
$workspace = [IO.Path]::GetFullPath((Join-Path $smokeRoot ((Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N').Substring(0, 8))))
if (-not $workspace.StartsWith($smokeRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'The smoke workspace must be inside target/installer-smoke.'
}
$null = New-Item -ItemType Directory -Path $workspace -Force
$installDirectory = Join-Path $workspace 'app'
$installedExe = Join-Path $installDirectory 'Optic.exe'
$uninstaller = Join-Path $installDirectory 'unins000.exe'
$clip = Join-Path $installDirectory 'preserved-self-test.mp4'
$script:logIndex = 0
$ownedProcesses = [System.Collections.Generic.List[System.Diagnostics.Process]]::new()
$transcript = Join-Path $workspace 'smoke.log'
Start-Transcript -LiteralPath $transcript | Out-Null

function Start-HiddenProcess {
    param([string] $FilePath, [string[]] $Arguments = @())
    # These controlled arguments have no embedded quotes or trailing backslash.
    $quoted = foreach ($argument in $Arguments) {
        if ($argument.Contains('"') -or $argument.EndsWith('\')) {
            throw 'Smoke process arguments cannot contain quotes or end with a backslash.'
        }
        '"' + $argument + '"'
    }
    $parameters = @{ FilePath = $FilePath; PassThru = $true; WindowStyle = 'Hidden' }
    if ($quoted) { $parameters.ArgumentList = $quoted -join ' ' }
    $process = Start-Process @parameters
    $ownedProcesses.Add($process)
    return $process
}

function Wait-Success {
    param([System.Diagnostics.Process] $Process, [int] $TimeoutSeconds = 150)
    if (-not $Process.WaitForExit($TimeoutSeconds * 1000)) {
        throw "Process $($Process.Id) did not exit within $TimeoutSeconds seconds. It was left running."
    }
    $Process.Refresh()
    if ($Process.ExitCode -ne 0) { throw "Process $($Process.Id) returned exit code $($Process.ExitCode)." }
}

function Invoke-Setup {
    param([string[]] $AdditionalArguments = @())
    $script:logIndex++
    $log = Join-Path $workspace ('setup-' + $script:logIndex + '.log')
    $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/DIR=$installDirectory", "/LOG=$log") + $AdditionalArguments
    Wait-Success (Start-HiddenProcess -FilePath $InstallerPath -Arguments $arguments)
    Assert-Installed
}

function Assert-Installed {
    if (-not (Test-Path -LiteralPath $installedExe -PathType Leaf)) { throw 'The installed executable is missing.' }
    if ((Get-FileHash -LiteralPath $installedExe -Algorithm SHA256).Hash -ne $sourceHash) {
        throw 'The installed executable differs from dist/Optic.exe.'
    }
    $record = Get-InstallRecord
    if (-not $record -or -not [string]::Equals(([string] $record.InstallLocation).TrimEnd('\'), $installDirectory, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The Installed apps entry has an unexpected installation directory.'
    }
    if ($record.DisplayVersion -ne $expectedVersion) { throw 'The Installed apps version differs from Optic.exe.' }
    if (-not (Test-Path -LiteralPath $shortcut -PathType Leaf)) { throw 'The Start menu shortcut is missing.' }
    $shell = New-Object -ComObject WScript.Shell
    try {
        $link = $shell.CreateShortcut($shortcut)
        try {
            if (-not [string]::Equals($link.TargetPath, $installedExe, [StringComparison]::OrdinalIgnoreCase)) {
                throw 'The Start menu shortcut points to an unexpected executable.'
            }
        }
        finally { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($link) }
    }
    finally { $null = [Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) }
}

function Assert-Startup {
    param([bool] $Enabled)
    $value = Get-StartupValue
    if ($Enabled) {
        if ($value -ne ('"' + $installedExe + '"')) { throw 'The startup entry does not point to the quoted installed executable.' }
    }
    elseif ($null -ne $value) { throw 'Startup should be disabled, but its Run value still exists.' }
}

function Start-TestApp {
    param([string] $ExePath = $installedExe)
    $process = Start-HiddenProcess -FilePath $ExePath
    if (-not $process.WaitForInputIdle(10000)) { throw 'Optic did not initialize its tray message loop.' }
    $process.Refresh()
    if ($process.HasExited) { throw 'Optic exited instead of staying in the tray.' }
    return $process
}

function Invoke-Uninstall {
    Wait-Success (Start-HiddenProcess -FilePath $uninstaller -Arguments @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=$(Join-Path $workspace 'uninstall.log')"))
    # Inno's temporary uninstaller copy can finish self-deletion after the
    # original process exits. Wait for that cleanup before checking its files.
    $cleanupWait = [Diagnostics.Stopwatch]::StartNew()
    while ((Test-Path -LiteralPath $installedExe) -or (Test-Path -LiteralPath $uninstaller)) {
        if ($cleanupWait.ElapsedMilliseconds -ge 10000) {
            throw 'The application or uninstaller is still present ten seconds after uninstall exited. No process was forcibly terminated.'
        }
        Start-Sleep -Milliseconds 100
    }
}

try {
    Write-Host "Smoke workspace: $workspace"
    # First install must close an owned portable copy, even when {app} is absent.
    $portable = Start-TestApp -ExePath $portableExe
    Invoke-Setup
    Wait-Success -Process $portable -TimeoutSeconds 10
    Assert-Startup -Enabled $false
    Write-Host 'PASS: first install closes portable Optic; app, Installed apps, shortcut, and default startup state.'

    # Synthetic media only: no desktop capture or changes to the Videos folder.
    Wait-Success (Start-HiddenProcess -FilePath $installedExe -Arguments @('--self-test', $clip)) -TimeoutSeconds 90
    if (-not (Test-Path -LiteralPath $clip -PathType Leaf) -or (Get-Item -LiteralPath $clip).Length -eq 0) {
        throw 'The synthetic MP4 fixture was not created.'
    }
    $clipHash = (Get-FileHash -LiteralPath $clip -Algorithm SHA256).Hash

    $app = Start-TestApp
    $duplicate = Start-HiddenProcess -FilePath $installedExe
    Wait-Success -Process $duplicate -TimeoutSeconds 10
    $app.Refresh()
    if ($app.HasExited) { throw 'A duplicate launch closed the original tray process.' }
    Wait-Success (Start-HiddenProcess -FilePath $installedExe -Arguments @('--shutdown', '10')) -TimeoutSeconds 15
    Wait-Success -Process $app -TimeoutSeconds 10
    Write-Host 'PASS: tray launch, duplicate suppression, and explicit graceful shutdown.'

    $app = Start-TestApp
    Invoke-Setup -AdditionalArguments @('/TASKS=startup')
    Wait-Success -Process $app -TimeoutSeconds 10
    Assert-Startup -Enabled $true
    Invoke-Setup
    Assert-Startup -Enabled $true
    Invoke-Setup -AdditionalArguments @('/TASKS=')
    Assert-Startup -Enabled $false
    Invoke-Setup
    Assert-Startup -Enabled $false
    if ((Get-FileHash -LiteralPath $clip -Algorithm SHA256).Hash -ne $clipHash) { throw 'An upgrade changed the synthetic clip.' }
    Write-Host 'PASS: running-app upgrade and remembered startup opt-in and opt-out.'

    $app = Start-TestApp
    Invoke-Uninstall
    Wait-Success -Process $app -TimeoutSeconds 10
    if ((Test-Path -LiteralPath $installedExe) -or (Test-Path -LiteralPath $uninstaller)) {
        throw 'Uninstall did not remove the application and uninstaller.'
    }
    foreach ($view in @([Microsoft.Win32.RegistryView]::Registry64, [Microsoft.Win32.RegistryView]::Registry32)) {
        if (Get-InstallRecord -View $view) { throw 'The Installed apps entry remains after uninstall.' }
    }
    if (Test-Path -LiteralPath $shortcut) { throw 'The Start menu shortcut remains after uninstall.' }
    Assert-Startup -Enabled $false
    if (-not (Test-Path -LiteralPath $clip) -or (Get-FileHash -LiteralPath $clip -Algorithm SHA256).Hash -ne $clipHash) {
        throw 'Uninstall deleted or changed an untracked synthetic clip.'
    }
    Write-Host 'PASS: running-app uninstall removes integration and preserves an untracked synthetic clip.'
    Write-Host 'The Videos recording folder was never touched. These checks do not exercise active screen capture or a save in progress.'
}
finally {
    # Clean up only the install created in this exact workspace. Keep logs/clip.
    $record = Get-InstallRecord
    if ($record -and [string]::Equals(([string] $record.InstallLocation).TrimEnd('\'), $installDirectory, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $uninstaller -PathType Leaf)) {
        try { Invoke-Uninstall }
        catch { Write-Warning "Smoke cleanup failed: $_. No process was forcibly terminated. See $workspace." }
    }
    foreach ($process in $ownedProcesses) {
        $process.Refresh()
        if (-not $process.HasExited) { Write-Warning "Owned test process $($process.Id) is still running; it was left intact." }
        $process.Dispose()
    }
    Stop-Transcript | Out-Null
}
