// Integration tests for the actual encoder, immutable snapshots and MP4 reader.
#include "Bridge.h"
#include <windows.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <wrl/client.h>
#include <cmath>
#include <cstdio>
#include <filesystem>
#include <stdexcept>
#include <vector>

namespace {
using Microsoft::WRL::ComPtr;
constexpr DWORD video_stream = static_cast<DWORD>(MF_SOURCE_READER_FIRST_VIDEO_STREAM);
constexpr DWORD all_streams = static_cast<DWORD>(MF_SOURCE_READER_ALL_STREAMS);
struct Frame {
    void* sample;
    double pts;
    double duration;
    bool keyframe;
};
std::vector<Frame> frames;
bool invalid_frame = false;

void require(bool condition, const char* message) {
    if (!condition) throw std::runtime_error(message);
}
void check(HRESULT result, const char* message) { require(SUCCEEDED(result), message); }

void __cdecl received(std::uint64_t generation, void* sample, double pts,
                       double duration, std::size_t bytes, bool keyframe) {
    if (generation != 7 || !sample || !bytes || !std::isfinite(pts) || duration <= 0) {
        invalid_frame = true;
        optic_sample_release(sample);
        return;
    }
    frames.push_back({sample, pts, duration, keyframe});
}

struct Samples {
    ~Samples() { for (const auto& frame : frames) optic_sample_release(frame.sample); }
};

void verify_decoding(const std::wstring& path, std::size_t expected) {
    ComPtr<IMFByteStream> stream;
    check(MFCreateFile(MF_ACCESSMODE_READ, MF_OPENMODE_FAIL_IF_NOT_EXIST, MF_FILEFLAGS_NONE,
                       path.c_str(), &stream), "Opening exported MP4 bytes");
    ComPtr<IMFSourceReader> reader;
    check(MFCreateSourceReaderFromByteStream(stream.Get(), nullptr, &reader), "Opening exported MP4");
    check(reader->SetStreamSelection(all_streams, FALSE), "Deselecting streams");
    check(reader->SetStreamSelection(video_stream, TRUE), "Selecting video");
    ComPtr<IMFMediaType> native;
    check(reader->GetNativeMediaType(video_stream, 0, &native), "Reading MP4 format");
    GUID subtype{};
    check(native->GetGUID(MF_MT_SUBTYPE, &subtype), "Reading MP4 codec");
    require(subtype == MFVideoFormat_H264, "MP4 must contain H.264");
    UINT32 width = 0, height = 0, numerator = 0, denominator = 0;
    check(MFGetAttributeSize(native.Get(), MF_MT_FRAME_SIZE, &width, &height), "Reading video size");
    require(width == 640 && height == 360, "Unexpected video size");
    check(MFGetAttributeRatio(native.Get(), MF_MT_FRAME_RATE, &numerator, &denominator), "Reading frame rate");
    require(numerator == 30 && denominator == 1, "Unexpected frame rate");
    ComPtr<IMFMediaType> decoded;
    check(MFCreateMediaType(&decoded), "Creating decoder format");
    check(decoded->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video), "Setting decoded kind");
    check(decoded->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_NV12), "Selecting NV12 decoding");
    check(reader->SetCurrentMediaType(video_stream, nullptr, decoded.Get()),
          "Configuring video decoder");
    std::size_t count = 0;
    LONGLONG previous = -1;
    for (;;) {
        DWORD flags = 0;
        LONGLONG timestamp = 0;
        ComPtr<IMFSample> sample;
        check(reader->ReadSample(video_stream, 0, nullptr, &flags,
                                &timestamp, &sample), "Decoding exported MP4");
        require(!(flags & MF_SOURCE_READERF_ERROR), "MP4 decoder reported an error");
        if (sample) {
            require(timestamp > previous, "Decoded timestamps must be strictly increasing");
            if (!count) require(timestamp == 0, "Exported video must start at zero");
            DWORD bytes = 0;
            check(sample->GetTotalLength(&bytes), "Reading decoded pixels");
            require(bytes >= 640 * 360 * 3 / 2, "Decoded frame has too few pixels");
            previous = timestamp;
            ++count;
        }
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM) break;
    }
    require(count == expected, "MP4 did not fully decode the retained snapshot");
}
}

