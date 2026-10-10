#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <wrl/client.h>
#include <emmintrin.h>

#include "Bridge.h"
#include "Media.h"
#include <atomic>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdio>
#include <deque>
#include <map>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <thread>
#include <vector>

namespace {
using Microsoft::WRL::ComPtr;
using Clock = std::chrono::steady_clock;
constexpr auto FrameInterval = std::chrono::nanoseconds(33'333'333);
constexpr const char* Interrupted =
    "Screen capture was interrupted. Start buffering again when the display is available.";

void check(HRESULT result, const char* operation) {
    if (SUCCEEDED(result)) return;
    const bool interrupted = result == DXGI_ERROR_ACCESS_LOST || result == DXGI_ERROR_DEVICE_REMOVED ||
        result == DXGI_ERROR_DEVICE_RESET || result == DXGI_ERROR_SESSION_DISCONNECTED ||
        result == E_ACCESSDENIED;
    char text[512];
    std::snprintf(text, sizeof(text), "%s failed (HRESULT 0x%08lX).%s%s", operation,
                  static_cast<unsigned long>(result), interrupted ? " " : "",
                  interrupted ? Interrupted : "");
    throw std::runtime_error(text);
}

// Desktop Duplication supplies an unrotated surface. The pointer shape and
// position describe the final desktop orientation, so compose after rotation.
void rotate_desktop(const std::uint8_t* source, std::size_t stride,
                    UINT source_width, UINT source_height, DXGI_MODE_ROTATION rotation,
                    std::vector<std::uint8_t>& destination) {
    const bool portrait = rotation == DXGI_MODE_ROTATION_ROTATE90 ||
                          rotation == DXGI_MODE_ROTATION_ROTATE270;
    const UINT width = portrait ? source_height : source_width;
    const UINT height = portrait ? source_width : source_height;
    destination.resize(static_cast<std::size_t>(width) * height * 4);
    if (rotation == DXGI_MODE_ROTATION_IDENTITY || rotation == DXGI_MODE_ROTATION_UNSPECIFIED) {
        for (UINT y = 0; y < height; ++y) {
            std::memcpy(destination.data() + static_cast<std::size_t>(y) * width * 4,
                        source + static_cast<std::size_t>(y) * stride,
                        static_cast<std::size_t>(width) * 4);
        }
        return;
    }
    for (UINT y = 0; y < height; ++y) {
        auto* row = destination.data() + static_cast<std::size_t>(y) * width * 4;
        for (UINT x = 0; x < width; ++x) {
            UINT sx = x, sy = y;
            switch (rotation) {
                case DXGI_MODE_ROTATION_ROTATE90: sx = y; sy = source_height - 1 - x; break;
                case DXGI_MODE_ROTATION_ROTATE180:
                    sx = source_width - 1 - x; sy = source_height - 1 - y; break;
                case DXGI_MODE_ROTATION_ROTATE270: sx = source_width - 1 - y; sy = x; break;
                default: throw std::runtime_error("The display rotation is unsupported.");
            }
            std::memcpy(row + static_cast<std::size_t>(x) * 4,
                        source + static_cast<std::size_t>(sy) * stride + sx * 4, 4);
        }
    }
}

struct Pointer {
    bool visible = false;
    POINT position{};
    DXGI_OUTDUPL_POINTER_SHAPE_INFO info{};
    std::vector<std::uint8_t> shape;
};

void validate_pointer(const Pointer& pointer) {
    const auto& shape = pointer.info;
    if (shape.Width == 0 || shape.Height == 0) throw std::runtime_error("Invalid cursor dimensions.");
    const bool mono = shape.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME;
    if (!mono && shape.Type != DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR &&
        shape.Type != DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR) {
        throw std::runtime_error("Unsupported cursor shape.");
    }
    const std::size_t minimum_pitch = mono ? (static_cast<std::size_t>(shape.Width) + 7) / 8 :
                                             static_cast<std::size_t>(shape.Width) * 4;
    if (shape.Pitch < minimum_pitch || (mono && (shape.Height % 2 != 0)) ||
        static_cast<std::uint64_t>(shape.Pitch) * shape.Height > pointer.shape.size()) {
        throw std::runtime_error("Invalid cursor image data.");
    }
}

void composite_pointer(std::vector<std::uint8_t>& desktop, UINT width, UINT height,
                       const Pointer& pointer) {
    if (!pointer.visible || pointer.shape.empty()) return;
    validate_pointer(pointer);
    const auto& info = pointer.info;
    const bool mono = info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME;
    const UINT pointer_height = mono ? info.Height / 2 : info.Height;
    const auto left = static_cast<std::int64_t>(pointer.position.x);
    const auto top = static_cast<std::int64_t>(pointer.position.y);
    const auto x_begin = std::max<std::int64_t>(0, left);
    const auto y_begin = std::max<std::int64_t>(0, top);
    const auto x_end = std::min<std::int64_t>(width, left + info.Width);
    const auto y_end = std::min<std::int64_t>(height, top + pointer_height);
    for (auto y = y_begin; y < y_end; ++y) {
        const auto py = static_cast<std::size_t>(y - top);
        const auto* shape_row = pointer.shape.data() + py * info.Pitch;
        auto* row = desktop.data() + static_cast<std::size_t>(y) * width * 4;
        for (auto x = x_begin; x < x_end; ++x) {
            const auto px = static_cast<std::size_t>(x - left);
            auto* pixel = row + static_cast<std::size_t>(x) * 4;
            if (mono) {
                const auto bit = static_cast<std::uint8_t>(0x80u >> (px % 8));
                const auto and_mask = (shape_row[px / 8] & bit) ? 0xFFu : 0u;
                const auto* xor_row = shape_row + static_cast<std::size_t>(pointer_height) * info.Pitch;
                const auto xor_mask = (xor_row[px / 8] & bit) ? 0xFFu : 0u;
                for (unsigned c = 0; c < 3; ++c) {
                    pixel[c] = static_cast<std::uint8_t>((pixel[c] & and_mask) ^ xor_mask);
                }
            } else {
                const auto* cursor = shape_row + px * 4;
                if (info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR) {
                    for (unsigned c = 0; c < 3; ++c) {
                        pixel[c] = cursor[3] ? pixel[c] ^ cursor[c] : cursor[c];
                    }
                } else {
                    // DXGI color cursors use straight BGRA alpha, matching
                    // the SRC_ALPHA / INV_SRC_ALPHA blend in the SDK sample.
                    const unsigned inverse_alpha = 255u - cursor[3];
                    for (unsigned c = 0; c < 3; ++c) {
                        pixel[c] = static_cast<std::uint8_t>(
                            (cursor[c] * cursor[3] + pixel[c] * inverse_alpha + 127u) / 255u);
                    }
                }
            }
            pixel[3] = 255;
        }
    }
}

struct AxisSample {
    UINT first;
    UINT second;
    unsigned fraction;
};

std::vector<AxisSample> scale_axis(UINT source, UINT destination) {
    std::vector<AxisSample> samples(destination);
    for (UINT i = 0; i < destination; ++i) {
        const double position = std::clamp((i + 0.5) * source / destination - 0.5,
                                           0.0, static_cast<double>(source - 1));
        const UINT first = static_cast<UINT>(position);
        samples[i] = {first, std::min(first + 1, source - 1),
                      static_cast<unsigned>((position - first) * 256.0 + 0.5)};
    }
    return samples;
}

void scale_desktop(const std::vector<std::uint8_t>& source, UINT source_width,
                   UINT source_height, UINT width, UINT height,
                   const std::vector<AxisSample>& xs, const std::vector<AxisSample>& ys,
                   std::vector<std::uint8_t>& destination) {
    destination.resize(static_cast<std::size_t>(width) * height * 4);
    if (source_width == width && source_height == height) {
        std::memcpy(destination.data(), source.data(), destination.size());
        return;
    }
    const auto zero = _mm_setzero_si128();
    const auto rounding = _mm_set1_epi16(128);
    for (UINT y = 0; y < height; ++y) {
        const auto& sample_y = ys[y];
        const auto* row0 = source.data() + static_cast<std::size_t>(sample_y.first) * source_width * 4;
        const auto* row1 = source.data() + static_cast<std::size_t>(sample_y.second) * source_width * 4;
        auto* output = destination.data() + static_cast<std::size_t>(y) * width * 4;
        const auto upper_weight = _mm_set1_epi16(static_cast<short>(256u - sample_y.fraction));
        const auto lower_weight = _mm_set1_epi16(static_cast<short>(sample_y.fraction));
        for (UINT x = 0; x < width; ++x) {
            const auto& sample_x = xs[x];
            const auto* a = row0 + sample_x.first * 4;
            const auto* b = row0 + sample_x.second * 4;
            const auto* c = row1 + sample_x.first * 4;
            const auto* d = row1 + sample_x.second * 4;
            std::uint32_t pa, pb, pc, pd;
            std::memcpy(&pa, a, 4);
            std::memcpy(&pb, b, 4);
            std::memcpy(&pc, c, 4);
            std::memcpy(&pd, d, 4);
            const auto va = _mm_unpacklo_epi8(_mm_cvtsi32_si128(static_cast<int>(pa)), zero);
            const auto vb = _mm_unpacklo_epi8(_mm_cvtsi32_si128(static_cast<int>(pb)), zero);
            const auto vc = _mm_unpacklo_epi8(_mm_cvtsi32_si128(static_cast<int>(pc)), zero);
            const auto vd = _mm_unpacklo_epi8(_mm_cvtsi32_si128(static_cast<int>(pd)), zero);
            const auto left_weight = _mm_set1_epi16(static_cast<short>(256u - sample_x.fraction));
            const auto right_weight = _mm_set1_epi16(static_cast<short>(sample_x.fraction));
            const auto upper = _mm_srli_epi16(_mm_add_epi16(rounding,
                _mm_add_epi16(_mm_mullo_epi16(va, left_weight), _mm_mullo_epi16(vb, right_weight))), 8);
            const auto lower = _mm_srli_epi16(_mm_add_epi16(rounding,
                _mm_add_epi16(_mm_mullo_epi16(vc, left_weight), _mm_mullo_epi16(vd, right_weight))), 8);
            const auto blended = _mm_srli_epi16(_mm_add_epi16(rounding,
                _mm_add_epi16(_mm_mullo_epi16(upper, upper_weight), _mm_mullo_epi16(lower, lower_weight))), 8);
            const auto pixel = static_cast<std::uint32_t>(_mm_cvtsi128_si32(_mm_packus_epi16(blended, zero))) |
                               0xFF000000u;
            std::memcpy(output + x * 4, &pixel, 4);
        }
    }
}

#ifndef OPTIC_CAPTURE_PIXEL_TEST
class CaptureManager;
std::atomic<CaptureManager*> manager_instance{nullptr};

// Rust accepts exactly one owned compressed sample per callback. This gate is
// independent of the manager's command lock: stop waits for the current Rust
// callback, then prevents every future callback before encoder draining begins.
struct FrameGate {
    std::mutex mutex;
    bool active = true;
    const std::uint64_t generation;
    const FrameCallback callback;

