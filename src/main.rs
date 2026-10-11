//! Optic records recent screen video and system playback audio on macOS and Windows.
//!
//! The Rust replay buffer owns compressed samples and save snapshots; the
//! Windows services use Rust; macOS capture and UI use a native Swift bridge.

#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]

#[cfg(any(target_os = "macos", target_os = "windows", test))]
mod buffer;
#[cfg(any(target_os = "macos", target_os = "windows", test))]
mod logging;

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod app;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod native;

#[cfg(target_os = "windows")]
mod windows;

fn main() {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    app::run();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        eprintln!("Optic supports macOS 13 or newer and Windows 10/11 x64.");
        std::process::exit(1);
    }
}
