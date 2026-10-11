//! Local, error-only diagnostics with bounded storage and no recording-state locks.

use std::{
    cell::Cell,
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, Once, OnceLock, TryLockError,
    },
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_SEGMENT_BYTES: u64 = 1024 * 1024;
const COMPLETED_SESSIONS: usize = 10;
const MAX_MESSAGE_BYTES: usize = 8192;
const MAX_LABEL_BYTES: usize = 128;
const LOG_NAMES: [&str; 3] = ["errors.log", "errors.1.log", "errors.2.log"];
const LEASE_NAME: &str = ".active";

static LOGGER: OnceLock<Logger> = OnceLock::new();
static PANIC_HOOK: Once = Once::new();
static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static IN_PANIC_LOGGER: Cell<bool> = const { Cell::new(false) };
}

/// Configures local diagnostics without creating a directory or file.
pub fn init() {
    let _ = global_logger();
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = info.payload().downcast_ref::<&str>().map_or_else(
                || {
                    info.payload().downcast_ref::<String>().map_or_else(
                        || "Rust panic with a non-text payload".to_owned(),
                        Clone::clone,
                    )
                },
                |message| (*message).to_owned(),
            );
            let message = info.location().map_or_else(
                || message.clone(),
                |location| {
                    format!(
                        "{message} at {}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                },
            );
            global_logger().panic_error(&message);
            previous(info);
        }));
    });
}

/// Records one failed operation. Logging failures never change the caller's result.
pub fn error(component: &str, event: &str, message: &str, generation: Option<u64>) {
    #[cfg(not(test))]
    global_logger().error(component, event, message, generation);

    // Unit tests must never create a user's real diagnostic folder.
    #[cfg(test)]
    TEST_LOGGER.with(|logger| {
        if let Some(logger) = logger.borrow().as_ref() {
            logger.error(component, event, message, generation);
        }
    });
}

fn global_logger() -> &'static Logger {
    LOGGER.get_or_init(|| Logger::new(default_directory(), personal_roots(), MAX_SEGMENT_BYTES))
}

#[cfg(any(test, not(any(target_os = "windows", target_os = "macos"))))]
const fn default_directory() -> Option<PathBuf> {
    None
}

#[cfg(all(not(test), any(target_os = "windows", target_os = "macos")))]
fn default_directory() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    return env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(|value| PathBuf::from(value).join("Optic").join("Logs"));
    #[cfg(target_os = "macos")]
    return env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|value| {
            PathBuf::from(value)
                .join("Library")
                .join("Logs")
                .join("Optic")
        });
}

fn personal_roots() -> Vec<String> {
    let mut roots = Vec::new();
    for key in ["USERPROFILE", "HOME", "LOCALAPPDATA", "APPDATA"] {
        if let Some(value) = env::var_os(key) {
            let value = value.to_string_lossy();
            let value = value.trim_end_matches(['/', '\\']);
            if value.len() > 1 {
                roots.push(value.to_owned());
                roots.push(value.replace('\\', "/"));
            }
        }
    }
    roots.sort_unstable_by_key(|root| std::cmp::Reverse(root.len()));
    roots.dedup();
    roots
}

struct Logger {
    directory: Option<PathBuf>,
    redactions: Vec<String>,
    segment_bytes: u64,
    state: Mutex<Option<Session>>,
}

impl Logger {
    const fn new(directory: Option<PathBuf>, redactions: Vec<String>, segment_bytes: u64) -> Self {
        Self {
            directory,
            redactions,
            segment_bytes,
            state: Mutex::new(None),
        }
    }

    fn error(&self, component: &str, event: &str, message: &str, generation: Option<u64>) {
        let record = self.record(component, event, message, generation);
        let mut session = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.write_record(&mut session, &record, true).is_err() {
            fallback(&record);
        }
    }

    fn panic_error(&self, message: &str) {
        IN_PANIC_LOGGER.with(|entered| {
            if entered.replace(true) {
                fallback("level=ERROR component=runtime event=rust.panic message=\"recursive panic while logging\"\n");
                return;
            }
            let record = self.record("runtime", "rust.panic", message, None);
            match self.state.try_lock() {
                Ok(mut session) => {
                    if self.write_record(&mut session, &record, false).is_err() {
                        fallback(&record);
                    }
                }
                Err(TryLockError::Poisoned(poisoned)) => {
                    if self.write_record(&mut poisoned.into_inner(), &record, false).is_err() {
                        fallback(&record);
                    }
                }
                Err(TryLockError::WouldBlock) => fallback(&record),
            }
            entered.set(false);
        });
    }

