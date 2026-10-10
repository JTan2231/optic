// Opt-in integration test: captures the real primary display without showing UI.
// Run only on an interactive Windows desktop. Retained clips stay in argv[1].
#include "Bridge.h"
#include <windows.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <wrl/client.h>
#include <atomic>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdio>
#include <filesystem>
#include <map>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <vector>

namespace {
using Microsoft::WRL::ComPtr;
using Clock = std::chrono::steady_clock;
using namespace std::chrono_literals;
constexpr DWORD VideoStream = static_cast<DWORD>(MF_SOURCE_READER_FIRST_VIDEO_STREAM);
constexpr DWORD AllStreams = static_cast<DWORD>(MF_SOURCE_READER_ALL_STREAMS);

struct Frame {
    std::shared_ptr<void> sample;
    double pts;
    double duration;
    std::size_t bytes;
    bool keyframe;
};

struct Generation {
    std::vector<Frame> frames;
    std::size_t received = 0;
    std::size_t bytes = 0;
    unsigned started = 0;
    unsigned stopped = 0;
    unsigned failed = 0;
    bool boundary = false;
    bool invalid = false;
    std::string message;
};

struct State {
    std::mutex mutex;
    std::condition_variable changed;
    std::map<std::uint64_t, Generation> generations;
    std::atomic<bool> callback_failure{false};
} state;

void require(bool condition, const char* message) {
    if (!condition) throw std::runtime_error(message);
}

void check(HRESULT result, const char* operation) {
    if (SUCCEEDED(result)) return;
    char error[384];
    std::snprintf(error, sizeof(error), "%s failed (HRESULT 0x%08lX).", operation,
                  static_cast<unsigned long>(result));
    throw std::runtime_error(error);
}

void __cdecl received(std::uint64_t generation, void* sample, double pts,
                       double duration, std::size_t bytes, bool keyframe) noexcept {
    // Even allocation failures release the single reference transferred by the
    // native bridge. Exceptions must never cross this C callback boundary.
    try {
        std::shared_ptr<void> owned(sample, optic_sample_release);
        std::lock_guard<std::mutex> lock(state.mutex);
        const auto found = state.generations.find(generation);
        if (found == state.generations.end()) {
            state.callback_failure = true;
        } else {
            auto& entry = found->second;
            ++entry.received;
            if (entry.boundary || entry.stopped || entry.failed || !sample || !bytes ||
                !std::isfinite(pts) || !std::isfinite(duration) || pts < 0 ||
                std::abs(duration - 1.0 / 30.0) > 0.000001 ||
                (!entry.frames.empty() && pts <= entry.frames.back().pts) ||
                (entry.frames.empty() && !keyframe) || entry.frames.size() >= 600 ||
                bytes > 64u * 1024u * 1024u - entry.bytes) {
                entry.invalid = true;
            } else {
                entry.frames.push_back({std::move(owned), pts, duration, bytes, keyframe});
                entry.bytes += bytes;
            }
        }
    } catch (...) {
        state.callback_failure = true;
    }
    state.changed.notify_all();
}

void __cdecl event(std::uint64_t generation, std::int32_t kind, const char* message) noexcept {
    try {
        std::lock_guard<std::mutex> lock(state.mutex);
        const auto found = state.generations.find(generation);
        if (found == state.generations.end()) {
            state.callback_failure = true;
        } else {
            auto& entry = found->second;
            if (kind == 1) {
                if (entry.stopped || entry.failed) entry.invalid = true;
                ++entry.started;
            }
            else if (kind == 2) ++entry.stopped;
            else if (kind == 3) ++entry.failed;
            else entry.invalid = true;
            if (entry.started > 1 || entry.stopped + entry.failed > 1) entry.invalid = true;
            entry.message = message ? message : "";
        }
    } catch (...) {
        state.callback_failure = true;
    }
    state.changed.notify_all();
}

void validate(const Generation& entry, bool allow_failure = false) {
    require(!state.callback_failure, "A native callback failed or used an unexpected generation");
    require(!entry.invalid, "Invalid, out-of-order or late native frame/event callback");
    if (!allow_failure && entry.failed) {
        throw std::runtime_error("Native capture failed: " + entry.message);
    }
}

template <typename Predicate>
void await(std::uint64_t generation, Predicate predicate, std::chrono::seconds timeout,
           const char* operation, bool allow_failure = false) {
    std::unique_lock<std::mutex> lock(state.mutex);
    auto& entry = state.generations.at(generation);
    const auto ready = [&] {
        return predicate(entry) || entry.invalid || state.callback_failure ||
               (!allow_failure && entry.failed);
    };
    const bool completed = state.changed.wait_for(lock, timeout, ready);
    validate(entry, allow_failure);
    require(completed && predicate(entry), operation);
}

void observe(std::uint64_t generation, std::chrono::milliseconds duration,
             bool allow_failure = false) {
    std::unique_lock<std::mutex> lock(state.mutex);
    auto& entry = state.generations.at(generation);
    state.changed.wait_for(lock, duration, [&] {
        return entry.invalid || state.callback_failure || (!allow_failure && entry.failed);
    });
    validate(entry, allow_failure);
}

std::size_t received_count(std::uint64_t generation) {
    std::lock_guard<std::mutex> lock(state.mutex);
    return state.generations.at(generation).received;
}

void mark_boundary(std::uint64_t generation) {
    std::lock_guard<std::mutex> lock(state.mutex);
    state.generations.at(generation).boundary = true;
}

void start(std::uint64_t generation) {
    {
        std::lock_guard<std::mutex> lock(state.mutex);
        require(state.generations.emplace(generation, Generation{}).second,
                "A capture generation was reused");
    }
    optic_capture_start(generation, received, event);
}

std::vector<Frame> snapshot(std::uint64_t generation) {
    std::lock_guard<std::mutex> lock(state.mutex);
    auto& entry = state.generations.at(generation);
    validate(entry);
    require(!entry.frames.empty() && entry.frames.front().keyframe,
            "Capture did not produce a decodable snapshot");
    return entry.frames;
}

void verify_decoding(const std::filesystem::path& path, const std::vector<Frame>& frames) {
    ComPtr<IMFSourceReader> reader;
    check(MFCreateSourceReaderFromURL(path.c_str(), nullptr, &reader), "Opening the live MP4");
    check(reader->SetStreamSelection(AllStreams, FALSE), "Deselecting streams");
    check(reader->SetStreamSelection(VideoStream, TRUE), "Selecting video");
    ComPtr<IMFMediaType> format;
    check(reader->GetNativeMediaType(VideoStream, 0, &format), "Reading MP4 format");
    GUID subtype{};
    check(format->GetGUID(MF_MT_SUBTYPE, &subtype), "Reading the MP4 codec");
    require(subtype == MFVideoFormat_H264, "Live MP4 must contain H.264");
    UINT32 width = 0, height = 0, numerator = 0, denominator = 0;
    check(MFGetAttributeSize(format.Get(), MF_MT_FRAME_SIZE, &width, &height), "Reading MP4 dimensions");
    require(width >= 2 && height >= 2 && height <= 1080 && width % 2 == 0 && height % 2 == 0,
            "Live MP4 must have even dimensions and height at most 1080");
    check(MFGetAttributeRatio(format.Get(), MF_MT_FRAME_RATE, &numerator, &denominator), "Reading MP4 frame rate");
    require(denominator != 0, "Live MP4 has an invalid frame-rate denominator");
    // Unlike the synthetic test, real capture uses elapsed monotonic timestamps.
    // The MP4 reader reports the average cadence, including scheduler jitter.
    const double reported_rate = static_cast<double>(numerator) / denominator;
    const double captured_duration = frames.back().pts - frames.front().pts + frames.back().duration;
    const double captured_rate = frames.size() / captured_duration;
    std::printf("Live cadence: metadata %u/%u = %.5f fps; captured %.5f fps over %.7f seconds\n",
                numerator, denominator, reported_rate, captured_rate, captured_duration);
    require(reported_rate >= 27.0 && reported_rate <= 33.0 && captured_rate >= 27.0 && captured_rate <= 33.0,
            "Live capture cadence differs materially from the nominal 30 fps");
    require(std::abs(reported_rate - captured_rate) < 0.1,
            "Live MP4 frame rate does not reflect the captured elapsed timestamps");
    ComPtr<IMFMediaType> decoded;
    check(MFCreateMediaType(&decoded), "Creating the decoder format");
    check(decoded->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video), "Setting the decoded media kind");
    check(decoded->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_NV12), "Selecting NV12 decoding");
    check(reader->SetCurrentMediaType(VideoStream, nullptr, decoded.Get()),
          "Configuring the live video decoder");
    std::size_t count = 0;
    LONGLONG previous = -1;
    LONGLONG last_duration = 0;
    const auto deadline = Clock::now() + 30s;
    for (std::size_t attempt = 0; ; ++attempt) {
        require(attempt < frames.size() * 4 + 32 && Clock::now() < deadline,
                "Live MP4 decoding did not complete within its bounded limit");
        DWORD flags = 0;
        LONGLONG timestamp = 0;
        ComPtr<IMFSample> sample;
        check(reader->ReadSample(VideoStream, 0, nullptr, &flags,
                                &timestamp, &sample), "Decoding the live MP4");
        require(!(flags & MF_SOURCE_READERF_ERROR), "Live MP4 decoder reported an error");
        if (sample) {
            require(count < frames.size(), "Live MP4 contains extra frames");
            require(timestamp > previous, "Live MP4 reordered the captured timestamps");
            const double expected = frames[count].pts - frames.front().pts;
            const double timestamp_error = std::abs(timestamp / 10'000'000.0 - expected);
            // The MP4 sink quantizes its media timeline (typically 1/30000 s).
            // A tenth of a millisecond permits that rounding while detecting
            // dropped elapsed time or reconstruction from fixed frame counts.
            if (timestamp_error >= 0.0001) {
                char message[256];
                std::snprintf(message, sizeof(message),
                              "Live MP4 changed frame %zu timing: expected %.7fs, decoded %.7fs (%.3fms error).",
                              count, expected, timestamp / 10'000'000.0, timestamp_error * 1000);
                throw std::runtime_error(message);
            }
            if (!count) require(timestamp == 0, "Live MP4 must start at time zero");
            check(sample->GetSampleDuration(&last_duration), "Reading the decoded screen duration");
            DWORD bytes = 0;
            check(sample->GetTotalLength(&bytes), "Reading decoded screen pixels");
            require(bytes >= static_cast<std::uint64_t>(width) * height * 3 / 2,
                    "Decoded screen frame has too few pixels");
            previous = timestamp;
            ++count;
        }
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM) break;
    }
    require(count == frames.size(), "Live MP4 did not completely decode the retained snapshot");
    const double decoded_duration = (previous + last_duration) / 10'000'000.0;
    require(std::abs(decoded_duration - captured_duration) < 0.0001,
            "Live MP4 duration does not preserve captured elapsed time");
    std::printf("Decoded live MP4: %zu frames, %ux%u, %.7f seconds, %.5f fps\n", count, width, height,
                decoded_duration, count / decoded_duration);
}

