//! Desktop Duplication capture. COM objects remain on one MTA worker.

use super::{
    audio::{self, LoopbackCapture, Packet},
    media::{AudioSample, EncodedSample, Encoder},
    pixels::{self, AxisSample, CursorKind, Pointer, Rotation, Viewport},
    AudioCallback, AudioSink, EventCallback, FrameCallback, FrameSink,
};
use std::{
    collections::VecDeque,
    marker::PhantomData,
    panic::{catch_unwind, AssertUnwindSafe},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Condvar, Mutex, MutexGuard, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::{Error as WindowsError, Interface},
    Win32::{
        Foundation::{HMODULE, POINT},
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_11_0,
            },
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
                D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            },
            Dxgi::{
                Common::{
                    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY,
                    DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
                    DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_UNSPECIFIED,
                },
                CreateDXGIFactory1, IDXGIFactory1, IDXGIOutputDuplication, DXGI_ERROR_ACCESS_LOST,
                DXGI_ERROR_NOT_FOUND, DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
                DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
            },
            Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY},
        },
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
    },
};

const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);
const FRAME_TICKS: i64 = 166_667;
const FREEZE_GRACE: Duration = Duration::from_secs(5);
const FIRST_RETRY: Duration = Duration::from_millis(100);
const MAX_RETRY: Duration = Duration::from_secs(1);
const FRESH_IMAGE_TIMEOUT: Duration = Duration::from_secs(1);
const INTERRUPTED: &str =
    "Screen capture was interrupted. Start buffering again when the display is available.";
const DISPLAY_CHANGED: &str = "The display configuration changed.";
const STOPPED: &str = "Screen capture stopped.";
const MAX_CURSOR_BYTES: u32 = 16 * 1024 * 1024;
static MANAGER: OnceLock<Result<Arc<Manager>, String>> = OnceLock::new();

/// Separates expected sleep/session stops from recoverable display changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interruption {
    Expected,
    DisplayChanged,
}

#[derive(Debug)]
struct CaptureError {
    interrupted: bool,
    recoverable: bool,
    message: String,
}

impl From<String> for CaptureError {
    fn from(message: String) -> Self {
        Self {
            interrupted: false,
            recoverable: false,
            message,
        }
    }
}

fn windows_error(error: &WindowsError, operation: &str, runtime_capture: bool) -> CaptureError {
    let code = error.code();
    let interrupted = code == DXGI_ERROR_SESSION_DISCONNECTED
        || (runtime_capture && code == DXGI_ERROR_ACCESS_LOST);
    CaptureError {
        interrupted,
        // Rebuild only capture resources. Encoder failures never use this path;
        // session disconnection still follows the deliberate interruption path.
        recoverable: runtime_capture && code != DXGI_ERROR_SESSION_DISCONNECTED,
        message: format!("{operation} failed (HRESULT 0x{:08X}).", code.0),
    }
}

fn os_result<T>(
    result: windows::core::Result<T>,
    operation: &str,
    runtime_capture: bool,
) -> Result<T, CaptureError> {
    result.map_err(|error| windows_error(&error, operation, runtime_capture))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ComApartment(PhantomData<Rc<()>>);

impl ComApartment {
    fn new() -> Result<Self, CaptureError> {
        // SAFETY: This dedicated worker initializes and tears down its own MTA.
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok();
        os_result(initialized, "Initializing the screen capture worker", false)?;
        Ok(Self(PhantomData))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: Construction succeeded on this thread; all COM objects drop first.
        unsafe { CoUninitialize() };
    }
}

struct FrameGate {
    active: Mutex<bool>,
    generation: u64,
    callback: FrameCallback,
    audio: Option<AudioSink>,
}

impl FrameGate {
    fn enabled(&self) -> bool {
        *lock(&self.active)
    }

    fn disable(&self) {
        *lock(&self.active) = false;
    }

    fn deliver(&self, sample: EncodedSample) {
        let active = lock(&self.active);
        if *active {
            // Holding this separate gate through the callback makes stop wait
            // for an in-flight frame. Callers release recorder locks before stop.
            (self.callback)(self.generation, sample);
        }
        drop(active);
    }

    fn deliver_audio(&self, sample: AudioSample) {
        let active = lock(&self.active);
        if *active {
            if let Some(callback) = &self.audio {
                callback(self.generation, sample);
            }
        }
        drop(active);
    }
}

struct Request {
    generation: u64,
    event: EventCallback,
    gate: Arc<FrameGate>,
    ending: AtomicU8,
    recover: AtomicBool,
}

impl Request {
    fn request_recovery(&self) {
        if self.ending.load(Ordering::Acquire) == 0 {
            self.recover.store(true, Ordering::Release);
        }
    }

    fn progress(&self, kind: i32, message: &str) {
        if self.ending.load(Ordering::Acquire) == 0 && self.gate.enabled() {
            self.event(kind, message);
        }
    }

    fn request_end(&self, kind: u8) {
        // A pending display diagnostic must not turn a deliberate stop or
        // known sleep/session interruption into an error.
        let mut previous = self.ending.load(Ordering::Acquire);
        while previous == 0 || (previous == 5 && matches!(kind, 2 | 4)) {
            match self.ending.compare_exchange_weak(
                previous,
                kind,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => previous = actual,
            }
        }
        self.gate.disable();
    }

    fn event(&self, kind: i32, message: &str) {
        if catch_unwind(AssertUnwindSafe(|| {
            (self.event)(self.generation, kind, message);
        }))
        .is_err()
        {
            crate::logging::error(
                "capture",
                "capture.callback_failed",
                "The capture event callback panicked.",
                Some(self.generation),
            );
        }
    }

    fn ended(&self, error: Option<&CaptureError>) {
        self.gate.disable();
        match self.ending.load(Ordering::Acquire) {
            4 => self.event(4, INTERRUPTED),
            5 => {
                // A display notification can race a failed frame acquisition.
                // Keep the OS error (including its HRESULT) when both occur.
                let message = error.map_or_else(
                    || format!("{DISPLAY_CHANGED} {INTERRUPTED}"),
                    |error| format!("{DISPLAY_CHANGED} {}", error.message),
                );
                self.event(5, &message);
            }
            2 => self.event(2, STOPPED),
            _ => match error {
                Some(error) => self.event(if error.interrupted { 5 } else { 3 }, &error.message),
                None => self.event(2, STOPPED),
            },
        }
    }
}

#[derive(Default)]
struct Control {
    requested: Option<Arc<Request>>,
    commands: VecDeque<Arc<Request>>,
    shutdown: bool,
}

struct Manager {
    control: Mutex<Control>,
    wake: Condvar,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Manager {
    fn new() -> Result<Arc<Self>, String> {
        let manager = Arc::new(Self {
            control: Mutex::new(Control::default()),
            wake: Condvar::new(),
            worker: Mutex::new(None),
        });
        let shared = Arc::clone(&manager);
        let worker = thread::Builder::new()
            .name("optic-capture".into())
            .spawn(move || shared.run())
            .map_err(|error| format!("Unable to create the screen capture worker: {error}"))?;
        *lock(&manager.worker) = Some(worker);
        Ok(manager)
    }

    fn forget(&self, request: &Arc<Request>) {
        let mut control = lock(&self.control);
        if control
            .requested
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, request))
        {
            control.requested = None;
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The worker's start, tick, and termination transitions are kept together to make event ordering and COM teardown explicit."
    )]
    fn run(&self) {
        let apartment = ComApartment::new();
        let mut current: Option<ActiveCapture> = None;
        let mut next_tick = Instant::now();
        loop {
            let command = {
                let mut control = lock(&self.control);
                while !control.shutdown
                    && control.commands.is_empty()
                    && current
                        .as_ref()
                        .is_none_or(|capture| capture.request.ending.load(Ordering::Acquire) == 0)
                {
                    if current.is_some() {
                        let remaining = next_tick.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        let (guard, _) = self
                            .wake
                            .wait_timeout(control, remaining)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        control = guard;
                    } else {
                        control = self
                            .wake
                            .wait(control)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                }
                if control.shutdown {
                    break;
                }
                if current
                    .as_ref()
                    .is_some_and(|capture| capture.request.ending.load(Ordering::Acquire) != 0)
                {
                    None
                } else {
                    control.commands.pop_front()
                }
            };
            if current
                .as_ref()
                .is_some_and(|capture| capture.request.ending.load(Ordering::Acquire) != 0)
            {
                if let Some(capture) = current.take() {
                    self.finish(capture, None);
                }
                continue;
            }
            if let Some(request) = command {
                if let Some(capture) = current.take() {
                    self.finish(capture, None);
                }
                if !request.gate.enabled() {
                    request.ended(None);
                    self.forget(&request);
                    continue;
                }
                let result = match &apartment {
                    Ok(_) => catch_unwind(AssertUnwindSafe(|| DesktopCapture::new(&request)))
                        .unwrap_or_else(|_| {
                            Err("Unable to initialize screen capture.".to_owned().into())
                        }),
                    Err(error) => Err(CaptureError {
                        interrupted: error.interrupted,
                        recoverable: error.recoverable,
                        message: error.message.clone(),
                    }),
                };
                match result {
                    Ok(desktop) => {
                        if request.gate.enabled() {
                            request.event(1, "Screen capture started.");
                        }
                        current = Some(ActiveCapture { request, desktop });
                        next_tick = Instant::now();
                    }
                    Err(error) => {
                        request.ended(Some(&error));
                        self.forget(&request);
                    }
                }
                continue;
            }
            let tick_started = Instant::now();
            if let Some(capture) = current.as_mut() {
                let canvas = capture.desktop.canvas;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    capture.desktop.advance(&capture.request, Instant::now, || {
                        DesktopSource::new(Some(canvas))
                    })
                }))
                .unwrap_or_else(|_| Err("Screen capture failed unexpectedly.".to_owned().into()));
                if let Err(error) = result {
                    let failed_capture = current.take();
                    if let Some(capture) = failed_capture {
                        self.finish(capture, Some(&error));
                    }
                }
            }
            // Limit capture starts to 60 fps. A slow tick can continue as soon
            // as it finishes; missed opportunities never form a catch-up queue.
            next_tick = tick_started + FRAME_INTERVAL;
        }
        if let Some(capture) = current.take() {
            self.finish(capture, None);
        }
        let pending = {
            let mut control = lock(&self.control);
            control.requested = None;
            std::mem::take(&mut control.commands)
        };
        for request in pending {
            request.ended(None);
        }
        drop(apartment);
    }

    fn finish(&self, mut capture: ActiveCapture, error: Option<&CaptureError>) {
        capture.request.gate.disable();
        // Draining cannot deliver frames after this point. Retained samples own
        // immutable data and remain usable after all encoder COM objects drop.
        let _ = catch_unwind(AssertUnwindSafe(|| capture.desktop.encoder.finish()));
        let request = Arc::clone(&capture.request);
        drop(capture);
        request.ended(error);
        self.forget(&request);
    }
}

