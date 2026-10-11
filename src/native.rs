//! Safe platform facade; only macOS retains a foreign-language bridge.

use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

#[cfg(target_os = "windows")]
pub use crate::windows::media::EncodedSample as Sample;
#[cfg(target_os = "windows")]
pub use crate::windows::{
    app::{finish_quit, prepare_console, recordings_folder, run, set_status, shutdown},
    capture::{start as capture_start, stop as capture_stop},
    FrameCallback,
};

#[cfg(target_os = "windows")]
pub const fn initialize_error_reporting() {}

#[cfg(target_os = "windows")]
pub fn generate_test_video(generation: u64, callback: FrameCallback) -> Result<(), String> {
    crate::windows::media::generate_test_video(generation, std::sync::Arc::new(callback))
}

#[cfg(target_os = "windows")]
pub fn export_samples(samples: &[&Sample], path: &Path) -> Result<(), String> {
    let samples: Vec<Sample> = samples.iter().map(|sample| (*sample).clone()).collect();
    crate::windows::media::export_samples(&samples, path)
}

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "macos")]
mod macos {
    use super::{Path, PathBuf};
    use std::ffi::{c_char, c_void, CStr, CString};

    pub type FrameCallback = extern "C" fn(u64, *mut c_void, f64, f64, usize, bool);
    pub type EventCallback = extern "C" fn(u64, i32, *const c_char);
    type ErrorCallback = extern "C" fn(*const c_char, *const c_char, *const c_char);

    unsafe extern "C" {
        fn optic_set_error_callback(callback: ErrorCallback);
        fn optic_run(callback: extern "C" fn(i32) -> bool);
        fn optic_finish_quit();
        fn optic_set_status(state: i32, message: *const c_char);
        fn optic_capture_start(generation: u64, frame: FrameCallback, event: EventCallback);
        fn optic_capture_stop(generation: u64);
        fn optic_sample_release(sample: *mut c_void);
        fn optic_export(
            samples: *const *mut c_void,
            count: usize,
            path: *const c_char,
            error: *mut c_char,
            error_capacity: usize,
        ) -> bool;
        fn optic_generate_test_video(
            generation: u64,
            frame: FrameCallback,
            error: *mut c_char,
            error_capacity: usize,
        ) -> bool;
    }

    /// Owns the retained immutable CoreMedia sample supplied by Swift.
    pub struct Sample(*mut c_void);

    impl Sample {
        /// Takes ownership of the retained CoreMedia sample passed by Swift.
        ///
        /// # Safety
        /// The non-null pointer must own one immutable retained sample reference
        /// from the Swift bridge, which permits release on any thread.
        pub const unsafe fn from_retained(pointer: *mut c_void) -> Self {
            Self(pointer)
        }
    }
    // SAFETY: Swift supplies immutable compressed samples whose retained
    // references may be transferred to and released on an export worker.
    unsafe impl Send for Sample {}
    // SAFETY: Shared references borrow immutable data kept alive by this owner.
    unsafe impl Sync for Sample {}

    impl Drop for Sample {
        fn drop(&mut self) {
            // SAFETY: Exactly one transferred retained reference belongs to us.
            unsafe { optic_sample_release(self.0) };
        }
    }

    pub fn initialize_error_reporting() {
        // SAFETY: The C ABI callback has process lifetime and copies all strings.
        unsafe { optic_set_error_callback(native_error) };
    }

    extern "C" fn native_error(
        component: *const c_char,
        event: *const c_char,
        message: *const c_char,
    ) {
        let _ = std::panic::catch_unwind(|| {
            let copy = |pointer: *const c_char| {
                if pointer.is_null() {
                    String::new()
                } else {
                    // SAFETY: Swift keeps its terminated strings alive for this callback.
                    unsafe { CStr::from_ptr(pointer) }
                        .to_string_lossy()
                        .into_owned()
                }
            };
            crate::logging::error(&copy(component), &copy(event), &copy(message), None);
        });
    }

    pub fn run(callback: extern "C" fn(i32) -> bool) {
        // SAFETY: The caller runs on the main thread; callback has static lifetime.
        unsafe { optic_run(callback) };
    }

    pub fn finish_quit() {
        // SAFETY: Swift queues termination on the main loop and retains no references.
        unsafe { optic_finish_quit() };
    }

    pub fn set_status(state: i32, message: &str) {
        if let Ok(message) = CString::new(message.replace('\0', " ")) {
            // SAFETY: Swift copies this terminated message synchronously.
            unsafe { optic_set_status(state, message.as_ptr()) };
        }
    }

    pub fn capture_start(generation: u64, frame: FrameCallback, event: EventCallback) {
        // SAFETY: The C ABI callbacks have static lifetime and accept retained samples.
        unsafe { optic_capture_start(generation, frame, event) };
    }

    pub fn capture_stop(generation: u64) {
        // SAFETY: Swift queues cancellation without borrowing Rust references.
        unsafe { optic_capture_stop(generation) };
    }

    pub fn recordings_folder() -> Result<PathBuf, String> {
        let home = std::env::var_os("HOME").ok_or("Could not locate your home folder")?;
        Ok(PathBuf::from(home).join("Movies/Optic"))
    }

    pub fn export_samples(samples: &[&Sample], path: &Path) -> Result<(), String> {
        let path =
            CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| "Invalid output path")?;
        let pointers: Vec<*mut c_void> = samples.iter().map(|sample| sample.0).collect();
        let mut error = [0; 2048];
        // SAFETY: All owners remain borrowed throughout export; the path is
        // terminated and the error buffer is writable for its supplied length.
        if unsafe {
            optic_export(
                pointers.as_ptr(),
                pointers.len(),
                path.as_ptr(),
                error.as_mut_ptr(),
                error.len(),
            )
        } {
            Ok(())
        } else {
            Err(bridge_error(&error))
        }
    }

    pub fn generate_test_video(generation: u64, callback: FrameCallback) -> Result<(), String> {
        let mut error = [0; 2048];
        // SAFETY: Callback is static and receives retained samples; error is writable.
        if unsafe {
            optic_generate_test_video(generation, callback, error.as_mut_ptr(), error.len())
        } {
            Ok(())
        } else {
            Err(bridge_error(&error))
        }
    }

    fn bridge_error(error: &[c_char]) -> String {
        // SAFETY: Every bridge operation terminates its error within this buffer.
        let detail = unsafe { CStr::from_ptr(error.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        if detail.is_empty() {
            "Native operation failed".into()
        } else {
            detail
        }
    }
}
