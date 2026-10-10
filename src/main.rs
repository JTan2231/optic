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
