#include "Media.h"

#include <windows.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <codecapi.h>
#include <wmcodecdsp.h>
#include <wrl/client.h>
#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <deque>
#include <limits>
#include <mutex>
#include <stdexcept>
#include <thread>
#include <utility>
#include <vector>

namespace optic {
namespace {
using Microsoft::WRL::ComPtr;
using Clock = std::chrono::steady_clock;
constexpr std::int64_t ticksPerSecond = 10'000'000;
constexpr std::int64_t frameDuration = ticksPerSecond / 30;

// Extended absolute paths avoid relying on the machine's LongPathsEnabled
// registry setting, including the temporary suffix used while finalizing MP4.
std::wstring file_path(const wchar_t* path) {
    const std::wstring supplied(path);
    if (supplied.rfind(L"\\\\?\\", 0) == 0) return supplied;
    const DWORD length = GetFullPathNameW(path, 0, nullptr, nullptr);
    if (!length) throw std::runtime_error("Invalid output path.");
    std::wstring absolute(length, L'\0');
    const DWORD copied = GetFullPathNameW(path, length, absolute.data(), nullptr);
    if (!copied || copied >= length) throw std::runtime_error("Could not resolve the output path.");
    absolute.resize(copied);
    if (absolute.rfind(L"\\\\", 0) == 0) return L"\\\\?\\UNC\\" + absolute.substr(2);
    return L"\\\\?\\" + absolute;
}

void check(HRESULT result, const char* operation) {
    if (FAILED(result)) {
        char message[512];
        std::snprintf(message, sizeof(message), "%s failed (HRESULT 0x%08lX).",
                      operation, static_cast<unsigned long>(result));
        throw std::runtime_error(message);
    }
}

class MediaRuntime final {
public:
    MediaRuntime() { check(MFStartup(MF_VERSION, MFSTARTUP_FULL), "Starting Media Foundation"); }
    ~MediaRuntime() { MFShutdown(); }
};

class ComRuntime final {
public:
    ComRuntime() { check(CoInitializeEx(nullptr, COINIT_MULTITHREADED), "Initializing COM"); }
    ~ComRuntime() { CoUninitialize(); }
};

// Plain immutable values are the only representation retained by Rust. In
// particular, no COM sample, buffer, transform or apartment crosses threads.
struct VideoFormat final {
    const UINT32 width;
    const UINT32 height;
    const std::vector<std::uint8_t> sequenceHeader;
    VideoFormat(UINT32 w, UINT32 h, std::vector<std::uint8_t> header)
        : width(w), height(h), sequenceHeader(std::move(header)) {}
    bool operator==(const VideoFormat& other) const {
        return width == other.width && height == other.height
            && sequenceHeader == other.sequenceHeader;
    }
};

struct EncodedSample final {
    const std::shared_ptr<const VideoFormat> format;
    const std::shared_ptr<const std::vector<std::uint8_t>> bytes;
    const std::int64_t pts;
    const std::int64_t duration;
    const bool keyframe;
    EncodedSample(std::shared_ptr<const VideoFormat> f,
                  std::shared_ptr<const std::vector<std::uint8_t>> b,
                  std::int64_t p, std::int64_t d, bool key)
        : format(std::move(f)), bytes(std::move(b)), pts(p), duration(d), keyframe(key) {}
};

ComPtr<IMFMediaType> video_type(UINT32 width, UINT32 height, REFGUID subtype) {
    ComPtr<IMFMediaType> type;
    check(MFCreateMediaType(&type), "Creating the video format");
    check(type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video), "Setting the media kind");
    check(type->SetGUID(MF_MT_SUBTYPE, subtype), "Setting the video subtype");
    check(MFSetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, width, height), "Setting the video size");
    check(MFSetAttributeRatio(type.Get(), MF_MT_FRAME_RATE, 30, 1), "Setting the frame rate");
    check(MFSetAttributeRatio(type.Get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1), "Setting the pixel ratio");
    check(type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive), "Setting progressive video");
    check(type->SetUINT32(MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709), "Setting video primaries");
    check(type->SetUINT32(MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_sRGB), "Setting the transfer function");
    check(type->SetUINT32(MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709), "Setting the color matrix");
    check(type->SetUINT32(MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235), "Setting the video range");
    if (subtype == MFVideoFormat_H264) {
        check(type->SetUINT32(MF_MT_AVG_BITRATE, 8'000'000), "Setting the video bitrate");
        check(type->SetUINT32(MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High), "Setting the H.264 profile");
    } else {
        check(type->SetUINT32(MF_MT_ALL_SAMPLES_INDEPENDENT, TRUE), "Setting independent input samples");
        check(type->SetUINT32(MF_MT_FIXED_SIZE_SAMPLES, TRUE), "Setting fixed input samples");
        check(type->SetUINT32(MF_MT_SAMPLE_SIZE, width * height * 3 / 2), "Setting the input size");
        check(type->SetUINT32(MF_MT_DEFAULT_STRIDE, width), "Setting the input stride");
    }
    return type;
}

