; Build with scripts/build-installer.ps1, or pass /DSourceExe, /DAppVersion,
; /DOutputDir and optionally /DSignTool to ISCC directly.
; Keep AppId, the startup value name, and maintenance mutex stable across releases.

#if Ver < EncodeVer(6, 4, 0)
  #error Inno Setup 6.4 or newer is required.
#endif
#ifndef SourceExe
  #define SourceExe AddBackslash(SourcePath) + "..\dist\Optic.exe"
#endif
#ifndef AppVersion
  #define AppVersion GetVersionNumbersString(SourceExe)
#endif
#ifndef OutputDir
  #define OutputDir AddBackslash(SourcePath) + "..\dist"
#endif

[Setup]
AppId={{743C904A-A9C6-4BFD-A7C9-A9440E714ACD}
AppName=Optic
AppVersion={#AppVersion}
AppPublisher=Optic
AppPublisherURL=https://github.com/jtan2231/optic
AppSupportURL=https://github.com/jtan2231/optic/issues
AppUpdatesURL=https://github.com/jtan2231/optic/releases
DefaultDirName={localappdata}\Programs\Optic
DisableDirPage=yes
DisableProgramGroupPage=yes
UsePreviousAppDir=yes
UsePreviousTasks=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
MinVersion=10.0
; The native records in [Code] use the 32-bit installer ABI. This is independent
; of 64-bit install mode and the architecture of the Optic executable.
#if Ver >= EncodeVer(7, 0, 0)
SetupArchitecture=x86
#endif
SetupMutex=Local\Optic.Setup.v1
CloseApplications=no
RestartApplications=no
RestartIfNeededByRun=no
UninstallDisplayIcon={app}\Optic.exe
UninstallDisplayName=Optic
SetupIconFile={#AddBackslash(SourcePath)}..\native\windows\Optic.ico
LicenseFile={#AddBackslash(SourcePath)}..\LICENSE
OutputDir={#OutputDir}
OutputBaseFilename=OpticSetup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
#ifdef SignTool
SignTool={#SignTool}
SignedUninstaller=yes
#endif

[Tasks]
Name: "startup"; Description: "Launch Optic when I sign in (starts stopped)"; Flags: unchecked

[Files]
; A fresh helper can close a portable copy even on a first installation, and
; understands the shutdown protocol when an old installed copy does not.
Source: "{#SourceExe}"; DestName: "OpticMaintenance.exe"; Flags: dontcopy
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "Optic.exe"; Flags: ignoreversion
Source: "{#AddBackslash(SourcePath)}..\LICENSE"; DestDir: "{app}"

[Icons]
Name: "{userprograms}\Optic"; Filename: "{app}\Optic.exe"; WorkingDir: "{app}"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Optic"; ValueData: """{app}\Optic.exe"""; Flags: uninsdeletevalue; Tasks: startup
; An upgrade with startup unchecked must also remove a prior opt-in.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "Optic"; Flags: deletevalue uninsdeletevalue; Tasks: not startup

[Run]
Filename: "{app}\Optic.exe"; WorkingDir: "{app}"; Description: "Launch Optic"; Flags: postinstall nowait skipifsilent; BeforeInstall: ReleaseMaintenanceLock

; There are deliberately no wildcard deletion rules. Inno removes its tracked
; files, shortcut, and registry value; recordings in the Videos folder survive.
[Code]
const
  MaintenanceMutexName = 'Local\Optic.Maintenance.v1';
  WaitObject0 = 0;
  WaitAbandoned = $00000080;
  WaitTimeout = $00000102;
  CreateNoWindow = $08000000;
  StartfUseShowWindow = $00000001;
  ShutdownWaitMilliseconds = 125000;

type
  TStartupInfo32 = record
    cb: Cardinal;
    lpReserved: Cardinal;
    lpDesktop: Cardinal;
    lpTitle: Cardinal;
    dwX: Cardinal;
    dwY: Cardinal;
    dwXSize: Cardinal;
    dwYSize: Cardinal;
    dwXCountChars: Cardinal;
    dwYCountChars: Cardinal;
    dwFillAttribute: Cardinal;
    dwFlags: Cardinal;
    wShowWindow: Word;
    cbReserved2: Word;
    lpReserved2: Cardinal;
    hStdInput: Cardinal;
    hStdOutput: Cardinal;
    hStdError: Cardinal;
  end;
  TProcessInformation32 = record
    hProcess: Cardinal;
    hThread: Cardinal;
    dwProcessId: Cardinal;
    dwThreadId: Cardinal;
  end;
  TMessage32 = record
    hwnd: Cardinal;
    message: Cardinal;
    wParam: Cardinal;
    lParam: Cardinal;
    time: Cardinal;
    ptX: Integer;
    ptY: Integer;
    lPrivate: Cardinal;
  end;

function WinCreateMutex(SecurityAttributes: Cardinal; InitialOwner: Integer;
  Name: String): Cardinal;
  external 'CreateMutexW@kernel32.dll stdcall';
function WinWaitForSingleObject(Handle, Milliseconds: Cardinal): Cardinal;
  external 'WaitForSingleObject@kernel32.dll stdcall';
function WinReleaseMutex(Handle: Cardinal): Integer;
  external 'ReleaseMutex@kernel32.dll stdcall';
function WinCloseHandle(Handle: Cardinal): Integer;
  external 'CloseHandle@kernel32.dll stdcall';
function WinCreateProcess(ApplicationName, CommandLine: String;
  ProcessAttributes, ThreadAttributes: Cardinal; InheritHandles: Integer;
  CreationFlags, Environment: Cardinal; CurrentDirectory: String;
  var StartupInfo: TStartupInfo32;
  var ProcessInformation: TProcessInformation32): Integer;
  external 'CreateProcessW@kernel32.dll stdcall';
function WinGetExitCodeProcess(Handle: Cardinal; var ExitCode: Cardinal): Integer;
  external 'GetExitCodeProcess@kernel32.dll stdcall';
function WinGetTickCount: Cardinal;
  external 'GetTickCount@kernel32.dll stdcall';
procedure WinPostQuitMessage(ExitCode: Integer);
  external 'PostQuitMessage@user32.dll stdcall';
function WinPeekMessage(var Message: TMessage32; Window: Cardinal;
  MinMessage, MaxMessage, RemoveMessage: Cardinal): Integer;
  external 'PeekMessageW@user32.dll stdcall';
function WinTranslateMessage(var Message: TMessage32): Integer;
  external 'TranslateMessage@user32.dll stdcall';
function WinDispatchMessage(var Message: TMessage32): Integer;
  external 'DispatchMessageW@user32.dll stdcall';

var
  MaintenanceHandle: Cardinal;
  MaintenanceOwned: Boolean;

procedure ReleaseMaintenanceLock;
begin
  if MaintenanceHandle <> 0 then
  begin
    if MaintenanceOwned then
      WinReleaseMutex(MaintenanceHandle);
    WinCloseHandle(MaintenanceHandle);
    MaintenanceHandle := 0;
    MaintenanceOwned := False;
  end;
end;

function AcquireMaintenanceLock: String;
var
  WaitResult: Cardinal;
begin
  Result := '';
  if MaintenanceOwned then
    Exit;
  MaintenanceHandle := WinCreateMutex(0, 0, MaintenanceMutexName);
  if MaintenanceHandle = 0 then
  begin
    Result := 'Could not prepare Optic for installation: ' +
      SysErrorMessage(DLLGetLastError);
    Exit;
  end;
  WaitResult := WinWaitForSingleObject(MaintenanceHandle, 5000);
  if (WaitResult = WaitObject0) or (WaitResult = WaitAbandoned) then
    MaintenanceOwned := True
  else
  begin
    if WaitResult = WaitTimeout then
      Result := 'Another Optic installation or uninstall is in progress. ' +
        'Let it finish, then try again.'
    else
      Result := 'Could not prepare Optic for installation: ' +
        SysErrorMessage(DLLGetLastError);
    ReleaseMaintenanceLock;
  end;
end;

procedure PumpMessages;
var
  Message: TMessage32;
begin
  while WinPeekMessage(Message, 0, 0, 0, 1) <> 0 do
  begin
    if Message.message = $0012 then
    begin
      // Preserve WM_QUIT for Inno's outer message loop and leave files intact.
      WinPostQuitMessage(Integer(Message.wParam));
      RaiseException('Installation cancelled while waiting for Optic to close.');
    end;
    WinTranslateMessage(Message);
    WinDispatchMessage(Message);
  end;
end;

function ShutDownOptic(const ExePath: String): String;
var
  CommandLine: String;
  StartupInfo: TStartupInfo32;
  ProcessInfo: TProcessInformation32;
  WaitResult, ExitCode, StartedAt, CurrentTick, Elapsed: Cardinal;
begin
  Result := '';
  if not FileExists(ExePath) then
    Exit;

  Log('Asking Optic to finish any active save and exit.');
  // All pointer/handle fields must be null unless explicitly initialized.
  StartupInfo.cb := SizeOf(StartupInfo);
  StartupInfo.lpReserved := 0;
  StartupInfo.lpDesktop := 0;
  StartupInfo.lpTitle := 0;
  StartupInfo.dwX := 0;
  StartupInfo.dwY := 0;
  StartupInfo.dwXSize := 0;
  StartupInfo.dwYSize := 0;
  StartupInfo.dwXCountChars := 0;
  StartupInfo.dwYCountChars := 0;
  StartupInfo.dwFillAttribute := 0;
  StartupInfo.dwFlags := StartfUseShowWindow;
  StartupInfo.wShowWindow := SW_HIDE;
  StartupInfo.cbReserved2 := 0;
  StartupInfo.lpReserved2 := 0;
  StartupInfo.hStdInput := 0;
  StartupInfo.hStdOutput := 0;
  StartupInfo.hStdError := 0;
  CommandLine := '"' + ExePath + '" --shutdown 120';
  // Exec(ewWaitUntilTerminated) is unbounded if an older binary ignores the flag.
  if WinCreateProcess(ExePath, CommandLine, 0, 0, 0, CreateNoWindow, 0,
    ExtractFileDir(ExePath), StartupInfo, ProcessInfo) = 0 then
  begin
    Result := 'Could not ask Optic to close: ' + SysErrorMessage(DLLGetLastError);
    Exit;
  end;
  WinCloseHandle(ProcessInfo.hThread);
  try
    StartedAt := WinGetTickCount;
    repeat
      WaitResult := WinWaitForSingleObject(ProcessInfo.hProcess, 100);
      if WaitResult <> WaitTimeout then
        Break;
      PumpMessages;
      CurrentTick := WinGetTickCount;
      if CurrentTick >= StartedAt then
        Elapsed := CurrentTick - StartedAt
      else
        Elapsed := ($FFFFFFFF - StartedAt) + CurrentTick + 1;
    until Elapsed >= ShutdownWaitMilliseconds;

    if WaitResult = WaitTimeout then
      Result := 'Optic did not finish closing within two minutes. ' +
        'Let its active save finish and quit Optic from its tray menu, then try again. ' +
        'If this is an older Optic version, replace it only after it has closed.'
    else if WaitResult <> WaitObject0 then
      Result := 'Could not wait for Optic to close: ' + SysErrorMessage(DLLGetLastError)
    else if WinGetExitCodeProcess(ProcessInfo.hProcess, ExitCode) = 0 then
      Result := 'Could not check whether Optic closed: ' + SysErrorMessage(DLLGetLastError)
    else if ExitCode <> 0 then
      Result := 'Optic could not close safely (exit code ' + IntToStr(ExitCode) + '). ' +
        'Let any active save finish and quit Optic from its tray menu, then try again.';
  finally
    // Never terminate the shutdown helper or the app: a save may still be running.
    WinCloseHandle(ProcessInfo.hProcess);
  end;
  if Result <> '' then
    Log(Result);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := AcquireMaintenanceLock;
  if Result = '' then
  begin
    ExtractTemporaryFile('OpticMaintenance.exe');
    Result := ShutDownOptic(ExpandConstant('{tmp}\OpticMaintenance.exe'));
  end;
  if Result <> '' then
    ReleaseMaintenanceLock;
end;

procedure DeinitializeSetup;
begin
  ReleaseMaintenanceLock;
end;

function InitializeUninstall: Boolean;
var
  ErrorMessage: String;
begin
  ErrorMessage := AcquireMaintenanceLock;
  if ErrorMessage = '' then
    ErrorMessage := ShutDownOptic(ExpandConstant('{app}\Optic.exe'));
  Result := ErrorMessage = '';
  if not Result then
  begin
    ReleaseMaintenanceLock;
    SuppressibleMsgBox(ErrorMessage + #13#10#13#10 +
      'Optic has not been uninstalled. Your recordings are preserved.',
      mbError, MB_OK, IDOK);
  end;
end;

procedure DeinitializeUninstall;
begin
  ReleaseMaintenanceLock;
end;
