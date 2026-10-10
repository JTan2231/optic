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