void codec_uint(ICodecAPI* codec, const GUID& key, ULONG value, const char* operation,
                bool required = true) {
    VARIANT setting;
    VariantInit(&setting);
    setting.vt = VT_UI4;
    setting.ulVal = value;
    const HRESULT result = codec->SetValue(&key, &setting);
    if (required) check(result, operation);
}

void codec_bool(ICodecAPI* codec, const GUID& key, bool value, const char* operation,
                bool required = true) {
    VARIANT setting;
    VariantInit(&setting);
    setting.vt = VT_BOOL;
    setting.boolVal = value ? VARIANT_TRUE : VARIANT_FALSE;
    const HRESULT result = codec->SetValue(&key, &setting);
    if (required) check(result, operation);
}

ComPtr<IMFSample> make_sample(const std::uint8_t* bytes, std::size_t count,
                            std::int64_t pts, std::int64_t duration, bool keyframe) {
    if (count == 0 || count > std::numeric_limits<DWORD>::max())
        throw std::runtime_error("The video frame has an invalid encoded size.");
    ComPtr<IMFMediaBuffer> buffer;
    check(MFCreateMemoryBuffer(static_cast<DWORD>(count), &buffer), "Allocating a video buffer");
    BYTE* destination = nullptr;
    check(buffer->Lock(&destination, nullptr, nullptr), "Locking a video buffer");
    std::memcpy(destination, bytes, count);
    check(buffer->Unlock(), "Unlocking a video buffer");
    check(buffer->SetCurrentLength(static_cast<DWORD>(count)), "Setting a video buffer length");
    ComPtr<IMFSample> sample;
    check(MFCreateSample(&sample), "Creating a video sample");
    check(sample->AddBuffer(buffer.Get()), "Attaching a video buffer");
    check(sample->SetSampleTime(pts), "Setting the video timestamp");
    check(sample->SetSampleDuration(duration), "Setting the video duration");
    if (keyframe) check(sample->SetUINT32(MFSampleExtension_CleanPoint, TRUE), "Marking the keyframe");
    return sample;
}

bool contains_idr(const std::vector<std::uint8_t>& bytes) {
    for (std::size_t i = 0; i + 3 < bytes.size(); ++i) {
        if (bytes[i] != 0 || bytes[i + 1] != 0) continue;
        std::size_t nal = i + 2;
        if (bytes[nal] == 0) ++nal;
        if (nal + 1 < bytes.size() && bytes[nal] == 1 && (bytes[nal + 1] & 31) == 5) return true;
    }
    return false;
}

// BT.709 limited-range NV12, averaging each 2x2 block before subsampling.
// Desktop pixels are already sRGB; the metadata uses the same transfer curve.
void bgra_to_nv12(const std::uint8_t* bgra, std::size_t stride, UINT32 width,
                  UINT32 height, std::vector<std::uint8_t>& nv12) {
    const auto clamp = [](int value) {
        return static_cast<std::uint8_t>(std::clamp(value, 0, 255));
    };
    for (UINT32 y = 0; y < height; y += 2) {
        const auto* row0 = bgra + static_cast<std::size_t>(y) * stride;
        const auto* row1 = row0 + stride;
        auto* luma0 = nv12.data() + static_cast<std::size_t>(y) * width;
        auto* luma1 = luma0 + width;
        auto* chroma = nv12.data() + static_cast<std::size_t>(width) * height
            + static_cast<std::size_t>(y / 2) * width;
        for (UINT32 x = 0; x < width; x += 2) {
            int r = 0, g = 0, b = 0;
            for (UINT32 dy = 0; dy < 2; ++dy) {
                const auto* row = dy ? row1 : row0;
                auto* luma = dy ? luma1 : luma0;
                for (UINT32 dx = 0; dx < 2; ++dx) {
                    const auto* pixel = row + static_cast<std::size_t>(x + dx) * 4;
                    b += pixel[0]; g += pixel[1]; r += pixel[2];
                    luma[x + dx] = clamp(16 + ((47 * pixel[2] + 157 * pixel[1]
                                              + 16 * pixel[0] + 128) >> 8));
                }
            }
            r = (r + 2) / 4; g = (g + 2) / 4; b = (b + 2) / 4;
            chroma[x] = clamp(128 + ((-26 * r - 87 * g + 112 * b + 128) >> 8));
            chroma[x + 1] = clamp(128 + ((112 * r - 102 * g - 10 * b + 128) >> 8));
        }
    }
}

} // namespace

struct Encoder::Impl {
    MediaRuntime runtime;
    ComPtr<IMFTransform> transform;
    ComPtr<ICodecAPI> codec;
    ComPtr<IMFMediaEventGenerator> events;
    std::shared_ptr<const VideoFormat> format;
    const UINT32 width;
    const UINT32 height;
    const std::uint64_t generation;
    const FrameCallback callback;
    std::vector<std::uint8_t> nv12;
    std::mutex callbackMutex;
    bool active = true;
    bool finished = false;
    bool hasInput = false;
    bool hardware = false;
    bool probing = false;
    bool drainComplete = false;
    UINT32 inputCredits = 0;
    std::int64_t previousInput = -1;
    std::int64_t previousOutput = -1;
    std::int64_t lastKeyframe = -1;

