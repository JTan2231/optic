import AVFoundation
import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import ScreenCaptureKit
import VideoToolbox

public typealias FrameCallback = @convention(c) (
    UInt64, UnsafeMutableRawPointer?, Double, Double, Int, Bool
) -> Void
// Event kinds: 1 = started, 2 = stopped, 3 = error, 4 = expected interruption.
// The message is borrowed UTF-8 and is valid only for the callback invocation.
public typealias EventCallback = @convention(c) (UInt64, Int32, UnsafePointer<CChar>?) -> Void

private struct MediaFailure: Error, CustomStringConvertible {
    let description: String
    init(_ message: String) { description = message }
}

private func check(_ status: OSStatus, _ operation: String) throws {
    if status != noErr { throw MediaFailure("\(operation) failed (OSStatus \(status)).") }
}

private func mediaErrorDescription(_ error: Error, operation: String) -> String {
    if let failure = error as? MediaFailure { return failure.description }
    // NSError descriptions and userInfo can contain personal output paths.
    // Preserve the native error identity alongside the operation instead.
    let failure = error as NSError
    return "\(operation) (\(failure.domain) code \(failure.code))."
}

private func writeError(_ error: Error, operation: String,
                        to buffer: UnsafeMutablePointer<CChar>?, capacity: Int) {
    guard let buffer, capacity > 0 else { return }
    let bytes = Array(mediaErrorDescription(error, operation: operation).utf8.prefix(capacity - 1))
    for (index, byte) in bytes.enumerated() { buffer[index] = CChar(bitPattern: byte) }
    buffer[bytes.count] = 0
}

private func isKeyframe(_ sample: CMSampleBuffer) -> Bool {
    guard let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
        as? [[String: Any]], let first = attachments.first else { return true }
    return !(first[kCMSampleAttachmentKey_NotSync as String] as? Bool ?? false)
}

// The gate makes stop a boundary: once disabled, no encoded sample can be transferred to Rust.
private final class FrameGate {
    private let lock = NSLock()
    private var active = true
    private let generation: UInt64
    private let callback: FrameCallback

    init(generation: UInt64, callback: @escaping FrameCallback) {
        self.generation = generation
        self.callback = callback
    }

    func disable() {
        lock.lock()
        active = false
        lock.unlock()
    }

    func deliver(_ sample: CMSampleBuffer) {
        lock.lock()
        defer { lock.unlock() }
        guard active else { return }
        let pts = CMTimeGetSeconds(CMSampleBufferGetPresentationTimeStamp(sample))
        let duration = CMTimeGetSeconds(CMSampleBufferGetDuration(sample))
        guard pts.isFinite else { return }
        // Exactly one retain is transferred. Rust eventually calls optic_sample_release.
        let pointer = Unmanaged.passRetained(sample).toOpaque()
        callback(generation, pointer, pts,
                 duration.isFinite && duration > 0 ? duration : 1.0 / 30.0,
                 CMSampleBufferGetTotalSampleSize(sample), isKeyframe(sample))
    }
}

private let compressionCallback: VTCompressionOutputCallback = { context, _, status, flags, sample in
    guard let context else { return }
    let encoder = Unmanaged<H264Encoder>.fromOpaque(context).takeUnretainedValue()
    if status != noErr {
        encoder.failed(MediaFailure("H.264 encoding failed (OSStatus \(status))."))
    } else if !flags.contains(.frameDropped), let sample, CMSampleBufferDataIsReady(sample) {
        encoder.gate.deliver(sample)
    }
}

private final class H264Encoder {
    let gate: FrameGate
    private var session: VTCompressionSession?
    private let failureLock = NSLock()
    private var callbackFailure: MediaFailure?
    private let onFailure: ((MediaFailure) -> Void)?
    private var lastForcedKeyframe: CMTime?

