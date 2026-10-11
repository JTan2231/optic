//! Media Foundation encoding and remuxing. COM objects remain on their worker.

use std::{
    ffi::c_void,
    marker::PhantomData,
    mem::ManuallyDrop,
    os::windows::ffi::OsStrExt,
    path::Path,
    rc::Rc,
    sync::{Arc, Condvar, Mutex, PoisonError},
    thread,
    time::{Duration, Instant},
};

#[allow(
    clippy::wildcard_imports,
    reason = "The Media Foundation APIs use a large coherent set of SDK interfaces and attribute constants"
)]
use windows::{
    core::{implement, Interface, GUID, HRESULT, PCWSTR},
    Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, E_NOTIMPL},
        // The encoder/sink interfaces and their attribute constants share this SDK namespace.
        Media::MediaFoundation::*,
        Storage::FileSystem::{
            DeleteFileW, GetFileAttributesW, GetFullPathNameW, MoveFileExW,
            INVALID_FILE_ATTRIBUTES, MOVEFILE_WRITE_THROUGH,
        },
        System::{
            Com::{
                CoCreateGuid, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
                StringFromGUID2, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
            },
            Variant::VARIANT,
        },
    },
};

use super::FrameSink;

const TICKS_PER_SECOND: i64 = 10_000_000;
const FRAME_DURATION: i64 = TICKS_PER_SECOND / 30;

trait MediaResult<T> {
    fn operation(self, operation: &str) -> Result<T, String>;
}

impl<T> MediaResult<T> for windows::core::Result<T> {
    fn operation(self, operation: &str) -> Result<T, String> {
        self.map_err(|error| {
            let code = u32::from_ne_bytes(error.code().0.to_ne_bytes());
            format!("{operation} failed (HRESULT 0x{code:08X}).")
        })
    }
}

struct ComRuntime(PhantomData<Rc<()>>);

impl ComRuntime {
    fn new() -> Result<Self, String> {
        // SAFETY: Each worker initializes and tears down its own MTA apartment.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.operation("Initializing COM")?;
        Ok(Self(PhantomData))
    }
}

impl Drop for ComRuntime {
    fn drop(&mut self) {
        // SAFETY: This guard exists only after successful initialization on this thread.
        unsafe { CoUninitialize() };
    }
}

struct MediaRuntime(PhantomData<Rc<()>>);

impl MediaRuntime {
    fn new() -> Result<Self, String> {
        // SAFETY: The caller initialized COM and owns the balanced startup guard.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.operation("Starting Media Foundation")?;
        Ok(Self(PhantomData))
    }
}

impl Drop for MediaRuntime {
    fn drop(&mut self) {
        // SAFETY: All media COM objects are dropped before this balanced shutdown.
        let _ = unsafe { MFShutdown() };
    }
}

/// Immutable metadata shared by all compressed frames in one capture stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFormat {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) sequence_header: Arc<[u8]>,
}

/// Only immutable Rust data crosses from capture to buffering or exporting.
#[derive(Clone, Debug)]
pub struct EncodedSample {
    pub(crate) format: Arc<VideoFormat>,
    pub(crate) bytes: Arc<[u8]>,
    pub(crate) pts: i64,
    pub(crate) duration: i64,
    pub(crate) keyframe: bool,
}

fn packed_pair(first: u32, second: u32) -> u64 {
    (u64::from(first) << 32) | u64::from(second)
}

// SDK enums are signed, even when the ABI consumes their unsigned bit pattern.
const fn sdk_uint(value: i32) -> u32 {
    u32::from_ne_bytes(value.to_ne_bytes())
}

const fn unpacked_pair(value: u64) -> (u32, u32) {
    let bytes = value.to_be_bytes();
    (
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    )
}

fn video_type(width: u32, height: u32, subtype: &GUID) -> Result<IMFMediaType, String> {
    // SAFETY: All interfaces belong to this initialized worker and all attribute values
    // are copied by Media Foundation; no Rust pointers are retained by these calls.
    unsafe {
        let media = MFCreateMediaType().operation("Creating the video format")?;
        media
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .operation("Setting the media kind")?;
        media
            .SetGUID(&MF_MT_SUBTYPE, subtype)
            .operation("Setting the video subtype")?;
        media
            .SetUINT64(&MF_MT_FRAME_SIZE, packed_pair(width, height))
            .operation("Setting the video size")?;
        media
            .SetUINT64(&MF_MT_FRAME_RATE, packed_pair(30, 1))
            .operation("Setting the frame rate")?;
        media
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed_pair(1, 1))
            .operation("Setting the pixel ratio")?;
        media
            .SetUINT32(
                &MF_MT_INTERLACE_MODE,
                sdk_uint(MFVideoInterlace_Progressive.0),
            )
            .operation("Setting progressive video")?;
        media
            .SetUINT32(&MF_MT_VIDEO_PRIMARIES, sdk_uint(MFVideoPrimaries_BT709.0))
            .operation("Setting video primaries")?;
        media
            .SetUINT32(&MF_MT_TRANSFER_FUNCTION, sdk_uint(MFVideoTransFunc_sRGB.0))
            .operation("Setting the transfer function")?;
        media
            .SetUINT32(&MF_MT_YUV_MATRIX, sdk_uint(MFVideoTransferMatrix_BT709.0))
            .operation("Setting the color matrix")?;
        media
            .SetUINT32(
                &MF_MT_VIDEO_NOMINAL_RANGE,
                sdk_uint(MFNominalRange_16_235.0),
            )
            .operation("Setting the video range")?;
        if *subtype == MFVideoFormat_H264 {
            media
                .SetUINT32(&MF_MT_AVG_BITRATE, 8_000_000)
                .operation("Setting the video bitrate")?;
            media
                .SetUINT32(&MF_MT_MPEG2_PROFILE, sdk_uint(eAVEncH264VProfile_High.0))
                .operation("Setting the H.264 profile")?;
        } else {
            media
                .SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
                .operation("Setting independent input samples")?;
            media
                .SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)
                .operation("Setting fixed input samples")?;
            media
                .SetUINT32(&MF_MT_SAMPLE_SIZE, width * height * 3 / 2)
                .operation("Setting the input size")?;
            media
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, width)
                .operation("Setting the input stride")?;
        }
        Ok(media)
    }
}

fn codec_uint(
    codec: &ICodecAPI,
    key: &GUID,
    value: u32,
    operation: &str,
    required: bool,
) -> Result<(), String> {
    let setting = VARIANT::from(value);
    // SAFETY: The codec copies this initialized VARIANT during the call.
    let result = unsafe { codec.SetValue(key, &raw const setting) };
    if required {
        result.operation(operation)
    } else {
        Ok(())
    }
}

fn codec_bool(
    codec: &ICodecAPI,
    key: &GUID,
    value: bool,
    operation: &str,
    required: bool,
) -> Result<(), String> {
    let setting = VARIANT::from(value);
    // SAFETY: The codec copies this initialized VARIANT during the call.
    let result = unsafe { codec.SetValue(key, &raw const setting) };
    if required {
        result.operation(operation)
    } else {
        Ok(())
    }
}

