//! Desktop Duplication capture. COM objects remain on one MTA worker.

use super::{
    media::{EncodedSample, Encoder},
    pixels::{self, AxisSample, CursorKind, Pointer, Rotation},
    EventCallback, FrameCallback, FrameSink,
};
use std::{
    collections::VecDeque,
    marker::PhantomData,
    panic::{catch_unwind, AssertUnwindSafe},
    rc::Rc,
    sync::{
        atomic::{AtomicU8, Ordering},
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

const FRAME_INTERVAL: Duration = Duration::from_nanos(33_333_333);
const INTERRUPTED: &str =
    "Screen capture was interrupted. Start buffering again when the display is available.";
const STOPPED: &str = "Screen capture stopped.";
const MAX_CURSOR_BYTES: u32 = 16 * 1024 * 1024;
static MANAGER: OnceLock<Result<Arc<Manager>, String>> = OnceLock::new();

#[derive(Debug)]
struct CaptureError {
    interrupted: bool,
    message: String,
}

impl From<String> for CaptureError {
    fn from(message: String) -> Self {
        Self {
            interrupted: false,
            message,
        }
    }
}

fn windows_error(error: &WindowsError, operation: &str, runtime_capture: bool) -> CaptureError {
    let code = error.code();
    let interrupted = runtime_capture
        && (code == DXGI_ERROR_ACCESS_LOST || code == DXGI_ERROR_SESSION_DISCONNECTED);
    CaptureError {
        interrupted,
        message: format!(
            "{operation} failed (HRESULT 0x{:08X}).{}",
            code.0,
            if interrupted {
                format!(" {INTERRUPTED}")
            } else {
                String::new()
            }
        ),
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
}

struct Request {
    generation: u64,
    event: EventCallback,
    gate: Arc<FrameGate>,
    ending: AtomicU8,
}

impl Request {
    fn request_end(&self, kind: u8) {
        let _ = self
            .ending
            .compare_exchange(0, kind, Ordering::AcqRel, Ordering::Acquire);
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
            2 => self.event(2, STOPPED),
            _ => match error {
                Some(error) => self.event(if error.interrupted { 4 } else { 3 }, &error.message),
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
            if let Some(capture) = current.as_mut() {
                let result = catch_unwind(AssertUnwindSafe(|| capture.desktop.tick()))
                    .unwrap_or_else(|_| {
                        Err("Screen capture failed unexpectedly.".to_owned().into())
                    });
                if let Err(error) = result {
                    let failed_capture = current.take();
                    if let Some(capture) = failed_capture {
                        self.finish(capture, Some(&error));
                    }
                }
            }
            // Skip missed deadlines instead of creating a catch-up queue.
            next_tick += FRAME_INTERVAL;
            if next_tick < Instant::now() {
                next_tick = Instant::now() + FRAME_INTERVAL;
            }
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
pub fn start(generation: u64, frame: FrameCallback, event: EventCallback) {
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
        }),
        ending: AtomicU8::new(0),
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

/// Handles display/session interruptions with the same synchronous frame gate.
pub fn interrupt() {
    request_end(None, 4);
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

struct DesktopCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    rotation: Rotation,
    display_width: usize,
    display_height: usize,
    width: usize,
    pointer: Pointer,
    desktop: Vec<u8>,
    working: Vec<u8>,
    encoded: Vec<u8>,
    xs: Vec<AxisSample>,
    ys: Vec<AxisSample>,
    encoder: Encoder,
    started: Instant,
    previous_pts: i64,
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

impl DesktopCapture {
    #[allow(
        clippy::too_many_lines,
        reason = "The ordered DXGI/device/encoder initialization transaction preserves the selected adapter and validates dimensions before allocating resources."
    )]
    fn new(request: &Request) -> Result<Self, CaptureError> {
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
        let width = (display_width * 1080 / display_height.max(1080) / 2 * 2).max(2);
        let height = (display_height.min(1080) / 2 * 2).max(2);
        let xs = pixels::scale_axis(display_width, width)?;
        let ys = pixels::scale_axis(display_height, height)?;
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
        let gate = Arc::clone(&request.gate);
        let sink: FrameSink = Arc::new(move |_generation, sample| gate.deliver(sample));
        let encoder = Encoder::new(
            u32::try_from(width).map_err(|_| "Invalid output width.".to_owned())?,
            u32::try_from(height).map_err(|_| "Invalid output height.".to_owned())?,
            request.generation,
            sink,
        )?;
        Ok(Self {
            device,
            context,
            duplication,
            staging: None,
            rotation,
            display_width,
            display_height,
            width,
            pointer: Pointer::default(),
            desktop: Vec::new(),
            working: Vec::new(),
            encoded: Vec::new(),
            xs,
            ys,
            encoder,
            started: Instant::now(),
            previous_pts: -1,
            render_dirty: false,
            _thread_only: PhantomData,
        })
    }

    fn tick(&mut self) -> Result<(), CaptureError> {
        self.acquire()?;
        if self.desktop.is_empty() {
            if self.started.elapsed() > Duration::from_secs(10) {
                return Err(
                    "Screen capture did not provide an image. Start buffering again."
                        .to_owned()
                        .into(),
                );
            }
            return Ok(());
        }
        if self.render_dirty {
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
                &mut self.encoded,
            )?;
            self.render_dirty = false;
        }
        let pts = i64::try_from(self.started.elapsed().as_nanos() / 100)
            .map_err(|_| "The capture timeline is too long.".to_owned())?
            .max(self.previous_pts + 1);
        self.previous_pts = pts;
        self.encoder.encode(&self.encoded, self.width * 4, pts)?;
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Frame and mapping leases surround the complete readback transaction; every early return releases the acquired OS resources."
    )]
    fn acquire(&mut self) -> Result<(), CaptureError> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: Both output pointers reference initialized writable locals.
        match unsafe {
            self.duplication
                .AcquireNextFrame(0, &raw mut info, &raw mut resource)
        } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(()),
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
        if info.LastPresentTime != 0 || self.desktop.is_empty() {
            let resource =
                resource.ok_or_else(|| "Screen capture supplied no image resource.".to_owned())?;
            let texture: ID3D11Texture2D =
                os_result(resource.cast(), "Opening the captured image", false)?;
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
                    interrupted: true,
                    message:
                        "The display size changed. Start buffering again to begin a new stream."
                            .into(),
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
                    false,
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
                false,
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
            false,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
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
                    runtime
                        && (code == DXGI_ERROR_ACCESS_LOST
                            || code == DXGI_ERROR_SESSION_DISCONNECTED)
                );
                assert!(error.message.contains("HRESULT 0x"));
            }
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
            duration: 333_333,
            keyframe: true,
        }
    }

    #[test]
    fn stop_gate_waits_for_callback_and_rejects_future_frames() -> Result<(), String> {
        let gate = Arc::new(FrameGate {
            active: Mutex::new(true),
            generation: 7,
            callback: blocking_callback,
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
                || (sample.duration - 333_333).abs() > 1
                || entry
                    .frames
                    .last()
                    .is_some_and(|previous| sample.pts <= previous.pts)
                || (entry.frames.is_empty() && !sample.keyframe)
                || entry.frames.len() >= 600
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
                4 => entry.interrupted += 1,
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
            |entry| entry.frames.len() >= 90,
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
        interrupt();
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
