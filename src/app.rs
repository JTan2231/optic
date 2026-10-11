use crate::{
    buffer::{AudioBuffer, AudioFrame, Frame, ReplayBuffer},
    logging,
    native::{self, AudioSample, Sample},
};
#[cfg(target_os = "macos")]
use std::ffi::{c_char, c_void, CStr};
use std::{
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
    Recovering = 4,
    Unavailable = 5,
}

impl Phase {
    const fn native_state(self) -> i32 {
        match self {
            Self::Stopped => 0,
            Self::Starting => 1,
            Self::Buffering | Self::Recovering | Self::Unavailable => 2,
            Self::Stopping => 3,
        }
    }

    const fn can_save(self) -> bool {
        matches!(self, Self::Buffering | Self::Recovering | Self::Unavailable)
    }

    const fn accepts_frames(self) -> bool {
        matches!(self, Self::Starting) || self.can_save()
    }

    fn save_status(self, message: &str) -> String {
        match self {
            Self::Recovering => format!("Recovering screen capture · {message}"),
            Self::Unavailable => format!("Screen capture unavailable · retrying · {message}"),
            _ => message.to_owned(),
        }
    }
}

struct Recorder {
    phase: Phase,
    generation: u64,
    buffer: ReplayBuffer<Arc<Sample>>,
    audio: AudioBuffer<Arc<AudioSample>>,
    saving: bool,
    shutdown: bool,
}

impl Recorder {
    const fn new() -> Self {
        Self {
            phase: Phase::Stopped,
            generation: 0,
            buffer: ReplayBuffer::new(BUFFER_SECONDS, BUFFER_BYTES),
            // Include the preceding video keyframe and a little callback latency.
            audio: AudioBuffer::new(BUFFER_SECONDS + 2.0),
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
    native::set_status(phase.native_state(), message);
}

fn action(action: i32) -> bool {
    match action {
        1 => toggle(),
        2 => save(),
        3 => {
            let mut state = recorder();
            state.shutdown = true;
            state.buffer.clear();
            state.audio.clear();
            let generation = state.generation;
            state.phase = Phase::Stopped;
            let finish_now = !state.saving;
            drop(state);
            native::capture_stop(generation);
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
            state.audio.clear();
            state.phase = Phase::Starting;
            let generation = state.generation;
            publish(Phase::Starting, "Starting screen capture…");
            drop(state);
            native::capture_start(generation, frame_received, audio_received, capture_event);
        }
        Phase::Starting | Phase::Buffering | Phase::Recovering | Phase::Unavailable => {
            // Clear immediately, before asynchronous capture shutdown finishes.
            state.buffer.clear();
            state.audio.clear();
            state.phase = Phase::Stopping;
            let generation = state.generation;
            publish(Phase::Stopping, "Stopping · buffer cleared");
            drop(state);
            native::capture_stop(generation);
        }
        Phase::Stopping => (),
    }
}

#[cfg(target_os = "macos")]
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
    // SAFETY: Swift transfers one retained immutable sample to this callback.
    let sample = unsafe { Sample::from_retained(sample) };
    let frame = Frame {
        sample: Arc::new(sample),
        pts,
        duration,
        size,
        keyframe,
    };
    receive_frame(generation, frame);
}

#[cfg(target_os = "windows")]
fn frame_received(generation: u64, sample: Sample) {
    receive_frame(generation, frame_from_sample(sample));
}

#[cfg(target_os = "windows")]
#[allow(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "100 ns timestamps fit exact f64 integers for normal capture durations"
)]
fn frame_from_sample(sample: Sample) -> Frame<Arc<Sample>> {
    Frame {
        pts: sample.pts as f64 / 10_000_000.0,
        duration: sample.duration as f64 / 10_000_000.0,
        size: sample.bytes.len(),
        keyframe: sample.keyframe,
        sample: Arc::new(sample),
    }
}

fn receive_frame(generation: u64, frame: Frame<Arc<Sample>>) {
    let mut state = recorder();
    if !state.shutdown && generation == state.generation && state.phase.accepts_frames() {
        state.buffer.push(frame);
    }
    // Unaccepted frames release their transferred reference here as well.
}