void save_snapshot(const std::vector<Frame>& frames, const std::filesystem::path& path) {
    std::vector<void*> samples;
    samples.reserve(frames.size());
    for (const auto& frame : frames) samples.push_back(frame.sample.get());
    char error[2048]{};
    require(optic_export(samples.data(), samples.size(), path.c_str(), error, sizeof(error)), error);
    require(std::filesystem::file_size(path) > 1024, "Live MP4 is unexpectedly small");
    verify_decoding(path, frames);
}

void clear(std::uint64_t generation) {
    std::lock_guard<std::mutex> lock(state.mutex);
    auto& entry = state.generations.at(generation);
    entry.frames.clear();
    entry.bytes = 0;
}

void stop_and_verify(std::uint64_t generation) {
    optic_capture_stop(generation);
    mark_boundary(generation);
    const auto count = received_count(generation);
    await(generation, [](const Generation& entry) { return entry.stopped != 0; },
          10s, "Capture did not emit a stopped event");
    observe(generation, 250ms);
    require(received_count(generation) == count, "Capture delivered a sample after stop returned");
    std::lock_guard<std::mutex> lock(state.mutex);
    const auto& entry = state.generations.at(generation);
    require(entry.stopped == 1 && entry.failed == 0, "Capture must emit exactly one stopped event");
}