struct ActiveCapture {
    request: Arc<Request>,
    desktop: DesktopCapture,
}

/// Starts a new capture generation asynchronously, replacing any prior stream.
#[cfg(test)]
pub fn start(generation: u64, frame: FrameCallback, event: EventCallback) {
    start_internal(generation, frame, None, event);
}

/// Records the active speakers/headphones alongside the desktop. Audio device
/// failures are best effort and do not stop the video generation.
pub fn start_with_audio(
    generation: u64,
    frame: FrameCallback,
    audio: AudioCallback,
    event: EventCallback,
) {
    start_internal(generation, frame, Some(Arc::new(audio)), event);
}

fn start_internal(
    generation: u64,
    frame: FrameCallback,
    audio: Option<AudioSink>,
    event: EventCallback,
) {
    let manager = match MANAGER.get_or_init(Manager::new) {
        Ok(manager) => manager,
        Err(error) => {
            event(generation, 3, error);
            return;
        }
    };
    let request = Arc::new(Request {
        generation,
        event,
        gate: Arc::new(FrameGate {
            active: Mutex::new(true),
            generation,
            callback: frame,
            audio,
        }),
        ending: AtomicU8::new(0),
        recover: AtomicBool::new(false),
    });
    let previous = {
        let mut control = lock(&manager.control);
        if control.shutdown {
            drop(control);
            event(generation, 3, "The screen capture worker has shut down.");
            return;
        }
        if control
            .requested
            .as_ref()
            .is_some_and(|current| current.generation == generation)
        {
            return;
        }
        control.commands.push_back(Arc::clone(&request));
        control.requested.replace(request)
    };
    if let Some(previous) = previous {
        previous.request_end(2);
    }
    manager.wake.notify_one();
}

fn request_end(generation: Option<u64>, kind: u8) {
    let Some(Ok(manager)) = MANAGER.get() else {
        return;
    };
    let request = lock(&manager.control).requested.clone();
    if let Some(request) = request
        .filter(|request| generation.is_none_or(|generation| generation == request.generation))
    {
        request.request_end(kind);
        manager.wake.notify_one();
    }
}

/// Returns only after any in-flight frame callback has completed.
pub fn stop(generation: u64) {
    request_end(Some(generation), 2);
}

/// Stops on sleep/session interruptions and rebuilds the source on display changes.
pub fn interrupt(reason: Interruption) {
    if reason == Interruption::Expected {
        request_end(None, 4);
    } else if let Some(Ok(manager)) = MANAGER.get() {
        if let Some(request) = lock(&manager.control).requested.as_ref() {
            request.request_recovery();
        }
        manager.wake.notify_one();
    }
}

/// Joins the worker before process exit so COM teardown finishes on its thread.
pub fn shutdown() {
    let Some(Ok(manager)) = MANAGER.get() else {
        return;
    };
    let request = {
        let mut control = lock(&manager.control);
        control.shutdown = true;
        control.requested.clone()
    };
    if let Some(request) = request {
        request.request_end(2);
    }
    manager.wake.notify_one();
    let worker = lock(&manager.worker).take();
    if let Some(worker) = worker {
        let _ = worker.join();
    }
}

struct FrameLease<'a> {
    duplication: &'a IDXGIOutputDuplication,
    active: bool,
}

impl Drop for FrameLease<'_> {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: A successful AcquireNextFrame established this lease.
            let _ = unsafe { self.duplication.ReleaseFrame() };
        }
    }
}

struct MapLease<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
}

