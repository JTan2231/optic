use std::ffi::{c_char, c_void};

pub type FrameCallback = extern "C" fn(u64, *mut c_void, f64, f64, usize, bool);
pub type EventCallback = extern "C" fn(u64, i32, *const c_char);

unsafe extern "C" {
    pub fn optic_run(callback: extern "C" fn(i32) -> bool);
    pub fn optic_finish_quit();
    pub fn optic_set_status(state: i32, message: *const c_char);
    pub fn optic_capture_start(generation: u64, frame: FrameCallback, event: EventCallback);
    pub fn optic_capture_stop(generation: u64);
    pub fn optic_sample_release(sample: *mut c_void);
    pub fn optic_export(
        samples: *const *mut c_void,
        count: usize,
        path: *const c_char,
        error: *mut c_char,
        error_capacity: usize,
    ) -> bool;
    pub fn optic_generate_test_video(
        generation: u64,
        frame: FrameCallback,
        error: *mut c_char,
        error_capacity: usize,
    ) -> bool;
}

/// Owns the retained, immutable compressed `CMSampleBuffer` transferred by Swift.
pub struct Sample(pub *mut c_void);

// SAFETY: Compressed sample data is immutable. The bridge permits transferring
// its owned Core Foundation reference to an export worker and releasing there.
unsafe impl Send for Sample {}
// SAFETY: Shared references only borrow immutable compressed sample data;
// the owning Sample keeps its Core Foundation reference alive during export.
unsafe impl Sync for Sample {}

impl Drop for Sample {
    fn drop(&mut self) {
        // SAFETY: Swift transferred exactly one retained sample reference to
        // this owner, and Drop releases it exactly once on any thread.
        unsafe { optic_sample_release(self.0) };
    }
}