class CaptureCleanup final {
public:
    std::uint64_t active = 0;
    ~CaptureCleanup() {
        if (!active) return;
        optic_capture_stop(active);
        try {
            await(active, [](const Generation& entry) { return entry.stopped || entry.failed; },
                  10s, "Cleaning up capture timed out", true);
        } catch (...) {}
    }
};
} // namespace

int wmain(int count, wchar_t** args) {
    const HRESULT com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(com)) return 1;
    const HRESULT media = MFStartup(MF_VERSION);
    if (FAILED(media)) { CoUninitialize(); return 1; }
    int result = 1;
    try {
        require(count <= 2, "Usage: capture-live.exe [output-directory]");
        const std::filesystem::path directory = std::filesystem::absolute(
            count == 2 ? std::filesystem::path(args[1]) : std::filesystem::path(L"target/live-test"));
        std::filesystem::create_directories(directory);
        const auto suffix = std::to_wstring(GetCurrentProcessId()) + L"-" + std::to_wstring(GetTickCount64());
        const auto live = directory / (L"live-" + suffix + L".mp4");
        const auto stopped = directory / (L"after-stop-" + suffix + L".mp4");
        const auto restarted = directory / (L"restart-" + suffix + L".mp4");
        CaptureCleanup cleanup;

        cleanup.active = 1;
        start(1);
        await(1, [](const Generation& entry) { return entry.started != 0; },
              20s, "Screen capture did not start on this interactive desktop");
        observe(1, 3000ms);
        await(1, [](const Generation& entry) { return entry.frames.size() >= 50; },
              7s, "Screen capture produced fewer than fifty frames");
        auto first_snapshot = snapshot(1);
        require(first_snapshot.back().pts - first_snapshot.front().pts >= 1.5,
                "Capture did not preserve elapsed recording time");
        const auto before_export = received_count(1);
        save_snapshot(first_snapshot, live);
        await(1, [before_export](const Generation& entry) { return entry.received > before_export; },
              3s, "Capture stopped progressing while exporting a snapshot");

        stop_and_verify(1);
        cleanup.active = 0;
        clear(1); // The detached snapshot must be enough to export after stop.
        save_snapshot(first_snapshot, stopped);

        cleanup.active = 2;
        start(2);
        await(2, [](const Generation& entry) { return entry.started != 0; },
              20s, "Capture did not restart");
        observe(2, 2000ms);
        await(2, [](const Generation& entry) { return entry.frames.size() >= 30; },
              7s, "Restarted capture produced fewer than thirty frames");
        auto second_snapshot = snapshot(2);
        require(second_snapshot.front().pts < 1.0 && second_snapshot.front().keyframe,
                "Restart must start a fresh timestamp timeline with a keyframe");
        require(second_snapshot.front().pts < first_snapshot.back().pts,
                "Restart retained the previous recording timeline");
        save_snapshot(second_snapshot, restarted);

        optic_capture_interrupt();
        mark_boundary(2);
        const auto at_interruption = received_count(2);
        await(2, [](const Generation& entry) { return entry.failed != 0; },
              10s, "Capture interruption did not emit a failure event", true);
        observe(2, 250ms, true);
        require(received_count(2) == at_interruption, "Capture delivered samples after interruption");
        {
            std::lock_guard<std::mutex> lock(state.mutex);
            const auto& entry = state.generations.at(2);
            require(entry.failed == 1 && entry.stopped == 0 && !entry.message.empty(),
                    "Capture interruption must emit exactly one failure event");
        }
        cleanup.active = 0;
        clear(2);

        for (std::uint64_t generation = 3; generation < 11; ++generation) {
            cleanup.active = generation;
            start(generation);
            stop_and_verify(generation);
            cleanup.active = 0;
            clear(generation);
        }
        observe(1, 100ms);
        observe(2, 100ms, true);
        std::puts("Windows live capture tests passed: recording, concurrent export, retained snapshots, "
                  "stop gate, restart, interruption, rapid start/stop.");
        std::wprintf(L"Clips retained in %ls\n", directory.c_str());
        result = 0;
    } catch (const std::exception& error) {
        std::fprintf(stderr, "Windows live capture test failed: %s\n", error.what());
    }
    MFShutdown();
    CoUninitialize();
    return result;
}