    init(width: Int, height: Int, gate: FrameGate,
         onFailure: ((MediaFailure) -> Void)? = nil) throws {
        self.gate = gate
        self.onFailure = onFailure
        let attributes: [String: Any] = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA,
            kCVPixelBufferWidthKey as String: width,
            kCVPixelBufferHeightKey as String: height,
            kCVPixelBufferIOSurfacePropertiesKey as String: [:]
        ]
        var result = VTCompressionSessionCreate(
            allocator: kCFAllocatorDefault, width: Int32(width), height: Int32(height),
            codecType: kCMVideoCodecType_H264,
            encoderSpecification: [
                kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: true
            ] as CFDictionary,
            imageBufferAttributes: attributes as CFDictionary, compressedDataAllocator: nil,
            outputCallback: compressionCallback,
            refcon: Unmanaged.passUnretained(self).toOpaque(), compressionSessionOut: &session)
        if result != noErr {
            if let session { VTCompressionSessionInvalidate(session) }
            session = nil
            result = VTCompressionSessionCreate(
                allocator: kCFAllocatorDefault, width: Int32(width), height: Int32(height),
                codecType: kCMVideoCodecType_H264,
                encoderSpecification: [
                    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: false
                ] as CFDictionary,
                imageBufferAttributes: attributes as CFDictionary, compressedDataAllocator: nil,
                outputCallback: compressionCallback,
                refcon: Unmanaged.passUnretained(self).toOpaque(), compressionSessionOut: &session)
        }
        try check(result, "Creating the H.264 encoder")
        guard let session else { throw MediaFailure("The H.264 encoder returned no session.") }
        let properties: [CFString: Any] = [
            kVTCompressionPropertyKey_RealTime: true,
            kVTCompressionPropertyKey_AllowFrameReordering: false,
            kVTCompressionPropertyKey_AverageBitRate: 8_000_000,
            kVTCompressionPropertyKey_ExpectedFrameRate: 30,
            kVTCompressionPropertyKey_MaxKeyFrameInterval: 30,
            kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: 1.0,
            kVTCompressionPropertyKey_ProfileLevel: kVTProfileLevel_H264_High_AutoLevel,
            kVTCompressionPropertyKey_ColorPrimaries: kCMFormatDescriptionColorPrimaries_ITU_R_709_2,
            kVTCompressionPropertyKey_TransferFunction: kCMFormatDescriptionTransferFunction_sRGB,
            kVTCompressionPropertyKey_YCbCrMatrix: kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2
        ]
        do {
            try check(VTSessionSetProperties(session, propertyDictionary: properties as CFDictionary),
                      "Configuring the H.264 encoder")
            try check(VTCompressionSessionPrepareToEncodeFrames(session), "Preparing the H.264 encoder")
        } catch {
            VTCompressionSessionInvalidate(session)
            self.session = nil
            throw error
        }
    }

    func failed(_ error: MediaFailure) {
        failureLock.lock()
        let first = callbackFailure == nil
        if first { callbackFailure = error }
        failureLock.unlock()
        if first { onFailure?(error) }
    }

    func encode(_ pixelBuffer: CVPixelBuffer, at pts: CMTime) throws {
        guard let session else { throw MediaFailure("The video encoder has stopped.") }
        let forceKeyframe = lastForcedKeyframe == nil
            || CMTimeGetSeconds(CMTimeSubtract(pts, lastForcedKeyframe!)) >= 1.0
        if forceKeyframe { lastForcedKeyframe = pts }
        try check(VTCompressionSessionEncodeFrame(
            session, imageBuffer: pixelBuffer, presentationTimeStamp: pts,
            duration: CMTime(value: 1, timescale: 30),
            frameProperties: forceKeyframe
                ? [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary : nil,
            sourceFrameRefcon: nil, infoFlagsOut: nil), "Submitting a video frame")
    }

    func finish() throws {
        guard let session else { return }
        // CompleteFrames emits every outstanding callback before returning; refcon remains alive.
        let status = VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        VTCompressionSessionInvalidate(session)
        self.session = nil
        try check(status, "Finishing the H.264 encoder")
        failureLock.lock()
        let failure = callbackFailure
        failureLock.unlock()
        if let failure { throw failure }
    }

    deinit {
        if let session { VTCompressionSessionInvalidate(session) }
    }
}

