use crate::{
    buffer::{Frame, ReplayBuffer},
    native::{self, Sample},
};
use std::{
    ffi::{c_char, c_void, CStr, CString},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

const BUFFER_SECONDS: f64 = 30.0;
const BUFFER_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Stopped = 0,
    Starting = 1,
    Buffering = 2,
    Stopping = 3,
}

impl Phase {
    const fn native_state(self) -> i32 {
        match self {
            Self::Stopped => 0,
            Self::Starting => 1,
            Self::Buffering => 2,
            Self::Stopping => 3,
        }
    }
}

struct Recorder {
    phase: Phase,
    generation: u64,
    buffer: ReplayBuffer<Arc<Sample>>,
    saving: bool,
    shutdown: bool,
}

impl Recorder {
    const fn new() -> Self {
        Self {
            phase: Phase::Stopped,
            generation: 0,
            buffer: ReplayBuffer::new(BUFFER_SECONDS, BUFFER_BYTES),
            saving: false,
            shutdown: false,
        }
    }
}

static RECORDER: OnceLock<Mutex<Recorder>> = OnceLock::new();

fn recorder() -> MutexGuard<'static, Recorder> {
    RECORDER
        .get_or_init(|| Mutex::new(Recorder::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn publish(phase: Phase, message: &str) {
    // Call with the recorder mutex held: enqueueing native UI updates in state
    // transition order prevents a delayed callback from restoring an old state.
    eprintln!("Optic: {message}");
    let Ok(message) = CString::new(message.replace('\0', " ")) else {
        return;
    };
    // SAFETY: The bridge copies the NUL-terminated message before returning;
    // the CString remains alive throughout the call, and the state is valid.
    unsafe { native::optic_set_status(phase.native_state(), message.as_ptr()) };
}

extern "C" fn action(action: i32) -> bool {
    match action {
        1 => toggle(),
        2 => save(),
        3 => {
            let mut state = recorder();
            state.shutdown = true;
            state.buffer.clear();
            let generation = state.generation;
            state.phase = Phase::Stopped;
            let finish_now = !state.saving;
            drop(state);
            // SAFETY: The bridge accepts any generation and enqueues shutdown
            // on its capture queue without retaining Rust references.
            unsafe { native::optic_capture_stop(generation) };
            return finish_now;
        }
        _ => (),
    }
    true
}

fn toggle() {
    let mut state = recorder();
    if state.shutdown {
        return;
    }
    match state.phase {
        Phase::Stopped => {
            state.generation += 1;
            state.buffer.clear();
            state.phase = Phase::Starting;
            let generation = state.generation;
            publish(Phase::Starting, "Starting screen capture…");
            drop(state);
            // SAFETY: Both callbacks have the bridge's C ABI and static
            // lifetime; they accept ownership of retained native samples.
            unsafe { native::optic_capture_start(generation, frame_received, capture_event) };
        }
        Phase::Starting | Phase::Buffering => {
            // Clear immediately, before asynchronous capture shutdown finishes.
            state.buffer.clear();
            state.phase = Phase::Stopping;
            let generation = state.generation;
            publish(Phase::Stopping, "Stopping · buffer cleared");
            drop(state);
            // SAFETY: The bridge accepts any generation and enqueues shutdown
            // on its capture queue without retaining Rust references.
            unsafe { native::optic_capture_stop(generation) };
        }
        Phase::Stopping => (),
    }
}

extern "C" fn frame_received(
    generation: u64,
    sample: *mut c_void,
    pts: f64,
    duration: f64,
    size: usize,
    keyframe: bool,
) {
    if sample.is_null() {
        return;
    }
    let frame = Frame {
        sample: Arc::new(Sample(sample)),
        pts,
        duration,
        size,
        keyframe,
    };
    let mut state = recorder();
    if !state.shutdown
        && generation == state.generation
        && matches!(state.phase, Phase::Starting | Phase::Buffering)
    {
        state.buffer.push(frame);
    }
    // Unaccepted frames release their transferred reference here as well.
}

extern "C" fn capture_event(generation: u64, event: i32, message: *const c_char) {
    let message = if message.is_null() {
        String::new()
    } else {
        // SAFETY: The bridge provides a NUL-terminated message that stays
        // valid until this callback returns; the owned copy outlives the call.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    };
    let mut state = recorder();
    if state.shutdown || generation != state.generation {
        return;
    }
    let output = match event {
        1 if state.phase == Phase::Starting => {
            state.phase = Phase::Buffering;
            Some((Phase::Buffering, "Buffering · last 30 seconds".to_owned()))
        }
        2 => {
            state.phase = Phase::Stopped;
            state.buffer.clear();
            Some((Phase::Stopped, "Stopped · buffer empty".to_owned()))
        }
        3 => {
            state.phase = Phase::Stopped;
            state.buffer.clear();
            Some((Phase::Stopped, format!("Capture failed: {message}")))
        }
        _ => None,
    };
    if let Some((phase, message)) = output {
        publish(phase, &message);
    }
    drop(state);
}

fn recording_path() -> Result<PathBuf, String> {
    let folder = native::recordings_folder()?;
    std::fs::create_dir_all(&folder)
        .map_err(|e| format!("Cannot create {}: {e}", folder.display()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?;
    Ok(folder.join(format!(
        "Optic-{}-{:09}.mp4",
        now.as_secs(),
        now.subsec_nanos()
    )))
}

fn export(frames: &[Frame<Arc<Sample>>], path: &Path) -> Result<(), String> {
    if frames.is_empty() || !frames[0].keyframe {
        return Err("No decodable video buffered yet".into());
    }
    let samples: Vec<*mut c_void> = frames.iter().map(|f| f.sample.0).collect();
    // SAFETY: The snapshot retains every sample throughout this synchronous call.
    unsafe { native::export_samples(&samples, path) }
}

fn save() {
    let mut state = recorder();
    if state.shutdown || state.phase != Phase::Buffering {
        return;
    }
    if state.saving {
        publish(Phase::Buffering, "A clip is already saving…");
        return;
    }
    let frames = state.buffer.snapshot();
    if frames.is_empty() {
        publish(Phase::Buffering, "Waiting for the first video frame…");
        return;
    }
    state.saving = true;
    let duration = state.buffer.duration();
    publish(Phase::Buffering, &format!("Saving {duration:.1} seconds…"));
    drop(state);
    std::thread::spawn(move || {
        let result = recording_path().and_then(|path| {
            export(&frames, &path)?;
            Ok(path)
        });
        // Drop the snapshot before announcing completion. A stopped buffer
        // remains empty; an already requested save owns its own references.
        drop(frames);
        let mut state = recorder();
        state.saving = false;
        let phase = state.phase;
        let shutdown = state.shutdown;
        if shutdown {
            drop(state);
            // SAFETY: The bridge enqueues termination on the main run loop;
            // it takes no pointers and may be called from this worker thread.
            unsafe { native::optic_finish_quit() };
            return;
        }
        match result {
            Ok(path) => publish(phase, &format!("Saved {}", path.display())),
            Err(error) => publish(phase, &format!("Save failed: {error}")),
        }
    });
}

static TEST_BUFFER: OnceLock<Mutex<ReplayBuffer<Arc<Sample>>>> = OnceLock::new();

extern "C" fn test_frame(
    _generation: u64,
    sample: *mut c_void,
    pts: f64,
    duration: f64,
    size: usize,
    keyframe: bool,
) {
    if sample.is_null() {
        return;
    }
    let frame = Frame {
        sample: Arc::new(Sample(sample)),
        pts,
        duration,
        size,
        keyframe,
    };
    if let Some(buffer) = TEST_BUFFER.get() {
        buffer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(frame);
    }
}

fn self_test(path: &Path) -> Result<(), String> {
    TEST_BUFFER
        .set(Mutex::new(ReplayBuffer::new(1.0, BUFFER_BYTES)))
        .map_err(|_| "Self-test already initialized")?;
    let mut error = [0; 2048];
    // SAFETY: The callback has the required C ABI and accepts the transferred
    // sample references. The nonempty error buffer is writable for its length.
    let success = unsafe {
        native::optic_generate_test_video(1, test_frame, error.as_mut_ptr(), error.len())
    };
    if !success {
        // SAFETY: The bridge always terminates its error within this nonempty
        // buffer, and the string is copied before the buffer leaves scope.
        return Err(unsafe { CStr::from_ptr(error.as_ptr()) }
            .to_string_lossy()
            .into_owned());
    }
    let mut buffer = TEST_BUFFER
        .get()
        .ok_or("Self-test buffer was not initialized")?
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let frames = buffer.snapshot();
    let duration = buffer.duration();
    let bytes = buffer.bytes();
    if frames.len() < 2 || !(0.5..=2.2).contains(&duration) {
        return Err(format!(
            "Unexpected replay buffer: {} frames, {duration:.3}s",
            frames.len()
        ));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    export(&frames, path)?;
    let file_size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if file_size < 1024 {
        return Err("Export produced an unexpectedly small file".into());
    }
    buffer.clear();
    if buffer.len() != 0 || buffer.bytes() != 0 {
        return Err("Buffer did not clear".into());
    }
    drop(buffer);
    println!("Media self-test passed: {} frames, {duration:.3}s, {bytes} buffered bytes, {file_size} file bytes\n{}", frames.len(), path.display());
    Ok(())
}

pub fn run() {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    #[cfg(target_os = "windows")]
    if first.is_some() {
        native::prepare_console();
    }
    if first
        .as_ref()
        .is_some_and(|argument| argument.to_str().is_none())
    {
        eprintln!("Unknown argument. Use --help for options.");
        std::process::exit(2);
    }
    match first.as_deref().and_then(|s| s.to_str()) {
        Some("--self-test") => {
            let path = args.next().map_or_else(
                || std::env::temp_dir().join("optic-self-test.mp4"),
                PathBuf::from,
            );
            if let Err(error) = self_test(&path) {
                eprintln!("Media self-test failed: {error}");
                std::process::exit(1);
            }
        }
        Some("--help" | "-h") => {
            #[cfg(target_os = "macos")]
            let (platform, toggle, save, folder) = (
                "macOS",
                "Control–Option–B",
                "Control–Option–S",
                "~/Movies/Optic",
            );
            #[cfg(target_os = "windows")]
            let (platform, toggle, save, folder) = (
                "Windows",
                "Ctrl+Alt+B",
                "Ctrl+Alt+S",
                "your Videos folder/Optic",
            );
            println!("Optic — {platform} screen replay\n\n{toggle}: start/stop buffering (stop clears history)\n{save}: save recent video to {folder}\n\n--self-test [path.mp4]  Verify encoding, buffering, and export without capturing your screen");
        }
        Some(_) => {
            eprintln!("Unknown argument. Use --help for options.");
            std::process::exit(2);
        }
        None => {
            drop(recorder());
            // SAFETY: This is the executable's main thread, and action has
            // the required C ABI and remains available for the app's lifetime.
            unsafe { native::optic_run(action) };
        }
    }
}
