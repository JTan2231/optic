// Standalone shell integration test. No capture implementation is linked.
// Build this source alone with the same flags/libraries as App.cpp and
// /SUBSYSTEM:WINDOWS /ENTRY:mainCRTStartup to exercise GUI console attachment.
#define OPTIC_UI_TEST 1
#include "App.cpp"

#include <atomic>
#include <chrono>
#include <thread>

namespace {
std::atomic<unsigned> toggles{0};
std::atomic<unsigned> saves{0};
std::atomic<unsigned> quits{0};
std::atomic<unsigned> interruptions{0};
std::atomic<bool> defer_quit{true};
struct NativeError {
    std::string component;
    std::string event;
    std::string message;
};
std::mutex errors_mutex;
std::vector<NativeError> errors;
std::atomic<bool> error_callback_failed{false};

void __cdecl test_error(const char* component, const char* event, const char* message) noexcept {
    try {
        // Taking this lock also verifies that native logging never invokes the
        // Rust-facing callback while holding the published-window mutex.
        std::lock_guard<std::mutex> window_guard(window_mutex);
        std::lock_guard<std::mutex> guard(errors_mutex);
        errors.push_back({component ? component : "", event ? event : "", message ? message : ""});
    } catch (...) {
        error_callback_failed = true;
    }
}

void expect(bool condition, const char* description) {
    if (!condition) throw std::runtime_error(description);
}

bool __cdecl test_action(std::int32_t action) {
    switch (action) {
        case 1: ++toggles; return true;
        case 2: ++saves; return true;
        case 3: ++quits; return !defer_quit.load();
        default: return true;
    }
}

void invoke(HWND window, std::function<void(Application&)> action) {
    HANDLE complete = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    expect(complete != nullptr, "Could not create test completion event");
    TestInvocation invocation{std::move(action), complete, {}};
    if (!PostMessageW(window, kTestInvocationMessage, 0, reinterpret_cast<LPARAM>(&invocation))) {
        CloseHandle(complete);
        throw std::runtime_error("Could not post test invocation");
    }
    // The UI test process is terminated on a timeout: the queued payload may
    // still refer to this stack, so unwinding and continuing would be unsafe.
    if (WaitForSingleObject(complete, 10000) != WAIT_OBJECT_0) {
        OutputDebugStringW(L"[optic] Tray test UI invocation timed out\n");
        TerminateProcess(GetCurrentProcess(), 1);
    }
    CloseHandle(complete);
    if (invocation.failure) std::rethrow_exception(invocation.failure);
}

void post(HWND window, UINT message, WPARAM wparam = 0, LPARAM lparam = 0) {
    expect(PostMessageW(window, message, wparam, lparam) != FALSE, "Could not post test window message");
}

std::wstring menu_title(HMENU menu, UINT item, UINT flag = MF_BYCOMMAND) {
    wchar_t text[2048]{};
    expect(GetMenuStringW(menu, item, text, static_cast<int>(std::size(text)), flag) > 0,
        "Could not inspect menu title");
    return text;
}

bool menu_enabled(HMENU menu, UINT command) {
    const UINT state = GetMenuState(menu, command, MF_BYCOMMAND);
    expect(state != static_cast<UINT>(-1), "Could not inspect menu enabled state");
    return !(state & (MF_DISABLED | MF_GRAYED));
}

void check_native_logging() {
    std::wstring message = L"Native failure \x00B7 (Windows error 5)";
    report_error("ui", "test.failed", message);
    std::fill(message.begin(), message.end(), L'X');
    expect(errors.size() == 1 && errors[0].component == "ui" && errors[0].event == "test.failed"
        && errors[0].message == "Native failure \xC2\xB7 (Windows error 5)",
        "The native error callback did not receive owned UTF-8 fields and the OS error code");
    std::vector<std::thread> workers;
    for (unsigned index = 0; index < 4; ++index) {
        workers.emplace_back([] {
            for (unsigned entry = 0; entry < 8; ++entry) {
                report_error("ui", "test.concurrent_failed", L"Synthetic concurrent failure");
            }
        });
    }
    for (auto& worker : workers) worker.join();
    expect(errors.size() == 33 && !error_callback_failed, "Concurrent native error callbacks lost entries");
    errors.clear();
    optic_set_status(0, "Stopped");
    optic_set_status(2, "Buffering");
    expect(errors.empty(), "Normal statuses or Rust-owned returned errors were logged by the native callback");
}

void check_folder() {
    wchar_t folder[32768]{};
    char error[256]{};
    expect(optic_recordings_folder(folder, std::size(folder), error, std::size(error)),
        "Recordings folder lookup failed");
    expect(std::wstring(folder) == optic::recordings_folder(), "UI and C ABI recordings folders differ");
    const std::wstring path(folder);
    expect(path.size() >= 6 && path.substr(path.size() - 6) == L"\\Optic",
        "Recordings folder does not end in Videos\\Optic");
    wchar_t short_path[1] = {L'X'};
    expect(!optic_recordings_folder(short_path, 1, error, std::size(error)),
        "An undersized folder buffer should fail");
    expect(short_path[0] == L'\0' && error[0] != '\0', "Folder failure buffers were not terminated");
    char short_error[1] = {'X'};
    expect(!optic_recordings_folder(nullptr, 0, short_error, 1) && short_error[0] == '\0',
        "A one-byte error buffer was not terminated");
}

void check_console() {
    const HANDLE original_stdout = GetStdHandle(STD_OUTPUT_HANDLE);
    const HANDLE original_stderr = GetStdHandle(STD_ERROR_HANDLE);
    HANDLE out_read = nullptr, out_write = nullptr, err_read = nullptr, err_write = nullptr;
    expect(CreatePipe(&out_read, &out_write, nullptr, 0) != FALSE, "Could not create stdout test pipe");
    if (!CreatePipe(&err_read, &err_write, nullptr, 0)) {
        CloseHandle(out_read);
        CloseHandle(out_write);
        throw std::runtime_error("Could not create stderr test pipe");
    }
    bool valid = false;
    bool content_valid = false;
    SetStdHandle(STD_OUTPUT_HANDLE, out_write);
    SetStdHandle(STD_ERROR_HANDLE, err_write);
    optic_prepare_console();
    valid = GetStdHandle(STD_OUTPUT_HANDLE) == out_write && GetStdHandle(STD_ERROR_HANDLE) == err_write;
    if (valid) {
        constexpr char expected[] = "redirected";
        DWORD written = 0, read = 0;
        char bytes[sizeof(expected)]{};
        if (WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), expected, sizeof(expected), &written, nullptr)
            && written == sizeof(expected)
            && ReadFile(out_read, bytes, sizeof(bytes), &read, nullptr)
            && read == sizeof(expected) && std::memcmp(bytes, expected, sizeof(expected)) == 0) {
            content_valid = WriteFile(GetStdHandle(STD_ERROR_HANDLE), expected, sizeof(expected), &written, nullptr)
                && written == sizeof(expected)
                && ReadFile(err_read, bytes, sizeof(bytes), &read, nullptr)
                && read == sizeof(expected) && std::memcmp(bytes, expected, sizeof(expected)) == 0;
        }
    }
    SetStdHandle(STD_OUTPUT_HANDLE, original_stdout);
    SetStdHandle(STD_ERROR_HANDLE, original_stderr);
    CloseHandle(out_read);
    CloseHandle(out_write);
    CloseHandle(err_read);
    CloseHandle(err_write);
    expect(valid, "Console attachment discarded a redirected stdout/stderr handle");
    expect(content_valid, "Redirected output did not reach its original pipes");
}