private final class CaptureSession: NSObject, SCStreamOutput, SCStreamDelegate {
    let generation: UInt64
    private let queue: DispatchQueue
    private let eventCallback: EventCallback
    private let gate: FrameGate
    private var stream: SCStream?
    private var encoder: H264Encoder?
    private var timer: DispatchSourceTimer?
    private var latestPixelBuffer: CVPixelBuffer?
    private var width = 0
    private var height = 0
    private var starting = false
    private var stopping = false
    private var stopRequested = false
    private var finished = false
    private var sendStopped = false
    private var completions: [() -> Void] = []

    init(generation: UInt64, queue: DispatchQueue,
         frameCallback: @escaping FrameCallback, eventCallback: @escaping EventCallback) {
        self.generation = generation
        self.queue = queue
        self.eventCallback = eventCallback
        gate = FrameGate(generation: generation, callback: frameCallback)
    }

    private func event(_ kind: Int32, _ message: String) {
        message.withCString { eventCallback(generation, kind, $0) }
    }

    func start() {
        guard !stopping, !finished else { return }
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { [weak self] content, error in
            guard let self else { return }
            self.queue.async {
                guard !self.stopping, !self.finished else { return }
                if let error {
                    self.fail(mediaErrorDescription(error, operation: "Unable to capture the screen"))
                    return
                }
                guard let display = content?.displays.first(where: { $0.displayID == CGMainDisplayID() }) else {
                    self.fail("The main display is unavailable.")
                    return
                }
                let scale = min(1.0, 1080.0 / Double(display.height))
                self.width = max(2, Int(Double(display.width) * scale) / 2 * 2)
                self.height = max(2, Int(Double(display.height) * scale) / 2 * 2)
                let configuration = SCStreamConfiguration()
                configuration.width = self.width
                configuration.height = self.height
                configuration.minimumFrameInterval = CMTime(value: 1, timescale: 30)
                configuration.pixelFormat = kCVPixelFormatType_32BGRA
                configuration.colorSpaceName = CGColorSpace.sRGB
                configuration.showsCursor = true
                configuration.capturesAudio = false
                configuration.queueDepth = 3
                let filter = SCContentFilter(display: display, excludingWindows: [])
                let stream = SCStream(filter: filter, configuration: configuration, delegate: self)
                do {
                    self.encoder = try H264Encoder(width: self.width, height: self.height, gate: self.gate) { [weak self] error in
                        guard let self else { return }
                        self.queue.async { self.fail(error.description) }
                    }
                    try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: self.queue)
                    self.stream = stream
                    self.starting = true
                    stream.startCapture { error in
                        self.queue.async { self.didStart(error) }
                    }
                } catch {
                    self.fail(mediaErrorDescription(error, operation: "Unable to configure screen capture"))
                }
            }
        }
    }

    private func didStart(_ error: Error?) {
        starting = false
        if stopping {
            if error == nil { stopStream() } else { finishStop() }
            return
        }
        if let error {
            fail(mediaErrorDescription(error, operation: "Unable to start screen capture"))
            return
        }
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now(), repeating: .nanoseconds(33_333_333), leeway: .milliseconds(2))
        timer.setEventHandler { [weak self] in self?.encodeLatestFrame() }
        self.timer = timer
        timer.resume()
        event(1, "Screen capture started.")
    }

    private func encodeLatestFrame() {
        guard !stopping, let latestPixelBuffer, let encoder else { return }
        do {
            try encoder.encode(latestPixelBuffer, at: CMClockGetTime(CMClockGetHostTimeClock()))
        } catch { fail(mediaErrorDescription(error, operation: "Unable to encode the screen capture")) }
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sample: CMSampleBuffer, of outputType: SCStreamOutputType) {
        guard !stopping, outputType == .screen, CMSampleBufferIsValid(sample) else { return }
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
            as? [[SCStreamFrameInfo: Any]]
        guard let rawStatus = attachments?.first?[.status] as? NSNumber,
              let status = SCFrameStatus(rawValue: rawStatus.intValue) else { return }
        switch status {
        case .idle: return // The encoding timer repeats the last complete image.
        case .blank, .suspended, .stopped:
            latestPixelBuffer = nil
            interrupt("Screen capture was interrupted. Resume buffering when the display is available.")
            return
        case .complete, .started: break
        @unknown default: return
        }
        guard let pixelBuffer = CMSampleBufferGetImageBuffer(sample) else { return }
        guard CVPixelBufferGetWidth(pixelBuffer) == width,
              CVPixelBufferGetHeight(pixelBuffer) == height else {
            interrupt("The screen capture size changed. Resume buffering to start a new video stream.")
            return
        }
        latestPixelBuffer = pixelBuffer
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        queue.async { self.fail(mediaErrorDescription(error, operation: "Screen capture stopped unexpectedly")) }
    }

    private func fail(_ message: String) {
        guard !stopping, !finished else { return }
        event(3, message)
        stop(emitStopped: false)
    }

    private func interrupt(_ message: String) {
        guard !stopping, !finished else { return }
        event(4, message)
        stop(emitStopped: false)
    }

    func stop(emitStopped: Bool, completion: (() -> Void)? = nil) {
        if finished { completion?(); return }
        if let completion { completions.append(completion) }
        sendStopped = sendStopped || emitStopped
        if stopping { return }
        stopping = true
        gate.disable()
        timer?.cancel()
        timer = nil
        latestPixelBuffer = nil
        if !starting { stopStream() }
    }

    private func stopStream() {
        guard !stopRequested else { return }
        stopRequested = true
        guard let stream else { finishStop(); return }
        stream.stopCapture { _ in
            self.queue.async { self.finishStop() }
        }
    }

    private func finishStop() {
        guard !finished else { return }
        finished = true
        try? encoder?.finish()
        encoder = nil
        if let stream { try? stream.removeStreamOutput(self, type: .screen) }
        stream = nil
        if sendStopped { event(2, "Screen capture stopped.") }
        let callbacks = completions
        completions.removeAll()
        for callback in callbacks { callback() }
    }
}

