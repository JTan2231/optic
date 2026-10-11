//! WASAPI loopback of the active playback endpoint. All COM calls stay on the
//! capture worker; packet reads never wait for audio to become available.

use std::{
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};
use windows::{
    core::Error,
    Win32::{
        Media::Audio::{
            eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
            MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
            AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
        },
        System::{
            Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL},
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
        },
    },
};

pub const SAMPLE_RATE: u64 = 48_000;
pub const FRAME_BYTES: usize = 4;
const DEVICE_CHECK: Duration = Duration::from_secs(1);
const CLOCK_CORRECTION: Duration = Duration::from_millis(2);

pub struct Packet {
    pub bytes: Vec<u8>,
    pub recorded_at: Instant,
}

fn failure(operation: &str, error: &Error) -> String {
    format!(
        "{operation} failed (HRESULT 0x{:08X}). Audio will retry while video continues.",
        error.code().0
    )
}

pub const fn frame_duration(frames: u64) -> Duration {
    Duration::from_nanos(frames.saturating_mul(1_000_000_000) / SAMPLE_RATE)
}

const fn pcm_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: 2,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 192_000,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// `GetBuffer` supplies QPC values already converted to 100 ns. Calibrating that
/// counter to Instant ties audio to the desktop timeline without wall clocks.
struct CounterClock {
    at: Instant,
    ticks: u64,
}

impl CounterClock {
    fn new() -> Result<Self, String> {
        let mut frequency = 0_i64;
        let mut counter = 0_i64;
        // SAFETY: Both system functions write only to these initialized locals.
        unsafe { QueryPerformanceFrequency(&raw mut frequency) }
            .map_err(|error| failure("Reading the audio clock frequency", &error))?;
        let before = Instant::now();
        // SAFETY: counter is an initialized, writable local.
        unsafe { QueryPerformanceCounter(&raw mut counter) }
            .map_err(|error| failure("Reading the audio clock", &error))?;
        let after = Instant::now();
        if frequency <= 0 || counter < 0 {
            return Err("The playback audio clock is invalid.".into());
        }
        let ticks = u64::try_from(i128::from(counter) * 10_000_000 / i128::from(frequency))
            .map_err(|_| "The playback audio clock is too large.")?;
        Ok(Self {
            at: before + after.duration_since(before) / 2,
            ticks,
        })
    }

    fn instant(&self, ticks: u64) -> Instant {
        let offset = Duration::from_nanos(ticks.abs_diff(self.ticks).saturating_mul(100));
        if ticks >= self.ticks {
            self.at.checked_add(offset)
        } else {
            self.at.checked_sub(offset)
        }
        .unwrap_or(self.at)
    }
}

#[derive(Default)]
struct PacketClock {
    anchor: Option<(u64, Instant)>,
    next_frame: Option<u64>,
}

impl PacketClock {
    fn recorded_at(
        &mut self,
        position: u64,
        timestamp: Option<Instant>,
        discontinuity: bool,
        frames: u64,
        now: Instant,
    ) -> Instant {
        if self.anchor.is_none()
            || discontinuity
            || self.next_frame.is_some_and(|next| next != position)
        {
            let at =
                timestamp.unwrap_or_else(|| now.checked_sub(frame_duration(frames)).unwrap_or(now));
            self.anchor = Some((position, at));
        }
        let (anchor_position, anchor_time) = self.anchor.unwrap_or((position, now));
        let mut at = anchor_time + frame_duration(position.saturating_sub(anchor_position));
        if let Some(measured) = timestamp {
            // Device oscillators can drift against the desktop's QPC clock over
            // a long session. Preserve exact sample cadence for small timestamp
            // jitter, and re-anchor when drift reaches two milliseconds. The
            // output timeline trims any sample overlap after a backward correction.
            let drift = measured.max(at).duration_since(measured.min(at));
            if drift >= CLOCK_CORRECTION {
                self.anchor = Some((position, measured));
                at = measured;
            }
        }
        self.next_frame = Some(position.saturating_add(frames));
        at
    }
}

struct Session {
    // Drop/Stop happens before the capture worker uninitializes COM.
    client: IAudioClient,
    capture: IAudioCaptureClient,
    endpoint_id: String,
    counter: CounterClock,
    clock: PacketClock,
    _thread_only: PhantomData<Rc<()>>,
}

