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
- Processes one save at a time. No launch-at-login setting yet.

Quitting while a save is active waits for that MP4 to finish before exiting.

The default shortcuts and replay length are currently fixed. If another app
has claimed a shortcut, Optic shows the registration failure in its menu; the
menu actions still work.

## Error logs

Optic records capture, encoding, saving, shortcut, tray, and command-line
operation failures in local UTF-8 text logs. Normal recording statuses,
successful operations and known sleep/lock/display
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
```

Install Rust 1.89 or newer with the `clippy` and `rustfmt` components. Full app checks also
require the native build tools described above. macOS lint checks require
Xcode 16 or newer with `swift-format`. Windows runs compile the native bridge
with MSVC `/W4 /WX`, test the real media pipeline, and build the packaged app.
Native tests cover cursor composition/rotation/scaling, tray menu states and
deferred quitting, complete MP4 decoding, keyframe trimming, Unicode paths,
and export failure cleanup. The synthetic checks do not capture your screen.
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