impl Drop for MapLease<'_> {
    fn drop(&mut self) {
        // SAFETY: This guard exists only after Map succeeded on this worker.
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

#[derive(Clone, Copy)]
struct Canvas {
    width: usize,
    height: usize,
}

/// Everything tied to the current adapter/output can be discarded independently
/// of the CPU canvas, encoder, generation and replay timeline.
struct DesktopSource {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    rotation: Rotation,
    display_width: usize,
    display_height: usize,
    canvas: Canvas,
    viewport: Viewport,
    pointer: Pointer,
    desktop: Vec<u8>,
    working: Vec<u8>,
    scaled: Vec<u8>,
    xs: Vec<AxisSample>,
    ys: Vec<AxisSample>,
    render_dirty: bool,
    _thread_only: PhantomData<Rc<()>>,
}

fn rotation(value: DXGI_MODE_ROTATION) -> Result<Rotation, CaptureError> {
    match value {
        DXGI_MODE_ROTATION_UNSPECIFIED | DXGI_MODE_ROTATION_IDENTITY => Ok(Rotation::Identity),
        DXGI_MODE_ROTATION_ROTATE90 => Ok(Rotation::Clockwise),
        DXGI_MODE_ROTATION_ROTATE180 => Ok(Rotation::HalfTurn),
        DXGI_MODE_ROTATION_ROTATE270 => Ok(Rotation::Counterclockwise),
        _ => Err("The display rotation is unsupported.".to_owned().into()),
    }
}

impl DesktopSource {
    #[allow(
        clippy::too_many_lines,
        reason = "The ordered DXGI/device initialization transaction preserves the selected adapter and validates dimensions before allocating resources."
    )]
    fn new(canvas: Option<Canvas>) -> Result<Self, CaptureError> {
        // SAFETY: DXGI creation runs on the initialized COM worker.
        let factory: IDXGIFactory1 = os_result(
            unsafe { CreateDXGIFactory1() },
            "Finding graphics adapters",
            false,
        )?;
        // SAFETY: MonitorFromPoint has no borrowed memory or ownership transfer.
        let primary = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        let mut selected = None;
        for adapter_index in 0..u32::MAX {
            // SAFETY: The factory is alive and returns an owned interface.
            let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
                Ok(adapter) => adapter,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => {
                    return Err(windows_error(
                        &error,
                        "Enumerating graphics adapters",
                        false,
                    ))
                }
            };
            for output_index in 0..u32::MAX {
                // SAFETY: The adapter is alive and returns an owned interface.
                let output = match unsafe { adapter.EnumOutputs(output_index) } {
                    Ok(output) => output,
                    Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(error) => return Err(windows_error(&error, "Enumerating displays", false)),
                };
                // SAFETY: GetDesc returns an initialized value for this output.
                let desc = os_result(
                    unsafe { output.GetDesc() },
                    "Reading display information",
                    false,
                )?;
                if desc.AttachedToDesktop.as_bool() && desc.Monitor == primary {
                    selected = Some((adapter, output, desc));
                    break;
                }
            }
            if selected.is_some() {
                break;
            }
        }
        let (adapter, output, desc) = selected
            .ok_or_else(|| CaptureError::from("The primary display is unavailable.".to_owned()))?;
        let display_width =
            i64::from(desc.DesktopCoordinates.right) - i64::from(desc.DesktopCoordinates.left);
        let display_height =
            i64::from(desc.DesktopCoordinates.bottom) - i64::from(desc.DesktopCoordinates.top);
        if !(2..=32768).contains(&display_width) || !(2..=32768).contains(&display_height) {
            return Err("The primary display dimensions are unsupported."
                .to_owned()
                .into());
        }
        let display_width =
            usize::try_from(display_width).map_err(|_| "Invalid display width.".to_owned())?;
        let display_height =
            usize::try_from(display_height).map_err(|_| "Invalid display height.".to_owned())?;
        let canvas = canvas.unwrap_or_else(|| Canvas {
            width: (display_width * 1080 / display_height.max(1080) / 2 * 2).max(2),
            height: (display_height.min(1080) / 2 * 2).max(2),
        });
        let viewport =
            pixels::fit_viewport(display_width, display_height, canvas.width, canvas.height)?;
        let xs = pixels::scale_axis(display_width, viewport.width)?;
        let ys = pixels::scale_axis(display_height, viewport.height)?;
        let mut device = None;
        let mut context = None;
        // SAFETY: Adapter and output buffers remain live for this call; the
        // returned device/context belong exclusively to this capture worker.
        os_result(
            // SAFETY: Live adapter and output locals remain writable; the
            // device/context are used exclusively by this capture worker.
            unsafe {
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[
                        D3D_FEATURE_LEVEL_11_0,
                        D3D_FEATURE_LEVEL_10_1,
                        D3D_FEATURE_LEVEL_10_0,
                    ]),
                    D3D11_SDK_VERSION,
                    Some(&raw mut device),
                    None,
                    Some(&raw mut context),
                )
            },
            "Creating the screen capture device",
            false,
        )?;
        let device = device.ok_or_else(|| "Screen capture did not return a device.".to_owned())?;
        let context =
            context.ok_or_else(|| "Screen capture did not return a device context.".to_owned())?;
        let output1: windows::Win32::Graphics::Dxgi::IDXGIOutput1 =
            os_result(output.cast(), "Opening Desktop Duplication", false)?;
        // SAFETY: The device belongs to the same adapter as the selected output.
        let duplication = os_result(
            unsafe { output1.DuplicateOutput(&device) },
            "Starting screen capture",
            false,
        )?;
        // SAFETY: Duplication owns and initializes this description.
        let rotation = rotation(unsafe { duplication.GetDesc() }.Rotation)?;
        Ok(Self {
            device,
            context,
            duplication,
            staging: None,
            rotation,
            display_width,
            display_height,
            canvas,
            viewport,
            pointer: Pointer::default(),
            desktop: Vec::new(),
            working: Vec::new(),
            scaled: Vec::new(),
            xs,
            ys,
            render_dirty: false,
            _thread_only: PhantomData,
        })
    }
}

trait ScreenSource {
    /// True only after fresh desktop pixels have been copied and released.
    fn acquire(&mut self) -> Result<bool, CaptureError>;
    fn render(&mut self, canvas: &mut Vec<u8>) -> Result<(), CaptureError>;
}

impl ScreenSource for DesktopSource {
    fn render(&mut self, canvas: &mut Vec<u8>) -> Result<(), CaptureError> {
        if self.render_dirty && !self.desktop.is_empty() {
            let source = if self.pointer.visible && !self.pointer.shape.is_empty() {
                self.working
                    .try_reserve(self.desktop.len().saturating_sub(self.working.len()))
                    .map_err(|error| {
                        format!("Unable to allocate the cursor composition image: {error}")
                    })?;
                self.working.clone_from(&self.desktop);
                pixels::composite_pointer(
                    &mut self.working,
                    self.display_width,
                    self.display_height,
                    &self.pointer,
                )?;
                &self.working
            } else {
                &self.desktop
            };
            pixels::scale_desktop(
                source,
                self.display_width,
                self.display_height,
                &self.xs,
                &self.ys,
                &mut self.scaled,
            )?;
            pixels::fit_scaled_desktop(
                &self.scaled,
                self.viewport,
                self.canvas.width,
                self.canvas.height,
                canvas,
            )?;
            self.render_dirty = false;
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Frame and mapping leases surround the complete readback transaction; every early return releases the acquired OS resources."
    )]
    fn acquire(&mut self) -> Result<bool, CaptureError> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: Both output pointers reference initialized writable locals.
        match unsafe {
            self.duplication
                .AcquireNextFrame(0, &raw mut info, &raw mut resource)
        } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(error) => return Err(windows_error(&error, "Reading the screen image", true)),
        }
        let mut frame_lease = FrameLease {
            duplication: &self.duplication,
            active: true,
        };
        if info.LastMouseUpdateTime != 0 {
            self.pointer.visible = info.PointerPosition.Visible.as_bool();
            self.pointer.x = info.PointerPosition.Position.x;
            self.pointer.y = info.PointerPosition.Position.y;
            self.render_dirty = true;
        }
        if info.PointerShapeBufferSize != 0 {
            if info.PointerShapeBufferSize > MAX_CURSOR_BYTES {
                return Err("The cursor image is unexpectedly large.".to_owned().into());
            }
            let shape_size = usize::try_from(info.PointerShapeBufferSize)
                .map_err(|_| "Invalid cursor size.".to_owned())?;
            self.pointer
                .shape
                .try_reserve(shape_size.saturating_sub(self.pointer.shape.len()))
                .map_err(|error| format!("Unable to allocate the cursor image: {error}"))?;
            self.pointer.shape.resize(shape_size, 0);
            let mut needed = 0;
            let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: The buffer is exactly the requested size; all output
            // pointers remain writable through this call and no pointer escapes.
            os_result(
                // SAFETY: Shape storage has the requested capacity and both
                // outputs are writable locals valid until the call returns.
                unsafe {
                    self.duplication.GetFramePointerShape(
                        info.PointerShapeBufferSize,
                        self.pointer.shape.as_mut_ptr().cast(),
                        &raw mut needed,
                        &raw mut shape_info,
                    )
                },
                "Reading the cursor image",
                true,
            )?;
            if needed > info.PointerShapeBufferSize {
                return Err("The cursor image has invalid data.".to_owned().into());
            }
            self.pointer
                .shape
                .truncate(usize::try_from(needed).map_err(|_| "Invalid cursor size.".to_owned())?);
            self.pointer.kind = match shape_info.Type {
                value
                    if value
                        == u32::try_from(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0)
                            .map_err(|_| "Invalid cursor type.".to_owned())? =>
                {
                    CursorKind::Monochrome
                }
                value
                    if value
                        == u32::try_from(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0)
                            .map_err(|_| "Invalid cursor type.".to_owned())? =>
                {
                    CursorKind::Color
                }
                value
                    if value
                        == u32::try_from(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0)
                            .map_err(|_| "Invalid cursor type.".to_owned())? =>
                {
                    CursorKind::MaskedColor
                }
                _ => return Err("Unsupported cursor shape.".to_owned().into()),
            };
            self.pointer.width = usize::try_from(shape_info.Width)
                .map_err(|_| "Invalid cursor width.".to_owned())?;
            self.pointer.height = usize::try_from(shape_info.Height)
                .map_err(|_| "Invalid cursor height.".to_owned())?;
            self.pointer.pitch = usize::try_from(shape_info.Pitch)
                .map_err(|_| "Invalid cursor pitch.".to_owned())?;
            pixels::validate_pointer(&self.pointer)?;
            self.render_dirty = true;
        }
        let fresh = info.LastPresentTime != 0 || self.desktop.is_empty();
        if fresh {
            let resource =
                resource.ok_or_else(|| "Screen capture supplied no image resource.".to_owned())?;
            let texture: ID3D11Texture2D =
                os_result(resource.cast(), "Opening the captured image", true)?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: The texture is live and desc is a writable local.
            unsafe { texture.GetDesc(&raw mut desc) };
            let (expected_width, expected_height) = if self.rotation.portrait() {
                (self.display_height, self.display_width)
            } else {
                (self.display_width, self.display_height)
            };
            if usize::try_from(desc.Width).ok() != Some(expected_width)
                || usize::try_from(desc.Height).ok() != Some(expected_height)
            {
                return Err(CaptureError {
                    interrupted: false,
                    recoverable: true,
                    message: "The display size changed.".into(),
                });
            }
            if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM || desc.SampleDesc.Count != 1 {
                return Err("Screen capture supplied an unsupported image format."
                    .to_owned()
                    .into());
            }
            if self.staging.is_none() {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    MipLevels: 1,
                    ArraySize: 1,
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0)
                        .map_err(|_| "Invalid CPU read flag.".to_owned())?,
                    MiscFlags: 0,
                    ..desc
                };
                // SAFETY: The fully initialized description requests a CPU
                // readback resource, with the output stored in self.staging.
                os_result(
                    // SAFETY: The description is initialized, and staging is
                    // an owned optional interface output on this worker.
                    unsafe {
                        self.device.CreateTexture2D(
                            &raw const staging_desc,
                            None,
                            Some(&raw mut self.staging),
                        )
                    },
                    "Creating the image readback surface",
                    true,
                )?;
            }
            let staging = self
                .staging
                .as_ref()
                .ok_or_else(|| "Screen capture supplied no readback surface.".to_owned())?;
            // SAFETY: The two textures have matching dimensions/format and all
            // immediate-context operations occur on this capture worker.
            unsafe { self.context.CopyResource(staging, &texture) };
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: Staging was created with CPU read access and mapped is writable.
            os_result(
                // SAFETY: Staging supports CPU read access and mapped is a
                // writable local; the guard below balances successful Map.
                unsafe {
                    self.context
                        .Map(staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
                },
                "Reading captured pixels",
                true,
            )?;
            let map_lease = MapLease {
                context: &self.context,
                texture: staging,
            };
            let stride = usize::try_from(mapped.RowPitch)
                .map_err(|_| "Invalid captured image stride.".to_owned())?;
            let required = stride
                .checked_mul(expected_height)
                .ok_or_else(|| "The captured image is too large.".to_owned())?;
            if mapped.pData.is_null()
                || stride < expected_width * 4
                || required > isize::MAX.unsigned_abs()
            {
                return Err("Screen capture supplied invalid mapped pixels."
                    .to_owned()
                    .into());
            }
            // SAFETY: A successful D3D11 Map guarantees RowPitch * texture
            // height bytes until Unmap; validated BGRA dimensions and stride
            // bound all pixel reads. The borrowed slice never escapes the guard.
            let source = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), required) };
            pixels::rotate_desktop(
                source,
                stride,
                expected_width,
                expected_height,
                self.rotation,
                &mut self.desktop,
            )?;
            drop(map_lease);
            self.render_dirty = true;
        }
        frame_lease.active = false;
        // SAFETY: This frame lease is released exactly once before checking device state.
        os_result(
            // SAFETY: Exactly one successful frame acquire is being released.
            unsafe { self.duplication.ReleaseFrame() },
            "Releasing the captured image",
            true,
        )?;
        // SAFETY: The device remains live and is accessed only on this worker.
        os_result(
            // SAFETY: This device remains alive on its capture worker.
            unsafe { self.device.GetDeviceRemovedReason() },
            "Checking the screen capture device",
            true,
        )?;
        Ok(fresh)
    }
}