struct LockedBuffer<'a> {
    buffer: &'a IMFMediaBuffer,
    data: *mut u8,
    length: usize,
}

impl<'a> LockedBuffer<'a> {
    fn new(buffer: &'a IMFMediaBuffer, operation: &str) -> Result<Self, String> {
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        // SAFETY: Writable out-pointers live through the call; the guard unlocks
        // the retained buffer before the pointer can escape the worker.
        unsafe { buffer.Lock(&raw mut data, None, Some(&raw mut length)) }.operation(operation)?;
        let length = match usize::try_from(length) {
            Ok(length) => length,
            Err(error) => {
                // SAFETY: Lock succeeded above, and no guard was constructed yet.
                let _ = unsafe { buffer.Unlock() };
                return Err(error.to_string());
            }
        };
        Ok(Self {
            buffer,
            data,
            length,
        })
    }

    fn finish(self, operation: &str) -> Result<(), String> {
        // SAFETY: This guard owns one successful lock, and is forgotten only after unlocking.
        let result = unsafe { self.buffer.Unlock() }.operation(operation);
        std::mem::forget(self);
        result
    }
}

impl Drop for LockedBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: This guard still owns one successful lock on its borrowed buffer.
        let _ = unsafe { self.buffer.Unlock() };
    }
}

fn make_sample(bytes: &[u8], pts: i64, duration: i64, keyframe: bool) -> Result<IMFSample, String> {
    let count =
        u32::try_from(bytes.len()).map_err(|_| "The video frame has an invalid encoded size.")?;
    if count == 0 {
        return Err("The video frame has an invalid encoded size.".into());
    }
    // SAFETY: The allocated buffer has exactly count bytes. The locked pointer is
    // writable for that allocation, and the sample retains the buffer on this worker.
    unsafe {
        let buffer = MFCreateMemoryBuffer(count).operation("Allocating a video buffer")?;
        let locked = LockedBuffer::new(&buffer, "Locking a video buffer")?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), locked.data, bytes.len());
        locked.finish("Unlocking a video buffer")?;
        buffer
            .SetCurrentLength(count)
            .operation("Setting a video buffer length")?;
        let sample = MFCreateSample().operation("Creating a video sample")?;
        sample
            .AddBuffer(&buffer)
            .operation("Attaching a video buffer")?;
        sample
            .SetSampleTime(pts)
            .operation("Setting the video timestamp")?;
        sample
            .SetSampleDuration(duration)
            .operation("Setting the video duration")?;
        if keyframe {
            sample
                .SetUINT32(&MFSampleExtension_CleanPoint, 1)
                .operation("Marking the keyframe")?;
        }
        Ok(sample)
    }
}

fn contains_idr(bytes: &[u8]) -> bool {
    (0..bytes.len().saturating_sub(3)).any(|index| {
        if bytes[index] != 0 || bytes[index + 1] != 0 {
            return false;
        }
        let mut nal = index + 2;
        if bytes[nal] == 0 {
            nal += 1;
        }
        nal + 1 < bytes.len() && bytes[nal] == 1 && (bytes[nal + 1] & 31) == 5
    })
}

/// BT.709 limited-range NV12, averaging each 2x2 block before subsampling.
fn bgra_to_nv12(bgra: &[u8], stride: usize, width: usize, height: usize, nv12: &mut [u8]) {
    let clamp = |value: i32| u8::try_from(value.clamp(0, 255)).unwrap_or_default();
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let (mut red, mut green, mut blue) = (0, 0, 0);
            for dy in 0..2 {
                for dx in 0..2 {
                    let pixel = (y + dy) * stride + (x + dx) * 4;
                    let b = i32::from(bgra[pixel]);
                    let g = i32::from(bgra[pixel + 1]);
                    let r = i32::from(bgra[pixel + 2]);
                    blue += b;
                    green += g;
                    red += r;
                    nv12[(y + dy) * width + x + dx] =
                        clamp(16 + ((47 * r + 157 * g + 16 * b + 128) >> 8));
                }
            }
            red = (red + 2) / 4;
            green = (green + 2) / 4;
            blue = (blue + 2) / 4;
            let chroma = width * height + (y / 2) * width + x;
            nv12[chroma] = clamp(128 + ((-26 * red - 87 * green + 112 * blue + 128) >> 8));
            nv12[chroma + 1] = clamp(128 + ((112 * red - 102 * green - 10 * blue + 128) >> 8));
        }
    }
}

/// Constructed, used and dropped only by the COM MTA capture worker.
#[allow(
    clippy::struct_excessive_bools,
    reason = "The Media Foundation event protocol has independent flags that can be active simultaneously"
)]
pub struct Encoder {
    transform: Option<IMFTransform>,
    codec: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    format: Option<Arc<VideoFormat>>,
    width: u32,
    height: u32,
    generation: u64,
    sink: FrameSink,
    nv12: Vec<u8>,
    finished: bool,
    has_input: bool,
    hardware: bool,
    probing: bool,
    drain_complete: bool,
    input_credits: u32,
    previous_input: i64,
    previous_output: i64,
    last_keyframe: i64,
    // Rust drops fields in declaration order; media interfaces precede shutdown.
    _runtime: MediaRuntime,
}

impl Encoder {
    pub(crate) fn new(
        width: u32,
        height: u32,
        generation: u64,
        sink: FrameSink,
    ) -> Result<Self, String> {
        if width < 2
            || height < 2
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || width > 16_384
            || height > 16_384
        {
            return Err("The encoder needs even video dimensions and a frame callback.".into());
        }
        let runtime = MediaRuntime::new()?;
        let size = usize::try_from(width * height * 3 / 2).map_err(|error| error.to_string())?;
        let mut encoder = Self {
            transform: None,
            codec: None,
            events: None,
            format: None,
            width,
            height,
            generation,
            sink,
            nv12: vec![0; size],
            finished: false,
            has_input: false,
            hardware: false,
            probing: false,
            drain_complete: false,
            input_credits: 0,
            previous_input: -1,
            previous_output: -1,
            last_keyframe: -1,
            _runtime: runtime,
        };
        if !encoder.try_hardware() {
            encoder.create_software()?;
        }
        Ok(encoder)
    }

    fn transform(&self) -> Result<&IMFTransform, String> {
        self.transform
            .as_ref()
            .ok_or_else(|| "The video encoder has stopped.".into())
    }

    fn codec(&self) -> Result<&ICodecAPI, String> {
        self.codec
            .as_ref()
            .ok_or_else(|| "The video encoder has stopped.".into())
    }

    fn reset_transform(&mut self) {
        if let Some(transform) = &self.transform {
            // SAFETY: Teardown stays on the encoder's owning MTA worker.
            unsafe {
                if !self.finished {
                    let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                }
                if self.hardware {
                    if let Ok(shutdown) = transform.cast::<IMFShutdown>() {
                        let _ = shutdown.Shutdown();
                    }
                }
            }
        }
        self.events = None;
        self.codec = None;
        self.transform = None;
        self.hardware = false;
        self.probing = false;
        self.format = None;
        self.input_credits = 0;
        self.drain_complete = false;
        self.has_input = false;
        self.previous_input = -1;
        self.previous_output = -1;
        self.last_keyframe = -1;
    }