private final class CaptureManager {
    static let shared = CaptureManager()
    private let queue = DispatchQueue(label: "optic.capture")
    private var current: CaptureSession?

    func start(generation: UInt64, frames: @escaping FrameCallback, events: @escaping EventCallback) {
        queue.async {
            if self.current?.generation == generation { return }
            let session = CaptureSession(generation: generation, queue: self.queue,
                                         frameCallback: frames, eventCallback: events)
            let previous = self.current
            self.current = session
            if let previous {
                previous.stop(emitStopped: true) {
                    if self.current === session { session.start() }
                }
            } else { session.start() }
        }
    }

    func stop(generation: UInt64) {
        queue.async {
            guard let session = self.current, session.generation == generation else { return }
            // Keep it retained through the asynchronous ScreenCaptureKit shutdown.
            session.stop(emitStopped: true) {
                if self.current === session { self.current = nil }
            }
        }
    }
}

@_cdecl("optic_capture_start")
public func optic_capture_start(_ generation: UInt64, _ frames: @escaping FrameCallback,
                                _ events: @escaping EventCallback) {
    CaptureManager.shared.start(generation: generation, frames: frames, events: events)
}

@_cdecl("optic_capture_stop")
public func optic_capture_stop(_ generation: UInt64) {
    CaptureManager.shared.stop(generation: generation)
}

@_cdecl("optic_sample_release")
public func optic_sample_release(_ sample: UnsafeMutableRawPointer?) {
    if let sample { Unmanaged<CMSampleBuffer>.fromOpaque(sample).release() }
}