struct OutputTimeline {
    started: Instant,
    paused_at: Option<Instant>,
    paused_duration: Duration,
    previous_pts: i64,
    pauses: Vec<(Instant, Instant)>,
    audio_end: i64,
}

impl OutputTimeline {
    const fn new(started: Instant) -> Self {
        Self {
            started,
            paused_at: None,
            paused_duration: Duration::ZERO,
            previous_pts: -1,
            pauses: Vec::new(),
            audio_end: 0,
        }
    }

    const fn pause(&mut self, at: Instant) {
        if self.paused_at.is_none() {
            self.paused_at = Some(at);
        }
    }

    fn resume(&mut self, now: Instant) {
        if let Some(paused_at) = self.paused_at.take() {
            self.paused_duration += now.saturating_duration_since(paused_at);
            self.pauses.push((paused_at, now));
        }
    }

    fn next_pts(&mut self, now: Instant) -> Result<i64, CaptureError> {
        let elapsed = now
            .saturating_duration_since(self.started)
            .saturating_sub(self.paused_duration);
        let pts = i64::try_from(elapsed.as_nanos() / 100)
            .map_err(|_| "The capture timeline is too long.".to_owned())?
            .max(if self.previous_pts < 0 {
                0
            } else {
                self.previous_pts.saturating_add(FRAME_TICKS)
            });
        self.previous_pts = pts;
        Ok(pts)
    }

    fn elapsed_at(&self, now: Instant) -> Duration {
        let mut elapsed = now.saturating_duration_since(self.started);
        for &(start, end) in &self.pauses {
            elapsed = elapsed.saturating_sub(now.min(end).saturating_duration_since(start));
        }
        if let Some(start) = self.paused_at {
            elapsed = elapsed.saturating_sub(now.saturating_duration_since(start));
        }
        elapsed
    }

    /// A queued WASAPI packet can cross a video outage or have been captured
    /// before a slow readback finished. Trim by PCM frames using the full pause
    /// history, then map its retained parts through the same video timeline.
    fn audio_samples(&mut self, packet: &Packet) -> Vec<AudioSample> {
        let frames = packet.bytes.len() / audio::FRAME_BYTES;
        let frame_at = |at: Instant| -> usize {
            let count = at
                .saturating_duration_since(packet.recorded_at)
                .as_nanos()
                .saturating_mul(u128::from(audio::SAMPLE_RATE))
                .div_ceil(1_000_000_000);
            usize::try_from(count).unwrap_or(frames).min(frames)
        };
        let mut spans = vec![(frame_at(self.started), frames)];
        for (start, end) in self
            .pauses
            .iter()
            .copied()
            .chain(self.paused_at.map(|start| {
                (
                    start,
                    packet.recorded_at + audio::frame_duration(u64::try_from(frames).unwrap_or(0)),
                )
            }))
        {
            let first = frame_at(start);
            let last = frame_at(end);
            spans = spans
                .into_iter()
                .flat_map(|(from, to)| {
                    [(from, to.min(first)), (from.max(last), to)]
                        .into_iter()
                        .filter(|&(from, to)| from < to)
                })
                .collect();
        }
        let mut samples = Vec::new();
        for (mut first, last) in spans {
            let at = packet.recorded_at + audio::frame_duration(u64::try_from(first).unwrap_or(0));
            let Ok(mut pts) = i64::try_from(self.elapsed_at(at).as_nanos() / 100) else {
                continue;
            };
            if pts < self.audio_end {
                let overlap = u128::try_from(self.audio_end - pts)
                    .unwrap_or(0)
                    .saturating_mul(u128::from(audio::SAMPLE_RATE))
                    .div_ceil(10_000_000);
                let skipped = usize::try_from(overlap).unwrap_or(last).min(last - first);
                first += skipped;
                let at =
                    packet.recorded_at + audio::frame_duration(u64::try_from(first).unwrap_or(0));
                let Ok(next_pts) = i64::try_from(self.elapsed_at(at).as_nanos() / 100) else {
                    continue;
                };
                pts = next_pts.max(self.audio_end);
            }
            if first >= last {
                continue;
            }
            let duration = i64::try_from(
                u128::try_from(last - first).unwrap_or(0) * 10_000_000
                    / u128::from(audio::SAMPLE_RATE),
            )
            .unwrap_or(0);
            if duration <= 0 {
                continue;
            }
            self.audio_end = pts.saturating_add(duration);
            samples.push(AudioSample {
                bytes: Arc::from(
                    &packet.bytes[first * audio::FRAME_BYTES..last * audio::FRAME_BYTES],
                ),
                pts,
                duration,
            });
        }
        samples
    }
}

struct Recovery {
    started: Instant,
    next_retry: Instant,
    backoff: Duration,
    unavailable: bool,
    message: String,
}

impl Recovery {
    fn new(now: Instant, message: String) -> Self {
        Self {
            started: now,
            next_retry: now + FIRST_RETRY,
            backoff: FIRST_RETRY,
            unavailable: false,
            message,
        }
    }

    fn failed_attempt(&mut self, now: Instant, message: String) {
        self.message = message;
        self.next_retry = now + self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_RETRY);
    }
}

/// A stable encoding stream with a replaceable desktop source. Keeping the
/// encoder avoids broken H.264 references or incompatible replay sample formats.
struct DesktopCapture<S = DesktopSource> {
    source: Option<S>,
    source_since: Instant,
    canvas: Canvas,
    encoded: Vec<u8>,
    encoder: Encoder,
    timeline: OutputTimeline,
    recovery: Option<Recovery>,
    audio: Option<LoopbackCapture>,
}

