#pragma once

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <string>

using ActionCallback = bool(__cdecl*)(std::int32_t);
using FrameCallback = void(__cdecl*)(std::uint64_t, void*, double, double, std::size_t, bool);
using EventCallback = void(__cdecl*)(std::uint64_t, std::int32_t, const char*);
// Events: 1 started, 2 stopped, 3 failed, 4 interrupted by the session/display.
using ErrorCallback = void(__cdecl*)(const char*, const char*, const char*);

extern "C" {
void optic_set_error_callback(ErrorCallback callback);
void optic_run(ActionCallback callback);
void optic_set_status(std::int32_t state, const char* message);
void optic_finish_quit();
void optic_prepare_console();
bool optic_recordings_folder(wchar_t* path, std::size_t capacity, char* error, std::size_t error_capacity);
void optic_capture_start(std::uint64_t generation, FrameCallback frame, EventCallback event);
void optic_capture_stop(std::uint64_t generation);
void optic_capture_interrupt();
void optic_sample_release(void* sample);
bool optic_export(void* const* samples, std::size_t count, const wchar_t* path,
                  char* error, std::size_t error_capacity);
bool optic_generate_test_video(std::uint64_t generation, FrameCallback frame,
                               char* error, std::size_t error_capacity);
}

namespace optic {
std::wstring recordings_folder();

// Error text is copied within the caller's buffer and is always NUL-terminated.
inline void copy_error(const char* message, char* buffer, std::size_t capacity) noexcept {
    if (!buffer || capacity == 0) return;
    const auto length = std::min(std::strlen(message), capacity - 1);
    std::memcpy(buffer, message, length);
    buffer[length] = '\0';
}
}
