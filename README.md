# Optic

A macOS menu bar and Windows tray app that saves recent screen video with a
shortcut. Rust owns the recording state, bounded replay buffer, and save jobs.
Native bridges use Apple's screen/media frameworks on macOS and the Windows
SDK on Windows. There are no external Rust dependencies.

## Run

On macOS, requires macOS 13 or newer, Rust, and Xcode Command Line Tools.

```sh
./scripts/build-app.sh
open dist/Optic.app
```

On Windows, requires Windows 10/11 x64, Rust's `x86_64-pc-windows-msvc`
toolchain, PowerShell 7, and Visual Studio 2022's C++ Build Tools with a
Windows 10/11 SDK. The packaged executable includes its C++ runtime and needs
no separately installed encoder or command-line tools.

```powershell
pwsh -File ./scripts/build-app.ps1
& ./dist/Optic.exe
```

Build tools are only required to build from source. Ordinary Windows launch
shows the tray icon without opening a console or taskbar window. Click the
icon to open its menu. Command-line `--help`, `-h`, and `--self-test [path.mp4]`
work on both platforms; Windows CLI output also supports redirection.

## Windows installer

Download and run `OpticSetup.exe`, then choose **Finish → Launch Optic**.
It installs for your Windows account in `%LOCALAPPDATA%\Programs\Optic`,
adds a Start menu shortcut and an Installed apps entry, and requires no
administrator access or developer tools. Optic launches in the tray **stopped**.
**Launch Optic when I sign in** is optional and off on a new installation;
upgrades remember your choice. Sign-in launches also start stopped.

Only one Optic tray instance runs per user and Windows session, including
copies launched from different folders. Installing an update finishes any
active save, closes the running copy, and replaces the executable. Choose
**Launch Optic** on the final page to reopen it stopped. Updates are installed
by running a newer setup; there is no background updater.

Uninstall through Windows' Installed apps. It waits for an active save and
removes the program, shortcut, and sign-in entry. Your recordings are preserved.
If shutdown fails or takes more than two minutes, maintenance stops before
replacing or removing the app; let the save finish, quit from the tray, and retry.

To build setup, install [Inno Setup 6.4 or newer](https://jrsoftware.org/isdl.php)
along with the Windows build tools above, then run:

```powershell
pwsh -File ./scripts/build-installer.ps1
# Use an existing packaged app or a compiler in a custom location:
pwsh -File ./scripts/build-installer.ps1 -SkipAppBuild -IsccPath 'C:\tools\Inno Setup\ISCC.exe'
```

The output is `dist/OpticSetup.exe`; `dist/Optic.exe` remains usable as a
portable app. `OPTIC_ISCC` can also select the compiler. The build rejects
mismatched versions in `Cargo.toml` and `native/windows/Optic.rc`.
For scripted shutdown, `Optic.exe --shutdown [seconds]` finishes an active save
and waits for the current session's process to exit (default 120 seconds,
maximum 600). Exit codes are 0 for closed or absent, 1 for shutdown failure,
and 2 for invalid arguments. A timeout leaves the saving process intact.

Local builds are unsigned. For a public release, obtain a trusted publisher
signing identity and install its code-signing certificate with an accessible
private key in `CurrentUser\My`. The signing scripts support certificates
backed by a hardware token; they never export the key. Supply the certificate's
40-character thumbprint and your provider's RFC 3161 timestamp URL:

```powershell
pwsh -File ./scripts/build-installer.ps1 -RequireSigning `
  -CertificateThumbprint 'YOUR_40_HEX_CERTIFICATE_THUMBPRINT' `
  -TimestampUrl 'https://YOUR_PROVIDER/RFC3161'
```