    FrameGate(std::uint64_t generation, FrameCallback callback) : generation(generation), callback(callback) {}

    bool enabled() {
        std::lock_guard<std::mutex> lock(mutex);
        return active;
    }

    void disable() {
        std::lock_guard<std::mutex> lock(mutex);
        active = false;
    }

    void deliver(void* sample, double pts, double duration, std::size_t bytes, bool keyframe) {
        std::unique_lock<std::mutex> lock(mutex);
        if (active && callback) {
            // frame_received never calls capture_start/stop, and Rust releases
            // its recorder mutex before calling either operation from the UI.
            callback(generation, sample, pts, duration, bytes, keyframe);
            return;
        }
        lock.unlock();
        optic_sample_release(sample);
    }
};

void __cdecl forward_frame(std::uint64_t generation, void* sample, double pts,
                           double duration, std::size_t bytes, bool keyframe);

class DesktopCapture final {
public:
    DesktopCapture(std::uint64_t generation) {
        ComPtr<IDXGIFactory1> factory;
        check(CreateDXGIFactory1(IID_PPV_ARGS(&factory)), "Finding graphics adapters");
        const HMONITOR primary = MonitorFromPoint(POINT{0, 0}, MONITOR_DEFAULTTOPRIMARY);
        ComPtr<IDXGIAdapter1> selected_adapter;
        ComPtr<IDXGIOutput> selected_output;
        for (UINT adapter_index = 0; !selected_output; ++adapter_index) {
            ComPtr<IDXGIAdapter1> adapter;
            const HRESULT adapter_result = factory->EnumAdapters1(adapter_index, &adapter);
            if (adapter_result == DXGI_ERROR_NOT_FOUND) break;
            check(adapter_result, "Enumerating graphics adapters");
            for (UINT output_index = 0; ; ++output_index) {
                ComPtr<IDXGIOutput> output;
                const HRESULT output_result = adapter->EnumOutputs(output_index, &output);
                if (output_result == DXGI_ERROR_NOT_FOUND) break;
                check(output_result, "Enumerating displays");
                DXGI_OUTPUT_DESC desc{};
                check(output->GetDesc(&desc), "Reading display information");
                if (desc.AttachedToDesktop && desc.Monitor == primary) {
                    selected_adapter = adapter;
                    selected_output = output;
                    output_desc_ = desc;
                    break;
                }
            }
        }
        if (!selected_output) throw std::runtime_error("The primary display is unavailable.");
        const auto display_width = output_desc_.DesktopCoordinates.right - output_desc_.DesktopCoordinates.left;
        const auto display_height = output_desc_.DesktopCoordinates.bottom - output_desc_.DesktopCoordinates.top;
        if (display_width < 2 || display_height < 2 || display_width > 32768 || display_height > 32768) {
            throw std::runtime_error("The primary display dimensions are unsupported.");
        }
        display_width_ = static_cast<UINT>(display_width);
        display_height_ = static_cast<UINT>(display_height);
        const double scale = std::min(1.0, 1080.0 / display_height_);
        width_ = std::max(2u, static_cast<UINT>(display_width_ * scale) / 2 * 2);
        height_ = std::max(2u, static_cast<UINT>(display_height_ * scale) / 2 * 2);
        xs_ = scale_axis(display_width_, width_);
        ys_ = scale_axis(display_height_, height_);

        const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_0,
                                            D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0};
        check(D3D11CreateDevice(selected_adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                               D3D11_CREATE_DEVICE_BGRA_SUPPORT, levels, ARRAYSIZE(levels),
                               D3D11_SDK_VERSION, &device_, nullptr, &context_),
              "Creating the screen capture device");
        ComPtr<IDXGIOutput1> output1;
        check(selected_output.As(&output1), "Opening Desktop Duplication");
        check(output1->DuplicateOutput(device_.Get(), &duplication_), "Starting screen capture");
        duplication_->GetDesc(&duplication_desc_);
        rotation_ = duplication_desc_.Rotation;
        if (rotation_ != DXGI_MODE_ROTATION_UNSPECIFIED && rotation_ != DXGI_MODE_ROTATION_IDENTITY &&
            rotation_ != DXGI_MODE_ROTATION_ROTATE90 && rotation_ != DXGI_MODE_ROTATION_ROTATE180 &&
            rotation_ != DXGI_MODE_ROTATION_ROTATE270) {
            throw std::runtime_error("The display rotation is unsupported.");
        }
        encoder_ = std::make_unique<optic::Encoder>(width_, height_, generation, forward_frame);
        started_ = Clock::now();
    }

