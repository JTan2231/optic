use crate::{
    buffer::{Frame, ReplayBuffer},
    logging,
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
        // Windows end-session queries must not initiate irreversible cleanup;
        // another application's response can still cancel the session shutdown.
        #[cfg(target_os = "windows")]
        4 => return !recorder().saving,
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
    // Terminal callbacks can race cancellation or arrive twice. Only the
    // active capture can fail; old/canceled work must not create error noise.
    if matches!(event, 3 | 4) && !matches!(state.phase, Phase::Starting | Phase::Buffering) {
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
        4 => {
            state.phase = Phase::Stopped;
            state.buffer.clear();
            Some((Phase::Stopped, message.clone()))
        }
        _ => None,
    };
    if let Some((phase, message)) = output {
        publish(phase, &message);
    }
    drop(state);
    if event == 3 {
        logging::error("capture", "capture.failed", &message, Some(generation));
    }
}

fn recording_path() -> Result<PathBuf, String> {
    let folder = native::recordings_folder()?;
    std::fs::create_dir_all(&folder)
        .map_err(|e| format!("Cannot create recordings folder: {e}"))?;
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
    let generation = state.generation;
    publish(Phase::Buffering, &format!("Saving {duration:.1} seconds…"));
    drop(state);
    let worker = std::thread::Builder::new()
        .name("optic-save".into())
        .spawn(move || {
            let result = recording_path().and_then(|path| {
                export(&frames, &path)?;
                Ok(path)
            });
            // Drop the snapshot before announcing completion. A stopped buffer
            // remains empty; an already requested save owns its own references.
            drop(frames);
            finish_save(result, generation);
        });
    if let Err(error) = worker {
        finish_save(
            Err(format!("Could not start save worker: {error}")),
            generation,
        );
    }
}

fn finish_save(result: Result<PathBuf, String>, generation: u64) {
    // Persist errors before locking the recorder or taking the deferred-quit
    // branch. A failed last clip must be reported even when its UI is closing.
    if let Err(error) = &result {
        logging::error("export", "save.failed", error, Some(generation));
    }
    let mut state = recorder();
    state.saving = false;
    let phase = state.phase;
    if state.shutdown {
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
    logging::init();
    native::initialize_error_reporting();
    if first
        .as_ref()
        .is_some_and(|argument| argument.to_str().is_none())
    {
        eprintln!("Unknown argument. Use --help for options.");
        std::process::exit(2);
    }
    match first.as_deref().and_then(|s| s.to_str()) {
        #[cfg(target_os = "windows")]
        Some("--shutdown") => {
            let timeout_seconds = args.next().map_or(120, |value| {
                let Some(value @ 1..=600) =
                    value.to_str().and_then(|value| value.parse::<u32>().ok())
                else {
                    eprintln!("Shutdown timeout must be an integer from 1 to 600 seconds.");
                    std::process::exit(2);
                };
                value
            });
            if args.next().is_some() {
                eprintln!("Use --shutdown [timeout-seconds].");
                std::process::exit(2);
            }
            if let Err(error) = native::shutdown(timeout_seconds * 1000) {
                logging::error("cli", "shutdown.failed", &error, None);
                eprintln!("Optic shutdown failed: {error}");
                std::process::exit(1);
            }
        }
        Some("--self-test") => {
            let path = args.next().map_or_else(
                || std::env::temp_dir().join("optic-self-test.mp4"),
                PathBuf::from,
            );
            if let Err(error) = self_test(&path) {
                logging::error("cli", "self_test.failed", &error, None);
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
            #[cfg(target_os = "windows")]
            println!("--shutdown [seconds]  Finish any active save and close this session's Optic instance (default 120, maximum 600)");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        error::Error,
        fs, io,
        sync::atomic::{AtomicU64, Ordering},
    };

    static SERIAL: Mutex<()> = Mutex::new(());
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestLogs(PathBuf);

    impl TestLogs {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "optic-app-logs-{}-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            )))
        }

        fn contents(&self) -> io::Result<String> {
            let mut output = String::new();
            for session in fs::read_dir(&self.0)? {
                let session = session?;
                if !session.file_type()?.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(session.path())? {
                    let path = entry?.path();
                    if path.extension().is_some_and(|extension| extension == "log") {
                        output.push_str(&fs::read_to_string(path)?);
                    }
                }
            }
            Ok(output)
        }
    }

    impl Drop for TestLogs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn reset_recorder(phase: Phase, generation: u64) {
        let mut state = recorder();
        *state = Recorder::new();
        state.phase = phase;
        state.generation = generation;
    }

    #[test]
    fn capture_errors_log_once_and_ignore_canceled_or_stale_work() -> Result<(), Box<dyn Error>> {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let logs = TestLogs::new();
        let message = CString::new("Encoder failed (HRESULT 0x80004005)")?;
        logging::with_test_logger(logs.0.clone(), || {
            reset_recorder(Phase::Starting, 7);
            capture_event(6, 3, message.as_ptr());
            assert_eq!(recorder().phase, Phase::Starting);
            capture_event(7, 3, message.as_ptr());
            assert_eq!(recorder().phase, Phase::Stopped);
            capture_event(7, 3, message.as_ptr());
            reset_recorder(Phase::Stopping, 8);
            capture_event(8, 3, message.as_ptr());
            assert_eq!(recorder().phase, Phase::Stopping);
        });
        let output = logs.contents()?;
        assert_eq!(output.lines().count(), 1);
        assert!(output.contains("capture.failed"));
        assert!(output.contains("generation=7"));
        assert!(output.contains("0x80004005"));
        Ok(())
    }

    #[test]
    fn normal_statuses_and_interruptions_create_no_logs() -> Result<(), Box<dyn Error>> {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let logs = TestLogs::new();
        let message = CString::new("Screen capture was interrupted")?;
        logging::with_test_logger(logs.0.clone(), || {
            reset_recorder(Phase::Starting, 9);
            capture_event(9, 1, std::ptr::null());
            assert_eq!(recorder().phase, Phase::Buffering);
            capture_event(9, 4, message.as_ptr());
            assert_eq!(recorder().phase, Phase::Stopped);
            capture_event(9, 2, std::ptr::null());
            finish_save(Ok(PathBuf::from("successful.mp4")), 9);
        });
        assert!(!logs.0.exists());
        Ok(())
    }

    #[test]
    fn failed_final_save_is_logged_before_deferred_quit() -> Result<(), Box<dyn Error>> {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let logs = TestLogs::new();
        logging::with_test_logger(logs.0.clone(), || {
            reset_recorder(Phase::Stopped, 11);
            {
                let mut state = recorder();
                state.saving = true;
                state.shutdown = true;
            }
            finish_save(Err("Finalizing the MP4 timed out".into()), 10);
            let state = recorder();
            assert!(!state.saving);
            assert!(state.shutdown);
            drop(state);
        });
        let output = logs.contents()?;
        assert_eq!(output.lines().count(), 1);
        assert!(output.contains("save.failed"));
        assert!(output.contains("generation=10"));
        assert!(output.contains("Finalizing the MP4 timed out"));
        Ok(())
    }
}