This signs and verifies the app before packaging, and signs and verifies setup
and its embedded uninstaller with SHA-256 and timestamps. `-RequireSigning`
fails without an explicit signing identity. `-SignToolPath` can override the
Windows SDK tool. See [Microsoft's signing guidance](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options)
for publisher certificates and signing services; a new signed release can
still receive a SmartScreen reputation prompt.

Optic appears as a recording circle in the menu bar or tray and starts **stopped**.
The menu provides the same actions as the global shortcuts:

| macOS shortcut | Windows shortcut | Action |
| --- | --- | --- |
| Control–Option–B | Ctrl+Alt+B | Start buffering; press again to stop and immediately clear history |
| Control–Option–S | Ctrl+Alt+S | Save the recent buffer as an MP4 while buffering continues |

Clips go to `~/Movies/Optic` on macOS, and to `Optic` inside your Videos known
folder on Windows (normally `%USERPROFILE%\Videos\Optic`; redirected folders
are respected). Use **Open Recordings Folder** in the menu to find them.
Saving is disabled when buffering is stopped. If you save shortly after
starting, the clip contains the history available so far. Stopping does not
cancel a save that you already requested, and starting again creates fresh
history.

On the first start, macOS may request Screen Recording permission. Allow Optic
in **System Settings → Privacy & Security → Screen Recording** (called
**Screen & System Audio Recording** on some versions), then quit and reopen
the app if macOS requires it. No microphone is used. Capture only starts when
you choose Start Buffering or press its shortcut.

## First version

- Captures the primary display, including the cursor.
- Keeps approximately the last 30 seconds in memory. Clips may include up to
  one additional keyframe interval, about a second, so they start cleanly.
- Video only: H.264 in MP4, 30 fps, up to 1080 pixels high, about 8 Mbps.
- Compressed history has a strict 64 MiB payload limit. High bitrate content
  may shorten the retained duration; native frame queues and a save in progress
  also use memory.
- Stops and clears history on capture failure or interruption. Sleep, screen
  locking, and display changes may require starting buffering again.
- Processes one save at a time. The Windows installer can enable launch at sign-in;
  macOS has no launch-at-login setting yet.

Quitting while a save is active waits for that MP4 to finish before exiting.

The default shortcuts and replay length are currently fixed. If another app
has claimed a shortcut, Optic shows the registration failure in its menu; the
menu actions still work.

## Error logs

Optic records capture, encoding, saving, shortcut, tray, and command-line
operation failures in local UTF-8 text logs. Normal recording statuses,
successful operations, duplicate launches, and known sleep/lock/display
interruptions create no entries. The tray still displays recording status and
interruptions.

Logs are stored in `%LOCALAPPDATA%\Optic\Logs` on Windows and
`~/Library/Logs/Optic` on macOS. Each process creates its own `optic-*` session
folder on its first error, containing `errors.log` and up to two rotated files.
Each file is capped at 1 MiB; cleanup retains the newest ten completed sessions
and skips active processes. These folders are separate from recordings and the
installation directory, so upgrades preserve them.

Each error includes a Unix timestamp in milliseconds, app version, platform,
process ID, component, event, message, and capture generation when applicable.
Native error codes are preserved; personal home paths are redacted and folder
diagnostics omit full paths. Messages are escaped and bounded to one line.

Logging uses synchronous writes outside recorder locks. If logging fails, Optic
attempts stderr output and continues handling the original operation. Panic
logging is best effort; native crashes and forced termination may leave no
entry. Tests use temporary log folders and do not capture your screen.

## Verify

Run the PowerShell 7 CI script to apply supported Rust/Swift fixes and then
validate the result:

```powershell
pwsh -File ./scripts/ci.ps1
# Validate without editing source:
pwsh -File ./scripts/ci.ps1 -CheckOnly
# Require full macOS app coverage in CI:
pwsh -File ./scripts/ci.ps1 -RequireMacOS
# Require full Windows app coverage in CI:
pwsh -File ./scripts/ci.ps1 -RequireWindows
# Require Windows installer compilation too:
pwsh -File ./scripts/ci.ps1 -CheckOnly -RequireWindows -RequireInstaller
```

Install Rust 1.89 or newer with the `clippy` and `rustfmt` components. Full app checks also
require the native build tools described above. macOS lint checks require
Xcode 16 or newer with `swift-format`. Windows runs compile the native bridge
with MSVC `/W4 /WX`, test the real media pipeline, and build the packaged app.
Native tests cover cursor composition/rotation/scaling, tray menu states and
deferred quitting, complete MP4 decoding, keyframe trimming, Unicode paths,
and export failure cleanup. The synthetic checks do not capture your screen.
Windows lifecycle tests also cover launches from different paths, maintenance
locking, graceful process shutdown and timeout, and canceled session shutdown.
When Inno Setup is available, CI compiles the installer; `-RequireInstaller`
makes a missing compiler a failure. Actual install/upgrade/uninstall smoke tests
are opt-in because they create a temporary per-user installation:

```powershell
pwsh -File ./scripts/test-installer.ps1
```

The smoke script refuses to replace an existing Optic installation, sign-in
entry, or Start menu shortcut. It checks upgrade shutdown, startup preference
persistence, single-instance behavior, uninstall cleanup, and preservation of
an untracked recording fixture without capturing your screen.
Each platform explicitly reports the other platform's checks as skipped;
`-RequireMacOS` or `-RequireWindows` makes the corresponding gap a failure.
Linux runs only cover the portable replay buffer.

The script enables Clippy `all`, `pedantic`, `nursery`, `cargo`, and selected
restriction lints for casts, panic/unwrap/expect, unfinished code, debug macros,
and undocumented unsafe code. Following the
[Clippy guidance](https://doc.rust-lang.org/clippy/usage.html#clippyrestriction),
the complete restriction group contains conflicting rules, so it is not enabled
wholesale. Rust compiler compatibility lints, Rustdoc lints, and all warnings
are errors during validation. On macOS,
Swift formatting/linting uses strict mode and the compiler checks complete
concurrency safety with warnings as errors. Rust unit tests run in debug and
release; the media self-test runs on both supported platforms, and plist
validation also runs on macOS.

Every step is logged under `target/ci/<timestamp>/`, with a `summary.json` of
exit codes. Checks continue after failures, and any failed fixer or validation
step produces exit code 1. Automatically applied fixes stay in the working tree
for review; remaining diagnostics need manual changes.

```sh
cargo test
cargo run -- --self-test target/optic-self-test.mp4
```

The unit tests check buffer timing, keyframe boundaries, memory bounds, and
sample release. The media self-test creates synthetic moving frames, encodes
them through the platform H.264 encoder, trims them through the Rust replay
buffer, and writes a finalized MP4 without re-encoding. It does not capture
your screen or require Screen Recording permission.

For actual capture, start buffering, wait a few seconds, save, and open the
result. Stop, start again, and save shortly afterward to confirm the old
history was cleared.

Windows CI also builds an opt-in `native-tests/capture-live.exe` beside its
logs. Run it with an output directory on an interactive desktop to check live
capture, saving while buffering, retained snapshots after stopping, restart,
interruption, and rapid cancellation. It records the primary display; CI's
default synthetic tests do not invoke it.

## Source

- `src/app.rs`: start/stop lifecycle, native callbacks, save jobs.
- `src/logging.rs`: error records, bounded per-process logs, retention, and panic diagnostics.
- `src/buffer.rs`: bounded compressed history and independent save snapshots.
- `native/Media.swift`: capture, encoding, and finalized MP4 export.
- `native/App.swift`: menu bar and global shortcuts.
- `native/windows/App.cpp`: tray menu, global shortcuts, and Videos folder lookup.
- `native/windows/Capture.cpp`: Desktop Duplication, cursor composition, and capture lifecycle.
- `native/windows/Media.cpp`: Media Foundation H.264 encoding and MP4 export.
- `build.rs`: compiles and links the native bridge into the Rust executable.
- `installer/Optic.iss`: per-user Windows install, upgrades, and uninstall.
- `scripts/build-installer.ps1`: installer build and optional release signing.

Use the bundled app for ordinary use so macOS sees its stable bundle identifier.
The build script applies a local ad-hoc signature; rebuilding or moving the app
may require refreshing its recording authorization. Distributing it to other
people would require Developer ID signing and notarization.

Windows builds target x64. The Windows bridge uses Desktop Duplication to
capture the primary display directly, with no capture picker. Locking,
suspending, or changing displays interrupts capture and clears history;
choose Start Buffering again when the display is available. Windows N editions
need Microsoft's Media Feature Pack for Media Foundation support.

## License

Optic is available under the [MIT license](LICENSE).