    void tick() {
        acquire();
        if (desktop_.empty()) {
            if (Clock::now() - started_ > std::chrono::seconds(10)) {
                throw std::runtime_error("Screen capture did not provide an image. Start buffering again.");
            }
            return;
        }
        if (render_dirty_) {
            if (pointer_.visible && !pointer_.shape.empty()) {
                working_ = desktop_;
                composite_pointer(working_, display_width_, display_height_, pointer_);
                scale_desktop(working_, display_width_, display_height_, width_, height_, xs_, ys_, encoded_);
            } else {
                scale_desktop(desktop_, display_width_, display_height_, width_, height_, xs_, ys_, encoded_);
            }
            render_dirty_ = false;
        }
        // A timer submits the latest complete image even on an idle desktop.
        // Use elapsed monotonic time so delayed ticks never compress real time.
        auto pts = std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - started_).count() / 100;
        pts = std::max(pts, previous_pts_ + 1);
        previous_pts_ = pts;
        encoder_->encode(encoded_.data(), static_cast<std::size_t>(width_) * 4, pts);
    }

    void finish() {
        if (!encoder_) return;
        encoder_->disable();
        // Drain with delivery disabled. Export snapshots own their compressed
        // samples independently and remain valid after this encoder is gone.
        try { encoder_->finish(); } catch (...) {}
        encoder_.reset();
        desktop_.clear();
        working_.clear();
        encoded_.clear();
    }

