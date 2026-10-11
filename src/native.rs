#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "windows")]
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
};
use std::{
    ffi::{c_char, c_void, CStr},
    path::{Path, PathBuf},
};

pub type FrameCallback = extern "C" fn(u64, *mut c_void, f64, f64, usize, bool);
pub type EventCallback = extern "C" fn(u64, i32, *const c_char);
type ErrorCallback = extern "C" fn(*const c_char, *const c_char, *const c_char);

unsafe extern "C" {
    fn optic_set_error_callback(callback: ErrorCallback);
    pub fn optic_run(callback: extern "C" fn(i32) -> bool);
    pub fn optic_finish_quit();
    pub fn optic_set_status(state: i32, message: *const c_char);
    pub fn optic_capture_start(generation: u64, frame: FrameCallback, event: EventCallback);
    pub fn optic_capture_stop(generation: u64);
    pub fn optic_sample_release(sample: *mut c_void);
    #[cfg(target_os = "macos")]
    fn optic_export(
        samples: *const *mut c_void,
        count: usize,
        path: *const c_char,
        error: *mut c_char,
        error_capacity: usize,
    ) -> bool;
    #[cfg(target_os = "windows")]
    fn optic_export(
        samples: *const *mut c_void,
        count: usize,
        path: *const u16,
        error: *mut c_char,
        error_capacity: usize,
    ) -> bool;
    #[cfg(target_os = "windows")]
    fn optic_recordings_folder(
        path: *mut u16,
        capacity: usize,
        error: *mut c_char,
        error_capacity: usize,
    ) -> bool;
    #[cfg(target_os = "windows")]
    fn optic_prepare_console();
    #[cfg(target_os = "windows")]
    fn optic_shutdown(timeout_ms: u32, error: *mut c_char, error_capacity: usize) -> bool;
    pub fn optic_generate_test_video(
        generation: u64,
        frame: FrameCallback,
        error: *mut c_char,
        error_capacity: usize,
    ) -> bool;
}

/// Registers the process-lifetime diagnostic callback before native work starts.
pub fn initialize_error_reporting() {
    // SAFETY: This callback has the native C ABI and static lifetime. Native
    // code invokes it without holding its UI locks and retains no Rust data.
    unsafe { optic_set_error_callback(native_error) };
}

extern "C" fn native_error(component: *const c_char, event: *const c_char, message: *const c_char) {
    // A logger failure must never unwind through the native C ABI.
    let _ = std::panic::catch_unwind(|| {
        let copy = |pointer: *const c_char| {
            if pointer.is_null() {
                String::new()
            } else {
                // SAFETY: Native code supplies NUL-terminated strings valid
                // until this callback returns. Copy before retaining anything.
                unsafe { CStr::from_ptr(pointer) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        crate::logging::error(&copy(component), &copy(event), &copy(message), None);
    });
}

/// Owns one immutable compressed sample transferred by the native bridge.
pub struct Sample(pub *mut c_void);

// SAFETY: Compressed sample data is immutable. The bridge permits transferring
// its owned sample to an export worker and releasing it there. Windows samples
// own immutable bytes and format metadata rather than mutable COM objects.
unsafe impl Send for Sample {}
// SAFETY: Shared references only borrow immutable compressed sample data;
// the owning Sample keeps its native allocation alive during export.
unsafe impl Sync for Sample {}

impl Drop for Sample {
    fn drop(&mut self) {
        // SAFETY: The bridge transferred exactly one retained sample reference to
        // this owner, and Drop releases it exactly once on any thread.
        unsafe { optic_sample_release(self.0) };
    }
}

#[cfg(target_os = "windows")]
pub fn prepare_console() {
    // SAFETY: This only attaches the calling process to its parent's console,
    // preserving redirected output handles. It retains no Rust references.
    unsafe {
        optic_prepare_console();
    };
}

#[cfg(target_os = "windows")]
pub fn shutdown(timeout_ms: u32) -> Result<(), String> {
    let mut error = [0; 2048];
    // SAFETY: The bridge copies its error into this writable buffer and retains
    // no references. It asks only this user's session instance to finish saving.
    if unsafe { optic_shutdown(timeout_ms, error.as_mut_ptr(), error.len()) } {
        Ok(())
    } else {
        Err(bridge_error(&error))
    }
}

fn bridge_error(error: &[c_char]) -> String {
    // SAFETY: Every native operation NUL-terminates errors within this buffer.
    unsafe { CStr::from_ptr(error.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

pub fn recordings_folder() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").ok_or("Could not locate your home folder")?;
        Ok(PathBuf::from(home).join("Movies/Optic"))
    }
    #[cfg(target_os = "windows")]
    {
        let mut path = vec![0; 32_768];
        let mut error = [0; 2048];
        // SAFETY: Both output buffers are writable for their supplied lengths.
        let success = unsafe {
            optic_recordings_folder(
                path.as_mut_ptr(),
                path.len(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if !success {
            return Err(bridge_error(&error));
        }
        let length = path
            .iter()
            .position(|&unit| unit == 0)
            .ok_or("Invalid recordings folder")?;
        Ok(PathBuf::from(OsString::from_wide(&path[..length])))
    }
}

/// Exports samples borrowed from an independently retained replay snapshot.
///
/// # Safety
/// Every pointer must reference a native sample and remain alive for this call.
pub unsafe fn export_samples(samples: &[*mut c_void], path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let path =
        CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| "Invalid output path")?;
    #[cfg(target_os = "windows")]
    let path = {
        let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
        if units.contains(&0) {
            return Err("Invalid output path".into());
        }
        units.push(0);
        units
    };
    let mut error = [0; 2048];
    // SAFETY: The caller retains all samples; path is NUL-terminated using the
    // platform's encoding, and the error buffer is writable for its length.
    let success = unsafe {
        optic_export(
            samples.as_ptr(),
            samples.len(),
            path.as_ptr(),
            error.as_mut_ptr(),
            error.len(),
        )
    };
    if success {
        Ok(())
    } else {
        let detail = bridge_error(&error);
        Err(if detail.is_empty() {
            "Video export failed".into()
        } else {
            detail
        })
    }
}