int wmain(int count, wchar_t** args) {
    const HRESULT com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(com)) return 1;
    const HRESULT media = MFStartup(MF_VERSION);
    if (FAILED(media)) { CoUninitialize(); return 1; }
    int result = 1;
    try {
        if (count == 3 && std::wstring(args[1]) == L"--verify") {
            // This mode verifies the Rust self-test clip, whose buffer retains
            // sixty frames from the preceding keyframe through the final frame.
            verify_decoding(args[2], 60);
            std::puts("Rust media self-test MP4 fully decoded: 60 frames");
        } else {
            const auto directory = std::filesystem::absolute(std::filesystem::temp_directory_path()) /
                (L"Optic-media-test-" + std::to_wstring(GetCurrentProcessId()));
            require(std::filesystem::create_directory(directory), "Test directory already exists");
            struct Cleanup {
                std::filesystem::path path;
                ~Cleanup() { std::error_code ignored; std::filesystem::remove_all(path, ignored); }
            } cleanup{std::filesystem::path(L"\\\\?\\" + directory.wstring())};
            Samples owned;
            char error[2048]{};
            require(optic_generate_test_video(7, received, error, sizeof(error)), error);
            require(!invalid_frame && frames.size() == 90, "Synthetic encoder must deliver ninety frames");
            require(frames.front().keyframe && frames.front().pts == 0, "First frame must be an initial keyframe");
            double keyframe_time = 0;
            for (std::size_t i = 0; i < frames.size(); ++i) {
                const auto& frame = frames[i];
                if (i) require(frame.pts > frames[i - 1].pts, "Encoder reordered timestamps");
                require(std::abs(frame.duration - 1.0 / 30.0) < 0.000001, "Unexpected frame duration");
                if (frame.keyframe) keyframe_time = frame.pts;
                require(frame.pts - keyframe_time < 1.04, "Keyframe spacing exceeds one second");
            }
            std::vector<void*> snapshot;
            for (const auto& frame : frames) snapshot.push_back(frame.sample);
            const auto full = (directory / L"full.mp4").wstring();
            require(optic_export(snapshot.data(), snapshot.size(), full.c_str(), error, sizeof(error)), error);
            verify_decoding(full, 90);
            const auto original_size = std::filesystem::file_size(full);
            require(!optic_export(snapshot.data(), snapshot.size(), full.c_str(), error, sizeof(error)),
                    "Exporter overwrote an existing file");
            require(std::filesystem::file_size(full) == original_size, "Existing file was modified");
            const auto clip = (directory / L"Replay-\x89c6\x9891-\x00e9.mp4").wstring();
            require(frames[30].keyframe, "One-second boundary must be a keyframe");
            require(optic_export(snapshot.data() + 30, snapshot.size() - 30, clip.c_str(), error, sizeof(error)), error);
            verify_decoding(clip, 60);
            auto deep_directory = directory;
            for (unsigned depth = 0; depth < 4; ++depth) deep_directory /= std::wstring(70, L'x');
            deep_directory = std::filesystem::path(L"\\\\?\\" + deep_directory.wstring());
            std::filesystem::create_directories(deep_directory);
            const auto deep_clip = (deep_directory / L"deep.mp4").wstring();
            require(deep_clip.size() > MAX_PATH, "Long-path test did not exceed MAX_PATH");
            // Export receives the ordinary long path: it must normalize that
            // internally without depending on the machine's registry setting.
            const auto ordinary_deep_clip = deep_clip.substr(4);
            require(optic_export(snapshot.data() + 30, snapshot.size() - 30, ordinary_deep_clip.c_str(), error, sizeof(error)), error);
            verify_decoding(deep_clip, 60);
            const auto invalid = (directory / L"invalid.mp4").wstring();
            require(!optic_export(snapshot.data() + 1, snapshot.size() - 1, invalid.c_str(), error, sizeof(error)),
                    "Exporter accepted a non-keyframe start");
            require(!std::filesystem::exists(invalid), "Failed export left an output file");
            std::vector<void*> reversed{snapshot[0], snapshot[2], snapshot[1]};
            require(!optic_export(reversed.data(), reversed.size(), invalid.c_str(), error, sizeof(error)),
                    "Exporter accepted out-of-order timestamps");
            const auto missing = (directory / L"missing" / L"output.mp4").wstring();
            require(!optic_export(snapshot.data(), snapshot.size(), missing.c_str(), error, sizeof(error)),
                    "Exporter accepted a missing output directory");
            for (const auto& file : std::filesystem::directory_iterator(directory)) {
                require(file.is_directory() || file.path().extension() == L".mp4", "Failed export left a temporary file");
            }
            std::puts("Windows media tests passed: encoding, complete decoding, keyframes, trimming, Unicode, long paths, errors");
        }
        result = 0;
    } catch (const std::exception& error) {
        std::fprintf(stderr, "Windows media test failed: %s\n", error.what());
    }
    MFShutdown();
    CoUninitialize();
    return result;
}