private:
    void acquire() {
        DXGI_OUTDUPL_FRAME_INFO info{};
        ComPtr<IDXGIResource> resource;
        const HRESULT result = duplication_->AcquireNextFrame(0, &info, &resource);
        if (result == DXGI_ERROR_WAIT_TIMEOUT) return;
        check(result, "Reading the screen image");
        // Every successful acquire is released, including exceptions during
        // cursor retrieval, readback, rotation, or memory allocation.
        struct FrameLease {
            IDXGIOutputDuplication* duplication;
            bool active = true;
            ~FrameLease() { if (active) duplication->ReleaseFrame(); }
        } lease{duplication_.Get()};

        if (info.LastMouseUpdateTime.QuadPart != 0) {
            pointer_.visible = info.PointerPosition.Visible != FALSE;
            pointer_.position = info.PointerPosition.Position;
            render_dirty_ = true;
        }
        if (info.PointerShapeBufferSize != 0) {
            // Driver supplied sizes are bounded separately from desktop data.
            if (info.PointerShapeBufferSize > 16u * 1024u * 1024u) {
                throw std::runtime_error("The cursor image is unexpectedly large.");
            }
            pointer_.shape.resize(info.PointerShapeBufferSize);
            UINT needed = 0;
            check(duplication_->GetFramePointerShape(static_cast<UINT>(pointer_.shape.size()),
                                                     pointer_.shape.data(), &needed, &pointer_.info),
                  "Reading the cursor image");
            validate_pointer(pointer_);
            render_dirty_ = true;
        }
        if (info.LastPresentTime.QuadPart != 0 || desktop_.empty()) {
            ComPtr<ID3D11Texture2D> texture;
            check(resource.As(&texture), "Opening the captured image");
            D3D11_TEXTURE2D_DESC desc{};
            texture->GetDesc(&desc);
            const bool portrait = rotation_ == DXGI_MODE_ROTATION_ROTATE90 ||
                                  rotation_ == DXGI_MODE_ROTATION_ROTATE270;
            const UINT expected_width = portrait ? display_height_ : display_width_;
            const UINT expected_height = portrait ? display_width_ : display_height_;
            if (desc.Width != expected_width || desc.Height != expected_height ||
                desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM || desc.SampleDesc.Count != 1) {
                throw std::runtime_error("The display size changed. Start buffering again to begin a new stream.");
            }
            if (!staging_) {
                auto staging_desc = desc;
                staging_desc.MipLevels = 1;
                staging_desc.ArraySize = 1;
                staging_desc.Usage = D3D11_USAGE_STAGING;
                staging_desc.BindFlags = 0;
                staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
                staging_desc.MiscFlags = 0;
                check(device_->CreateTexture2D(&staging_desc, nullptr, &staging_), "Creating the image readback surface");
            }
            context_->CopyResource(staging_.Get(), texture.Get());
            D3D11_MAPPED_SUBRESOURCE mapped{};
            check(context_->Map(staging_.Get(), 0, D3D11_MAP_READ, 0, &mapped), "Reading captured pixels");
            struct MapLease {
                ID3D11DeviceContext* context;
                ID3D11Texture2D* texture;
                ~MapLease() { context->Unmap(texture, 0); }
            } map_lease{context_.Get(), staging_.Get()};
            rotate_desktop(static_cast<const std::uint8_t*>(mapped.pData), mapped.RowPitch,
                           desc.Width, desc.Height, rotation_, desktop_);
            render_dirty_ = true;
        }
        lease.active = false;
        check(duplication_->ReleaseFrame(), "Releasing the captured image");
        check(device_->GetDeviceRemovedReason(), "Checking the screen capture device");
    }

    ComPtr<ID3D11Device> device_;
    ComPtr<ID3D11DeviceContext> context_;
    ComPtr<IDXGIOutputDuplication> duplication_;
    ComPtr<ID3D11Texture2D> staging_;
    DXGI_OUTPUT_DESC output_desc_{};
    DXGI_OUTDUPL_DESC duplication_desc_{};
    DXGI_MODE_ROTATION rotation_ = DXGI_MODE_ROTATION_IDENTITY;
    UINT display_width_ = 0, display_height_ = 0, width_ = 0, height_ = 0;
    Pointer pointer_;
    std::vector<std::uint8_t> desktop_, working_, encoded_;
    std::vector<AxisSample> xs_, ys_;
    std::unique_ptr<optic::Encoder> encoder_;
    Clock::time_point started_;
    std::int64_t previous_pts_ = -1;
    bool render_dirty_ = true;
};