    fn create_software(&mut self) -> Result<(), String> {
        self.reset_transform();
        // SAFETY: The worker has a COM apartment, and requests the known H.264 MFT interface.
        self.transform = Some(
            unsafe { CoCreateInstance(&CMSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }
                .operation("Creating the software H.264 encoder")?,
        );
        self.configure()
    }

    fn try_hardware(&mut self) -> bool {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activations = std::ptr::null_mut();
        let mut count = 0;
        // SAFETY: Media Foundation allocates an array of retained activation interfaces;
        // each interface and the CoTaskMem allocation are released below on every path.
        let enumeration = unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&raw const input),
                Some(&raw const output),
                &raw mut activations,
                &raw mut count,
            )
        };
        if enumeration.is_err() {
            return false;
        }
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut selected = false;
        let activation_count = usize::try_from(count).unwrap_or_default();
        if !activations.is_null() {
            // SAFETY: The successful enumeration returned count initialized interface slots.
            let entries = unsafe { std::slice::from_raw_parts_mut(activations, activation_count) };
            for activation in entries.iter().flatten() {
                if Instant::now() >= deadline {
                    break;
                }
                if self.probe_hardware(activation, deadline).is_ok() {
                    selected = true;
                    break;
                }
                self.reset_transform();
                // SAFETY: The activation belongs to this worker; failure abandons its transform.
                let _ = unsafe { activation.ShutdownObject() };
            }
            for entry in entries {
                *entry = None;
            }
        }
        // SAFETY: The MFTEnumEx array uses COM task memory, independent of its released entries.
        unsafe { CoTaskMemFree(Some(activations.cast())) };
        selected
    }

    fn probe_hardware(
        &mut self,
        activation: &IMFActivate,
        deadline: Instant,
    ) -> Result<(), String> {
        self.reset_transform();
        // SAFETY: All transform operations run on its MTA owner, and probe samples
        // contain worker-owned CPU bytes. No sample or transform crosses threads.
        unsafe {
            self.transform = Some(
                activation
                    .ActivateObject()
                    .operation("Activating hardware H.264")?,
            );
            self.hardware = true;
            let attributes = self
                .transform()?
                .GetAttributes()
                .operation("Reading hardware encoder attributes")?;
            if attributes
                .GetUINT32(&MF_TRANSFORM_ASYNC)
                .operation("Reading hardware encoder mode")?
                == 0
            {
                return Err("The hardware encoder does not use the event protocol.".into());
            }
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .operation("Unlocking hardware H.264")?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            self.events = Some(
                self.transform()?
                    .cast()
                    .operation("Opening hardware encoder events")?,
            );
            self.configure()?;
            let probe_deadline = deadline.min(Instant::now() + Duration::from_secs(3));
            self.wait_for_input(probe_deadline)?;
            let luma_length =
                usize::try_from(self.width * self.height).map_err(|error| error.to_string())?;
            self.nv12[..luma_length].fill(16);
            self.nv12[luma_length..].fill(128);
            codec_uint(
                self.codec()?,
                &CODECAPI_AVEncVideoForceKeyFrame,
                1,
                "Testing a hardware keyframe",
                true,
            )?;
            let sample = make_sample(&self.nv12, 0, FRAME_DURATION, false)?;
            self.probing = true;
            self.transform()?
                .ProcessInput(0, &sample, 0)
                .operation("Testing hardware video input")?;
            self.input_credits -= 1;
            self.transform()?
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .operation("Ending hardware probe input")?;
            self.transform()?
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .operation("Draining hardware probe video")?;
            while !self.drain_complete {
                if !self.pump_events()? {
                    thread::sleep(Duration::from_millis(1));
                }
                if Instant::now() >= probe_deadline {
                    return Err("The hardware encoder probe timed out.".into());
                }
            }
            if self.previous_output < 0 {
                return Err("The hardware encoder probe produced no video.".into());
            }
            self.transform()?
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .operation("Resetting hardware probe video")?;
            while self.pump_events()? {}
            self.input_credits = 0;
            self.drain_complete = false;
            self.previous_input = -1;
            self.previous_output = -1;
            self.last_keyframe = -1;
            self.probing = false;
            self.transform()?
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .operation("Restarting hardware H.264")?;
            self.wait_for_input(probe_deadline)
        }
    }

    fn configure(&mut self) -> Result<(), String> {
        let transform = self.transform()?.clone();
        // SAFETY: The transform and its codec interface belong to this COM worker.
        // Attribute values, types, and settings remain valid through each call.
        unsafe {
            let (mut inputs, mut outputs) = (0, 0);
            transform
                .GetStreamCount(&raw mut inputs, &raw mut outputs)
                .operation("Reading encoder stream counts")?;
            if inputs != 1 || outputs != 1 {
                return Err("The H.264 encoder does not provide one video stream.".into());
            }
            let (mut input_id, mut output_id) = ([0], [0]);
            let ids = transform.GetStreamIDs(&mut input_id, &mut output_id);
            if ids.as_ref().err().map(windows::core::Error::code) != Some(E_NOTIMPL) {
                ids.operation("Reading encoder stream identifiers")?;
                if input_id != [0] || output_id != [0] {
                    return Err("The H.264 encoder uses unsupported stream identifiers.".into());
                }
            }
            self.codec = Some(
                transform
                    .cast()
                    .operation("Opening H.264 encoder settings")?,
            );
            let codec = self.codec()?;
            codec_uint(
                codec,
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                0,
                "Disabling B frames",
                true,
            )?;
            codec_uint(
                codec,
                &CODECAPI_AVEncMPVGOPSize,
                30,
                "Setting the keyframe interval",
                true,
            )?;
            codec_uint(
                codec,
                &CODECAPI_AVEncCommonRateControlMode,
                sdk_uint(eAVEncCommonRateControlMode_UnconstrainedVBR.0),
                "Setting bitrate control",
                true,
            )?;
            codec_uint(
                codec,
                &CODECAPI_AVEncCommonMeanBitRate,
                8_000_000,
                "Setting the encoder bitrate",
                true,
            )?;
            codec_bool(
                codec,
                &CODECAPI_AVLowLatencyMode,
                true,
                "Enabling low latency",
                false,
            )?;
            codec_uint(
                codec,
                &CODECAPI_AVEncCommonQualityVsSpeed,
                0,
                "Setting encoder complexity",
                false,
            )?;
            let output = video_type(self.width, self.height, &MFVideoFormat_H264)?;
            transform
                .SetOutputType(0, &output, 0)
                .operation("Configuring H.264 output")?;
            let input = video_type(self.width, self.height, &MFVideoFormat_NV12)?;
            transform
                .SetInputType(0, &input, 0)
                .operation("Configuring NV12 input")?;
            self.refresh_format()?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .operation("Preparing the encoder")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .operation("Starting the encoder")?;
        }
        Ok(())
    }

    fn refresh_format(&mut self) -> Result<(), String> {
        // SAFETY: This worker owns the output type; blob calls copy into an allocated slice.
        unsafe {
            let output = self
                .transform()?
                .GetOutputCurrentType(0)
                .operation("Reading the encoded video format")?;
            let dimensions = unpacked_pair(
                output
                    .GetUINT64(&MF_MT_FRAME_SIZE)
                    .operation("Reading the encoded video dimensions")?,
            );
            let subtype = output
                .GetGUID(&MF_MT_SUBTYPE)
                .operation("Reading the encoded video subtype")?;
            if subtype != MFVideoFormat_H264 || dimensions != (self.width, self.height) {
                return Err("The encoder changed the video dimensions or codec.".into());
            }
            let size = output.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER);
            if size.as_ref().err().map(windows::core::Error::code) == Some(MF_E_ATTRIBUTENOTFOUND) {
                return Ok(());
            }
            let length = size.operation("Reading the H.264 sequence header size")?;
            if length == 0 || length > 65_536 {
                return Err("The H.264 sequence header is invalid.".into());
            }
            let mut header = vec![0; usize::try_from(length).map_err(|error| error.to_string())?];
            output
                .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None)
                .operation("Reading the H.264 sequence header")?;
            let next = Arc::new(VideoFormat {
                width: self.width,
                height: self.height,
                sequence_header: header.into(),
            });
            if self.format.as_ref().is_some_and(|format| **format != *next) {
                return Err(
                    "The H.264 format changed. Resume buffering to start a fresh stream.".into(),
                );
            }
            self.format = Some(next);
        }
        Ok(())
    }

    fn output_sample(&mut self, sample: &IMFSample) -> Result<(), String> {
        if self.format.is_none() {
            self.refresh_format()?;
        }
        let format = self
            .format
            .clone()
            .ok_or("The encoder did not provide an H.264 sequence header.")?;
        // SAFETY: The sample and contiguous buffer are owned by this worker. The
        // locked bytes are copied into immutable Rust storage before unlocking.
        unsafe {
            let pts = sample
                .GetSampleTime()
                .operation("Reading an encoded timestamp")?;
            if pts < 0 || (self.previous_output >= 0 && pts <= self.previous_output) {
                return Err("The H.264 encoder reordered video timestamps.".into());
            }
            if sample
                .GetUINT64(&MFSampleExtension_DecodeTimestamp)
                .is_ok_and(|decode| Some(decode) != u64::try_from(pts).ok())
            {
                return Err("The H.264 encoder enabled frame reordering.".into());
            }
            let duration = sample
                .GetSampleDuration()
                .ok()
                .filter(|duration| *duration > 0)
                .unwrap_or(FRAME_DURATION);
            let buffer = sample
                .ConvertToContiguousBuffer()
                .operation("Reading compressed video bytes")?;
            let locked = LockedBuffer::new(&buffer, "Locking compressed video bytes")?;
            if locked.length == 0 || locked.data.is_null() {
                return Err("The encoder returned an empty video frame.".into());
            }
            let bytes: Arc<[u8]> = std::slice::from_raw_parts(locked.data, locked.length).into();
            locked.finish("Unlocking compressed video bytes")?;
            let keyframe = sample
                .GetUINT32(&MFSampleExtension_CleanPoint)
                .unwrap_or_default()
                != 0
                || contains_idr(&bytes);
            if self.previous_output < 0 && !keyframe {
                return Err("The encoder's first video frame is not a keyframe.".into());
            }
            self.previous_output = pts;
            if !self.probing {
                (self.sink)(
                    self.generation,
                    EncodedSample {
                        format,
                        bytes,
                        pts,
                        duration,
                        keyframe,
                    },
                );
            }
        }
        Ok(())
    }

    fn pull_output(&mut self) -> Result<bool, String> {
        let transform = self.transform()?.clone();
        // SAFETY: Output pointers refer to worker-owned COM objects. The output
        // structure's ManuallyDrop slots are explicitly consumed on every result.
        unsafe {
            let info = transform
                .GetOutputStreamInfo(0)
                .operation("Reading encoder output requirements")?;
            let provided = if info.dwFlags & sdk_uint(MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0) == 0 {
                let sample = MFCreateSample().operation("Creating an encoder output sample")?;
                let minimum = self.width * self.height * 3 / 2;
                let buffer = MFCreateAlignedMemoryBuffer(
                    info.cbSize.max(minimum),
                    info.cbAlignment.saturating_sub(1),
                )
                .operation("Allocating encoder output memory")?;
                sample
                    .AddBuffer(&buffer)
                    .operation("Attaching encoder output memory")?;
                Some(sample)
            } else {
                None
            };
            // Do not AddRef the caller-provided sample for this borrowed output slot.
            // A MFT-provided sample, in contrast, transfers one reference to the slot.
            let borrowed = provided
                .as_ref()
                .map(|sample| IMFSample::from_raw(sample.as_raw()));
            let mut output = MFT_OUTPUT_DATA_BUFFER {
                pSample: ManuallyDrop::new(borrowed),
                ..Default::default()
            };
            let mut status = 0;
            let result =
                transform.ProcessOutput(0, std::slice::from_mut(&mut output), &raw mut status);
            drop(ManuallyDrop::take(&mut output.pEvents));
            let returned = ManuallyDrop::take(&mut output.pSample);
            let sample = if returned.as_ref().is_some_and(|sample| {
                provided
                    .as_ref()
                    .is_some_and(|input| sample.as_raw() == input.as_raw())
            }) {
                // The original provided owner releases this reference, not the borrowed slot.
                if let Some(borrowed) = returned {
                    std::mem::forget(borrowed);
                }
                provided
            } else {
                returned
            };
            if result.as_ref().err().map(windows::core::Error::code)
                == Some(MF_E_TRANSFORM_NEED_MORE_INPUT)
            {
                return Ok(false);
            }
            if result.as_ref().err().map(windows::core::Error::code)
                == Some(MF_E_TRANSFORM_STREAM_CHANGE)
            {
                let changed = transform
                    .GetOutputAvailableType(0, 0)
                    .operation("Reading the changed encoder output type")?;
                transform
                    .SetOutputType(0, &changed, 0)
                    .operation("Accepting the encoder output type")?;
                self.refresh_format()?;
                return Ok(true);
            }
            result.operation("Encoding an H.264 frame")?;
            if let Some(sample) = sample {
                self.output_sample(&sample)?;
            }
        }
        Ok(true)
    }

    fn drain_output(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.pull_output()? {
            if Instant::now() >= deadline {
                return Err("Draining the H.264 encoder timed out.".into());
            }
        }
        Ok(())
    }

    fn pump_events(&mut self) -> Result<bool, String> {
        let mut received = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        let events = self
            .events
            .clone()
            .ok_or("The hardware encoder has stopped.")?;
        loop {
            // SAFETY: Nonblocking events and their status are read on this MFT's worker.
            let event = unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
            if event.as_ref().err().map(windows::core::Error::code)
                == Some(MF_E_NO_EVENTS_AVAILABLE)
            {
                return Ok(received);
            }
            let event = event.operation("Reading hardware H.264 events")?;
            // SAFETY: This event is retained and owned by the same worker for all queries.
            let kind = unsafe {
                event
                    .GetStatus()
                    .operation("Reading hardware encoder status")?
                    .ok()
                    .operation("Processing hardware video")?;
                event
                    .GetType()
                    .operation("Reading a hardware encoder event")?
            };
            if kind == sdk_uint(METransformNeedInput.0) {
                self.input_credits = self
                    .input_credits
                    .checked_add(1)
                    .ok_or("The hardware encoder sent invalid input credits.")?;
            } else if kind == sdk_uint(METransformHaveOutput.0) {
                // Exactly one output pull per event, including stream changes.
                self.pull_output()?;
            } else if kind == sdk_uint(METransformDrainComplete.0) {
                self.drain_complete = true;
            }
            received = true;
            if Instant::now() >= deadline {
                return Err("Processing hardware H.264 events timed out.".into());
            }
        }
    }

    fn wait_for_input(&mut self, deadline: Instant) -> Result<(), String> {
        while self.input_credits == 0 {
            if !self.pump_events()? {
                thread::sleep(Duration::from_millis(1));
            }
            if Instant::now() >= deadline {
                return Err("Waiting for the hardware H.264 encoder timed out.".into());
            }
        }
        Ok(())
    }

    pub(crate) fn encode(&mut self, bgra: &[u8], stride: usize, pts: i64) -> Result<(), String> {
        if self.finished {
            return Err("The video encoder has stopped.".into());
        }
        let width = usize::try_from(self.width).map_err(|error| error.to_string())?;
        let height = usize::try_from(self.height).map_err(|error| error.to_string())?;
        let required = (height - 1)
            .checked_mul(stride)
            .and_then(|length| length.checked_add(width * 4));
        if stride < width * 4
            || required.is_none_or(|length| bgra.len() < length)
            || pts < 0
            || (self.has_input && pts <= self.previous_input)
        {
            return Err("The encoder received invalid pixels or a video timestamp.".into());
        }
        bgra_to_nv12(bgra, stride, width, height, &mut self.nv12);
        if self.hardware {
            self.wait_for_input(Instant::now() + Duration::from_secs(3))?;
        }
        if self.last_keyframe < 0 || pts - self.last_keyframe >= TICKS_PER_SECOND {
            codec_uint(
                self.codec()?,
                &CODECAPI_AVEncVideoForceKeyFrame,
                1,
                "Forcing a video keyframe",
                true,
            )?;
            self.last_keyframe = pts;
        }
        let sample = make_sample(&self.nv12, pts, FRAME_DURATION, false)?;
        let transform = self.transform()?.clone();
        // SAFETY: The owned input sample and transform remain on the worker.
        unsafe {
            if !self.has_input {
                sample
                    .SetUINT32(&MFSampleExtension_Discontinuity, 1)
                    .operation("Marking the start of video input")?;
            }
            let mut result = transform.ProcessInput(0, &sample, 0);
            if !self.hardware
                && result.as_ref().err().map(windows::core::Error::code) == Some(MF_E_NOTACCEPTING)
            {
                self.drain_output()?;
                result = transform.ProcessInput(0, &sample, 0);
            }
            result.operation("Submitting a video frame")?;
        }
        if self.hardware {
            self.input_credits -= 1;
        }
        self.has_input = true;
        self.previous_input = pts;
        if self.hardware {
            self.pump_events()?;
        } else {
            self.drain_output()?;
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        let transform = self.transform()?.clone();
        // SAFETY: All drain and shutdown operations remain on the transform's owner.
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .operation("Ending video input")?;
            transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .operation("Finishing the H.264 encoder")?;
            if self.hardware {
                let deadline = Instant::now() + Duration::from_secs(15);
                while !self.drain_complete {
                    if !self.pump_events()? {
                        thread::sleep(Duration::from_millis(1));
                    }
                    if Instant::now() >= deadline {
                        return Err("Finishing the hardware H.264 encoder timed out.".into());
                    }
                }
            } else {
                self.drain_output()?;
            }
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
                .operation("Closing the H.264 encoder")?;
            self.finished = true;
            if self.hardware {
                if let Ok(shutdown) = transform.cast::<IMFShutdown>() {
                    shutdown
                        .Shutdown()
                        .operation("Shutting down hardware H.264")?;
                }
            }
        }
        self.events = None;
        self.transform = None;
        self.codec = None;
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.reset_transform();
    }
}