    Impl(UINT32 w, UINT32 h, std::uint64_t gen, FrameCallback frames)
        : width(w), height(h), generation(gen), callback(frames) {
        if (!callback || width < 2 || height < 2 || width % 2 || height % 2
            || width > 16'384 || height > 16'384)
            throw std::runtime_error("The encoder needs even video dimensions and a frame callback.");
        nv12.resize(static_cast<std::size_t>(width) * height * 3 / 2);
        if (!try_hardware()) create_software();
    }

    ~Impl() {
        // The capture worker owns construction, encoding and teardown.
        reset_transform();
    }

    void reset_transform() noexcept {
        if (transform) {
            if (!finished) transform->ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            if (hardware) {
                ComPtr<IMFShutdown> shutdown;
                if (SUCCEEDED(transform.As(&shutdown))) shutdown->Shutdown();
            }
        }
        events.Reset();
        codec.Reset();
        transform.Reset();
        hardware = false;
        probing = false;
        format.reset();
        inputCredits = 0;
        drainComplete = false;
        hasInput = false;
        previousInput = previousOutput = lastKeyframe = -1;
    }

    void create_software() {
        reset_transform();
        check(CoCreateInstance(__uuidof(CMSH264EncoderMFT), nullptr, CLSCTX_INPROC_SERVER,
                               IID_PPV_ARGS(&transform)), "Creating the software H.264 encoder");
        configure();
    }