struct Session {
    const std::uint64_t generation;
    const EventCallback events;
    const std::shared_ptr<FrameGate> gate;
    std::unique_ptr<DesktopCapture> capture;
    std::atomic<std::int32_t> ending{0};
    bool terminal = false; // Worker thread only.

    Session(std::uint64_t generation, FrameCallback frames, EventCallback events) :
        generation(generation), events(events), gate(std::make_shared<FrameGate>(generation, frames)) {}

    void event(std::int32_t kind, const char* text) const {
        if (events) events(generation, kind, text);
    }

    void request_end(std::int32_t kind) {
        std::int32_t expected = 0;
        ending.compare_exchange_strong(expected, kind);
        gate->disable();
    }

    void finish_requested() {
        const auto kind = ending.load();
        finish(kind == 3 ? 3 : 2, kind == 3 ? Interrupted : "Screen capture stopped.");
    }

    void fail(const char* text) {
        if (ending.load() != 0) finish_requested();
        else finish(3, text);
    }

    void finish(std::int32_t event_kind, const char* text) {
        if (terminal) return;
        terminal = true;
        gate->disable();
        if (capture) capture->finish();
        capture.reset();
        event(event_kind, text);
    }
};

class CaptureManager final {
public:
    static CaptureManager& shared() {
        static CaptureManager manager;
        return manager;
    }