#[derive(Default)]
struct FinalizeState {
    result: Mutex<Option<HRESULT>>,
    ready: Condvar,
}

impl FinalizeState {
    fn wait(&self, deadline: Instant) -> Result<(), String> {
        let mut result = self.result.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(status) = *result {
                return status.ok().operation("Finalizing the MP4");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Finalizing the MP4 timed out.".into());
            }
            let (next, _) = self
                .ready
                .wait_timeout(result, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            result = next;
        }
    }
}

mod finalize_callback {
    #![allow(
        clippy::ref_as_ptr,
        clippy::inline_always,
        reason = "The windows implement macro generates these pointer casts and inline attributes"
    )]

    use super::{
        c_void, implement, Arc, FinalizeState, IMFSinkWriterCallback, IMFSinkWriterCallback_Impl,
        PoisonError, HRESULT,
    };

    #[implement(IMFSinkWriterCallback)]
    pub(super) struct FinalizeCallback {
        pub(super) state: Arc<FinalizeState>,
    }

    impl IMFSinkWriterCallback_Impl for FinalizeCallback_Impl {
        fn OnFinalize(&self, status: HRESULT) -> windows::core::Result<()> {
            let mut result = self
                .state
                .result
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *result = Some(status);
            drop(result);
            self.state.ready.notify_all();
            Ok(())
        }

        fn OnMarker(&self, _stream: u32, _context: *const c_void) -> windows::core::Result<()> {
            Ok(())
        }
    }
}