    fn write_record(
        &self,
        session: &mut Option<Session>,
        record: &str,
        wait_for_cleanup: bool,
    ) -> io::Result<()> {
        if session.is_none() {
            let directory = self.directory.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Local log directory unavailable")
            })?;
            *session = Some(Session::create(directory, wait_for_cleanup)?);
        }
        session.as_mut().map_or_else(
            || Err(io::Error::other("Log session unavailable")),
            |session| session.write(record.as_bytes(), self.segment_bytes),
        )
    }

    fn record(
        &self,
        component: &str,
        event: &str,
        message: &str,
        generation: Option<u64>,
    ) -> String {
        let mut record = format!(
            "time_unix_ms={} level=ERROR version=\"{}\" platform=\"{}\" pid={} component=",
            now_millis(),
            env!("CARGO_PKG_VERSION"),
            env::consts::OS,
            std::process::id()
        );
        quoted(
            &mut record,
            &self.redact(bounded(component, MAX_LABEL_BYTES)),
        );
        record.push_str(" event=");
        quoted(&mut record, &self.redact(bounded(event, MAX_LABEL_BYTES)));
        record.push_str(" message=");
        let prefix = bounded(message, MAX_MESSAGE_BYTES);
        let mut sanitized = self.redact(prefix);
        if prefix.len() < message.len() {
            sanitized.push_str(" [truncated]");
        }
        quoted(&mut record, &sanitized);
        if let Some(generation) = generation {
            record.push_str(" generation=");
            record.push_str(&generation.to_string());
        }
        record.push('\n');
        record
    }

    fn redact(&self, value: &str) -> String {
        let mut value = value.to_owned();
        for root in &self.redactions {
            if cfg!(target_os = "windows") {
                let lower = value.to_ascii_lowercase();
                let needle = root.to_ascii_lowercase();
                let mut redacted = String::new();
                let mut start = 0;
                for (offset, _) in lower.match_indices(&needle) {
                    redacted.push_str(&value[start..offset]);
                    redacted.push_str("<user>");
                    start = offset + root.len();
                }
                redacted.push_str(&value[start..]);
                value = redacted;
            } else {
                value = value.replace(root, "<user>");
            }
        }
        value
    }
}

fn bounded(value: &str, bytes: usize) -> &str {
    let mut end = value.len().min(bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn quoted(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{:04x}", u32::from(character));
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

fn fallback(record: &str) {
    let _ = io::stderr().lock().write_all(record.as_bytes());
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

struct Session {
    directory: PathBuf,
    // The operating system releases this lock on normal exit and crashes.
    _lease: File,
    output: Option<File>,
    bytes: u64,
}

impl Session {
    fn create(root: &Path, wait_for_cleanup: bool) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let cleanup = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(".cleanup.lock"))?;
        if wait_for_cleanup {
            cleanup.lock()?;
        } else {
            cleanup.try_lock().map_err(io::Error::other)?;
        }
        // Publishing a directory and acquiring its lease happen under the same
        // lock as cleanup, so another process cannot mistake it for a dead session.
        let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "optic-{:020}-{:010}-{sequence:020}",
            now_millis(),
            std::process::id()
        );
        let directory = root.join(name);
        fs::create_dir(&directory)?;
        let lease = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(directory.join(LEASE_NAME))?;
        lease.try_lock().map_err(io::Error::other)?;
        cleanup_sessions(root);
        Ok(Self {
            directory,
            _lease: lease,
            output: None,
            bytes: 0,
        })
    }

    fn write(&mut self, record: &[u8], max_bytes: u64) -> io::Result<()> {
        let record_bytes = u64::try_from(record.len()).map_err(io::Error::other)?;
        if record_bytes > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Error record exceeds the log segment limit",
            ));
        }
        self.open_output()?;
        if self.bytes > 0 && self.bytes.saturating_add(record_bytes) > max_bytes {
            self.rotate()?;
            self.open_output()?;
        }
        if let Some(output) = &mut self.output {
            if let Err(error) = output.write_all(record) {
                // Keep a partial record from joining the next entry when the
                // filesystem permits rolling it back. Re-read on the next try.
                let _ = output.set_len(self.bytes);
                self.output = None;
                return Err(error);
            }
            self.bytes += record_bytes;
            output.flush()?;
        }
        Ok(())
    }

    fn open_output(&mut self) -> io::Result<()> {
        if self.output.is_none() {
            let output = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.directory.join(LOG_NAMES[0]))?;
            self.bytes = output.metadata()?.len();
            self.output = Some(output);
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.output = None;
        ignore_missing(fs::remove_file(self.directory.join(LOG_NAMES[2])))?;
        ignore_missing(fs::rename(
            self.directory.join(LOG_NAMES[1]),
            self.directory.join(LOG_NAMES[2]),
        ))?;
        fs::rename(
            self.directory.join(LOG_NAMES[0]),
            self.directory.join(LOG_NAMES[1]),
        )?;
        self.bytes = 0;
        Ok(())
    }
}