    void start(std::uint64_t generation, FrameCallback frames, EventCallback events) {
        auto session = std::make_shared<Session>(generation, frames, events);
        std::shared_ptr<Session> previous;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if (requested_ && requested_->generation == generation) return;
            gates_[generation] = session->gate;
            commands_.push_back(session);
            previous = requested_;
            requested_ = session;
        }
        if (previous) previous->request_end(2);
        wake_.notify_one();
    }

    void stop(std::uint64_t generation) {
        auto session = requested();
        if (!session || session->generation != generation) return;
        session->request_end(2);
        wake_.notify_one();
    }

    void interrupt() {
        auto session = requested();
        if (!session) return;
        session->request_end(3);
        wake_.notify_one();
    }

    std::shared_ptr<FrameGate> gate(std::uint64_t generation) {
        std::lock_guard<std::mutex> lock(mutex_);
        const auto found = gates_.find(generation);
        return found == gates_.end() ? nullptr : found->second.lock();
    }

    ~CaptureManager() {
        manager_instance.store(nullptr);
        auto session = requested();
        if (session) session->request_end(2);
        {
            std::lock_guard<std::mutex> lock(mutex_);
            exiting_ = true;
        }
        wake_.notify_one();
        if (worker_.joinable()) worker_.join();
    }