#[derive(Default)]
struct ExportState {
    cancelled: bool,
    done: bool,
    success: bool,
    error: String,
}

struct ExportJob {
    state: Mutex<ExportState>,
    ready: Condvar,
    deadline: Instant,
}

impl ExportJob {
    fn new(deadline: Instant) -> Self {
        Self {
            state: Mutex::new(ExportState::default()),
            ready: Condvar::new(),
            deadline,
        }
    }

    fn check_cancel(&self) -> Result<(), String> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.cancelled || Instant::now() >= self.deadline {
            Err("Writing the MP4 timed out.".into())
        } else {
            Ok(())
        }
    }
}

struct PartialFile {
    path: Vec<u16>,
    stream: Option<IMFByteStream>,
    committed: bool,
}

impl PartialFile {
    fn close(&mut self) {
        if let Some(stream) = self.stream.take() {
            // SAFETY: The byte stream belongs to this worker and closing is idempotent.
            let _ = unsafe { stream.Close() };
        }
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        self.close();
        if !self.committed && !self.path.is_empty() {
            // SAFETY: This NUL-terminated path names the worker's exclusively created file.
            let _ = unsafe { DeleteFileW(PCWSTR(self.path.as_ptr())) };
        }
    }
}

fn file_path(path: &Path) -> Result<Vec<u16>, String> {
    let mut supplied: Vec<u16> = path.as_os_str().encode_wide().collect();
    if supplied.is_empty() {
        return Err("No output path supplied.".into());
    }
    if supplied.contains(&0) {
        return Err("Invalid output path.".into());
    }
    supplied.push(0);
    if supplied.starts_with(&[92, 92, 63, 92]) {
        return Ok(supplied);
    }
    // SAFETY: Both calls use valid NUL-terminated input and the second supplies
    // the capacity Windows requested, including its terminating UTF-16 unit.
    unsafe {
        let length = GetFullPathNameW(PCWSTR(supplied.as_ptr()), None, None);
        if length == 0 {
            return Err("Invalid output path.".into());
        }
        let mut absolute = vec![0; usize::try_from(length).map_err(|error| error.to_string())?];
        let copied = GetFullPathNameW(PCWSTR(supplied.as_ptr()), Some(&mut absolute), None);
        if copied == 0 || copied >= length {
            return Err("Could not resolve the output path.".into());
        }
        absolute.truncate(usize::try_from(copied).map_err(|error| error.to_string())?);
        let mut extended = if absolute.starts_with(&[92, 92]) {
            let mut prefix: Vec<u16> = "\\\\?\\UNC\\".encode_utf16().collect();
            prefix.extend_from_slice(&absolute[2..]);
            prefix
        } else {
            let mut prefix = vec![92, 92, 63, 92];
            prefix.extend_from_slice(&absolute);
            prefix
        };
        extended.push(0);
        Ok(extended)
    }
}