#[cfg(target_os = "macos")]
extern "C" fn audio_received(
    generation: u64,
    sample: *mut c_void,
    pts: f64,
    duration: f64,
    size: usize,
) {
    if sample.is_null() {
        return;
    }
    // SAFETY: Swift transfers one retained immutable PCM sample reference.
    let sample = unsafe { AudioSample::from_retained(sample) };
    receive_audio(
        generation,
        AudioFrame {
            sample: Arc::new(sample),
            pts,
            duration,
            size,
        },
    );
}

#[cfg(target_os = "windows")]
#[allow(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "100 ns timestamps fit exact f64 integers for normal capture durations"
)]
fn audio_received(generation: u64, sample: AudioSample) {
    let packet = AudioFrame {
        pts: sample.pts as f64 / 10_000_000.0,
        duration: sample.duration as f64 / 10_000_000.0,
        size: sample.bytes.len(),
        sample: Arc::new(sample),
    };
    receive_audio(generation, packet);
}

fn receive_audio(generation: u64, packet: AudioFrame<Arc<AudioSample>>) {
    let mut state = recorder();
    if !state.shutdown && generation == state.generation && state.phase.accepts_frames() {
        state.audio.push(packet);
    }
}

#[cfg(target_os = "macos")]
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
    handle_capture_event(generation, event, &message);
}

#[cfg(target_os = "windows")]
fn capture_event(generation: u64, event: i32, message: &str) {
    handle_capture_event(generation, event, message);
}

