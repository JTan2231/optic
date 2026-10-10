//! Optic records a bounded history of screen video on macOS 13 or newer.
//!
//! The Rust replay buffer owns compressed samples and save snapshots; the
//! Swift bridge provides the menu bar interface, capture, encoding, and export.

#[cfg(any(target_os = "macos", test))]
mod buffer;

#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod native;

fn main() {
    #[cfg(target_os = "macos")]
    app::run();
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("Optic currently supports macOS 13 or newer.");
        std::process::exit(1);
    }
}