BOOL CALLBACK find_window(HWND window, LPARAM context) {
    DWORD process = 0;
    GetWindowThreadProcessId(window, &process);
    if (process != GetCurrentProcessId()) return TRUE;
    wchar_t name[128]{};
    if (GetClassNameW(window, name, static_cast<int>(std::size(name))) && std::wcscmp(name, kWindowClass) == 0) {
        *reinterpret_cast<HWND*>(context) = window;
        return FALSE;
    }
    return TRUE;
}

class ApplicationTest {
    static void check_menu(const Application& app, const wchar_t* toggle_title,
        bool toggle_enabled, bool save_enabled, bool quit_enabled) {
        const bool owned = app.active_menu_ == nullptr;
        HMENU menu = owned ? app.create_menu() : app.active_menu_;
        expect(menu != nullptr, "Could not construct tray menu");
        try {
            expect(menu_title(menu, kToggleCommand) == toggle_title, "Unexpected toggle menu title");
            expect(menu_title(menu, kSaveCommand) == L"Save Replay\tCtrl+Alt+S", "Unexpected save menu title");
            expect(menu_title(menu, kFolderCommand) == L"Open Recordings Folder", "Unexpected folder menu title");
            expect(menu_title(menu, kQuitCommand) == L"Quit Optic", "Unexpected quit menu title");
            expect(menu_title(menu, 0, MF_BYPOSITION) == menu_text(app.message_), "Unexpected status menu title");
            expect(menu_enabled(menu, kToggleCommand) == toggle_enabled, "Unexpected toggle enabled state");
            expect(menu_enabled(menu, kSaveCommand) == save_enabled, "Unexpected save enabled state");
            expect(menu_enabled(menu, kQuitCommand) == quit_enabled, "Unexpected quit enabled state");
            expect(menu_enabled(menu, kFolderCommand), "Recordings folder menu should remain available");
        } catch (...) {
            if (owned) DestroyMenu(menu);
            throw;
        }
        if (owned) DestroyMenu(menu);
    }

public:
    static void run(HWND window) {
        invoke(window, [](Application& app) {
            expect(app.state_ == 0 && app.message_ == L"Stopped", "The app did not launch stopped");
            expect(!app.toggle_hotkey_, "The conflicting toggle shortcut was registered");
            expect(std::any_of(app.shortcut_failures_.begin(), app.shortcut_failures_.end(),
                [](const std::wstring& text) { return text.find(L"Ctrl+Alt+B") != std::wstring::npos; }),
                "The conflicting shortcut was not reported in the menu");
            // Keep one menu alive across all posted transitions. Inspecting
            // this existing handle verifies refresh during a popup's lifetime,
            // rather than creating a new menu that starts in the latest state.
            app.active_menu_ = app.create_menu();
            expect(app.active_menu_ != nullptr, "Could not create the persistent test menu");
            check_menu(app, L"Start Buffering\tCtrl+Alt+B", true, false, true);
            fail_next_status_post = true;
        });
        // Force the real status-post error path without changing any desktop
        // resources. The callback itself takes window_mutex: synchronous
        // reporting from optic_set_status would deadlock this test.
        optic_set_status(2, "Synthetic dropped status");
        invoke(window, [](Application& app) {
            expect(app.state_ == 0 && app.message_ == L"Stopped", "A failed status post changed the UI state");
            std::lock_guard<std::mutex> guard(errors_mutex);
            expect(std::count_if(errors.begin(), errors.end(), [](const NativeError& error) {
                return error.event == "tray.status_update_failed"
                    && error.message.find(std::to_string(ERROR_NOT_ENOUGH_QUOTA)) != std::string::npos;
            }) == 1, "A failed status post was not deferred and logged exactly once with its OS code");
        });
        post(window, WM_HOTKEY, kSaveCommand);
        post(window, WM_COMMAND, kToggleCommand);
        invoke(window, [](Application& app) {
            expect(toggles == 1 && saves == 0 && app.state_ == 1, "Start/save guards failed");
            check_menu(app, L"Stop Buffering\tCtrl+Alt+B", true, false, true);
        });
        char copied_message[] = "Buffering \xC2\xB7 last 30 seconds & ready";
        optic_set_status(2, copied_message);
        std::fill(std::begin(copied_message), std::end(copied_message) - 1, 'X');
        invoke(window, [](Application& app) {
            expect(app.state_ == 2 && app.message_ == L"Buffering \x00B7 last 30 seconds & ready",
                "A posted status did not copy its caller's UTF-8 text");
            check_menu(app, L"Stop Buffering\tCtrl+Alt+B", true, true, true);
        });
        post(window, WM_HOTKEY, kSaveCommand);
        post(window, WM_HOTKEY, kToggleCommand);
        post(window, WM_HOTKEY, kSaveCommand);
        post(window, WM_HOTKEY, kToggleCommand);
        invoke(window, [](Application& app) {
            expect(toggles == 2 && saves == 1 && app.state_ == 3, "Buffering/stopping action guards failed");
            check_menu(app, L"Stopping\x2026\tCtrl+Alt+B", false, false, true);
        });
        optic_set_status(0, "Stopped");
        invoke(window, [](Application& app) {
            expect(app.state_ == 0, "The stopped status was not applied");
            check_menu(app, L"Start Buffering\tCtrl+Alt+B", true, false, true);
        });
        post(window, WM_HOTKEY, kToggleCommand);
        optic_set_status(2, "");
        invoke(window, [](Application& app) {
            expect(toggles == 3 && app.state_ == 2 && app.message_ == L"Buffering",
                "Restart or empty-message state fallback failed");
            check_menu(app, L"Stop Buffering\tCtrl+Alt+B", true, true, true);
        });
        post(window, WM_POWERBROADCAST, PBT_APMSUSPEND);
        post(window, WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK);
        post(window, WM_DISPLAYCHANGE);
        invoke(window, [](Application&) {
            expect(interruptions == 3, "Power, session, and display interruption notifications were not forwarded");
        });
        post(window, WM_COMMAND, kQuitCommand);
        optic_set_status(0, "Should be ignored during quitting");
        post(window, WM_HOTKEY, kToggleCommand);
        post(window, WM_HOTKEY, kSaveCommand);
        invoke(window, [](Application& app) {
            expect(quits == 1 && toggles == 3 && saves == 1, "Quit action guards failed");
            expect(app.quitting_ && app.awaiting_termination_ && app.state_ == 2,
                "Quit did not defer termination or ignored status was applied");
            expect(app.message_ == L"Finishing saved clip\x2026", "Unexpected deferred quit status");
            check_menu(app, L"Stop Buffering\tCtrl+Alt+B", false, false, false);
            DestroyMenu(app.active_menu_);
            app.active_menu_ = nullptr;
        });
        optic_finish_quit();
        // Exercise cleanup of status payloads queued behind termination.
        for (unsigned index = 0; index < 32; ++index) optic_set_status(2, "Ignored pending status");
    }
};