fn handle_capture_event(generation: u64, event: i32, message: &str) {
    let mut state = recorder();
    if state.shutdown || generation != state.generation {
        return;
    }
    // Terminal callbacks can race cancellation or arrive twice. Only the
    // active capture can fail; old/canceled work must not create error noise.
    if matches!(event, 3..=5 | 9) && !state.phase.accepts_frames() {
        return;
    }
    let mut log_event = None;
    let output = match event {
        1 if state.phase == Phase::Starting => {
            state.phase = Phase::Buffering;
            Some((Phase::Buffering, "Buffering · last 30 seconds".to_owned()))
        }
        2 => {
            state.phase = Phase::Stopped;
            state.buffer.clear();
            state.audio.clear();
            Some((Phase::Stopped, "Stopped · buffer empty".to_owned()))
        }
        3 => {
            state.phase = Phase::Stopped;
            state.buffer.clear();
            state.audio.clear();
            log_event = Some("capture.failed");
            Some((Phase::Stopped, format!("Capture failed: {message}")))
        }
        4 | 5 => {
            state.phase = Phase::Stopped;
            state.buffer.clear();
            state.audio.clear();
            if event == 5 {
                log_event = Some("capture.interrupted");
            }
            Some((Phase::Stopped, message.to_owned()))
        }
        6 if matches!(state.phase, Phase::Starting | Phase::Buffering) => {
            state.phase = Phase::Recovering;
            log_event = Some("capture.recovering");
            let status = if state.saving {
                Phase::Recovering.save_status("saving clip…")
            } else {
                "Recovering screen capture…".to_owned()
            };
            Some((Phase::Recovering, status))
        }
        7 if matches!(state.phase, Phase::Recovering | Phase::Unavailable) => {
            state.phase = Phase::Buffering;
            let status = if state.saving {
                "Buffering · saving clip…"
            } else {
                "Buffering · last 30 seconds"
            };
            Some((Phase::Buffering, status.to_owned()))
        }
        8 if state.phase == Phase::Recovering => {
            state.phase = Phase::Unavailable;
            log_event = Some("capture.unavailable");
            let status = if state.saving {
                Phase::Unavailable.save_status("saving clip…")
            } else {
                "Screen capture unavailable · retrying…".to_owned()
            };
            Some((Phase::Unavailable, status))
        }
        9 => {
            // Audio is best effort: preserve video, recovery state, and saves.
            drop(state);
            logging::error("audio", "audio.capture_failed", message, Some(generation));
            return;
        }
        _ => None,
    };
    if let Some((phase, message)) = output {
        publish(phase, &message);
    }
    drop(state);
    if let Some(event) = log_event {
        logging::error("capture", event, message, Some(generation));
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

fn export(
    frames: &[Frame<Arc<Sample>>],
    audio: &[AudioFrame<Arc<AudioSample>>],
    path: &Path,
) -> Result<(), String> {
    if frames.is_empty() || !frames[0].keyframe {
        return Err("No decodable video buffered yet".into());
    }
    let samples: Vec<&Sample> = frames.iter().map(|frame| frame.sample.as_ref()).collect();
    let audio: Vec<&AudioSample> = audio.iter().map(|packet| packet.sample.as_ref()).collect();
    native::export_samples(&samples, &audio, path)
}

fn save() {
    let mut state = recorder();
    if state.shutdown || !state.phase.can_save() {
        return;
    }
    let phase = state.phase;
    if state.saving {
        publish(phase, &phase.save_status("A clip is already saving…"));
        return;
    }
    let frames = state.buffer.snapshot();
    if frames.is_empty() {
        publish(
            phase,
            &phase.save_status("Waiting for the first video frame…"),
        );
        return;
    }
    // Both snapshots share the recorder lock; the exporter crops edge packets.
    let audio = state.audio.snapshot_between(
        frames[0].pts,
        frames
            .last()
            .map_or(frames[0].pts, |frame| frame.pts + frame.duration),
    );
    state.saving = true;
    let duration = state.buffer.duration();
    let generation = state.generation;
    publish(
        phase,
        &phase.save_status(&format!("Saving {duration:.1} seconds…")),
    );
    drop(state);
    let worker = std::thread::Builder::new()
        .name("optic-save".into())
        .spawn(move || {
            let result = recording_path().and_then(|path| {
                export(&frames, &audio, &path)?;
                Ok(path)
            });
            // Drop the snapshot before announcing completion. A stopped buffer
            // remains empty; an already requested save owns its own references.
            drop(frames);
            drop(audio);
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
        native::finish_quit();
        return;
    }
    let saved = result.is_ok();
    match result {
        Ok(path) => publish(
            phase,
            &phase.save_status(&format!("Saved {}", path.display())),
        ),
        Err(error) => publish(phase, &phase.save_status(&format!("Save failed: {error}"))),
    }
    drop(state);
    if saved {
        native::play_save_sound();
    }
}

static TEST_BUFFER: OnceLock<Mutex<ReplayBuffer<Arc<Sample>>>> = OnceLock::new();
static TEST_AUDIO: OnceLock<Mutex<AudioBuffer<Arc<AudioSample>>>> = OnceLock::new();

#[cfg(target_os = "macos")]
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
    // SAFETY: The Swift generator transfers one retained immutable sample.
    let sample = unsafe { Sample::from_retained(sample) };
    let frame = Frame {
        sample: Arc::new(sample),
        pts,
        duration,
        size,
        keyframe,
    };
    receive_test_frame(frame);
}

#[cfg(target_os = "windows")]
fn test_frame(_generation: u64, sample: Sample) {
    receive_test_frame(frame_from_sample(sample));
}

fn receive_test_frame(frame: Frame<Arc<Sample>>) {
    if let Some(buffer) = TEST_BUFFER.get() {
        buffer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(frame);
    }
}

#[cfg(target_os = "macos")]
extern "C" fn test_audio(
    _generation: u64,
    sample: *mut c_void,
    pts: f64,
    duration: f64,
    size: usize,
) {
    if sample.is_null() {
        return;
    }
    // SAFETY: The Swift generator transfers one retained immutable PCM sample.
    let sample = unsafe { AudioSample::from_retained(sample) };
    receive_test_audio(AudioFrame {
        sample: Arc::new(sample),
        pts,
        duration,
        size,
    });
}

#[cfg(target_os = "windows")]
#[allow(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "100 ns timestamps fit exact f64 integers for synthetic clip durations"
)]
fn test_audio(_generation: u64, sample: AudioSample) {
    receive_test_audio(AudioFrame {
        pts: sample.pts as f64 / 10_000_000.0,
        duration: sample.duration as f64 / 10_000_000.0,
        size: sample.bytes.len(),
        sample: Arc::new(sample),
    });
}

fn receive_test_audio(packet: AudioFrame<Arc<AudioSample>>) {
    if let Some(buffer) = TEST_AUDIO.get() {
        buffer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(packet);
    }
}