private:
    CaptureManager() : worker_([this] { run(); }) { manager_instance.store(this); }

    std::shared_ptr<Session> requested() {
        std::lock_guard<std::mutex> lock(mutex_);
        return requested_;
    }

    void forget(const std::shared_ptr<Session>& session) {
        std::lock_guard<std::mutex> lock(mutex_);
        if (requested_ == session) requested_.reset();
        gates_.erase(session->generation);
    }

    void run() {
        const HRESULT com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        std::shared_ptr<Session> current;
        auto next_tick = Clock::now();
        for (;;) {
            std::shared_ptr<Session> command;
            bool ending = false;
            {
                std::unique_lock<std::mutex> lock(mutex_);
                if (current) {
                    wake_.wait_until(lock, next_tick, [this, &current] {
                        return exiting_ || !commands_.empty() || current->ending.load() != 0;
                    });
                } else {
                    wake_.wait(lock, [this] { return exiting_ || !commands_.empty(); });
                }
                if (exiting_) break;
                ending = current && current->ending.load() != 0;
                if (!ending && !commands_.empty()) {
                    command = std::move(commands_.front());
                    commands_.pop_front();
                }
            }
            if (ending) {
                current->finish_requested();
                forget(current);
                current.reset();
                continue;
            }
            if (command) {
                auto session = command;
                if (current && current != session) {
                    current->finish(2, "Screen capture stopped.");
                    forget(current);
                    current.reset();
                }
                if (session->terminal) continue;
                // A pending stop can close a gate before initialization.
                if (!session->gate->enabled()) {
                    session->finish_requested();
                    forget(session);
                    continue;
                }
                try {
                    check(com, "Initializing the screen capture worker");
                    session->capture = std::make_unique<DesktopCapture>(session->generation);
                    current = session;
                    next_tick = Clock::now();
                    if (session->gate->enabled()) session->event(1, "Screen capture started.");
                } catch (const std::exception& error) {
                    session->fail(error.what());
                    forget(session);
                } catch (...) {
                    session->fail("Unable to initialize screen capture.");
                    forget(session);
                }
                continue;
            }
            if (current && current->gate->enabled()) {
                try {
                    current->capture->tick();
                } catch (const std::exception& error) {
                    current->fail(error.what());
                    forget(current);
                    current.reset();
                } catch (...) {
                    current->fail("Screen capture failed unexpectedly.");
                    forget(current);
                    current.reset();
                }
            }
            // Skip missed deadlines rather than creating a catch-up queue.
            next_tick += FrameInterval;
            if (next_tick < Clock::now()) next_tick = Clock::now() + FrameInterval;
        }
        if (current) current->finish(2, "Screen capture stopped.");
        if (SUCCEEDED(com)) CoUninitialize();
    }

    std::mutex mutex_;
    std::condition_variable wake_;
    std::deque<std::shared_ptr<Session>> commands_;
    std::map<std::uint64_t, std::weak_ptr<FrameGate>> gates_;
    std::shared_ptr<Session> requested_;
    bool exiting_ = false;
    std::thread worker_;
};

void __cdecl forward_frame(std::uint64_t generation, void* sample, double pts,
                           double duration, std::size_t bytes, bool keyframe) {
    auto gate = CaptureManager::shared().gate(generation);
    if (gate) gate->deliver(sample, pts, duration, bytes, keyframe);
    else optic_sample_release(sample);
}
#endif
} // namespace

#ifndef OPTIC_CAPTURE_PIXEL_TEST
extern "C" void optic_capture_start(std::uint64_t generation, FrameCallback frame, EventCallback event) {
    try {
        CaptureManager::shared().start(generation, frame, event);
    } catch (const std::exception& error) {
        if (event) event(generation, 3, error.what());
    } catch (...) {
        if (event) event(generation, 3, "Unable to create the screen capture worker.");
    }
}

extern "C" void optic_capture_stop(std::uint64_t generation) {
    try {
        if (auto* manager = manager_instance.load()) manager->stop(generation);
    } catch (...) {
        // Cancellation never creates a worker and never unwinds into Rust.
    }
}