fn endpoint() -> Result<(IMMDevice, String), String> {
    // SAFETY: The capture worker owns an initialized MTA; no outer aggregation.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|error| failure("Opening playback devices", &error))?;
    // SAFETY: The enumerator remains live for this synchronous query.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .map_err(|error| failure("Finding the active playback device", &error))?;
    // SAFETY: GetId returns a NUL-terminated string allocated with CoTaskMemAlloc.
    let raw_id = unsafe { device.GetId() }
        .map_err(|error| failure("Identifying the playback device", &error))?;
    // SAFETY: The API owns the string until the matching CoTaskMemFree below.
    let id = unsafe { raw_id.to_string() };
    // SAFETY: This allocation came from GetId and has not previously been freed.
    unsafe { CoTaskMemFree(Some(raw_id.0.cast())) };
    Ok((
        device,
        id.map_err(|_| "The playback device has an invalid identifier.")?,
    ))
}

impl Session {
    fn new(device: &IMMDevice, endpoint_id: String) -> Result<Self, String> {
        // SAFETY: The endpoint is live on the initialized COM capture worker.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|error| failure("Opening system audio capture", &error))?;
        let format = pcm_format();
        // SAFETY: The format describes interleaved stereo PCM. Shared-mode
        // conversion handles the endpoint's native channels/sample rate; loopback
        // reads its combined playback mix. A larger endpoint queue tolerates a
        // delayed video tick without adding capture latency.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                10_000_000,
                0,
                &raw const format,
                None,
            )
        }
        .map_err(|error| failure("Starting playback audio capture", &error))?;
        // SAFETY: The client was initialized with LOOPBACK on a render endpoint.
        let capture: IAudioCaptureClient = unsafe { client.GetService() }
            .map_err(|error| failure("Opening the playback audio stream", &error))?;
        let counter = CounterClock::new()?;
        // SAFETY: This initialized stream has not already been started.
        unsafe { client.Start() }
            .map_err(|error| failure("Starting the playback audio stream", &error))?;
        Ok(Self {
            client,
            capture,
            endpoint_id,
            counter,
            clock: PacketClock::default(),
            _thread_only: PhantomData,
        })
    }

    fn drain(&mut self) -> Result<Vec<Packet>, String> {
        let mut packets = Vec::new();
        loop {
            // SAFETY: The capture interface remains live on its owning worker.
            let pending = unsafe { self.capture.GetNextPacketSize() }
                .map_err(|error| failure("Reading available playback audio", &error))?;
            if pending == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            let mut position = 0;
            let mut qpc = 0;
            // SAFETY: All outputs are writable locals. The packet lease below
            // releases the buffer before another read, on the same worker.
            unsafe {
                self.capture.GetBuffer(
                    &raw mut data,
                    &raw mut frames,
                    &raw mut flags,
                    Some(&raw mut position),
                    Some(&raw mut qpc),
                )
            }
            .map_err(|error| failure("Reading playback audio samples", &error))?;
            if frames == 0 {
                break;
            }
            let mut lease = PacketLease {
                capture: &self.capture,
                frames,
                active: true,
            };
            let size = usize::try_from(frames)
                .ok()
                .and_then(|frames| frames.checked_mul(FRAME_BYTES))
                .filter(|&bytes| bytes <= isize::MAX.unsigned_abs())
                .ok_or("The playback audio packet is too large.")?;
            let silent = flags & u32::try_from(AUDCLNT_BUFFERFLAGS_SILENT.0).unwrap_or(2) != 0;
            let bytes = if silent {
                vec![0; size]
            } else if data.is_null() {
                return Err("The playback audio packet has no samples.".into());
            } else {
                // SAFETY: WASAPI guarantees frames * nBlockAlign valid bytes
                // until ReleaseBuffer. The negotiated format has four-byte frames.
                unsafe { std::slice::from_raw_parts(data, size) }.to_vec()
            };
            let timestamp_valid = qpc != 0
                && flags & u32::try_from(AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0).unwrap_or(4) == 0;
            let discontinuity =
                flags & u32::try_from(AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0).unwrap_or(1) != 0;
            let recorded_at = self.clock.recorded_at(
                position,
                timestamp_valid.then(|| self.counter.instant(qpc)),
                discontinuity,
                u64::from(frames),
                Instant::now(),
            );
            // SAFETY: This packet was acquired successfully and has not yet been released.
            let released = unsafe { self.capture.ReleaseBuffer(frames) };
            lease.active = false;
            released.map_err(|error| failure("Releasing playback audio samples", &error))?;
            packets.push(Packet { bytes, recorded_at });
        }
        Ok(packets)
    }
}