fn copy_clip(samples: &[EncodedSample]) -> Result<Vec<EncodedSample>, String> {
    let first = samples.first().ok_or("No video frames supplied.")?;
    let mut previous = -1;
    for (index, sample) in samples.iter().enumerate() {
        if sample.bytes.is_empty()
            || sample.duration <= 0
            || sample.pts < 0
            || (index > 0 && sample.pts <= previous)
        {
            return Err("Video frames have invalid data or out-of-order timestamps.".into());
        }
        if index == 0 && !sample.keyframe {
            return Err("The clip must start at a keyframe.".into());
        }
        if first.format.sequence_header.is_empty() || first.format != sample.format {
            return Err("The clip contains incompatible H.264 video formats.".into());
        }
        previous = sample.pts;
    }
    // Each clone retains only immutable owned values. A timed-out export never
    // borrows the caller's snapshot, error buffer, path, or recorder state.
    Ok(samples.to_vec())
}

#[allow(
    clippy::too_many_lines,
    reason = "Keeping the remux operation together makes COM and temporary-file drop order explicit"
)]
fn write_clip(samples: &[EncodedSample], path: &[u16], job: &ExportJob) -> Result<(), String> {
    let _com = ComRuntime::new()?;
    let _runtime = MediaRuntime::new()?;
    // SAFETY: All file paths are NUL-terminated and all COM objects are created,
    // used, and dropped on this initialized MTA export worker.
    unsafe {
        if GetFileAttributesW(PCWSTR(path.as_ptr())) != INVALID_FILE_ATTRIBUTES {
            return Err("The output file already exists.".into());
        }
        let identifier = CoCreateGuid().operation("Naming the temporary MP4")?;
        let mut suffix = [0; 40];
        let suffix_length = StringFromGUID2(&raw const identifier, &mut suffix);
        if suffix_length <= 0 {
            return Err("Unable to name the temporary MP4.".into());
        }
        let suffix_length =
            usize::try_from(suffix_length - 1).map_err(|error| error.to_string())?;
        let mut temporary_path = path[..path.len() - 1].to_vec();
        temporary_path.extend(".optic-partial-".encode_utf16());
        temporary_path.extend_from_slice(&suffix[..suffix_length]);
        temporary_path.push(0);
        let stream = MFCreateFile(
            MF_ACCESSMODE_READWRITE,
            MF_OPENMODE_FAIL_IF_EXIST,
            MF_FILEFLAGS_NONE,
            PCWSTR(temporary_path.as_ptr()),
        )
        .operation("Creating the MP4 output")?;
        let mut partial = PartialFile {
            path: temporary_path,
            stream: Some(stream),
            committed: false,
        };
        let mut attributes = None;
        MFCreateAttributes(&raw mut attributes, 3).operation("Creating MP4 writer settings")?;
        let attributes = attributes.ok_or("The MP4 writer settings were not created.")?;
        attributes
            .SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)
            .operation("Selecting the MP4 container")?;
        attributes
            .SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)
            .operation("Disabling MP4 writer throttling")?;
        let completion = Arc::new(FinalizeState::default());
        let callback: IMFSinkWriterCallback = finalize_callback::FinalizeCallback {
            state: Arc::clone(&completion),
        }
        .into();
        attributes
            .SetUnknown(&MF_SINK_WRITER_ASYNC_CALLBACK, &callback)
            .operation("Setting MP4 completion")?;
        let writer =
            MFCreateSinkWriterFromURL(PCWSTR::null(), partial.stream.as_ref(), &attributes)
                .operation("Creating the MP4 writer")?;
        let first = samples.first().ok_or("No video frames supplied.")?;
        let format = &first.format;
        let media = video_type(format.width, format.height, &MFVideoFormat_H264)?;
        media
            .SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &format.sequence_header)
            .operation("Setting H.264 parameter sets")?;
        let index = writer
            .AddStream(&media)
            .operation("Adding the MP4 video stream")?;
        // Equal compressed input/output types remux bytes without a transcoder.
        writer
            .SetInputMediaType(index, &media, None)
            .operation("Setting compressed MP4 input")?;
        writer.BeginWriting().operation("Starting MP4 output")?;
        let offset = first.pts;
        for encoded in samples {
            job.check_cancel()?;
            let rebased = encoded.pts - offset;
            let sample = make_sample(&encoded.bytes, rebased, encoded.duration, encoded.keyframe)?;
            sample
                .SetUINT64(
                    &MFSampleExtension_DecodeTimestamp,
                    u64::try_from(rebased).map_err(|error| error.to_string())?,
                )
                .operation("Rebasing the decode timestamp")?;
            writer
                .WriteSample(index, &sample)
                .operation("Appending MP4 video")?;
        }
        job.check_cancel()?;
        writer
            .NotifyEndOfSegment(index)
            .operation("Ending MP4 video")?;
        writer.Finalize().operation("Starting MP4 finalization")?;
        completion.wait(job.deadline)?;
        drop(writer);
        // Finalize may close this stream itself. Do not flush a closed stream.
        partial.close();
        // Cancellation and publication share one mutex. Once the caller reports
        // timeout, this job can never subsequently publish a successful output.
        let mut state = job.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.cancelled || Instant::now() >= job.deadline {
            return Err("Writing the MP4 timed out.".into());
        }
        if let Err(error) = MoveFileExW(
            PCWSTR(partial.path.as_ptr()),
            PCWSTR(path.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        ) {
            if error.code() == HRESULT::from_win32(ERROR_ALREADY_EXISTS.0)
                || error.code() == HRESULT::from_win32(ERROR_FILE_EXISTS.0)
            {
                return Err("The output file already exists.".into());
            }
            return Err(error).operation("Publishing the MP4");
        }
        partial.committed = true;
        state.success = true;
        state.done = true;
        drop(state);
        job.ready.notify_all();
    }
    Ok(())
}

