#pragma once

#include "Bridge.h"
#include <cstddef>
#include <cstdint>
#include <memory>

namespace optic {

// Construct and encode on a COM MTA worker. Pixels are top-down BGRA at the
// final, even output dimensions. No COM objects or pixels escape the worker.
class Encoder final {
public:
    Encoder(std::uint32_t width, std::uint32_t height, std::uint64_t generation,
            FrameCallback callback);
    ~Encoder();
    Encoder(const Encoder&) = delete;
    Encoder& operator=(const Encoder&) = delete;
    void encode(const std::uint8_t* bgra, std::size_t stride, std::int64_t pts100ns);
    void finish();
    bool hardware_accelerated() const noexcept;
    // Thread safe: waits for any current callback and prevents later callbacks.
    void disable();

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

} // namespace optic