struct PacketLease<'a> {
    capture: &'a IAudioCaptureClient,
    frames: u32,
    active: bool,
}

impl Drop for PacketLease<'_> {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: This guard follows one successful nonempty GetBuffer.
            let _ = unsafe { self.capture.ReleaseBuffer(self.frames) };
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: Stop is called on the worker that started this live stream.
        let _ = unsafe { self.client.Stop() };
    }
}

/// Audio failure never terminates desktop capture. One warning per unavailable
/// episode prevents device/service failures from flooding the recorder log.
pub struct LoopbackCapture {
    session: Option<Session>,
    next_check: Instant,
    warned: bool,
}

impl LoopbackCapture {
    pub const fn new(now: Instant) -> Self {
        Self {
            session: None,
            next_check: now,
            warned: false,
        }
    }

    fn refresh(&mut self, now: Instant) -> Result<(), String> {
        if now < self.next_check {
            return Ok(());
        }
        self.next_check = now + DEVICE_CHECK;
        let (device, id) = endpoint()?;
        if self
            .session
            .as_ref()
            .is_none_or(|session| session.endpoint_id != id)
        {
            self.session = None;
            self.session = Some(Session::new(&device, id)?);
        }
        Ok(())
    }

    pub fn poll(&mut self, now: Instant) -> (Vec<Packet>, Option<String>) {
        let result = self.refresh(now).and_then(|()| {
            self.session
                .as_mut()
                .map_or_else(|| Ok(Vec::new()), Session::drain)
        });
        match result {
            Ok(packets) => {
                if self.session.is_some() {
                    self.warned = false;
                }
                (packets, None)
            }
            Err(message) => {
                self.session = None;
                self.next_check = now + DEVICE_CHECK;
                let warning = (!self.warned).then_some(message);
                self.warned = true;
                (Vec::new(), warning)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::{
        Media::Audio::IAudioRenderClient,
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
    };

    #[test]
    fn packets_use_device_frames_between_discontinuities() {
        let base = Instant::now();
        let mut clock = PacketClock::default();
        assert_eq!(clock.recorded_at(0, Some(base), true, 480, base), base);
        assert_eq!(
            clock.recorded_at(
                480,
                Some(base + Duration::from_millis(11)),
                false,
                480,
                base
            ),
            base + Duration::from_millis(10)
        );
        assert_eq!(
            clock.recorded_at(960, None, false, 480, base),
            base + Duration::from_millis(20)
        );
        let resumed = base + Duration::from_secs(4);
        assert_eq!(
            clock.recorded_at(100_000, Some(resumed), true, 480, resumed),
            resumed
        );
        assert_eq!(
            clock.recorded_at(100_480, None, false, 480, resumed),
            resumed + Duration::from_millis(10)
        );
    }

    #[test]
    fn device_clock_drift_is_corrected_against_video_clock() {
        let base = Instant::now();
        let mut clock = PacketClock::default();
        assert_eq!(clock.recorded_at(0, Some(base), false, 480, base), base);
        assert_eq!(
            clock.recorded_at(
                480,
                Some(base + Duration::from_millis(13)),
                false,
                480,
                base
            ),
            base + Duration::from_millis(13)
        );
        assert_eq!(
            clock.recorded_at(
                960,
                Some(base + Duration::from_millis(24)),
                false,
                480,
                base
            ),
            base + Duration::from_millis(23)
        );
        assert_eq!(
            clock.recorded_at(
                1440,
                Some(base + Duration::from_millis(30)),
                false,
                480,
                base
            ),
            base + Duration::from_millis(30)
        );
    }

    struct TestApartment;

    impl Drop for TestApartment {
        fn drop(&mut self) {
            // SAFETY: The test constructs this only after successful COM initialization.
            unsafe { CoUninitialize() };
        }
    }

    struct TestPlayback(IAudioClient);

    impl Drop for TestPlayback {
        fn drop(&mut self) {
            // SAFETY: This test owns the initialized render client on its COM thread.
            let _ = unsafe { self.0.Stop() };
        }
    }

    fn fill_tone(render: &IAudioRenderClient, frames: u32, cursor: &mut u64) -> Result<(), String> {
        if frames == 0 {
            return Ok(());
        }
        // SAFETY: The caller requests only the free frames in the initialized endpoint buffer.
        let data = unsafe { render.GetBuffer(frames) }
            .map_err(|error| failure("Opening the test tone buffer", &error))?;
        let length =
            usize::try_from(frames).map_err(|_| "Invalid test tone frame count.")? * FRAME_BYTES;
        // SAFETY: The render buffer contains precisely frames * four PCM bytes until ReleaseBuffer.
        let bytes = unsafe { std::slice::from_raw_parts_mut(data, length) };
        for frame in bytes.as_chunks_mut::<FRAME_BYTES>().0 {
            // A quiet 400 Hz triangular wave avoids any dependency on an external
            // playback application or files and is easy to distinguish from silence.
            let phase = i16::try_from(*cursor % 120).map_err(|_| "Invalid test tone phase.")?;
            let triangle = if phase < 60 { phase } else { 120 - phase };
            let sample = (triangle * 40 - 1200).to_le_bytes();
            frame[..2].copy_from_slice(&sample);
            frame[2..].copy_from_slice(&sample);
            *cursor += 1;
        }
        // SAFETY: The full requested render packet is initialized with valid stereo PCM.
        unsafe { render.ReleaseBuffer(frames, 0) }
            .map_err(|error| failure("Submitting the test tone", &error))
    }

    /// Explicit hardware validation: plays a quiet tone for two seconds and
    /// confirms the active playback endpoint's mixed audio reaches WASAPI PCM.
    #[test]
    #[ignore = "requires an active playback endpoint and briefly plays a quiet tone"]
    fn live_loopback_captures_playback_tone() -> Result<(), String> {
        // SAFETY: This test initializes and uninitializes COM on its own test thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|error| failure("Initializing the audio test", &error))?;
        let _apartment = TestApartment;
        let mut loopback = LoopbackCapture::new(Instant::now());
        let (_, warning) = loopback.poll(Instant::now());
        if let Some(message) = warning {
            return Err(message);
        }
        let (device, _) = endpoint()?;
        // SAFETY: The playback endpoint is live on this test's initialized COM thread.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|error| failure("Opening test tone playback", &error))?;
        let format = pcm_format();
        // SAFETY: Shared-mode conversion accepts initialized stereo PCM; this is
        // a render stream, so it deliberately has no LOOPBACK flag.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                2_000_000,
                0,
                &raw const format,
                None,
            )
        }
        .map_err(|error| failure("Initializing test tone playback", &error))?;
        let playback = TestPlayback(client);
        // SAFETY: The initialized render client supports IAudioRenderClient.
        let render: IAudioRenderClient = unsafe { playback.0.GetService() }
            .map_err(|error| failure("Opening test tone rendering", &error))?;
        // SAFETY: The initialized client remains live on this test thread.
        let capacity = unsafe { playback.0.GetBufferSize() }
            .map_err(|error| failure("Reading test tone buffer capacity", &error))?;
        let mut cursor = 0;
        fill_tone(&render, capacity, &mut cursor)?;
        // SAFETY: The first render packet was initialized and this stream is not already started.
        unsafe { playback.0.Start() }
            .map_err(|error| failure("Starting test tone playback", &error))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut packets = Vec::new();
        while Instant::now() < deadline {
            let (available, warning) = loopback.poll(Instant::now());
            if let Some(message) = warning {
                return Err(message);
            }
            packets.extend(available);
            // SAFETY: The initialized render client remains live on this test thread.
            let padding = unsafe { playback.0.GetCurrentPadding() }
                .map_err(|error| failure("Reading test tone playback position", &error))?;
            fill_tone(&render, capacity.saturating_sub(padding), &mut cursor)?;
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            packets.len() >= 10,
            "Loopback did not deliver playback packets."
        );
        assert!(
            packets
                .iter()
                .any(|packet| packet.bytes.iter().any(|&byte| byte != 0)),
            "Loopback produced only silence while a tone was playing."
        );
        assert!(packets
            .iter()
            .all(|packet| !packet.bytes.is_empty() && packet.bytes.len() % FRAME_BYTES == 0));
        for pair in packets.windows(2) {
            assert!(
                pair[1].recorded_at > pair[0].recorded_at,
                "Playback packet timestamps did not increase."
            );
        }
        Ok(())
    }
}