private func rebasedSample(_ sample: CMSampleBuffer, offset: CMTime) throws -> CMSampleBuffer {
    var count = 0
    try check(CMSampleBufferGetSampleTimingInfoArray(sample, entryCount: 0,
        arrayToFill: nil, entriesNeededOut: &count), "Reading video timing")
    guard count > 0 else { throw MediaFailure("The video frame has no timing information.") }
    var timings = Array(repeating: CMSampleTimingInfo(duration: .invalid,
        presentationTimeStamp: .invalid, decodeTimeStamp: .invalid), count: count)
    try check(CMSampleBufferGetSampleTimingInfoArray(sample, entryCount: count,
        arrayToFill: &timings, entriesNeededOut: &count), "Reading video timing")
    for index in timings.indices {
        timings[index].presentationTimeStamp = CMTimeSubtract(timings[index].presentationTimeStamp, offset)
        if timings[index].decodeTimeStamp.isNumeric {
            timings[index].decodeTimeStamp = CMTimeSubtract(timings[index].decodeTimeStamp, offset)
        }
    }
    var copy: CMSampleBuffer?
    try check(CMSampleBufferCreateCopyWithNewTiming(allocator: kCFAllocatorDefault,
        sampleBuffer: sample, sampleTimingEntryCount: timings.count, sampleTimingArray: &timings,
        sampleBufferOut: &copy), "Rebasing video timing")
    guard let copy else { throw MediaFailure("Unable to copy the video sample.") }
    return copy
}

// Synchronous worker API. The caller keeps every sample alive until this function returns.
@_cdecl("optic_export")
public func optic_export(_ pointers: UnsafePointer<UnsafeMutableRawPointer?>?, _ count: Int,
                         _ path: UnsafePointer<CChar>?, _ errorBuffer: UnsafeMutablePointer<CChar>?,
                         _ errorCapacity: Int) -> Bool {
    if let errorBuffer, errorCapacity > 0 { errorBuffer[0] = 0 }
    do {
        guard let pointers, count > 0, let path else { throw MediaFailure("No video frames or output path supplied.") }
        let url = URL(fileURLWithPath: String(cString: path))
        guard !FileManager.default.fileExists(atPath: url.path) else {
            throw MediaFailure("The output file already exists.")
        }
        var samples: [CMSampleBuffer] = []
        samples.reserveCapacity(count)
        for index in 0..<count {
            guard let pointer = pointers[index] else { throw MediaFailure("A video frame is missing.") }
            samples.append(Unmanaged<CMSampleBuffer>.fromOpaque(pointer).takeUnretainedValue())
        }
        let first = samples[0]
        guard isKeyframe(first) else { throw MediaFailure("The clip must start at a keyframe.") }
        guard let format = CMSampleBufferGetFormatDescription(first),
              CMFormatDescriptionGetMediaSubType(format) == kCMVideoCodecType_H264 else {
            throw MediaFailure("The clip does not contain H.264 video.")
        }
        let offset = CMSampleBufferGetPresentationTimeStamp(first)
        guard offset.isNumeric else { throw MediaFailure("The clip has an invalid start time.") }
        let writer = try AVAssetWriter(outputURL: url, fileType: .mp4)
        var succeeded = false
        defer {
            if !succeeded {
                writer.cancelWriting()
                try? FileManager.default.removeItem(at: url)
            }
        }
        let input = AVAssetWriterInput(mediaType: .video, outputSettings: nil, sourceFormatHint: format)
        input.expectsMediaDataInRealTime = false
        guard writer.canAdd(input) else { throw MediaFailure("Unable to write this H.264 format to MP4.") }
        writer.add(input)
        guard writer.startWriting() else { throw writer.error ?? MediaFailure("Unable to start writing the MP4 file.") }
        writer.startSession(atSourceTime: .zero)
        let deadline = ProcessInfo.processInfo.systemUptime + 60.0
        var previousPTS: CMTime?
        for sample in samples {
            guard let sampleFormat = CMSampleBufferGetFormatDescription(sample),
                  CMFormatDescriptionEqual(format, otherFormatDescription: sampleFormat) else {
                throw MediaFailure("The clip contains incompatible video formats.")
            }
            let pts = CMSampleBufferGetPresentationTimeStamp(sample)
            guard pts.isNumeric, previousPTS == nil || CMTimeCompare(pts, previousPTS!) > 0 else {
                throw MediaFailure("Video frames have invalid or out-of-order timestamps.")
            }
            previousPTS = pts
            while !input.isReadyForMoreMediaData {
                guard writer.status == .writing else { throw writer.error ?? MediaFailure("The MP4 writer stopped unexpectedly.") }
                guard ProcessInfo.processInfo.systemUptime < deadline else { throw MediaFailure("Writing the MP4 timed out.") }
                Thread.sleep(forTimeInterval: 0.002)
            }
            let copy = try rebasedSample(sample, offset: offset)
            guard input.append(copy) else { throw writer.error ?? MediaFailure("Unable to append a video frame to MP4.") }
        }
        input.markAsFinished()
        let completion = DispatchSemaphore(value: 0)
        writer.finishWriting { completion.signal() }
        let remaining = max(0, min(30.0, deadline - ProcessInfo.processInfo.systemUptime))
        guard completion.wait(timeout: .now() + remaining) == .success else {
            throw MediaFailure("Finalizing the MP4 timed out.")
        }
        guard writer.status == .completed else { throw writer.error ?? MediaFailure("The MP4 file could not be finalized.") }
        succeeded = true
        return true
    } catch {
        writeError(error, operation: "Exporting the MP4 failed", to: errorBuffer, capacity: errorCapacity)
        return false
    }
}