extern "C" void optic_capture_interrupt() {
    try {
        if (auto* manager = manager_instance.load()) manager->interrupt();
    } catch (...) {
        // Win32 notification callbacks must not observe C++ exceptions.
    }
}
#else
// Standalone algorithm tests: compile this source with OPTIC_CAPTURE_PIXEL_TEST.
// They do not capture the user's screen and need no graphics adapter or encoder.
int main() {
    auto require = [](bool value, const char* message) {
        if (!value) throw std::runtime_error(message);
    };
    try {
        const std::vector<std::uint8_t> image = {
            1, 1, 1, 255, 2, 2, 2, 255, 3, 3, 3, 255,
            4, 4, 4, 255, 5, 5, 5, 255, 6, 6, 6, 255};
        std::vector<std::uint8_t> rotated;
        rotate_desktop(image.data(), 12, 3, 2, DXGI_MODE_ROTATION_ROTATE90, rotated);
        const std::uint8_t clockwise[] = {4, 1, 5, 2, 6, 3};
        for (unsigned i = 0; i < 6; ++i) require(rotated[i * 4] == clockwise[i], "90-degree rotation");
        rotate_desktop(image.data(), 12, 3, 2, DXGI_MODE_ROTATION_ROTATE180, rotated);
        for (unsigned i = 0; i < 6; ++i) require(rotated[i * 4] == 6 - i, "180-degree rotation");
        rotate_desktop(image.data(), 12, 3, 2, DXGI_MODE_ROTATION_ROTATE270, rotated);
        const std::uint8_t counterclockwise[] = {3, 6, 2, 5, 1, 4};
        for (unsigned i = 0; i < 6; ++i) require(rotated[i * 4] == counterclockwise[i], "270-degree rotation");

        Pointer pointer;
        pointer.visible = true;
        pointer.position = {0, 0};
        pointer.info = {DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, 4, 2, 1, {0, 0}};
        // AND/XOR combinations: black, white, unchanged, inverted.
        pointer.shape = {0x30, 0x50};
        std::vector<std::uint8_t> desktop(4 * 4, 0x40);
        composite_pointer(desktop, 4, 1, pointer);
        require(desktop[0] == 0 && desktop[4] == 255 && desktop[8] == 0x40 &&
                desktop[12] == 0xBF, "Monochrome AND/XOR cursor");

        pointer.info = {DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, 2, 1, 8, {0, 0}};
        pointer.shape = {0x10, 0x20, 0x30, 0, 0xAA, 0xBB, 0xCC, 255};
        desktop.assign(8, 0x40);
        composite_pointer(desktop, 2, 1, pointer);
        require(desktop[0] == 0x10 && desktop[1] == 0x20 && desktop[2] == 0x30 &&
                desktop[4] == (0x40 ^ 0xAA), "Masked color cursor");

        pointer.info = {DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, 2, 1, 8, {7, 7}};
        pointer.position = {-1, 0}; // Top left already accounts for hotspot.
        pointer.shape = {0, 0, 0, 0, 64, 32, 16, 128};
        desktop.assign(8, 100);
        composite_pointer(desktop, 2, 1, pointer);
        require(desktop[0] == 82 && desktop[1] == 66 && desktop[2] == 58 &&
                desktop[4] == 100, "Clipped color cursor alpha");

        std::vector<std::uint8_t> scaled;
        const std::vector<std::uint8_t> square = {
            0, 0, 0, 255, 40, 40, 40, 255, 80, 80, 80, 255, 120, 120, 120, 255};
        scale_desktop(square, 2, 2, 1, 1, scale_axis(2, 1), scale_axis(2, 1), scaled);
        require(scaled[0] == 60 && scaled[3] == 255, "Bilinear image scaling");
        std::puts("Capture pixel tests passed: rotation, cursor masks, alpha, clipping, scaling.");
        return 0;
    } catch (const std::exception& error) {
        std::fprintf(stderr, "Capture pixel test failed: %s\n", error.what());
        return 1;
    }
}
#endif