impl DesktopCapture {
    fn new(request: &Request) -> Result<Self, CaptureError> {
        let source = DesktopSource::new(None)?;
        let canvas = source.canvas;
        let gate = Arc::clone(&request.gate);
        let sink: FrameSink = Arc::new(move |_generation, sample| gate.deliver(sample));
        let encoder = Encoder::new(
            u32::try_from(canvas.width).map_err(|_| "Invalid output width.".to_owned())?,
            u32::try_from(canvas.height).map_err(|_| "Invalid output height.".to_owned())?,
            request.generation,
            sink,
        )?;
        let now = Instant::now();
        Ok(Self {
            source: Some(source),
            source_since: now,
            canvas,
            encoded: Vec::new(),
            encoder,
            timeline: OutputTimeline::new(now),
            recovery: None,
            audio: request
                .gate
                .audio
                .as_ref()
                .map(|_| LoopbackCapture::new(now)),
        })
    }
}

impl<S: ScreenSource> DesktopCapture<S> {
    fn begin_recovery(&mut self, request: &Request, now: Instant, message: String) {
        // Release the old duplication and D3D device before creating another.
        // The retained BGRA canvas has no dependency on those COM resources.
        self.source = None;
        if self.recovery.is_none() {
            request.progress(6, &format!("{message} Retrying screen capture."));
            self.recovery = Some(Recovery::new(now, message));
        }
    }

    fn update_availability(&mut self, request: &Request, now: Instant) {
        if let Some(recovery) = self.recovery.as_mut() {
            if !recovery.unavailable && now >= recovery.started + FREEZE_GRACE {
                recovery.unavailable = true;
                // Use the grace boundary even if this tick was delayed. The
                // remaining outage must not inflate or evict replay history.
                self.timeline.pause(recovery.started + FREEZE_GRACE);
                request.progress(
                    8,
                    &format!("{} Still retrying screen capture.", recovery.message),
                );
            }
        }
    }

    fn rebuild_source(
        &mut self,
        clock: &impl Fn() -> Instant,
        create: impl FnOnce() -> Result<S, CaptureError>,
    ) -> Result<(), CaptureError> {
        let now = clock();
        if self.source.is_some()
            || self
                .recovery
                .as_ref()
                .is_none_or(|recovery| now < recovery.next_retry)
        {
            return Ok(());
        }
        match create() {
            Ok(source) => {
                self.source = Some(source);
                self.source_since = clock();
            }
            // A disconnected session remains a terminal interruption.
            Err(error) if error.interrupted && !error.recoverable => return Err(error),
            Err(error) => {
                if let Some(recovery) = self.recovery.as_mut() {
                    recovery.failed_attempt(clock(), error.message);
                }
            }
        }
        Ok(())
    }