    bool try_hardware() {
        MFT_REGISTER_TYPE_INFO input{ MFMediaType_Video, MFVideoFormat_NV12 };
        MFT_REGISTER_TYPE_INFO output{ MFMediaType_Video, MFVideoFormat_H264 };
        IMFActivate** activations = nullptr;
        UINT32 count = 0;
        if (FAILED(MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER,
                             MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                             &input, &output, &activations, &count))) return false;
        // Probe with real CPU NV12 input before accepting any capture samples.
        // Drivers needing an unsupported device manager or missing required
        // codec settings are rejected, leaving software as the safe fallback.
        const auto deadline = Clock::now() + std::chrono::seconds(6);
        bool selected = false;
        for (UINT32 i = 0; i < count && Clock::now() < deadline; ++i) {
            try {
                reset_transform();
                check(activations[i]->ActivateObject(IID_PPV_ARGS(&transform)), "Activating hardware H.264");
                hardware = true;
                ComPtr<IMFAttributes> attributes;
                check(transform->GetAttributes(&attributes), "Reading hardware encoder attributes");
                UINT32 asynchronous = FALSE;
                check(attributes->GetUINT32(MF_TRANSFORM_ASYNC, &asynchronous), "Reading hardware encoder mode");
                if (!asynchronous) throw std::runtime_error("The hardware encoder does not use the event protocol.");
                check(attributes->SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK, TRUE), "Unlocking hardware H.264");
                attributes->SetUINT32(MF_LOW_LATENCY, TRUE);
                check(transform.As(&events), "Opening hardware encoder events");
                configure();
                const auto probeDeadline = std::min(deadline, Clock::now() + std::chrono::seconds(3));
                wait_for_input(probeDeadline);
                std::fill(nv12.begin(), nv12.begin() + static_cast<std::size_t>(width) * height, std::uint8_t{16});
                std::fill(nv12.begin() + static_cast<std::size_t>(width) * height, nv12.end(), std::uint8_t{128});
                codec_uint(codec.Get(), CODECAPI_AVEncVideoForceKeyFrame, 1, "Testing a hardware keyframe");
                auto probe = make_sample(nv12.data(), nv12.size(), 0, frameDuration, false);
                probing = true;
                check(transform->ProcessInput(0, probe.Get(), 0), "Testing hardware video input");
                --inputCredits;
                // Drain even a look-ahead encoder for the capability probe.
                check(transform->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0), "Ending hardware probe input");
                check(transform->ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0), "Draining hardware probe video");
                while (!drainComplete) {
                    if (!pump_events()) std::this_thread::sleep_for(std::chrono::milliseconds(1));
                    if (Clock::now() >= probeDeadline) throw std::runtime_error("The hardware encoder probe timed out.");
                }
                if (previousOutput < 0) throw std::runtime_error("The hardware encoder probe produced no video.");
                check(transform->ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0), "Resetting hardware probe video");
                // Consume any stale input-credit notifications before restart.
                while (pump_events()) {}
                inputCredits = 0;
                drainComplete = false;
                previousInput = previousOutput = lastKeyframe = -1;
                probing = false;
                check(transform->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0), "Restarting hardware H.264");
                wait_for_input(probeDeadline);
                selected = true;
                break;
            } catch (const std::exception&) {
                reset_transform();
                activations[i]->ShutdownObject();
            }
        }
        for (UINT32 i = 0; i < count; ++i) activations[i]->Release();
        CoTaskMemFree(activations);
        return selected;
    }

    void configure() {
        DWORD inputCount = 0, outputCount = 0;
        check(transform->GetStreamCount(&inputCount, &outputCount), "Reading encoder stream counts");
        if (inputCount != 1 || outputCount != 1)
            throw std::runtime_error("The H.264 encoder does not provide one video stream.");
        DWORD inputId = 0, outputId = 0;
        const HRESULT identifiers = transform->GetStreamIDs(1, &inputId, 1, &outputId);
        if (identifiers != E_NOTIMPL) {
            check(identifiers, "Reading encoder stream identifiers");
            if (inputId != 0 || outputId != 0)
                throw std::runtime_error("The H.264 encoder uses unsupported stream identifiers.");
        }
        check(transform.As(&codec), "Opening H.264 encoder settings");
        codec_uint(codec.Get(), CODECAPI_AVEncMPVDefaultBPictureCount, 0, "Disabling B frames");
        codec_uint(codec.Get(), CODECAPI_AVEncMPVGOPSize, 30, "Setting the keyframe interval");
        codec_uint(codec.Get(), CODECAPI_AVEncCommonRateControlMode,
                   eAVEncCommonRateControlMode_UnconstrainedVBR, "Setting bitrate control");
        codec_uint(codec.Get(), CODECAPI_AVEncCommonMeanBitRate, 8'000'000, "Setting the encoder bitrate");
        codec_bool(codec.Get(), CODECAPI_AVLowLatencyMode, true, "Enabling low latency", false);
        codec_uint(codec.Get(), CODECAPI_AVEncCommonQualityVsSpeed, 0, "Setting encoder complexity", false);
        auto output = video_type(width, height, MFVideoFormat_H264);
        check(transform->SetOutputType(0, output.Get(), 0), "Configuring H.264 output");
        auto input = video_type(width, height, MFVideoFormat_NV12);
        check(transform->SetInputType(0, input.Get(), 0), "Configuring NV12 input");
        refresh_format();
        check(transform->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0), "Preparing the encoder");
        check(transform->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0), "Starting the encoder");
    }

    void refresh_format() {
        ComPtr<IMFMediaType> output;
        check(transform->GetOutputCurrentType(0, &output), "Reading the encoded video format");
        UINT32 outputWidth = 0, outputHeight = 0;
        check(MFGetAttributeSize(output.Get(), MF_MT_FRAME_SIZE, &outputWidth, &outputHeight),
              "Reading the encoded video dimensions");
        GUID subtype{};
        check(output->GetGUID(MF_MT_SUBTYPE, &subtype), "Reading the encoded video subtype");
        if (subtype != MFVideoFormat_H264 || outputWidth != width || outputHeight != height)
            throw std::runtime_error("The encoder changed the video dimensions or codec.");
        UINT32 length = 0;
        const HRESULT result = output->GetBlobSize(MF_MT_MPEG_SEQUENCE_HEADER, &length);
        if (result == MF_E_ATTRIBUTENOTFOUND) return; // Some encoders publish it with their first output.
        check(result, "Reading the H.264 sequence header size");
        if (length == 0 || length > 65'536) throw std::runtime_error("The H.264 sequence header is invalid.");
        std::vector<std::uint8_t> header(length);
        check(output->GetBlob(MF_MT_MPEG_SEQUENCE_HEADER, header.data(), length, nullptr),
              "Reading the H.264 sequence header");
        auto next = std::make_shared<const VideoFormat>(width, height, std::move(header));
        if (format && !(*format == *next))
            throw std::runtime_error("The H.264 format changed. Resume buffering to start a fresh stream.");
        format = std::move(next);
    }

    void output_sample(IMFSample* sample) {
        if (!format) refresh_format();
        if (!format) throw std::runtime_error("The encoder did not provide an H.264 sequence header.");
        LONGLONG pts = 0, duration = frameDuration;
        check(sample->GetSampleTime(&pts), "Reading an encoded timestamp");
        if (pts < 0 || (previousOutput >= 0 && pts <= previousOutput))
            throw std::runtime_error("The H.264 encoder reordered video timestamps.");
        UINT64 decodeTime = 0;
        if (SUCCEEDED(sample->GetUINT64(MFSampleExtension_DecodeTimestamp, &decodeTime))
            && decodeTime != static_cast<UINT64>(pts))
            throw std::runtime_error("The H.264 encoder enabled frame reordering.");
        if (FAILED(sample->GetSampleDuration(&duration)) || duration <= 0) duration = frameDuration;
        ComPtr<IMFMediaBuffer> buffer;
        check(sample->ConvertToContiguousBuffer(&buffer), "Reading compressed video bytes");
        BYTE* data = nullptr;
        DWORD length = 0;
        check(buffer->Lock(&data, nullptr, &length), "Locking compressed video bytes");
        std::shared_ptr<const std::vector<std::uint8_t>> bytes;
        try {
            if (!length) throw std::runtime_error("The encoder returned an empty video frame.");
            bytes = std::make_shared<const std::vector<std::uint8_t>>(data, data + length);
        } catch (...) {
            buffer->Unlock();
            throw;
        }
        check(buffer->Unlock(), "Unlocking compressed video bytes");
        UINT32 cleanPoint = 0;
        sample->GetUINT32(MFSampleExtension_CleanPoint, &cleanPoint);
        const bool keyframe = cleanPoint != 0 || contains_idr(*bytes);
        if (previousOutput < 0 && !keyframe)
            throw std::runtime_error("The encoder's first video frame is not a keyframe.");
        previousOutput = pts;
        auto retained = std::make_unique<EncodedSample>(format, std::move(bytes), pts, duration, keyframe);
        if (probing) return;
        std::lock_guard<std::mutex> lock(callbackMutex);
        if (!active) return;
        const auto size = retained->bytes->size();
        // Exactly one heap allocation transfers to Rust, including immutable
        // byte/format ownership. optic_sample_release may run on any thread.
        callback(generation, retained.release(), static_cast<double>(pts) / ticksPerSecond,
                 static_cast<double>(duration) / ticksPerSecond, size, keyframe);
    }

    bool pull_output() {
        MFT_OUTPUT_STREAM_INFO info{};
        check(transform->GetOutputStreamInfo(0, &info), "Reading encoder output requirements");
        ComPtr<IMFSample> provided;
        if (!(info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES)) {
            check(MFCreateSample(&provided), "Creating an encoder output sample");
            ComPtr<IMFMediaBuffer> buffer;
            const DWORD minimum = width * height * 3 / 2;
            check(MFCreateAlignedMemoryBuffer(std::max(info.cbSize, minimum),
                                              info.cbAlignment ? info.cbAlignment - 1 : 0, &buffer),
                  "Allocating encoder output memory");
            check(provided->AddBuffer(buffer.Get()), "Attaching encoder output memory");
        }
        MFT_OUTPUT_DATA_BUFFER output{};
        output.pSample = provided.Get();
        DWORD status = 0;
        const HRESULT result = transform->ProcessOutput(0, 1, &output, &status);
        if (output.pEvents) output.pEvents->Release();
        // A transform-provided sample transfers one COM reference to us.
        ComPtr<IMFSample> owned;
        if (output.pSample && output.pSample != provided.Get()) owned.Attach(output.pSample);
        if (result == MF_E_TRANSFORM_NEED_MORE_INPUT) return false;
        if (result == MF_E_TRANSFORM_STREAM_CHANGE) {
            ComPtr<IMFMediaType> changed;
            check(transform->GetOutputAvailableType(0, 0, &changed), "Reading the changed encoder output type");
            check(transform->SetOutputType(0, changed.Get(), 0), "Accepting the encoder output type");
            refresh_format();
            return true;
        }
        check(result, "Encoding an H.264 frame");
        if (output.pSample) output_sample(output.pSample);
        return true;
    }

    void drain_output() {
        const auto deadline = Clock::now() + std::chrono::seconds(15);
        while (pull_output()) {
            if (Clock::now() >= deadline) throw std::runtime_error("Draining the H.264 encoder timed out.");
        }
    }

    bool pump_events() {
        bool received = false;
        const auto deadline = Clock::now() + std::chrono::seconds(3);
        for (;;) {
            ComPtr<IMFMediaEvent> event;
            const HRESULT result = events->GetEvent(MF_EVENT_FLAG_NO_WAIT, &event);
            if (result == MF_E_NO_EVENTS_AVAILABLE) return received;
            check(result, "Reading hardware H.264 events");
            HRESULT status = S_OK;
            check(event->GetStatus(&status), "Reading hardware encoder status");
            check(status, "Processing hardware video");
            MediaEventType kind = MEUnknown;
            check(event->GetType(&kind), "Reading a hardware encoder event");
            if (kind == METransformNeedInput) {
                if (inputCredits == std::numeric_limits<UINT32>::max())
                    throw std::runtime_error("The hardware encoder sent invalid input credits.");
                ++inputCredits;
            } else if (kind == METransformHaveOutput) {
                // Exactly one ProcessOutput per HaveOutput event, including
                // stream-change responses, as required by asynchronous MFTs.
                pull_output();
            } else if (kind == METransformDrainComplete) {
                drainComplete = true;
            }
            received = true;
            if (Clock::now() >= deadline)
                throw std::runtime_error("Processing hardware H.264 events timed out.");
        }
    }

    void wait_for_input(Clock::time_point deadline) {
        while (!inputCredits) {
            if (!pump_events()) std::this_thread::sleep_for(std::chrono::milliseconds(1));
            if (Clock::now() >= deadline)
                throw std::runtime_error("Waiting for the hardware H.264 encoder timed out.");
        }
    }

    void encode(const std::uint8_t* bgra, std::size_t stride, std::int64_t pts) {
        if (finished) throw std::runtime_error("The video encoder has stopped.");
        if (!bgra || stride < static_cast<std::size_t>(width) * 4 || pts < 0
            || (hasInput && pts <= previousInput))
            throw std::runtime_error("The encoder received invalid pixels or a video timestamp.");
        bgra_to_nv12(bgra, stride, width, height, nv12);
        if (hardware) wait_for_input(Clock::now() + std::chrono::seconds(3));
        if (lastKeyframe < 0 || pts - lastKeyframe >= ticksPerSecond) {
            codec_uint(codec.Get(), CODECAPI_AVEncVideoForceKeyFrame, 1, "Forcing a video keyframe");
            lastKeyframe = pts;
        }
        auto sample = make_sample(nv12.data(), nv12.size(), pts, frameDuration, false);
        if (!hasInput)
            check(sample->SetUINT32(MFSampleExtension_Discontinuity, TRUE), "Marking the start of video input");
        HRESULT result = transform->ProcessInput(0, sample.Get(), 0);
        if (!hardware && result == MF_E_NOTACCEPTING) {
            drain_output();
            result = transform->ProcessInput(0, sample.Get(), 0);
        }
        check(result, "Submitting a video frame");
        if (hardware) --inputCredits;
        hasInput = true;
        previousInput = pts;
        if (hardware) pump_events(); else drain_output();
    }

    void finish() {
        if (finished) return;
        check(transform->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0), "Ending video input");
        check(transform->ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0), "Finishing the H.264 encoder");
        if (hardware) {
            const auto deadline = Clock::now() + std::chrono::seconds(15);
            while (!drainComplete) {
                if (!pump_events()) std::this_thread::sleep_for(std::chrono::milliseconds(1));
                if (Clock::now() >= deadline)
                    throw std::runtime_error("Finishing the hardware H.264 encoder timed out.");
            }
        } else {
            drain_output();
        }
        check(transform->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0), "Closing the H.264 encoder");
        finished = true;
        if (hardware) {
            ComPtr<IMFShutdown> shutdown;
            if (SUCCEEDED(transform.As(&shutdown))) check(shutdown->Shutdown(), "Shutting down hardware H.264");
        }
        events.Reset();
        transform.Reset();
        codec.Reset();
    }
};