fn ignore_missing(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Called only while holding the shared cleanup lock. Unknown files and
/// symbolic links are never removed, even inside an owned session directory.
fn cleanup_sessions(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut completed = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() || !session_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let directory = entry.path();
        if !owned_contents(&directory) {
            continue;
        }
        let Ok(lease) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.join(LEASE_NAME))
        else {
            continue;
        };
        if lease.try_lock().is_ok() {
            completed.push((directory, lease));
        }
    }
    completed.sort_unstable_by(|(left, _), (right, _)| right.cmp(left));
    for (directory, lease) in completed.into_iter().skip(COMPLETED_SESSIONS) {
        // The cleanup lock excludes other cleanup/session publication while the
        // handle is closed for Windows-compatible file removal.
        drop(lease);
        for name in LOG_NAMES {
            let _ = fs::remove_file(directory.join(name));
        }
        let _ = fs::remove_file(directory.join(LEASE_NAME));
        let _ = fs::remove_dir(directory);
    }
}

fn session_name(name: &str) -> bool {
    let Some(name) = name.strip_prefix("optic-") else {
        return false;
    };
    let mut parts = name.split('-');
    [20, 10, 20].into_iter().all(|length| {
        parts.next().is_some_and(|part| {
            part.len() == length && part.bytes().all(|byte| byte.is_ascii_digit())
        })
    }) && parts.next().is_none()
}

fn owned_contents(directory: &Path) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    let mut has_lease = false;
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let Ok(kind) = entry.file_type() else {
            return false;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !kind.is_file() || (name != LEASE_NAME && !LOG_NAMES.contains(&name.as_ref())) {
            return false;
        }
        has_lease |= name == LEASE_NAME;
    }
    has_lease
}

#[cfg(test)]
thread_local! {
    static TEST_LOGGER: std::cell::RefCell<Option<Logger>> = const { std::cell::RefCell::new(None) };
}