    fn read_source(
        &mut self,
        request: &Request,
        clock: &impl Fn() -> Instant,
    ) -> Result<(), CaptureError> {
        let attempt_started = clock();
        let Some(source) = self.source.as_mut() else {
            return Ok(());
        };
        match source.acquire() {
            Ok(true) => {
                source.render(&mut self.encoded)?;
                // Device reconstruction/readback can block. Account for that
                // time before declaring the first fresh desktop recovered.
                let now = clock();
                self.update_availability(request, now);
                if self.recovery.take().is_some() {
                    self.timeline.resume(now);
                    request.progress(7, "Screen capture recovered.");
                }
            }
            Ok(false) if self.recovery.is_none() => source.render(&mut self.encoded)?,
            Ok(false) if clock() >= self.source_since + FRESH_IMAGE_TIMEOUT => {
                self.source = None;
                if let Some(recovery) = self.recovery.as_mut() {
                    recovery.failed_attempt(clock(), "Waiting for a fresh screen image.".into());
                }
            }
            Ok(false) => {}
            Err(error) if error.recoverable => {
                let now = clock();
                self.begin_recovery(request, attempt_started, error.message.clone());
                if let Some(recovery) = self.recovery.as_mut() {
                    recovery.failed_attempt(now, error.message);
                }
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn advance(
        &mut self,
        request: &Request,
        clock: impl Fn() -> Instant,
        create: impl FnOnce() -> Result<S, CaptureError>,
    ) -> Result<(), CaptureError> {
        if request.ending.load(Ordering::Acquire) != 0 || !request.gate.enabled() {
            return Ok(());
        }
        let now = clock();
        if request.recover.swap(false, Ordering::AcqRel) {
            self.begin_recovery(request, now, DISPLAY_CHANGED.into());
        }
        self.update_availability(request, now);
        self.rebuild_source(&clock, create)?;
        self.read_source(request, &clock)?;
        let now = clock();
        self.update_availability(request, now);
        if let Some(audio) = self.audio.as_mut() {
            let (packets, warning) = audio.poll(now);
            if let Some(message) = warning {
                request.progress(9, &message);
            }
            for packet in packets {
                for sample in self.timeline.audio_samples(&packet) {
                    request.gate.deliver_audio(sample);
                }
            }
        }
        if self.encoded.is_empty()
            && self.recovery.is_none()
            && now >= self.timeline.started + Duration::from_secs(10)
        {
            self.begin_recovery(
                request,
                now,
                "Screen capture did not provide an image.".into(),
            );
        }
        if !self.encoded.is_empty()
            && self
                .recovery
                .as_ref()
                .is_none_or(|recovery| !recovery.unavailable)
            && request.gate.enabled()
        {
            let pts = self.timeline.next_pts(now)?;
            // Feed the last good raw image through the same encoder. Repeating
            // compressed bytes would break timestamps and inter-frame references.
            self.encoder
                .encode(&self.encoded, self.canvas.width * 4, pts)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        collections::BTreeMap,
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };
    use windows::Win32::{
        Foundation::{E_ACCESSDENIED, E_FAIL},
        Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET},
    };

    #[test]
    fn interruption_classification_depends_on_live_capture() {
        for code in [
            DXGI_ERROR_ACCESS_LOST,
            DXGI_ERROR_SESSION_DISCONNECTED,
            DXGI_ERROR_DEVICE_REMOVED,
            DXGI_ERROR_DEVICE_RESET,
            E_ACCESSDENIED,
            E_FAIL,
        ] {
            for runtime in [false, true] {
                let error = windows_error(
                    &WindowsError::from_hresult(code),
                    "Synthetic capture operation",
                    runtime,
                );
                assert_eq!(
                    error.interrupted,
                    code == DXGI_ERROR_SESSION_DISCONNECTED
                        || (runtime && code == DXGI_ERROR_ACCESS_LOST)
                );
                assert!(error.message.contains("HRESULT 0x"));
                assert_eq!(
                    error.recoverable,
                    runtime && code != DXGI_ERROR_SESSION_DISCONNECTED
                );
            }
        }
    }

    static TERMINAL_EVENTS: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "The two delayed capture scenarios verify one shared outage timeline without real desktop dependencies."
    )]
    fn slow_readback_and_reconstruction_do_not_add_the_outage_to_replay() -> Result<(), String> {
        let _com = ComApartment::new().map_err(|error| error.message)?;
        for slow_readback in [true, false] {
            let base = Instant::now();
            let clock = Rc::new(Cell::new(base));
            let canvas = Canvas {
                width: 64,
                height: 36,
            };
            let request = Request {
                generation: 124,
                event: |_, _, _| {},
                gate: Arc::new(FrameGate {
                    active: Mutex::new(true),
                    generation: 124,
                    callback: |_, _| {},
                    audio: None,
                }),
                ending: AtomicU8::new(0),
                recover: AtomicBool::new(false),
            };
            let mut capture = DesktopCapture {
                source: Some(SyntheticSource {
                    reads: VecDeque::from([
                        Ok(true),
                        Err(windows_error(
                            &WindowsError::from_hresult(DXGI_ERROR_ACCESS_LOST),
                            "Synthetic readback",
                            true,
                        )),
                    ]),
                    delay: None,
                    source_width: 64,
                    source_height: 36,
                    canvas,
                    dropped: Arc::new(AtomicBool::new(false)),
                }),
                source_since: base,
                canvas,
                encoded: Vec::new(),
                encoder: Encoder::new(64, 36, 124, Arc::new(|_, _| {}))?,
                timeline: OutputTimeline::new(base),
                recovery: None,
                audio: None,
            };
            capture
                .advance(
                    &request,
                    || clock.get(),
                    || Err("Unexpected rebuild.".to_owned().into()),
                )
                .map_err(|error| error.message)?;
            if slow_readback {
                let source = capture.source.as_mut().ok_or("Missing test source")?;
                source.delay = Some((Rc::clone(&clock), Duration::from_secs(20)));
            }
            clock.set(base + FRAME_INTERVAL);
            capture
                .advance(
                    &request,
                    || clock.get(),
                    || Err("Unexpected rebuild.".to_owned().into()),
                )
                .map_err(|error| error.message)?;
            let grace_end = base + FRAME_INTERVAL + FREEZE_GRACE;
            assert_eq!(
                capture.recovery.as_ref().map(|recovery| recovery.started),
                Some(base + FRAME_INTERVAL)
            );
            if slow_readback {
                assert_eq!(capture.timeline.paused_at, Some(grace_end));
                assert_eq!(
                    capture.timeline.previous_pts, 0,
                    "No filler after stalled readback exceeded grace"
                );
            }
            clock.set(clock.get() + FIRST_RETRY);
            capture
                .advance(
                    &request,
                    || clock.get(),
                    || {
                        if !slow_readback {
                            // A slow successful reconstruction also crosses the grace.
                            clock.set(clock.get() + Duration::from_secs(20));
                        }
                        Ok(SyntheticSource {
                            reads: VecDeque::from([Ok(true)]),
                            delay: None,
                            source_width: 64,
                            source_height: 36,
                            canvas,
                            dropped: Arc::new(AtomicBool::new(false)),
                        })
                    },
                )
                .map_err(|error| error.message)?;
            assert!(capture.recovery.is_none());
            assert!(capture.timeline.paused_at.is_none());
            assert_eq!(capture.timeline.previous_pts, 50_166_666);
            capture.encoder.finish()?;
        }
        Ok(())
    }

    #[test]
    fn display_recovery_keeps_frames_enabled_but_deliberate_endings_win() {
        for ending in [2, 4] {
            let request = Request {
                generation: 7,
                event: |_, _, _| {},
                gate: Arc::new(FrameGate {
                    active: Mutex::new(true),
                    generation: 7,
                    callback: |_, _| {},
                    audio: None,
                }),
                ending: AtomicU8::new(0),
                recover: AtomicBool::new(false),
            };
            request.request_recovery();
            request.request_recovery();
            assert!(request.gate.enabled());
            assert_eq!(request.ending.load(Ordering::Acquire), 0);
            assert!(request.recover.swap(false, Ordering::AcqRel));
            assert!(!request.recover.swap(false, Ordering::AcqRel));
            request.request_end(ending);
            request.request_recovery();
            assert!(!request.gate.enabled());
            assert_eq!(request.ending.load(Ordering::Acquire), ending);
            assert!(!request.recover.load(Ordering::Acquire));
        }
    }

    #[test]
    fn paused_timeline_excludes_long_outages_and_never_overlaps_samples() -> Result<(), CaptureError>
    {
        let base = Instant::now();
        let mut timeline = OutputTimeline::new(base);
        assert_eq!(timeline.next_pts(base)?, 0);
        assert_eq!(
            timeline.next_pts(base + Duration::from_secs(1))?,
            10_000_000
        );
        timeline.pause(base + Duration::from_secs(5));
        timeline.pause(base + Duration::from_secs(10));
        timeline.resume(base + Duration::from_secs(20));
        assert_eq!(
            timeline.next_pts(base + Duration::from_secs(20))?,
            50_000_000
        );
        assert_eq!(
            timeline.next_pts(base + Duration::from_secs(20))?,
            50_000_000 + FRAME_TICKS
        );
        Ok(())
    }

    #[test]
    fn queued_audio_is_trimmed_at_video_pause_boundaries() {
        let base = Instant::now();
        let mut timeline = OutputTimeline::new(base);
        timeline.pause(base + Duration::from_secs(5));
        let before_pause = timeline.audio_samples(&Packet {
            bytes: vec![1; 480 * audio::FRAME_BYTES],
            recorded_at: base + Duration::from_millis(4995),
        });
        assert_eq!(before_pause.len(), 1);
        assert_eq!(before_pause[0].bytes.len(), 240 * audio::FRAME_BYTES);
        assert_eq!(before_pause[0].pts, 49_950_000);
        assert_eq!(before_pause[0].duration, 50_000);
        assert!(timeline
            .audio_samples(&Packet {
                bytes: vec![2; 480 * audio::FRAME_BYTES],
                recorded_at: base + Duration::from_secs(10),
            })
            .is_empty());
        timeline.resume(base + Duration::from_secs(20));
        let after_pause = timeline.audio_samples(&Packet {
            bytes: vec![3; 960 * audio::FRAME_BYTES],
            recorded_at: base + Duration::from_millis(19995),
        });
        assert_eq!(after_pause.len(), 1);
        assert_eq!(after_pause[0].bytes.len(), 720 * audio::FRAME_BYTES);
        assert_eq!(after_pause[0].pts, 50_000_000);
        assert_eq!(after_pause[0].duration, 150_000);
    }

    #[test]
    fn audio_clock_resets_cannot_overlap_retained_samples() {
        let base = Instant::now();
        let mut timeline = OutputTimeline::new(base);
        let first = timeline.audio_samples(&Packet {
            bytes: vec![1; 960 * audio::FRAME_BYTES],
            recorded_at: base.checked_sub(Duration::from_millis(10)).unwrap_or(base),
        });
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].bytes.len(), 480 * audio::FRAME_BYTES);
        assert_eq!(first[0].pts, 0);
        assert_eq!(first[0].duration, 100_000);
        let overlapping = timeline.audio_samples(&Packet {
            bytes: vec![2; 480 * audio::FRAME_BYTES],
            recorded_at: base + Duration::from_millis(5),
        });
        assert_eq!(overlapping.len(), 1);
        assert_eq!(overlapping[0].bytes.len(), 240 * audio::FRAME_BYTES);
        assert_eq!(overlapping[0].pts, 100_000);
        assert_eq!(overlapping[0].duration, 50_000);
    }

    struct SyntheticSource {
        reads: VecDeque<Result<bool, CaptureError>>,
        delay: Option<(Rc<Cell<Instant>>, Duration)>,
        source_width: usize,
        source_height: usize,
        canvas: Canvas,
        dropped: Arc<AtomicBool>,
    }

    impl ScreenSource for SyntheticSource {
        fn acquire(&mut self) -> Result<bool, CaptureError> {
            if let Some((clock, delay)) = self.delay.take() {
                clock.set(clock.get() + delay);
            }
            self.reads.pop_front().unwrap_or(Ok(false))
        }

        fn render(&mut self, output: &mut Vec<u8>) -> Result<(), CaptureError> {
            let viewport = pixels::fit_viewport(
                self.source_width,
                self.source_height,
                self.canvas.width,
                self.canvas.height,
            )?;
            let mut scaled = vec![0; pixels::image_len(viewport.width, viewport.height)?];
            scaled.as_chunks_mut::<4>().0.fill([32, 128, 224, 255]);
            pixels::fit_scaled_desktop(
                &scaled,
                viewport,
                self.canvas.width,
                self.canvas.height,
                output,
            )?;
            Ok(())
        }
    }

    impl Drop for SyntheticSource {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    static RECOVERY_EVENTS: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());

    fn recovery_event(_generation: u64, kind: i32, message: &str) {
        lock(&RECOVERY_EVENTS).push((kind, message.to_owned()));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "One synthetic source episode verifies retry pacing, retained encoder/timeline, cancellation and fully decoded output without accessing the desktop."
    )]
    fn failed_readback_recovers_without_resetting_the_encoded_stream() -> Result<(), String> {
        let _com = ComApartment::new().map_err(|error| error.message)?;
        let base = Instant::now();
        let canvas = Canvas {
            width: 64,
            height: 36,
        };
        let request = Request {
            generation: 123,
            event: recovery_event,
            gate: Arc::new(FrameGate {
                active: Mutex::new(true),
                generation: 123,
                callback: |_, _| {},
                audio: None,
            }),
            ending: AtomicU8::new(0),
            recover: AtomicBool::new(false),
        };
        let received = Arc::new(Mutex::new(Vec::new()));
        let samples = Arc::clone(&received);
        let encoder = Encoder::new(
            64,
            36,
            123,
            Arc::new(move |generation, sample| {
                assert_eq!(generation, 123);
                lock(&samples).push(sample);
            }),
        )?;
        let old_source_dropped = Arc::new(AtomicBool::new(false));
        let source = SyntheticSource {
            reads: VecDeque::from([
                Ok(true),
                Err(windows_error(
                    &WindowsError::from_hresult(DXGI_ERROR_ACCESS_LOST),
                    "Reading captured pixels",
                    true,
                )),
            ]),
            delay: None,
            source_width: 64,
            source_height: 36,
            canvas,
            dropped: Arc::clone(&old_source_dropped),
        };
        let mut capture = DesktopCapture {
            source: Some(source),
            source_since: base,
            canvas,
            encoded: Vec::new(),
            encoder,
            timeline: OutputTimeline::new(base),
            recovery: None,
            audio: None,
        };
        let mut attempts = Vec::new();
        let mut emitted_before_pause = -1;
        for index in 0..360 {
            let now = base + FRAME_INTERVAL * index;
            if index == 200 || index == 280 {
                request.request_recovery();
            }
            capture
                .advance(
                    &request,
                    || now,
                    || {
                        assert!(old_source_dropped.load(Ordering::Acquire));
                        attempts.push(now);
                        if attempts.len() == 3 {
                            // Recreating duplication without receiving pixels must
                            // neither reset the grace nor announce successful recovery.
                            return Ok(SyntheticSource {
                                reads: VecDeque::new(),
                                delay: None,
                                source_width: 64,
                                source_height: 36,
                                canvas,
                                dropped: Arc::new(AtomicBool::new(false)),
                            });
                        }
                        Err(windows_error(
                            &WindowsError::from_hresult(E_ACCESSDENIED),
                            "Rebuilding a synthetic source",
                            false,
                        ))
                    },
                )
                .map_err(|error| error.message)?;
            if index == 302 {
                emitted_before_pause = capture.timeline.previous_pts;
            }
        }
        assert!(
            attempts.len() > 5 && attempts.len() < 15,
            "Retries must use bounded backoff"
        );
        for pair in attempts.windows(2) {
            assert!(pair[1].duration_since(pair[0]) >= FIRST_RETRY);
        }
        assert_eq!(capture.timeline.previous_pts, emitted_before_pause);
        assert!(capture
            .recovery
            .as_ref()
            .is_some_and(|recovery| recovery.unavailable));
        assert_eq!(capture.encoded.len(), 64 * 36 * 4);

        // Successful reconstruction alone is not recovery: no fresh pixels yet.
        let now = base + Duration::from_secs(20);
        capture
            .advance(
                &request,
                || now,
                || {
                    Ok(SyntheticSource {
                        reads: VecDeque::from([Ok(false), Ok(true)]),
                        delay: None,
                        source_width: 36,
                        source_height: 64,
                        canvas,
                        dropped: Arc::new(AtomicBool::new(false)),
                    })
                },
            )
            .map_err(|error| error.message)?;
        assert!(capture.recovery.is_some());
        assert_eq!(capture.timeline.previous_pts, emitted_before_pause);
        for index in 1..61 {
            let now = now + FRAME_INTERVAL * index;
            capture
                .advance(
                    &request,
                    || now,
                    || Err("Unexpected reconstruction.".to_owned().into()),
                )
                .map_err(|error| error.message)?;
        }
        assert!(capture.recovery.is_none());
        assert_eq!(&capture.encoded[..4], &[0, 0, 0, 255]);
        let center = (18 * 64 + 32) * 4;
        assert_eq!(&capture.encoded[center..center + 4], &[32, 128, 224, 255]);
        let events = lock(&RECOVERY_EVENTS);
        assert_eq!(
            events.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            [6, 8, 7]
        );
        assert!(events[0].1.contains("HRESULT 0x887A0026"));
        drop(events);

        // A queued display notification cannot reconstruct after Stop.
        request.request_recovery();
        request.request_end(2);
        let previous_pts = capture.timeline.previous_pts;
        capture
            .advance(
                &request,
                || base + Duration::from_secs(30),
                || Err("Reconstruction after Stop.".to_owned().into()),
            )
            .map_err(|error| error.message)?;
        assert_eq!(capture.timeline.previous_pts, previous_pts);
        assert_eq!(lock(&RECOVERY_EVENTS).len(), 3);
        capture.encoder.finish()?;
        let frames = lock(&received).clone();
        assert!((360..=364).contains(&frames.len()));
        assert!(frames[0].keyframe);
        for pair in frames.windows(2) {
            assert_eq!(pair[0].format, pair[1].format);
            assert!(pair[1].pts >= pair[0].pts + pair[0].duration);
            assert!(pair[1].pts - pair[0].pts < FRAME_TICKS * 2);
        }
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "Optic-recovery-{}-{suffix}.mp4",
            std::process::id()
        ));
        let result = super::super::media::export_samples(&frames, &path)
            .and_then(|()| super::super::media::verify_captured_clip(&path, &frames));
        let _ = std::fs::remove_file(&path);
        result
    }

    fn terminal_event(_generation: u64, kind: i32, message: &str) {
        lock(&TERMINAL_EVENTS).push((kind, message.to_owned()));
    }

    fn observed_ending(kinds: &[u8], error: Option<&CaptureError>) -> (i32, String) {
        let request = Request {
            generation: 7,
            event: terminal_event,
            gate: Arc::new(FrameGate {
                active: Mutex::new(true),
                generation: 7,
                callback: |_, _| {},
                audio: None,
            }),
            ending: AtomicU8::new(0),
            recover: AtomicBool::new(false),
        };
        for &kind in kinds {
            request.request_end(kind);
        }
        request.ended(error);
        assert!(!request.gate.enabled());
        let mut events = lock(&TERMINAL_EVENTS);
        assert_eq!(events.len(), 1);
        events.remove(0)
    }

    #[test]
    fn unexpected_interruptions_preserve_causes_and_expected_endings_stay_quiet() {
        let error = windows_error(
            &WindowsError::from_hresult(DXGI_ERROR_ACCESS_LOST),
            "Reading the screen image",
            true,
        );
        assert_eq!(
            observed_ending(&[], Some(&error)),
            (5, error.message.clone())
        );
        let (kind, message) = observed_ending(&[5], Some(&error));
        assert_eq!(kind, 5);
        assert!(message.contains(DISPLAY_CHANGED));
        assert!(message.contains("Reading the screen image failed (HRESULT 0x887A0026)"));
        assert_eq!(
            observed_ending(&[5], None),
            (5, format!("{DISPLAY_CHANGED} {INTERRUPTED}"))
        );
        for kinds in [&[4][..], &[5, 4], &[4, 5]] {
            assert_eq!(
                observed_ending(kinds, Some(&error)),
                (4, INTERRUPTED.to_owned())
            );
        }
        for kinds in [&[2][..], &[5, 2], &[2, 5]] {
            assert_eq!(
                observed_ending(kinds, Some(&error)),
                (2, STOPPED.to_owned())
            );
        }
    }

    #[derive(Default)]
    struct GateObservation {
        entered: bool,
        release: bool,
        received: usize,
    }

    static GATE_TEST: OnceLock<(Mutex<GateObservation>, Condvar)> = OnceLock::new();

    fn blocking_callback(_generation: u64, _sample: EncodedSample) {
        let (state, changed) =
            GATE_TEST.get_or_init(|| (Mutex::new(GateObservation::default()), Condvar::new()));
        let mut state = lock(state);
        state.received += 1;
        state.entered = true;
        changed.notify_all();
        while !state.release {
            state = changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        drop(state);
    }

    fn sample() -> EncodedSample {
        EncodedSample {
            format: Arc::new(super::super::media::VideoFormat {
                width: 2,
                height: 2,
                sequence_header: Arc::from([]),
            }),
            bytes: Arc::from([1]),
            pts: 0,
            duration: FRAME_TICKS,
            keyframe: true,
        }
    }

    #[test]
    fn stop_gate_waits_for_callback_and_rejects_future_frames() -> Result<(), String> {
        let gate = Arc::new(FrameGate {
            active: Mutex::new(true),
            generation: 7,
            callback: blocking_callback,
            audio: None,
        });
        let delivery_gate = Arc::clone(&gate);
        let delivery = thread::spawn(move || delivery_gate.deliver(sample()));
        let (state, changed) =
            GATE_TEST.get_or_init(|| (Mutex::new(GateObservation::default()), Condvar::new()));
        let mut observed = lock(state);
        while !observed.entered {
            observed = changed
                .wait(observed)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        drop(observed);
        let (completed, completion) = std::sync::mpsc::channel();
        let stopping_gate = Arc::clone(&gate);
        let stopping = thread::spawn(move || {
            stopping_gate.disable();
            let _ = completed.send(());
        });
        assert!(completion.recv_timeout(Duration::from_millis(50)).is_err());
        lock(state).release = true;
        changed.notify_all();
        completion
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        delivery
            .join()
            .map_err(|_| "The frame callback thread panicked.")?;
        stopping.join().map_err(|_| "The stop thread panicked.")?;
        gate.deliver(sample());
        assert_eq!(lock(state).received, 1);
        Ok(())
    }

    #[derive(Default)]
    struct Generation {
        frames: Vec<EncodedSample>,
        received: usize,
        bytes: usize,
        started: u32,
        stopped: u32,
        failed: u32,
        interrupted: u32,
        boundary: bool,
        invalid: bool,
        message: String,
    }

    #[derive(Default)]
    struct Observations {
        generations: Mutex<BTreeMap<u64, Generation>>,
        changed: Condvar,
    }

    static LIVE: OnceLock<Observations> = OnceLock::new();

    fn observations() -> &'static Observations {
        LIVE.get_or_init(Observations::default)
    }

    fn received(generation: u64, sample: EncodedSample) {
        let state = observations();
        if let Some(entry) = lock(&state.generations).get_mut(&generation) {
            entry.received += 1;
            if entry.boundary
                || entry.stopped + entry.failed + entry.interrupted != 0
                || sample.bytes.is_empty()
                || sample.pts < 0
                || sample.duration <= 0
                || (sample.duration - FRAME_TICKS).abs() > 1
                || entry
                    .frames
                    .last()
                    .is_some_and(|previous| sample.pts <= previous.pts)
                || (entry.frames.is_empty() && !sample.keyframe)
                || entry.frames.len() >= 1200
                || sample.bytes.len() > 64 * 1024 * 1024 - entry.bytes
            {
                entry.invalid = true;
            } else {
                entry.bytes += sample.bytes.len();
                entry.frames.push(sample);
            }
        }
        state.changed.notify_all();
    }

    fn event(generation: u64, kind: i32, message: &str) {
        let state = observations();
        if let Some(entry) = lock(&state.generations).get_mut(&generation) {
            match kind {
                1 => {
                    if entry.stopped + entry.failed + entry.interrupted != 0 {
                        entry.invalid = true;
                    }
                    entry.started += 1;
                }
                2 => entry.stopped += 1,
                3 => entry.failed += 1,
                4 | 5 => entry.interrupted += 1,
                6..=8 => {}
                _ => entry.invalid = true,
            }
            if entry.started > 1 || entry.stopped + entry.failed + entry.interrupted > 1 {
                entry.invalid = true;
            }
            entry.message = message.to_owned();
        }
        state.changed.notify_all();
    }

    fn validate(entry: &Generation, allow_interruption: bool) -> Result<(), String> {
        if entry.invalid {
            return Err("Invalid, out-of-order, or late capture callback.".into());
        }
        if entry.failed != 0 || (!allow_interruption && entry.interrupted != 0) {
            return Err(format!("Capture ended unexpectedly: {}", entry.message));
        }
        Ok(())
    }

    fn wait_for(
        generation: u64,
        predicate: impl Fn(&Generation) -> bool,
        timeout: Duration,
        allow_interruption: bool,
    ) -> Result<(), String> {
        let state = observations();
        let mut entries = lock(&state.generations);
        let deadline = Instant::now() + timeout;
        loop {
            let entry = entries
                .get(&generation)
                .ok_or("The capture generation is missing.")?;
            validate(entry, allow_interruption)?;
            if predicate(entry) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Waiting for live capture timed out.".into());
            }
            let (guard, _) = state
                .changed
                .wait_timeout(entries, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries = guard;
        }
    }

    fn begin(generation: u64) -> Result<(), String> {
        if lock(&observations().generations)
            .insert(generation, Generation::default())
            .is_some()
        {
            return Err("A live test generation was reused.".into());
        }
        start(generation, received, event);
        Ok(())
    }

    fn snapshot(generation: u64) -> Result<Vec<EncodedSample>, String> {
        let entries = lock(&observations().generations);
        let entry = entries
            .get(&generation)
            .ok_or("The capture generation is missing.")?;
        validate(entry, false)?;
        if entry.frames.is_empty() {
            return Err("Capture produced no snapshot.".into());
        }
        let frames = entry.frames.clone();
        drop(entries);
        Ok(frames)
    }

    fn count(generation: u64) -> Result<usize, String> {
        lock(&observations().generations)
            .get(&generation)
            .map(|entry| entry.received)
            .ok_or_else(|| "The capture generation is missing.".to_owned())
    }

    fn mark_boundary(generation: u64) -> Result<(), String> {
        lock(&observations().generations)
            .get_mut(&generation)
            .ok_or("The capture generation is missing.")?
            .boundary = true;
        Ok(())
    }

    fn stop_and_verify(generation: u64) -> Result<(), String> {
        stop(generation);
        mark_boundary(generation)?;
        let before = count(generation)?;
        wait_for(
            generation,
            |entry| entry.stopped == 1,
            Duration::from_secs(10),
            false,
        )?;
        thread::sleep(Duration::from_millis(250));
        if count(generation)? != before {
            return Err("Capture delivered a frame after stop returned.".into());
        }
        let entries = lock(&observations().generations);
        let entry = entries
            .get(&generation)
            .ok_or("The capture generation is missing.")?;
        validate(entry, false)?;
        if entry.stopped != 1 || entry.failed != 0 || entry.interrupted != 0 {
            return Err("Capture must emit exactly one stopped event.".into());
        }
        drop(entries);
        Ok(())
    }

    fn save_snapshot(frames: &[EncodedSample], path: &Path) -> Result<(), String> {
        super::super::media::export_samples(frames, path)?;
        super::super::media::verify_captured_clip(path, frames)
    }

    struct Cleanup(u64);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            stop(self.0);
        }
    }

    /// Captures the interactive desktop only when explicitly invoked. Keeps all
    /// generated clips in the directory supplied by `OPTIC_LIVE_CAPTURE_DIR`.
    #[test]
    #[ignore = "requires an interactive desktop and OPTIC_LIVE_CAPTURE_DIR"]
    #[allow(
        clippy::too_many_lines,
        reason = "One live scenario verifies that snapshots survive stop, restart, interruption, and immediate cancellation in sequence."
    )]
    fn live_capture_export_stop_restart_and_interruption() -> Result<(), String> {
        let directory = std::env::var_os("OPTIC_LIVE_CAPTURE_DIR")
            .map(std::path::PathBuf::from)
            .ok_or("Set OPTIC_LIVE_CAPTURE_DIR before running this ignored desktop test.")?;
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| error.to_string())?
                .as_millis()
        );
        let mut cleanup = Cleanup(1_000_001);
        begin(cleanup.0)?;
        wait_for(
            cleanup.0,
            |entry| entry.started == 1,
            Duration::from_secs(20),
            false,
        )?;
        wait_for(
            cleanup.0,
            |entry| entry.frames.len() >= 180,
            Duration::from_secs(10),
            false,
        )?;
        let first = snapshot(cleanup.0)?;
        if first
            .last()
            .zip(first.first())
            .is_none_or(|(last, first)| last.pts - first.pts < 15_000_000)
        {
            return Err("Capture did not preserve elapsed recording time.".into());
        }
        let before_export = count(cleanup.0)?;
        save_snapshot(&first, &directory.join(format!("live-{suffix}.mp4")))?;
        wait_for(
            cleanup.0,
            |entry| entry.received > before_export,
            Duration::from_secs(3),
            false,
        )?;
        stop_and_verify(cleanup.0)?;
        {
            let mut entries = lock(&observations().generations);
            let entry = entries
                .get_mut(&cleanup.0)
                .ok_or("The capture generation is missing.")?;
            entry.frames.clear();
            entry.bytes = 0;
            drop(entries);
        }
        save_snapshot(&first, &directory.join(format!("after-stop-{suffix}.mp4")))?;
        cleanup.0 += 1;
        begin(cleanup.0)?;
        wait_for(
            cleanup.0,
            |entry| entry.started == 1 && entry.frames.len() >= 60,
            Duration::from_secs(20),
            false,
        )?;
        let second = snapshot(cleanup.0)?;
        if second
            .first()
            .is_none_or(|sample| !sample.keyframe || sample.pts >= 10_000_000)
            || second
                .first()
                .zip(first.last())
                .is_none_or(|(second, first)| second.pts >= first.pts)
        {
            return Err("Restart must start a fresh timestamp timeline with a keyframe.".into());
        }
        save_snapshot(&second, &directory.join(format!("restart-{suffix}.mp4")))?;
        interrupt(Interruption::Expected);
        mark_boundary(cleanup.0)?;
        let before_interrupt = count(cleanup.0)?;
        wait_for(
            cleanup.0,
            |entry| entry.interrupted == 1,
            Duration::from_secs(10),
            true,
        )?;
        thread::sleep(Duration::from_millis(250));
        if count(cleanup.0)? != before_interrupt {
            return Err("Capture delivered frames after interruption.".into());
        }
        {
            let entries = lock(&observations().generations);
            let entry = entries
                .get(&cleanup.0)
                .ok_or("The capture generation is missing.")?;
            validate(entry, true)?;
            if entry.interrupted != 1
                || entry.stopped != 0
                || entry.failed != 0
                || entry.message.is_empty()
            {
                return Err("Capture must emit exactly one interruption event.".into());
            }
            drop(entries);
        }
        for generation in 1_000_003..1_000_011 {
            cleanup.0 = generation;
            begin(generation)?;
            stop_and_verify(generation)?;
        }
        println!("Live capture clips retained in {}", directory.display());
        Ok(())
    }
}