// Produces real H.264 samples without asking for screen recording permission.
@_cdecl("optic_generate_test_video")
public func optic_generate_test_video(_ generation: UInt64, _ frames: @escaping FrameCallback,
                                      _ errorBuffer: UnsafeMutablePointer<CChar>?,
                                      _ errorCapacity: Int) -> Bool {
    if let errorBuffer, errorCapacity > 0 { errorBuffer[0] = 0 }
    let gate = FrameGate(generation: generation, callback: frames)
    do {
        let width = 640, height = 360
        let encoder = try H264Encoder(width: width, height: height, gate: gate)
        defer { try? encoder.finish() }
        for frame in 0..<90 {
            try autoreleasepool {
                var pixelBuffer: CVPixelBuffer?
                try check(CVPixelBufferCreate(kCFAllocatorDefault, width, height,
                    kCVPixelFormatType_32BGRA, [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary,
                    &pixelBuffer), "Creating the test image")
                guard let pixelBuffer else { throw MediaFailure("Unable to allocate a test video frame.") }
                try check(CVPixelBufferLockBaseAddress(pixelBuffer, []), "Locking the test image")
                guard let base = CVPixelBufferGetBaseAddress(pixelBuffer) else {
                    CVPixelBufferUnlockBaseAddress(pixelBuffer, [])
                    throw MediaFailure("The test video image has no pixels.")
                }
                let rowBytes = CVPixelBufferGetBytesPerRow(pixelBuffer)
                for y in 0..<height {
                    let row = base.advanced(by: y * rowBytes).assumingMemoryBound(to: UInt32.self)
                    for x in 0..<width {
                        let square = x >= (frame * 5) % (width - 80) && x < (frame * 5) % (width - 80) + 80
                            && y >= 130 && y < 210
                        row[x] = square ? 0xFF_40_E0_60 : 0xFF_00_00_00 | UInt32(x % 200) << 8 | UInt32(y % 200)
                    }
                }
                CVPixelBufferUnlockBaseAddress(pixelBuffer, [])
                try encoder.encode(pixelBuffer, at: CMTime(value: Int64(frame), timescale: 30))
            }
        }
        try encoder.finish()
        return true
    } catch {
        gate.disable()
        writeError(error, operation: "Generating the test video failed", to: errorBuffer, capacity: errorCapacity)
        return false
    }
}