/// Provides an isolated logger for a synchronous unit-test operation.
#[cfg(test)]
pub fn with_test_logger<T>(directory: PathBuf, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Logger>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LOGGER.with(|logger| {
                *logger.borrow_mut() = self.0.take();
            });
        }
    }
    let _restore = Restore(TEST_LOGGER.with(|logger| {
        logger.replace(Some(Logger::new(
            Some(directory),
            Vec::new(),
            MAX_SEGMENT_BYTES,
        )))
    }));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, sync::Arc, thread};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> io::Result<Self> {
            let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "optic-log-test-{}-{}-{sequence}",
                std::process::id(),
                now_millis()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sessions(root: &Path) -> io::Result<Vec<PathBuf>> {
        fs::read_dir(root)?
            .filter_map(|entry| match entry {
                Ok(entry) if session_name(&entry.file_name().to_string_lossy()) => {
                    Some(Ok(entry.path()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    #[test]
    fn creates_files_only_on_first_error_and_writes_context() -> io::Result<()> {
        let root = TestDirectory::new()?;
        let directory = root.0.join("logs");
        let logger = Logger::new(Some(directory.clone()), Vec::new(), MAX_SEGMENT_BYTES);
        assert!(!directory.exists());
        logger.error("capture", "capture.failed", "Device lost", Some(42));
        let directories = sessions(&directory)?;
        assert_eq!(directories.len(), 1);
        let text = fs::read_to_string(directories[0].join(LOG_NAMES[0]))?;
        assert!(text.starts_with("time_unix_ms="));
        assert!(text.contains("level=ERROR"));
        assert!(text.contains(
            "component=\"capture\" event=\"capture.failed\" message=\"Device lost\" generation=42"
        ));
        assert_eq!(text.lines().count(), 1);
        drop(logger);
        Ok(())
    }

    #[test]
    fn concurrent_writes_stay_complete_and_unique() -> io::Result<()> {
        let root = TestDirectory::new()?;
        let logger = Arc::new(Logger::new(
            Some(root.0.clone()),
            Vec::new(),
            MAX_SEGMENT_BYTES,
        ));
        thread::scope(|scope| {
            for worker in 0..8 {
                let logger = Arc::clone(&logger);
                scope.spawn(move || {
                    for entry in 0..40 {
                        logger.error(
                            "test",
                            "write.failed",
                            &format!("worker {worker}, entry {entry}"),
                            None,
                        );
                    }
                });
            }
        });
        let directories = sessions(&root.0)?;
        assert_eq!(directories.len(), 1);
        let text = fs::read_to_string(directories[0].join(LOG_NAMES[0]))?;
        let lines: HashSet<_> = text.lines().collect();
        assert_eq!(lines.len(), 320);
        assert!(lines
            .iter()
            .all(|line| line.starts_with("time_unix_ms=") && line.ends_with('"')));
        drop(logger);
        Ok(())
    }

    #[test]
    fn rotation_bounds_all_three_segments() -> io::Result<()> {
        let root = TestDirectory::new()?;
        let logger = Logger::new(Some(root.0.clone()), Vec::new(), 4096);
        for index in 0..60 {
            logger.error(
                "test",
                "write.failed",
                &format!("entry {index}: {}", "x".repeat(1000)),
                None,
            );
        }
        let directories = sessions(&root.0)?;
        assert_eq!(directories.len(), 1);
        let mut total = 0;
        for name in LOG_NAMES {
            let file = directories[0].join(name);
            let bytes = fs::metadata(&file)?.len();
            assert!(bytes > 0 && bytes <= 4096);
            total += bytes;
            assert!(fs::read_to_string(file)?
                .lines()
                .all(|line| line.ends_with('"')));
        }
        assert!(total <= 3 * 4096);
        assert!(fs::read_to_string(directories[0].join(LOG_NAMES[0]))?.contains("entry 59:"));
        drop(logger);
        Ok(())
    }

    fn old_session(root: &Path, time: u64) -> io::Result<PathBuf> {
        let directory = root.join(format!("optic-{time:020}-0000000001-00000000000000000000"));
        fs::create_dir(&directory)?;
        fs::write(directory.join(LEASE_NAME), [])?;
        fs::write(directory.join(LOG_NAMES[0]), b"old error\n")?;
        Ok(directory)
    }

    #[test]
    fn retention_preserves_active_and_unknown_sessions() -> io::Result<()> {
        let root = TestDirectory::new()?;
        let active = old_session(&root.0, 1)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(active.join(LEASE_NAME))?;
        lease.try_lock().map_err(io::Error::other)?;
        let unknown = old_session(&root.0, 2)?;
        fs::write(unknown.join("user-file.txt"), b"keep")?;
        for time in 3..=15 {
            let _ = old_session(&root.0, time)?;
        }
        let logger = Logger::new(Some(root.0.clone()), Vec::new(), MAX_SEGMENT_BYTES);
        logger.error("test", "write.failed", "new failure", None);
        assert!(active.exists());
        assert!(unknown.join("user-file.txt").exists());
        for time in 3..=5 {
            assert!(!root
                .0
                .join(format!("optic-{time:020}-0000000001-00000000000000000000"))
                .exists());
        }
        assert_eq!(sessions(&root.0)?.len(), 13); // 10 completed + 2 active + unknown.
        drop(lease);
        drop(logger);
        Ok(())
    }

    #[test]
    fn concurrent_sessions_hold_leases_before_cleanup() -> io::Result<()> {
        let root = TestDirectory::new()?;
        for time in 1..=15 {
            let _ = old_session(&root.0, time)?;
        }
        let loggers: Vec<_> = (0..8)
            .map(|_| Logger::new(Some(root.0.clone()), Vec::new(), MAX_SEGMENT_BYTES))
            .collect();
        thread::scope(|scope| {
            for logger in &loggers {
                scope.spawn(|| logger.error("test", "write.failed", "concurrent process", None));
            }
        });
        assert_eq!(sessions(&root.0)?.len(), COMPLETED_SESSIONS + loggers.len());
        for logger in &loggers {
            let session = logger
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(session
                .as_ref()
                .is_some_and(|session| session.directory.join(LOG_NAMES[0]).exists()));
            drop(session);
        }
        drop(loggers);
        Ok(())
    }

    #[test]
    fn unavailable_directory_fails_open_and_retries() -> io::Result<()> {
        let root = TestDirectory::new()?;
        let blocked = root.0.join("blocked");
        fs::write(&blocked, b"file instead of directory")?;
        let logger = Logger::new(Some(blocked.clone()), Vec::new(), MAX_SEGMENT_BYTES);
        logger.error("test", "write.failed", "Original operation failure", None);
        assert!(logger
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none());
        fs::remove_file(&blocked)?;
        logger.error(
            "test",
            "write.failed",
            "Retry after directory recovery",
            None,
        );
        assert_eq!(sessions(&blocked)?.len(), 1);
        drop(logger);
        Ok(())
    }

    #[test]
    fn escapes_controls_redacts_roots_and_bounds_unicode() {
        let logger = Logger::new(
            None,
            vec!["C:\\Users\\Person".to_owned(), "/Users/Person".to_owned()],
            MAX_SEGMENT_BYTES,
        );
        let record = logger.record(
            "native\nbridge",
            "save.failed",
            "C:\\Users\\Person\\clips\\x.mp4\n/Users/Person/clip \"bad\"\r\t\u{1b}",
            None,
        );
        assert_eq!(record.lines().count(), 1);
        assert!(!record.contains("Person"));
        assert!(record.contains("<user>"));
        assert!(record.contains("\\n"));
        assert!(record.contains("\\\"bad\\\"\\r\\t\\u001b"));
        let long = logger.record("test", "error", &"é".repeat(MAX_MESSAGE_BYTES), None);
        assert!(long.contains("[truncated]"));
        assert!(long.len() < 10_000);
    }

    #[test]
    fn panic_logging_does_not_block_on_its_own_mutex() -> io::Result<()> {
        let root = TestDirectory::new()?;
        let logger = Logger::new(Some(root.0.clone()), Vec::new(), MAX_SEGMENT_BYTES);
        let guard = logger
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        logger.panic_error("failure while logger held");
        assert_eq!(sessions(&root.0)?.len(), 0);
        drop(guard);
        logger.panic_error("failure after mutex released");
        let directories = sessions(&root.0)?;
        assert_eq!(directories.len(), 1);
        assert!(
            fs::read_to_string(directories[0].join(LOG_NAMES[0]))?.contains("event=\"rust.panic\"")
        );
        IN_PANIC_LOGGER.with(|entered| entered.set(true));
        logger.panic_error("recursive failure");
        IN_PANIC_LOGGER.with(|entered| entered.set(false));
        assert_eq!(
            fs::read_to_string(directories[0].join(LOG_NAMES[0]))?
                .lines()
                .count(),
            1
        );
        drop(logger);
        Ok(())
    }

    #[test]
    fn panic_logging_does_not_wait_for_another_cleanup() -> io::Result<()> {
        let root = TestDirectory::new()?;
        let cleanup = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.0.join(".cleanup.lock"))?;
        cleanup.lock()?;
        let logger = Logger::new(Some(root.0.clone()), Vec::new(), MAX_SEGMENT_BYTES);
        logger.panic_error("failure while another session cleans logs");
        assert_eq!(sessions(&root.0)?.len(), 0);
        drop(cleanup);
        logger.panic_error("failure after cleanup released");
        assert_eq!(sessions(&root.0)?.len(), 1);
        drop(logger);
        Ok(())
    }

    #[test]
    fn scoped_test_logger_never_needs_real_user_directory() -> io::Result<()> {
        let root = TestDirectory::new()?;
        with_test_logger(root.0.clone(), || {
            error("test", "test.failed", "Scoped error", None);
        });
        let directories = sessions(&root.0)?;
        assert_eq!(directories.len(), 1);
        assert!(fs::read_to_string(directories[0].join(LOG_NAMES[0]))?.contains("Scoped error"));
        Ok(())
    }
}