pub fn export_samples(samples: &[EncodedSample], path: &Path) -> Result<(), String> {
    let samples = copy_clip(samples)?;
    let path = file_path(path)?;
    let job = Arc::new(ExportJob::new(Instant::now() + Duration::from_secs(60)));
    let worker_job = Arc::clone(&job);
    // Dropping this handle detaches immediately: COM cleanup cannot extend the
    // synchronous deadline, and the worker owns every input and completion value.
    drop(
        thread::Builder::new()
            .name("optic-export".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    write_clip(&samples, &path, &worker_job)
                }))
                .unwrap_or_else(|_| Err("The MP4 writer failed unexpectedly.".into()));
                if let Err(error) = result {
                    let mut state = worker_job
                        .state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    state.error = error;
                    state.done = true;
                    drop(state);
                    worker_job.ready.notify_all();
                }
            })
            .map_err(|error| error.to_string())?,
    );
    let mut state = job.state.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if state.done {
            return if state.success {
                Ok(())
            } else {
                Err(state.error.clone())
            };
        }
        let remaining = job.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            state.cancelled = true;
            return Err("Writing the MP4 timed out.".into());
        }
        let (next, _) = job
            .ready
            .wait_timeout(state, remaining)
            .unwrap_or_else(PoisonError::into_inner);
        state = next;
    }
}

pub fn generate_test_video(generation: u64, sink: FrameSink) -> Result<(), String> {
    let _com = ComRuntime::new()?;
    let (width, height) = (640_u32, 360_u32);
    let mut encoder = Encoder::new(width, height, generation, sink)?;
    let mut pixels =
        vec![0; usize::try_from(width * height * 4).map_err(|error| error.to_string())?];
    for frame in 0..90_i64 {
        let square_x = u32::try_from((frame * 5) % i64::from(width - 80))
            .map_err(|error| error.to_string())?;
        for y in 0..height {
            for x in 0..width {
                let square = x >= square_x && x < square_x + 80 && (130..210).contains(&y);
                let index =
                    usize::try_from((y * width + x) * 4).map_err(|error| error.to_string())?;
                pixels[index] = if square {
                    0x60
                } else {
                    u8::try_from(y % 200).unwrap_or_default()
                };
                pixels[index + 1] = if square {
                    0xe0
                } else {
                    u8::try_from(x % 200).unwrap_or_default()
                };
                pixels[index + 2] = if square { 0x40 } else { 0 };
                pixels[index + 3] = 0xff;
            }
        }
        encoder.encode(
            &pixels,
            usize::try_from(width * 4).map_err(|error| error.to_string())?,
            frame * TICKS_PER_SECOND / 30,
        )?;
    }
    encoder.finish()
}

#[cfg(test)]
pub fn verify_clip(path: &Path, expected_frames: usize) -> Result<(), String> {
    verify_decoding(path, expected_frames, None)
}

#[cfg(test)]
pub fn verify_captured_clip(path: &Path, samples: &[EncodedSample]) -> Result<(), String> {
    verify_decoding(path, samples.len(), Some(samples))
}

