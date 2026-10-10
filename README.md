# Optic

A macOS menu bar app that saves recent screen video with a shortcut. Rust owns
the recording state, bounded replay buffer, and save jobs. A Swift bridge calls
AppKit, ScreenCaptureKit, VideoToolbox, and AVFoundation. There are no external
Rust dependencies.

## Run

Requires macOS 13 or newer, Rust, and Xcode Command Line Tools.

```sh
./scripts/build-app.sh
open dist/Optic.app
```

Optic appears as a recording circle in the menu bar and starts **stopped**.
The menu provides the same actions as the global shortcuts:

| Shortcut | Action |
| --- | --- |
| Control–Option–B | Start buffering; press again to stop and immediately clear history |
| Control–Option–S | Save the recent buffer as an MP4 while buffering continues |

Clips go to `~/Movies/Optic`. Use **Open Recordings Folder** in the menu to find
them. Saving is disabled when buffering is stopped. If you save shortly after
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

## Verify

Run the PowerShell 7 CI script to apply supported Rust/Swift fixes and then
validate the result:

```powershell
pwsh -File ./scripts/ci.ps1
# Validate without editing source:
pwsh -File ./scripts/ci.ps1 -CheckOnly
# Require full macOS app coverage in CI:
pwsh -File ./scripts/ci.ps1 -RequireMacOS
```

Install Rust with the `clippy` and `rustfmt` components. Full app checks also
require macOS and Xcode 16 or newer with `swift-format`. Windows/Linux runs
check the portable Rust code and explicitly report the macOS checks as skipped;
`-RequireMacOS` makes that coverage gap a failure.

The script enables Clippy `all`, `pedantic`, `nursery`, `cargo`, and selected
restriction lints for casts, panic/unwrap/expect, unfinished code, debug macros,
and undocumented unsafe code. Following the
[Clippy guidance](https://doc.rust-lang.org/clippy/usage.html#clippyrestriction),
the complete restriction group contains conflicting rules, so it is not enabled
wholesale. Rust compiler compatibility lints, Rustdoc lints, and all warnings
are errors during validation. On macOS,
Swift formatting/linting uses strict mode and the compiler checks complete
concurrency safety with warnings as errors. Rust unit tests run in debug and
release; the media self-test and plist validation also run on macOS.

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
them through VideoToolbox, trims them through the Rust replay buffer, and
writes an MP4 through AVFoundation. It does not capture your screen or require
Screen Recording permission.

For actual capture, start buffering, wait a few seconds, save, and open the
result. Stop, start again, and save shortly afterward to confirm the old
history was cleared.

## Source

- `src/app.rs`: start/stop lifecycle, native callbacks, save jobs.
- `src/buffer.rs`: bounded compressed history and independent save snapshots.
- `native/Media.swift`: capture, encoding, and finalized MP4 export.
- `native/App.swift`: menu bar and global shortcuts.
- `build.rs`: compiles and links the native bridge into the Rust executable.

Use the bundled app for ordinary use so macOS sees its stable bundle identifier.
The build script applies a local ad-hoc signature; rebuilding or moving the app
may require refreshing its recording authorization. Distributing it to other
people would require Developer ID signing and notarization.

## License

Optic is available under the [MIT license](LICENSE).