Encoder::Encoder(std::uint32_t width, std::uint32_t height, std::uint64_t generation,
                 FrameCallback callback)
    : impl_(std::make_unique<Impl>(width, height, generation, callback)) {}
Encoder::~Encoder() = default;
void Encoder::encode(const std::uint8_t* bgra, std::size_t stride, std::int64_t pts100ns) {
    impl_->encode(bgra, stride, pts100ns);
}
void Encoder::finish() { impl_->finish(); }
bool Encoder::hardware_accelerated() const noexcept { return impl_->hardware; }
void Encoder::disable() {
    std::lock_guard<std::mutex> lock(impl_->callbackMutex);
    impl_->active = false;
}

namespace {

class FinalizeCallback final : public IMFSinkWriterCallback {
public:
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid, void** object) override {
        if (!object) return E_POINTER;
        *object = nullptr;
        if (iid == __uuidof(IUnknown) || iid == __uuidof(IMFSinkWriterCallback)) {
            *object = static_cast<IMFSinkWriterCallback*>(this);
            AddRef();
            return S_OK;
        }
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return ++references_; }
    ULONG STDMETHODCALLTYPE Release() override {
        const ULONG remaining = --references_;
        if (!remaining) delete this;
        return remaining;
    }
    HRESULT STDMETHODCALLTYPE OnFinalize(HRESULT result) override {
        std::lock_guard<std::mutex> lock(mutex_);
        result_ = result;
        done_ = true;
        ready_.notify_all();
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE OnMarker(DWORD, LPVOID) override { return S_OK; }
    HRESULT wait(Clock::time_point deadline) {
        std::unique_lock<std::mutex> lock(mutex_);
        if (!ready_.wait_until(lock, deadline, [this] { return done_; }))
            throw std::runtime_error("Finalizing the MP4 timed out.");
        return result_;
    }
private:
    std::atomic<ULONG> references_{1};
    std::mutex mutex_;
    std::condition_variable ready_;
    bool done_ = false;
    HRESULT result_ = E_PENDING;
};

struct ExportJob final {
    std::mutex mutex;
    std::condition_variable ready;
    bool cancelled = false;
    bool done = false;
    bool success = false;
    std::string error;
    const Clock::time_point deadline = Clock::now() + std::chrono::seconds(60);
};

class PartialFile final {
public:
    std::wstring path;
    ComPtr<IMFByteStream> stream;
    bool committed = false;
    ~PartialFile() {
        if (stream) { stream->Close(); stream.Reset(); }
        if (!committed && !path.empty()) DeleteFileW(path.c_str());
    }
};

void check_cancel(const std::shared_ptr<ExportJob>& job) {
    std::lock_guard<std::mutex> lock(job->mutex);
    if (job->cancelled || Clock::now() >= job->deadline)
        throw std::runtime_error("Writing the MP4 timed out.");
}

void write_clip(const std::vector<EncodedSample>& samples, const std::wstring& path,
                const std::shared_ptr<ExportJob>& job) {
    ComRuntime com;
    MediaRuntime runtime;
    if (GetFileAttributesW(path.c_str()) != INVALID_FILE_ATTRIBUTES)
        throw std::runtime_error("The output file already exists.");
    GUID identifier{};
    check(CoCreateGuid(&identifier), "Naming the temporary MP4");
    wchar_t suffix[40];
    if (!StringFromGUID2(identifier, suffix, 40))
        throw std::runtime_error("Unable to name the temporary MP4.");
    PartialFile partial;
    const auto temporaryPath = path + L".optic-partial-" + suffix;
    check(MFCreateFile(MF_ACCESSMODE_READWRITE, MF_OPENMODE_FAIL_IF_EXIST, MF_FILEFLAGS_NONE,
                       temporaryPath.c_str(), &partial.stream), "Creating the MP4 output");
    partial.path = temporaryPath;
    ComPtr<IMFAttributes> attributes;
    check(MFCreateAttributes(&attributes, 3), "Creating MP4 writer settings");
    check(attributes->SetGUID(MF_TRANSCODE_CONTAINERTYPE, MFTranscodeContainerType_MPEG4),
          "Selecting the MP4 container");
    check(attributes->SetUINT32(MF_SINK_WRITER_DISABLE_THROTTLING, TRUE), "Disabling MP4 writer throttling");
    ComPtr<FinalizeCallback> completion;
    completion.Attach(new FinalizeCallback());
    check(attributes->SetUnknown(MF_SINK_WRITER_ASYNC_CALLBACK, completion.Get()), "Setting MP4 completion");
    ComPtr<IMFSinkWriter> writer;
    check(MFCreateSinkWriterFromURL(nullptr, partial.stream.Get(), attributes.Get(), &writer),
          "Creating the MP4 writer");
    const auto& format = *samples.front().format;
    auto type = video_type(format.width, format.height, MFVideoFormat_H264);
    check(type->SetBlob(MF_MT_MPEG_SEQUENCE_HEADER, format.sequenceHeader.data(),
                        static_cast<UINT32>(format.sequenceHeader.size())), "Setting H.264 parameter sets");
    DWORD streamIndex = 0;
    check(writer->AddStream(type.Get(), &streamIndex), "Adding the MP4 video stream");
    // Identical compressed input/output types remux the owned H.264 bytes;
    // the sink writer does not instantiate a transcoder.
    check(writer->SetInputMediaType(streamIndex, type.Get(), nullptr), "Setting compressed MP4 input");
    check(writer->BeginWriting(), "Starting MP4 output");
    const auto offset = samples.front().pts;
    for (const auto& encoded : samples) {
        check_cancel(job);
        auto sample = make_sample(encoded.bytes->data(), encoded.bytes->size(), encoded.pts - offset,
                                  encoded.duration, encoded.keyframe);
        check(sample->SetUINT64(MFSampleExtension_DecodeTimestamp,
                                static_cast<UINT64>(encoded.pts - offset)), "Rebasing the decode timestamp");
        check(writer->WriteSample(streamIndex, sample.Get()), "Appending MP4 video");
    }
    check_cancel(job);
    check(writer->NotifyEndOfSegment(streamIndex), "Ending MP4 video");
    check(writer->Finalize(), "Starting MP4 finalization");
    check(completion->wait(job->deadline), "Finalizing the MP4");
    writer.Reset();
    // Finalize flushes and may itself close the file byte stream. Closing is
    // idempotent for our ownership; Flush on an already closed stream fails.
    partial.stream->Close();
    partial.stream.Reset();
    // Publication and cancellation share a mutex. A caller that reports a
    // timeout can never later find a successful output published by this job.
    std::lock_guard<std::mutex> lock(job->mutex);
    if (job->cancelled || Clock::now() >= job->deadline)
        throw std::runtime_error("Writing the MP4 timed out.");
    // No REPLACE_EXISTING flag: a racing writer cannot be overwritten.
    if (!MoveFileExW(partial.path.c_str(), path.c_str(), MOVEFILE_WRITE_THROUGH)) {
        const DWORD error = GetLastError();
        if (error == ERROR_ALREADY_EXISTS || error == ERROR_FILE_EXISTS)
            throw std::runtime_error("The output file already exists.");
        check(HRESULT_FROM_WIN32(error), "Publishing the MP4");
    }
    partial.committed = true;
    job->success = true;
    job->done = true;
    job->ready.notify_all();
}

std::vector<EncodedSample> copy_clip(void* const* pointers, std::size_t count) {
    if (!pointers || count == 0) throw std::runtime_error("No video frames supplied.");
    std::vector<EncodedSample> samples;
    samples.reserve(count);
    const VideoFormat* format = nullptr;
    std::int64_t previous = -1;
    for (std::size_t i = 0; i < count; ++i) {
        if (!pointers[i]) throw std::runtime_error("A video frame is missing.");
        const auto& sample = *static_cast<const EncodedSample*>(pointers[i]);
        if (!sample.format || !sample.bytes || sample.bytes->empty() || sample.duration <= 0
            || sample.pts < 0 || (i && sample.pts <= previous))
            throw std::runtime_error("Video frames have invalid data or out-of-order timestamps.");
        if (!i && !sample.keyframe) throw std::runtime_error("The clip must start at a keyframe.");
        if (!format) format = sample.format.get();
        if (format->sequenceHeader.empty() || !(*format == *sample.format))
            throw std::runtime_error("The clip contains incompatible H.264 video formats.");
        previous = sample.pts;
        // Retain only immutable shared values, so a timed-out worker never
        // borrows sample pointers or error buffers owned by its Rust caller.
        samples.emplace_back(sample);
    }
    return samples;
}

} // namespace
} // namespace optic