#[cfg(test)]
#[allow(
    clippy::too_many_lines,
    reason = "The integration verifier checks the entire decoded stream and its elapsed timeline in one bounded operation"
)]
fn verify_decoding(
    path: &Path,
    expected_frames: usize,
    captured: Option<&[EncodedSample]>,
) -> Result<(), String> {
    let _com = ComRuntime::new()?;
    let _runtime = MediaRuntime::new()?;
    let path = file_path(path)?;
    let video = sdk_uint(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0);
    let all = sdk_uint(MF_SOURCE_READER_ALL_STREAMS.0);
    // SAFETY: Decoder COM objects and pixel samples remain on this MTA thread;
    // NUL-terminated paths and writable output arguments live through each call.
    unsafe {
        let stream = MFCreateFile(
            MF_ACCESSMODE_READ,
            MF_OPENMODE_FAIL_IF_NOT_EXIST,
            MF_FILEFLAGS_NONE,
            PCWSTR(path.as_ptr()),
        )
        .operation("Opening exported MP4 bytes")?;
        let reader =
            MFCreateSourceReaderFromByteStream(&stream, None).operation("Opening exported MP4")?;
        reader
            .SetStreamSelection(all, false)
            .operation("Deselecting streams")?;
        reader
            .SetStreamSelection(video, true)
            .operation("Selecting video")?;
        let native = reader
            .GetNativeMediaType(video, 0)
            .operation("Reading MP4 format")?;
        if native
            .GetGUID(&MF_MT_SUBTYPE)
            .operation("Reading MP4 codec")?
            != MFVideoFormat_H264
        {
            return Err("MP4 must contain H.264".into());
        }
        let (width, height) = unpacked_pair(
            native
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .operation("Reading video size")?,
        );
        if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
            return Err("Unexpected video size".into());
        }
        let (numerator, denominator) = unpacked_pair(
            native
                .GetUINT64(&MF_MT_FRAME_RATE)
                .operation("Reading frame rate")?,
        );
        if let Some(samples) = captured {
            if width < 2 || !(2..=1080).contains(&height) || width % 2 != 0 || height % 2 != 0 {
                return Err("Live MP4 must have even dimensions and height at most 1080".into());
            }
            let first = samples.first().ok_or("No captured frames supplied")?;
            let last = samples.last().ok_or("No captured frames supplied")?;
            if denominator == 0 {
                return Err("Live MP4 has an invalid frame-rate denominator".into());
            }
            let reported_rate = f64::from(numerator) / f64::from(denominator);
            // Floating-point conversion is only for test cadence diagnostics;
            // frame-by-frame timestamps below remain exact integer media ticks.
            #[allow(
                clippy::cast_precision_loss,
                clippy::as_conversions,
                reason = "Media ticks and test frame counts fit comfortably within f64 precision"
            )]
            let captured_rate = samples.len() as f64 * TICKS_PER_SECOND as f64
                / (last.pts - first.pts + last.duration) as f64;
            if !(27.0..=33.0).contains(&reported_rate) || !(27.0..=33.0).contains(&captured_rate) {
                return Err(
                    "Live capture cadence differs materially from the nominal 30 fps".into(),
                );
            }
            if (reported_rate - captured_rate).abs() >= 0.1 {
                return Err(
                    "Live MP4 frame rate does not reflect the captured elapsed timestamps".into(),
                );
            }
        } else if (numerator, denominator) != (30, 1) {
            return Err("Unexpected frame rate".into());
        }
        let decoded = MFCreateMediaType().operation("Creating decoder format")?;
        decoded
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .operation("Setting decoded kind")?;
        decoded
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .operation("Selecting NV12 decoding")?;
        reader
            .SetCurrentMediaType(video, None, &decoded)
            .operation("Configuring video decoder")?;
        let (mut count, mut previous, mut last_duration) = (0, -1, 0);
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut attempts = 0;
        loop {
            if attempts >= expected_frames.saturating_mul(4).saturating_add(32)
                || Instant::now() >= deadline
            {
                return Err("MP4 decoding did not complete within its bounded limit".into());
            }
            attempts += 1;
            let (mut flags, mut timestamp, mut sample) = (0, 0, None);
            reader
                .ReadSample(
                    video,
                    0,
                    None,
                    Some(&raw mut flags),
                    Some(&raw mut timestamp),
                    Some(&raw mut sample),
                )
                .operation("Decoding exported MP4")?;
            if flags & sdk_uint(MF_SOURCE_READERF_ERROR.0) != 0 {
                return Err("MP4 decoder reported an error".into());
            }
            if let Some(sample) = sample {
                if timestamp <= previous {
                    return Err("Decoded timestamps must be strictly increasing".into());
                }
                if count == 0 && timestamp != 0 {
                    return Err("Exported video must start at zero".into());
                }
                if let Some(samples) = captured {
                    let encoded = samples.get(count).ok_or("Live MP4 contains extra frames")?;
                    let first = samples.first().ok_or("No captured frames supplied")?;
                    let expected = encoded.pts - first.pts;
                    // The MP4 sink rounds timestamps to its media time scale.
                    // 0.1 ms permits rounding without losing captured elapsed time.
                    if timestamp.abs_diff(expected) >= 1000 {
                        return Err(format!("Live MP4 changed frame {count} timing: expected {expected} ticks, decoded {timestamp} ticks."));
                    }
                    last_duration = sample
                        .GetSampleDuration()
                        .operation("Reading the decoded screen duration")?;
                }
                if sample
                    .GetTotalLength()
                    .operation("Reading decoded pixels")?
                    < width * height * 3 / 2
                {
                    return Err("Decoded frame has too few pixels".into());
                }
                previous = timestamp;
                count += 1;
            }
            if flags & sdk_uint(MF_SOURCE_READERF_ENDOFSTREAM.0) != 0 {
                break;
            }
        }
        if count != expected_frames {
            return Err("MP4 did not fully decode the retained snapshot".into());
        }
        if let Some(samples) = captured {
            let first = samples.first().ok_or("No captured frames supplied")?;
            let last = samples.last().ok_or("No captured frames supplied")?;
            let captured_duration = last.pts - first.pts + last.duration;
            if (previous + last_duration).abs_diff(captured_duration) >= 1000 {
                return Err("Live MP4 duration does not preserve captured elapsed time".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    static SERIAL: Mutex<()> = Mutex::new(());

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, String> {
            let _com = ComRuntime::new()?;
            // SAFETY: COM is initialized and the writable GUID string is local.
            let suffix = unsafe {
                let identifier = CoCreateGuid().operation("Naming the media test directory")?;
                let mut suffix = [0; 40];
                let count = StringFromGUID2(&raw const identifier, &mut suffix);
                if count <= 0 {
                    return Err("Unable to name the media test directory".into());
                }
                String::from_utf16_lossy(
                    &suffix[..usize::try_from(count - 1).map_err(|error| error.to_string())?],
                )
            };
            let path = std::env::temp_dir().join(format!("Optic-media-test-{suffix}"));
            std::fs::create_dir(&path).map_err(|error| error.to_string())?;
            Ok(Self(path))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn encoder_snapshots_fully_decode_and_export_failures_leave_no_files() -> Result<(), String> {
        let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let directory = TestDirectory::new()?;
        let received = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&received);
        generate_test_video(
            7,
            Arc::new(move |generation, sample| {
                output
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((generation, sample));
            }),
        )?;
        let frames = received.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            frames.len(),
            90,
            "Synthetic encoder must deliver ninety frames"
        );
        assert!(frames[0].1.keyframe);
        assert_eq!(frames[0].1.pts, 0);
        let mut keyframe_time = 0;
        for (index, (generation, frame)) in frames.iter().enumerate() {
            assert_eq!(*generation, 7);
            assert!(!frame.bytes.is_empty());
            assert_eq!((frame.format.width, frame.format.height), (640, 360));
            assert_eq!(frame.duration, FRAME_DURATION);
            if index > 0 {
                assert!(frame.pts > frames[index - 1].1.pts);
            }
            if frame.keyframe {
                keyframe_time = frame.pts;
            }
            assert!(
                frame.pts - keyframe_time < 10_400_000,
                "Keyframe spacing exceeds one second"
            );
        }
        let samples: Vec<_> = frames.iter().map(|(_, sample)| sample.clone()).collect();
        drop(frames);
        let full = directory.0.join("full.mp4");
        export_samples(&samples, &full)?;
        verify_clip(&full, 90)?;
        let original_size = std::fs::metadata(&full)
            .map_err(|error| error.to_string())?
            .len();
        assert!(
            export_samples(&samples, &full).is_err(),
            "Exporter overwrote an existing file"
        );
        assert_eq!(
            std::fs::metadata(&full)
                .map_err(|error| error.to_string())?
                .len(),
            original_size
        );
        assert!(
            samples[30].keyframe,
            "One-second boundary must be a keyframe"
        );
        let clip = directory.0.join("Replay-视频-é.mp4");
        export_samples(&samples[30..], &clip)?;
        verify_clip(&clip, 60)?;
        let mut deep_directory = directory.0.clone();
        for _ in 0..4 {
            deep_directory.push("x".repeat(70));
        }
        std::fs::create_dir_all(&deep_directory).map_err(|error| error.to_string())?;
        let deep_clip = deep_directory.join("deep.mp4");
        assert!(deep_clip.as_os_str().encode_wide().count() > 260);
        export_samples(&samples[30..], &deep_clip)?;
        verify_clip(&deep_clip, 60)?;
        let invalid = directory.0.join("invalid.mp4");
        assert!(
            export_samples(&samples[1..], &invalid).is_err(),
            "Exporter accepted a non-keyframe start"
        );
        assert!(!invalid.exists());
        let reversed = [samples[0].clone(), samples[2].clone(), samples[1].clone()];
        assert!(
            export_samples(&reversed, &invalid).is_err(),
            "Exporter accepted out-of-order timestamps"
        );
        let missing = directory.0.join("missing/output.mp4");
        assert!(
            export_samples(&samples, &missing).is_err(),
            "Exporter accepted a missing output directory"
        );
        for entry in std::fs::read_dir(&directory.0).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            assert!(
                path.is_dir() || path.extension().is_some_and(|extension| extension == "mp4"),
                "Failed export left a temporary file"
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "CI supplies OPTIC_VERIFY_CLIP after the app's synthetic self-test"]
    fn verify_external_clip() -> Result<(), String> {
        let path =
            std::env::var_os("OPTIC_VERIFY_CLIP").ok_or("OPTIC_VERIFY_CLIP was not supplied")?;
        verify_clip(Path::new(&path), 60)
    }
}