void print_result(const char* text) {
    const HANDLE output = GetStdHandle(STD_OUTPUT_HANDLE);
    DWORD written = 0;
    if (output && output != INVALID_HANDLE_VALUE) {
        WriteFile(output, text, static_cast<DWORD>(std::strlen(text)), &written, nullptr);
    }
    OutputDebugStringA(text);
}
}

extern "C" void optic_capture_interrupt() {
    ++interruptions;
}

int main() {
    try {
        optic_set_error_callback(test_error);
        check_native_logging();
        check_console();
        check_folder();
        expect(errors.empty(), "Rust-owned recordings folder failures were logged twice");
        // A pre-existing external registration is also a valid conflict.
        const bool registered_conflict = RegisterHotKey(nullptr, 77, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'B') != FALSE;
        expect(registered_conflict || GetLastError() == ERROR_HOTKEY_ALREADY_REGISTERED,
            "Could not arrange the shortcut conflict test");
        std::exception_ptr failure;
        std::thread worker([&failure] {
            HWND window = nullptr;
            try {
                const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
                while (!window && std::chrono::steady_clock::now() < deadline) {
                    EnumWindows(find_window, reinterpret_cast<LPARAM>(&window));
                    if (!window) std::this_thread::sleep_for(std::chrono::milliseconds(10));
                }
                expect(window != nullptr, "The tray window was not created");
                ApplicationTest::run(window);
            } catch (...) {
                failure = std::current_exception();
                defer_quit = false;
                if (window) {
                    // Finish an already deferred quit, or request ordinary quit.
                    optic_finish_quit();
                    PostMessageW(window, WM_CLOSE, 0, 0);
                }
            }
        });
        optic_run(test_action);
        worker.join();
        if (registered_conflict) UnregisterHotKey(nullptr, 77);
        if (failure) std::rethrow_exception(failure);
        expect(!error_callback_failed && std::count_if(errors.begin(), errors.end(), [](const NativeError& error) {
            return error.component == "shortcuts" && error.event == "shortcut.registration_failed"
                && error.message.find("Ctrl+Alt+B") != std::string::npos
                && error.message.find("error ") != std::string::npos;
        }) == 1, "The failed shortcut registration was not logged exactly once with its native error code");
        expect(application_window == nullptr && quits == 1, "Deferred termination did not remove the tray window");
        optic_set_status(0, "Post-termination status");
        optic_finish_quit();
        print_result("Windows tray self-test passed: native error callbacks/filtering, menu states, shortcuts, status ownership, interruptions, deferred quit, folder lookup, console redirection\n");
        return 0;
    } catch (const std::exception& exception) {
        const std::string message = std::string("Windows tray self-test failed: ") + exception.what() + "\n";
        print_result(message.c_str());
        return 1;
    }
}