extern "C" void optic_sample_release(void* sample) {
    delete static_cast<optic::EncodedSample*>(sample);
}

extern "C" bool optic_export(void* const* pointers, std::size_t count, const wchar_t* path,
                             char* error, std::size_t capacity) {
    optic::copy_error("", error, capacity);
    try {
        if (!path || !*path) throw std::runtime_error("No output path supplied.");
        auto samples = optic::copy_clip(pointers, count);
        auto job = std::make_shared<optic::ExportJob>();
        std::thread worker([samples = std::move(samples), output = optic::file_path(path), job] {
            try {
                optic::write_clip(samples, output, job);
            } catch (const std::exception& failure) {
                std::lock_guard<std::mutex> lock(job->mutex);
                job->error = failure.what();
                job->done = true;
                job->ready.notify_all();
            } catch (...) {
                std::lock_guard<std::mutex> lock(job->mutex);
                job->error = "The MP4 writer failed unexpectedly.";
                job->done = true;
                job->ready.notify_all();
            }
        });
        // The worker owns its input and completion state from launch. Detach
        // immediately so no exception/timeout path can destroy a joinable
        // thread, and COM teardown cannot extend the synchronous API deadline.
        worker.detach();
        std::unique_lock<std::mutex> lock(job->mutex);
        if (!job->ready.wait_until(lock, job->deadline, [&job] { return job->done; })) {
            job->cancelled = true;
            // A blocked OS/driver write cannot be safely killed. Its worker
            // owns every reference and removes its temporary file when the
            // operation unblocks; the public API still has a bounded return.
            optic::copy_error("Writing the MP4 timed out.", error, capacity);
            return false;
        }
        const bool success = job->success;
        const auto detail = job->error;
        lock.unlock();
        if (!success) optic::copy_error(detail.c_str(), error, capacity);
        return success;
    } catch (const std::exception& failure) {
        optic::copy_error(failure.what(), error, capacity);
        return false;
    } catch (...) {
        optic::copy_error("Video export failed unexpectedly.", error, capacity);
        return false;
    }
}

