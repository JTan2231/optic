//! Optic records a bounded history of screen video on macOS and Windows.
//!
//! The Rust replay buffer owns compressed samples and save snapshots; the
//! native bridges provide the tray interface, capture, encoding, and export.

#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]

#[cfg(any(target_os = "macos", target_os = "windows", test))]
mod buffer;

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod app;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod native;

fn main() {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    app::run();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        eprintln!("Optic supports macOS 13 or newer and Windows 10/11 x64.");
        std::process::exit(1);
    }
}