fn self_test(path: &Path) -> Result<(), String> {
    TEST_BUFFER
        .set(Mutex::new(ReplayBuffer::new(1.0, BUFFER_BYTES)))
        .map_err(|_| "Self-test already initialized")?;
    TEST_AUDIO
        .set(Mutex::new(AudioBuffer::new(3.0)))
        .map_err(|_| "Audio self-test already initialized")?;
    native::generate_test_video(1, test_frame)?;
    native::generate_test_audio(1, test_audio)?;
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
    let mut audio_buffer = TEST_AUDIO
        .get()
        .ok_or("Audio self-test buffer was not initialized")?
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let audio = audio_buffer.snapshot_between(
        frames[0].pts,
        frames
            .last()
            .map_or(frames[0].pts, |frame| frame.pts + frame.duration),
    );
    if audio.is_empty() {
        return Err("Self-test produced no playback audio".into());
    }
    export(&frames, &audio, path)?;
    audio_buffer.clear();
    drop(audio_buffer);
    let file_size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if file_size < 1024 {
        return Err("Export produced an unexpectedly small file".into());
    }
    buffer.clear();
    if buffer.len() != 0 || buffer.bytes() != 0 {
        return Err("Buffer did not clear".into());
    }
    drop(buffer);
    println!("Media self-test passed: {} video frames, {} audio packets, {duration:.3}s, {bytes} buffered video bytes, {file_size} file bytes\n{}", frames.len(), audio.len(), path.display());
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
            println!("Optic — {platform} screen replay\n\n{toggle}: start/stop buffering (stop clears history)\n{save}: save recent video and playback audio to {folder}\n\n--self-test [path.mp4]  Verify synthetic video/audio encoding, buffering, and export without capturing your screen or playback device");
            #[cfg(target_os = "windows")]
            println!("--shutdown [seconds]  Finish any active save and close this session's Optic instance (default 120, maximum 600)");
        }
        Some(_) => {
            eprintln!("Unknown argument. Use --help for options.");
            std::process::exit(2);
        }
        None => {
            drop(recorder());
            #[cfg(target_os = "macos")]
            native::run(macos_action);
            #[cfg(target_os = "windows")]
            {
                native::run(action);
                crate::windows::capture::shutdown();
            }
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn macos_action(value: i32) -> bool {
    action(value)
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

    #[cfg(target_os = "windows")]
    fn synthetic_frame(generation: u64, pts: i64, keyframe: bool) {
        frame_received(
            generation,
            Sample {
                format: Arc::new(crate::windows::media::VideoFormat {
                    width: 2,
                    height: 2,
                    sequence_header: Arc::from([]),
                }),
                bytes: Arc::from([1]),
                pts,
                duration: 166_666,
                keyframe,
            },
        );
    }

    #[cfg(target_os = "windows")]
    fn synthetic_audio(generation: u64, pts: i64) {
        audio_received(
            generation,
            AudioSample {
                bytes: Arc::from([0_u8; 1920]),
                pts,
                duration: 100_000,
            },
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn audio_and_video_snapshots_survive_stop_and_reject_old_generations() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        reset_recorder(Phase::Buffering, 30);
        synthetic_frame(30, 0, true);
        synthetic_audio(30, 0);
        let (frames, audio) = {
            let state = recorder();
            (
                state.buffer.snapshot(),
                state.audio.snapshot_between(0.0, 1.0),
            )
        };
        handle_capture_event(30, 2, "");
        synthetic_audio(30, 100_000);
        assert!(recorder().audio.snapshot_between(0.0, 1.0).is_empty());
        assert_eq!(frames.len(), 1);
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].sample.bytes.len(), 1920);
        reset_recorder(Phase::Starting, 31);
        synthetic_audio(30, 0);
        assert!(recorder().audio.snapshot_between(0.0, 1.0).is_empty());
        synthetic_audio(31, 0);
        assert_eq!(recorder().audio.snapshot_between(0.0, 1.0).len(), 1);
        handle_capture_event(31, 4, "Screen capture interrupted");
        assert!(recorder().audio.snapshot_between(0.0, 1.0).is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn audio_failure_keeps_capture_history_and_ignores_stale_warnings() -> Result<(), Box<dyn Error>>
    {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let logs = TestLogs::new();
        logging::with_test_logger(logs.0.clone(), || {
            reset_recorder(Phase::Buffering, 32);
            synthetic_frame(32, 0, true);
            synthetic_audio(32, 0);
            handle_capture_event(31, 9, "Old output device unavailable");
            handle_capture_event(32, 9, "Audio output unavailable; retrying");
            let state = recorder();
            assert_eq!(state.phase, Phase::Buffering);
            assert_eq!(state.buffer.len(), 1);
            assert_eq!(state.audio.snapshot_between(0.0, 1.0).len(), 1);
            drop(state);
            handle_capture_event(32, 2, "");
            handle_capture_event(32, 9, "Stopped output device unavailable");
        });
        let output = logs.contents()?;
        assert_eq!(output.lines().count(), 1);
        assert!(output.contains("audio.capture_failed"));
        assert!(output.contains("generation=32"));
        Ok(())
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn recovery_preserves_history_and_accepts_frames_until_fresh_capture_returns() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let logs = TestLogs::new();
        logging::with_test_logger(logs.0.clone(), || {
            reset_recorder(Phase::Buffering, 20);
            synthetic_frame(20, 0, true);
            handle_capture_event(
                20,
                6,
                "Screen capture access was lost (HRESULT 0x887A0026).",
            );
            {
                let state = recorder();
                assert_eq!(state.phase, Phase::Recovering);
                assert_eq!(state.generation, 20);
                assert_eq!(state.buffer.len(), 1);
            }
            synthetic_frame(20, 333_333, false);
            handle_capture_event(20, 8, "The screen is still unavailable; retrying.");
            {
                let state = recorder();
                assert_eq!(state.phase, Phase::Unavailable);
                assert_eq!(state.generation, 20);
                assert_eq!(state.buffer.len(), 2);
            }
            synthetic_frame(20, 666_666, false);
            handle_capture_event(20, 7, "");
            let state = recorder();
            assert_eq!(state.phase, Phase::Buffering);
            assert_eq!(state.generation, 20);
            assert_eq!(state.buffer.len(), 3);
            assert_eq!(state.buffer.bytes(), 3);
            let frames = state.buffer.snapshot();
            drop(state);
            assert_eq!(frames.first().map(|frame| frame.pts), Some(0.0));
            assert!(frames.first().is_some_and(|frame| frame.keyframe));
        });
    }

    #[test]
    fn recovery_states_keep_save_and_stop_enabled_and_preserve_status_during_save() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        for phase in [Phase::Recovering, Phase::Unavailable] {
            reset_recorder(phase, 21);
            assert_eq!(phase.native_state(), Phase::Buffering.native_state());
            assert!(phase.can_save());
            recorder().saving = true;
            save();
            assert!(recorder().saving);
            assert_eq!(recorder().phase, phase);
            finish_save(Ok(PathBuf::from("recovery-clip.mp4")), 21);
            let state = recorder();
            assert!(!state.saving);
            assert_eq!(state.phase, phase);
            assert_eq!(state.generation, 21);
            drop(state);
            let saving = phase.save_status("Saving 2.0 seconds…");
            let saved = phase.save_status("Saved recovery-clip.mp4");
            assert!(saving.contains("capture"));
            assert!(saved.contains("capture"));
            if phase == Phase::Unavailable {
                assert!(saving.contains("retrying"));
                assert!(saved.contains("retrying"));
            }
        }
        for phase in [Phase::Stopped, Phase::Starting, Phase::Stopping] {
            assert!(!phase.can_save());
        }
    }

    #[test]
    fn recovery_logs_once_per_episode_and_ignores_canceled_stale_or_duplicate_callbacks(
    ) -> Result<(), Box<dyn Error>> {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let logs = TestLogs::new();
        let message = "Reading the screen image failed (HRESULT 0x887A0026).";
        logging::with_test_logger(logs.0.clone(), || {
            reset_recorder(Phase::Buffering, 22);
            handle_capture_event(21, 6, message);
            handle_capture_event(22, 7, "");
            handle_capture_event(22, 8, message);
            assert_eq!(recorder().phase, Phase::Buffering);
            handle_capture_event(22, 6, message);
            handle_capture_event(22, 6, message);
            handle_capture_event(21, 7, "");
            assert_eq!(recorder().phase, Phase::Recovering);
            handle_capture_event(22, 8, message);
            handle_capture_event(22, 8, message);
            handle_capture_event(22, 6, message);
            assert_eq!(recorder().phase, Phase::Unavailable);
            handle_capture_event(22, 7, "");
            handle_capture_event(22, 7, "");
            handle_capture_event(22, 6, message);
            assert_eq!(recorder().phase, Phase::Recovering);
            for phase in [Phase::Stopping, Phase::Stopped] {
                reset_recorder(phase, 23);
                for event in 6..=8 {
                    handle_capture_event(23, event, message);
                    assert_eq!(recorder().phase, phase);
                }
            }
            reset_recorder(Phase::Buffering, 24);
            recorder().shutdown = true;
            for event in 6..=8 {
                handle_capture_event(24, event, message);
                assert_eq!(recorder().phase, Phase::Buffering);
            }
        });
        let output = logs.contents()?;
        assert_eq!(output.lines().count(), 3);
        assert_eq!(output.matches("event=\"capture.recovering\"").count(), 2);
        assert_eq!(output.matches("event=\"capture.unavailable\"").count(), 1);
        assert!(output.lines().all(|line| line.contains("generation=22")));
        assert!(output.lines().all(|line| line.contains("0x887A0026")));
        Ok(())
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn expected_interruption_during_recovery_still_stops_and_clears_history() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let logs = TestLogs::new();
        logging::with_test_logger(logs.0.clone(), || {
            for phase in [Phase::Recovering, Phase::Unavailable] {
                reset_recorder(phase, 25);
                synthetic_frame(25, 0, true);
                assert_eq!(recorder().buffer.len(), 1);
                handle_capture_event(25, 4, "Screen capture was interrupted by locking.");
                let state = recorder();
                assert_eq!(state.phase, Phase::Stopped);
                assert_eq!(state.buffer.len(), 0);
                assert_eq!(state.buffer.bytes(), 0);
            }
        });
        assert!(!logs.0.exists());
    }

    #[test]
    fn capture_errors_log_once_and_ignore_canceled_or_stale_work() -> Result<(), Box<dyn Error>> {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let logs = TestLogs::new();
        let message = "Encoder failed (HRESULT 0x80004005)";
        logging::with_test_logger(logs.0.clone(), || {
            reset_recorder(Phase::Starting, 7);
            handle_capture_event(6, 3, message);
            assert_eq!(recorder().phase, Phase::Starting);
            handle_capture_event(7, 3, message);
            assert_eq!(recorder().phase, Phase::Stopped);
            handle_capture_event(7, 3, message);
            reset_recorder(Phase::Stopping, 8);
            handle_capture_event(8, 3, message);
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
    fn normal_statuses_and_expected_interruptions_create_no_logs() {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let logs = TestLogs::new();
        let message = "Screen capture was interrupted";
        logging::with_test_logger(logs.0.clone(), || {
            reset_recorder(Phase::Starting, 9);
            handle_capture_event(9, 1, "");
            assert_eq!(recorder().phase, Phase::Buffering);
            handle_capture_event(9, 4, message);
            assert_eq!(recorder().phase, Phase::Stopped);
            handle_capture_event(9, 2, "");
            finish_save(Ok(PathBuf::from("successful.mp4")), 9);
        });
        assert!(!logs.0.exists());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn unexpected_interruption_clears_history_and_logs_only_the_active_capture(
    ) -> Result<(), Box<dyn Error>> {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let logs = TestLogs::new();
        let message = "Acquiring a screen frame failed (HRESULT 0x887A0026).";
        logging::with_test_logger(logs.0.clone(), || {
            reset_recorder(Phase::Starting, 12);
            handle_capture_event(11, 5, message);
            assert_eq!(recorder().phase, Phase::Starting);
            handle_capture_event(12, 1, "");
            frame_received(
                12,
                Sample {
                    format: Arc::new(crate::windows::media::VideoFormat {
                        width: 2,
                        height: 2,
                        sequence_header: Arc::from([]),
                    }),
                    bytes: Arc::from([1]),
                    pts: 0,
                    duration: 333_333,
                    keyframe: true,
                },
            );
            assert_eq!(recorder().buffer.len(), 1);
            handle_capture_event(12, 5, message);
            {
                let state = recorder();
                assert_eq!(state.phase, Phase::Stopped);
                assert_eq!(state.buffer.len(), 0);
                assert_eq!(state.buffer.bytes(), 0);
            }
            handle_capture_event(12, 5, message);
            reset_recorder(Phase::Stopping, 13);
            handle_capture_event(13, 5, message);
            assert_eq!(recorder().phase, Phase::Stopping);
            reset_recorder(Phase::Stopped, 14);
            handle_capture_event(14, 5, message);
            assert_eq!(recorder().phase, Phase::Stopped);
            reset_recorder(Phase::Buffering, 15);
            recorder().shutdown = true;
            handle_capture_event(15, 5, message);
            assert_eq!(recorder().phase, Phase::Buffering);
        });
        let output = logs.contents()?;
        assert_eq!(output.lines().count(), 1);
        assert!(output.contains("capture.interrupted"));
        assert!(output.contains("generation=12"));
        assert!(output.contains("0x887A0026"));
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