extern "C" bool optic_generate_test_video(std::uint64_t generation, FrameCallback callback,
                                          char* error, std::size_t capacity) {
    optic::copy_error("", error, capacity);
    try {
        optic::ComRuntime com;
        constexpr UINT32 width = 640, height = 360;
        optic::Encoder encoder(width, height, generation, callback);
        std::vector<std::uint8_t> pixels(width * height * 4);
        for (std::int64_t frame = 0; frame < 90; ++frame) {
            const UINT32 squareX = static_cast<UINT32>((frame * 5) % (width - 80));
            for (UINT32 y = 0; y < height; ++y) {
                for (UINT32 x = 0; x < width; ++x) {
                    const bool square = x >= squareX && x < squareX + 80 && y >= 130 && y < 210;
                    auto* pixel = pixels.data() + (static_cast<std::size_t>(y) * width + x) * 4;
                    pixel[0] = square ? 0x60 : static_cast<std::uint8_t>(y % 200);
                    pixel[1] = square ? 0xe0 : static_cast<std::uint8_t>(x % 200);
                    pixel[2] = square ? 0x40 : 0;
                    pixel[3] = 0xff;
                }
            }
            encoder.encode(pixels.data(), width * 4, frame * optic::ticksPerSecond / 30);
        }
        encoder.finish();
        return true;
    } catch (const std::exception& failure) {
        optic::copy_error(failure.what(), error, capacity);
        return false;
    } catch (...) {
        optic::copy_error("Generating the test video failed unexpectedly.", error, capacity);
        return false;
    }
}
