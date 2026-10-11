// The Windows shell implements the same action/status protocol as App.swift.
// All actions run on the window thread; worker threads only post copied status.
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <windowsx.h>
#include <shellapi.h>
#include <shlobj.h>
#include <wtsapi32.h>
#include <sddl.h>

#include "Bridge.h"

#include <cmath>
#include <atomic>
#include <cwchar>
#include <exception>
#include <iterator>
#include <memory>
#include <mutex>
#include <new>
#include <stdexcept>
#include <vector>

#ifdef OPTIC_UI_TEST
#include <functional>
#endif

namespace {
constexpr wchar_t kWindowClass[] = L"Optic.TrayWindow";
constexpr wchar_t kShutdownMessageName[] = L"Optic.Shutdown.v1";
#ifdef OPTIC_UI_TEST
// Shell tests must never close a real installed or portable Optic instance.
constexpr wchar_t kInstancePrefix[] = L"Local\\Optic.Test.Instance.";
constexpr wchar_t kMaintenanceMutex[] = L"Local\\Optic.Test.Maintenance.v1";
constexpr wchar_t kLegacyWindowTitle[] = L"Optic.Test.Legacy";
#else
constexpr wchar_t kInstancePrefix[] = L"Local\\Optic.Instance.";
constexpr wchar_t kMaintenanceMutex[] = L"Local\\Optic.Maintenance.v1";
constexpr wchar_t kLegacyWindowTitle[] = L"Optic";
#endif
constexpr UINT kTrayMessage = WM_APP + 1;
constexpr UINT kStatusMessage = WM_APP + 2;
constexpr UINT kFinishQuitMessage = WM_APP + 3;
constexpr UINT kStatusErrorMessage = WM_APP + 5;
#ifdef OPTIC_UI_TEST
constexpr UINT kTestInvocationMessage = WM_APP + 4;
std::atomic<bool> fail_next_status_post{false};
class Application;
struct TestInvocation {
    std::function<void(Application&)> action;
    HANDLE complete;
    std::exception_ptr failure;
};
#endif
constexpr UINT_PTR kTrayRetryTimer = 1;
constexpr UINT kToggleCommand = 1;
constexpr UINT kSaveCommand = 2;
constexpr UINT kFolderCommand = 4;
constexpr UINT kQuitCommand = 3;
constexpr UINT kTrayId = 1;

std::mutex window_mutex;
HWND application_window = nullptr;
std::atomic<ErrorCallback> error_callback{nullptr};
std::atomic<DWORD> pending_status_error{ERROR_SUCCESS};

struct Status {
    std::int32_t state;
    std::wstring message;
};

class Handle {
public:
    explicit Handle(HANDLE value = nullptr) noexcept : value_(value) {}
    ~Handle() { if (value_) CloseHandle(value_); }
    Handle(const Handle&) = delete;
    Handle& operator=(const Handle&) = delete;
    HANDLE get() const noexcept { return value_; }
    HANDLE release() noexcept {
        const HANDLE result = value_;
        value_ = nullptr;
        return result;
    }
private:
    HANDLE value_;
};

std::runtime_error windows_error(const char* operation) {
    return std::runtime_error(std::string(operation) + " (Windows error "
        + std::to_string(GetLastError()) + ")");
}

std::wstring user_sid(HANDLE process) {
    HANDLE raw_token = nullptr;
    if (!OpenProcessToken(process, TOKEN_QUERY, &raw_token)) throw windows_error("Could not identify the Optic user");
    Handle token(raw_token);
    DWORD bytes = 0;
    GetTokenInformation(token.get(), TokenUser, nullptr, 0, &bytes);
    if (!bytes) throw windows_error("Could not read the Optic user");
    std::vector<unsigned char> data(bytes);
    if (!GetTokenInformation(token.get(), TokenUser, data.data(), bytes, &bytes)) {
        throw windows_error("Could not read the Optic user");
    }
    LPWSTR raw_sid = nullptr;
    const auto* user = reinterpret_cast<const TOKEN_USER*>(data.data());
    if (!ConvertSidToStringSidW(user->User.Sid, &raw_sid)) throw windows_error("Could not identify the Optic user");
    std::unique_ptr<wchar_t, decltype(&LocalFree)> sid(raw_sid, LocalFree);
    return sid.get();
}

std::wstring instance_name() {
    // Local names are isolated by Windows session. The SID also isolates Run
    // As users within that session, and the name never depends on the exe path.
    return kInstancePrefix + user_sid(GetCurrentProcess());
}

bool shutdown_window(HWND window, UINT message, const std::wstring& sid, DWORD own_session, DWORD timeout) {
    DWORD process_id = 0;
    if (!GetWindowThreadProcessId(window, &process_id)) return true;
    Handle process(OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, process_id));
    if (!process.get()) {
        // The window can disappear between lookup and OpenProcess.
        if (GetLastError() == ERROR_INVALID_PARAMETER) return true;
        throw windows_error("Could not wait for Optic to close");
    }
    if (WaitForSingleObject(process.get(), 0) == WAIT_OBJECT_0) return true;
    DWORD session = 0;
    if (!ProcessIdToSessionId(process_id, &session)) throw windows_error("Could not identify the running Optic session");
    if (session != own_session || user_sid(process.get()) != sid) return false;
    if (!PostMessageW(window, message, 0, 0)) {
        if (WaitForSingleObject(process.get(), 0) == WAIT_OBJECT_0) return true;
        throw windows_error("Could not request Optic shutdown");
    }
    // The executable stays in use until process exit, beyond tray removal.
    const DWORD result = WaitForSingleObject(process.get(), timeout);
    if (result == WAIT_OBJECT_0) return true;
    if (result != WAIT_TIMEOUT) throw windows_error("Could not wait for Optic to close");
    throw std::runtime_error("Timed out waiting for Optic to finish its saved clip and close; the process was left running");
}

class MutexLease {
public:
    explicit MutexLease(const wchar_t* name) : handle_(CreateMutexW(nullptr, FALSE, name)) {
        if (!handle_.get()) throw windows_error("Could not open the Optic launch lock");
    }
    ~MutexLease() { release(); }
    bool acquire() {
        const DWORD result = WaitForSingleObject(handle_.get(), 0);
        if (result == WAIT_TIMEOUT) return false;
        if (result != WAIT_OBJECT_0 && result != WAIT_ABANDONED) throw windows_error("Could not lock the Optic launch");
        owned_ = true;
        return true;
    }
    void release() noexcept {
        if (owned_) {
            ReleaseMutex(handle_.get());
            owned_ = false;
        }
    }
    void hold_until_process_exit() noexcept {
        // The running app intentionally hands this one kernel handle to the
        // process. Windows closes it on exit and abandons the owning thread's
        // mutex. A tray/window teardown is too early: COM and Rust may still
        // be unwinding while the executable image remains in use.
        owned_ = false;
        (void)handle_.release();
    }
private:
    Handle handle_;
    bool owned_ = false;
};

std::wstring from_utf8(const char* text) {
    if (!text || !*text) return {};
    const int count = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, text, -1, nullptr, 0);
    if (count == 0) return L"Invalid status text";
    std::wstring result(static_cast<std::size_t>(count), L'\0');
    if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, text, -1, result.data(), count) == 0) {
        return L"Invalid status text";
    }
    result.resize(static_cast<std::size_t>(count - 1));
    return result;
}

void write_diagnostic(const std::wstring& text) noexcept {
    try {
        const std::wstring line = L"[optic] " + text + L"\n";
        OutputDebugStringW(line.c_str());
        const HANDLE handle = GetStdHandle(STD_ERROR_HANDLE);
        if (!handle || handle == INVALID_HANDLE_VALUE) return;
        DWORD written = 0;
        DWORD mode = 0;
        if (GetConsoleMode(handle, &mode)) {
            WriteConsoleW(handle, line.data(), static_cast<DWORD>(line.size()), &written, nullptr);
        } else {
            const int count = WideCharToMultiByte(CP_UTF8, 0, line.data(),
                static_cast<int>(line.size()), nullptr, 0, nullptr, nullptr);
            if (count <= 0) return;
            std::string bytes(static_cast<std::size_t>(count), '\0');
            WideCharToMultiByte(CP_UTF8, 0, line.data(), static_cast<int>(line.size()),
                bytes.data(), count, nullptr, nullptr);
            WriteFile(handle, bytes.data(), static_cast<DWORD>(bytes.size()), &written, nullptr);
        }
    } catch (...) {
        OutputDebugStringW(L"[optic] Unable to write diagnostic\n");
    }
}

// The bridge supplies borrowed UTF-8 strings; Rust copies them immediately.
// Call only outside window_mutex and outside optic_set_status, whose caller
// can hold the Rust recorder lock. Returned media/CLI errors are logged by Rust.
void report_error(const char* component, const char* event, const std::wstring& text) noexcept {
    try {
        if (const auto callback = error_callback.load()) {
            const int count = WideCharToMultiByte(CP_UTF8, 0, text.data(),
                static_cast<int>(text.size()), nullptr, 0, nullptr, nullptr);
            if (count == 0 && !text.empty()) { write_diagnostic(text); return; }
            std::string bytes(static_cast<std::size_t>(count), '\0');
            if (count && !WideCharToMultiByte(CP_UTF8, 0, text.data(), static_cast<int>(text.size()),
                bytes.data(), count, nullptr, nullptr)) {
                write_diagnostic(text);
                return;
            }
            callback(component, event, bytes.c_str());
            return;
        }
    } catch (...) {
        // Neither allocation failures nor a native test callback may unwind
        // through a Win32 or Rust callback boundary.
    }
    write_diagnostic(text);
}

// Called with window_mutex held, including from a Rust caller that holds its
// recorder lock. Keep only a bounded, allocation-free pending error and wake
// the UI; the message loop delivers it after returning from any action callback.
void queue_status_failure(DWORD error) noexcept {
    if (!application_window) return;
    DWORD expected = ERROR_SUCCESS;
    if (pending_status_error.compare_exchange_strong(expected, error ? error : ERROR_GEN_FAILURE)) {
        PostMessageW(application_window, kStatusErrorMessage, 0, 0);
    }
}

void report_pending_status_failure() noexcept {
    const DWORD error = pending_status_error.exchange(ERROR_SUCCESS);
    if (error == ERROR_SUCCESS) return;
    try {
        report_error("ui", "tray.status_update_failed",
            L"Could not post status update (Windows error " + std::to_wstring(error) + L")");
    } catch (...) {
        OutputDebugStringW(L"[optic] Could not post status update\n");
    }
}

std::wstring state_name(std::int32_t state) {
    switch (state) {
        case 1: return L"Starting\x2026";
        case 2: return L"Buffering";
        case 3: return L"Stopping\x2026";
        default: return L"Stopped";
    }
}

std::wstring menu_text(const std::wstring& text) {
    std::wstring result;
    result.reserve(text.size());
    for (wchar_t character : text) {
        result.push_back(character);
        if (character == L'&') result.push_back(L'&');
    }
    return result;
}

// Use the taskbar's theme, including high contrast, and render at its DPI.
COLORREF icon_color() noexcept {
    HIGHCONTRASTW contrast{};
    contrast.cbSize = sizeof(contrast);
    if (SystemParametersInfoW(SPI_GETHIGHCONTRAST, sizeof(contrast), &contrast, 0)
        && (contrast.dwFlags & HCF_HIGHCONTRASTON)) {
        return GetSysColor(COLOR_WINDOWTEXT);
    }
    DWORD light = 0;
    DWORD size = sizeof(light);
    if (RegGetValueW(HKEY_CURRENT_USER,
        L"Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
        L"SystemUsesLightTheme", RRF_RT_REG_DWORD, nullptr, &light, &size) != ERROR_SUCCESS) {
        light = 0;
    }
    return light ? RGB(30, 30, 30) : RGB(245, 245, 245);
}

int tray_icon_size(HWND window) noexcept {
    using GetDpi = UINT(WINAPI*)(HWND);
    using GetMetric = int(WINAPI*)(int, UINT);
    const HMODULE user = GetModuleHandleW(L"user32.dll");
    const auto get_dpi = reinterpret_cast<GetDpi>(GetProcAddress(user, "GetDpiForWindow"));
    const auto get_metric = reinterpret_cast<GetMetric>(GetProcAddress(user, "GetSystemMetricsForDpi"));
    const HWND taskbar = FindWindowW(L"Shell_TrayWnd", nullptr);
    const UINT dpi = get_dpi ? get_dpi(taskbar ? taskbar : window) : 96;
    const int size = get_metric ? get_metric(SM_CXSMICON, dpi ? dpi : 96) : GetSystemMetrics(SM_CXSMICON);
    return std::clamp(size, 16, 64);
}

HICON make_icon(int size, bool buffering) {
    BITMAPINFO info{};
    info.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
    info.bmiHeader.biWidth = size;
    info.bmiHeader.biHeight = -size;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    void* storage = nullptr;
    HBITMAP bitmap = CreateDIBSection(nullptr, &info, DIB_RGB_COLORS, &storage, nullptr, 0);
    if (!bitmap || !storage) {
        const DWORD error = GetLastError();
        if (bitmap) DeleteObject(bitmap);
        SetLastError(error);
        return nullptr;
    }
    auto* pixels = static_cast<DWORD*>(storage);
    const COLORREF outline = icon_color();
    const COLORREF center = buffering ? RGB(220, 45, 57) : outline;
    for (int y = 0; y < size; ++y) {
        for (int x = 0; x < size; ++x) {
            unsigned red = 0, green = 0, blue = 0, covered = 0;
            for (int sy = 0; sy < 4; ++sy) {
                for (int sx = 0; sx < 4; ++sx) {
                    const double dx = (x + (sx + 0.5) / 4.0) / size - 0.5;
                    const double dy = (y + (sy + 0.5) / 4.0) / size - 0.5;
                    const double radius = std::hypot(dx, dy);
                    const bool ring = radius >= 0.35 && radius <= 0.46;
                    const bool dot = buffering ? radius <= 0.235 : radius >= 0.16 && radius <= 0.23;
                    if (!ring && !dot) continue;
                    const COLORREF color = ring ? outline : center;
                    red += GetRValue(color);
                    green += GetGValue(color);
                    blue += GetBValue(color);
                    ++covered;
                }
            }
            // A 32-bit icon uses premultiplied BGRA pixels.
            pixels[y * size + x] = ((covered * 255 / 16) << 24)
                | ((red / 16) << 16) | ((green / 16) << 8) | (blue / 16);
        }
    }
    const std::size_t stride = static_cast<std::size_t>((size + 15) / 16 * 2);
    std::vector<unsigned char> mask_data(stride * static_cast<std::size_t>(size), 0);
    HBITMAP mask = CreateBitmap(size, size, 1, 1, mask_data.data());
    DWORD error = mask ? ERROR_SUCCESS : GetLastError();
    HICON icon = nullptr;
    if (mask) {
        ICONINFO icon_info{};
        icon_info.fIcon = TRUE;
        icon_info.hbmColor = bitmap;
        icon_info.hbmMask = mask;
        icon = CreateIconIndirect(&icon_info);
        if (!icon) error = GetLastError();
        DeleteObject(mask);
    }
    DeleteObject(bitmap);
    if (!icon) SetLastError(error);
    return icon;
}

class Application {
public:
    explicit Application(ActionCallback callback) : callback_(callback) {}

    ~Application() {
        if (window_) DestroyWindow(window_);
        if (icon_) DestroyIcon(icon_);
    }

    bool initialize(const std::wstring& identity) {
        const HINSTANCE instance = GetModuleHandleW(nullptr);
        WNDCLASSW window_class{};
        window_class.lpfnWndProc = window_proc;
        window_class.hInstance = instance;
        window_class.lpszClassName = kWindowClass;
        window_class.hCursor = LoadCursorW(nullptr, IDC_ARROW);
        if (!RegisterClassW(&window_class) && GetLastError() != ERROR_CLASS_ALREADY_EXISTS) {
            return false;
        }
        taskbar_created_ = RegisterWindowMessageW(L"TaskbarCreated");
        if (!taskbar_created_) return false;
        shutdown_message_ = RegisterWindowMessageW(kShutdownMessageName);
        if (!shutdown_message_) return false;
        // An ordinary hidden top-level window receives Explorer/display/power
        // broadcasts. A message-only window would miss those notifications.
        window_ = CreateWindowExW(WS_EX_TOOLWINDOW, kWindowClass, identity.c_str(), WS_POPUP,
            0, 0, 0, 0, nullptr, nullptr, instance, this);
        if (!window_) return false;
        {
            std::lock_guard<std::mutex> guard(window_mutex);
            application_window = window_;
        }
        session_notifications_ = WTSRegisterSessionNotification(window_, NOTIFY_FOR_THIS_SESSION) != FALSE;
        if (!session_notifications_) {
            report_error("ui", "tray.session_notifications_failed",
                L"Could not register session notifications (Windows error " + std::to_wstring(GetLastError()) + L")");
        }
        toggle_hotkey_ = RegisterHotKey(window_, kToggleCommand, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'B') != FALSE;
        if (!toggle_hotkey_) shortcut_failure(L"Ctrl+Alt+B", GetLastError());
        save_hotkey_ = RegisterHotKey(window_, kSaveCommand, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'S') != FALSE;
        if (!save_hotkey_) shortcut_failure(L"Ctrl+Alt+S", GetLastError());
        update_icon();
        return true;
    }

    void run() {
        MSG message{};
        BOOL result = 0;
        while ((result = GetMessageW(&message, nullptr, 0, 0)) > 0) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
            report_pending_status_failure();
        }
        const DWORD error = result == -1 ? GetLastError() : ERROR_SUCCESS;
        report_pending_status_failure();
        if (result == -1) report_error("ui", "tray.message_loop_failed",
            L"Windows message loop failed (Windows error " + std::to_wstring(error) + L")");
    }

private:
#ifdef OPTIC_UI_TEST
    friend class ApplicationTest;
#endif
    ActionCallback callback_;
    HWND window_ = nullptr;
    HICON icon_ = nullptr;
    UINT taskbar_created_ = 0;
    UINT shutdown_message_ = 0;
    std::int32_t state_ = 0;
    std::wstring message_ = L"Stopped";
    std::vector<std::wstring> shortcut_failures_;
    bool tray_added_ = false;
    bool icon_failure_reported_ = false;
    bool tray_failure_reported_ = false;
    bool tray_version_four_ = false;
    bool toggle_hotkey_ = false;
    bool save_hotkey_ = false;
    bool session_notifications_ = false;
    bool quitting_ = false;
    bool awaiting_termination_ = false;
    bool session_ending_ = false;
    bool shutdown_blocked_ = false;
    bool menu_open_ = false;
    HMENU active_menu_ = nullptr;
    HWND menu_window_ = nullptr;

    const wchar_t* toggle_title() const noexcept {
        return state_ == 0 ? L"Start Buffering\tCtrl+Alt+B"
            : state_ == 3 ? L"Stopping\x2026\tCtrl+Alt+B" : L"Stop Buffering\tCtrl+Alt+B";
    }

    void refresh_menu() {
        if (!active_menu_) return;
        const auto status_text = menu_text(message_);
        auto update = [this](UINT item, bool by_position, const wchar_t* text, bool enabled) {
            MENUITEMINFOW info{};
            info.cbSize = sizeof(info);
            info.fMask = MIIM_STATE;
            info.fState = enabled ? MFS_ENABLED : MFS_DISABLED;
            if (text) {
                info.fMask |= MIIM_STRING;
                info.dwTypeData = const_cast<wchar_t*>(text);
            }
            SetMenuItemInfoW(active_menu_, item, by_position ? TRUE : FALSE, &info);
        };
        update(0, true, status_text.c_str(), false);
        update(kToggleCommand, false, toggle_title(),
            !quitting_ && !session_ending_ && (state_ == 0 || state_ == 1 || state_ == 2));
        update(kSaveCommand, false, nullptr, !quitting_ && !session_ending_ && state_ == 2);
        update(kQuitCommand, false, nullptr, !quitting_);
        DrawMenuBar(window_);
        // WM_ENTERIDLE supplies the current popup's HWND. Invalidate it as
        // well as the owner so changes appear during TrackPopupMenu's loop.
        if (menu_window_) RedrawWindow(menu_window_, nullptr, nullptr,
            RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW);
    }

    NOTIFYICONDATAW tray_data() const {
        NOTIFYICONDATAW data{};
        data.cbSize = sizeof(data);
        data.hWnd = window_;
        data.uID = kTrayId;
        data.uCallbackMessage = kTrayMessage;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.hIcon = icon_;
        const std::wstring tip = L"Optic: " + message_;
        const auto length = std::min(tip.size(), std::size(data.szTip) - 1);
        std::copy_n(tip.data(), length, data.szTip);
        data.szTip[length] = L'\0';
        return data;
    }

    void update_icon() {
        refresh_menu();
        HICON replacement = make_icon(tray_icon_size(window_), state_ == 2 && !quitting_);
        if (replacement) {
            icon_failure_reported_ = false;
            if (icon_) DestroyIcon(icon_);
            icon_ = replacement;
        } else if (!icon_failure_reported_) {
            const DWORD error = GetLastError();
            icon_failure_reported_ = true;
            report_error("ui", "tray.icon_failed",
                L"Could not create the tray icon (Windows error " + std::to_wstring(error) + L")");
        }
        auto data = tray_data();
        if (tray_added_) {
            if (Shell_NotifyIconW(NIM_MODIFY, &data)) {
                tray_failure_reported_ = false;
                return;
            }
            tray_added_ = false;
        }
        if (Shell_NotifyIconW(NIM_ADD, &data)) {
            tray_failure_reported_ = false;
            tray_added_ = true;
            data.uVersion = NOTIFYICON_VERSION_4;
            tray_version_four_ = Shell_NotifyIconW(NIM_SETVERSION, &data) != FALSE;
            KillTimer(window_, kTrayRetryTimer);
        } else {
            const DWORD error = GetLastError();
            if (!tray_failure_reported_) {
                tray_failure_reported_ = true;
                report_error("ui", "tray.registration_failed",
                    L"Could not add the tray icon (Windows error " + std::to_wstring(error) + L")");
            }
            SetTimer(window_, kTrayRetryTimer, 2000, nullptr);
        }
    }

    void shortcut_failure(const wchar_t* shortcut, DWORD error) {
        const std::wstring text = L"Shortcut unavailable: " + std::wstring(shortcut)
            + L" (error " + std::to_wstring(error) + L")";
        shortcut_failures_.insert(shortcut_failures_.begin(), text);
        report_error("shortcuts", "shortcut.registration_failed", text);
    }

    void set_status(const Status& status) {
        if (quitting_) return;
        if (shutdown_blocked_ && callback_(4)) {
            ShutdownBlockReasonDestroy(window_);
            shutdown_blocked_ = false;
        }
        state_ = status.state;
        message_ = status.message.empty() ? state_name(state_) : status.message;
        update_icon();
    }

    void toggle() {
        if (quitting_ || session_ending_ || (state_ != 0 && state_ != 1 && state_ != 2)) return;
        state_ = state_ == 0 ? 1 : 3;
        message_ = state_name(state_);
        update_icon();
        callback_(1);
    }

    void save() {
        if (!quitting_ && !session_ending_ && state_ == 2) callback_(2);
    }

    void quit() {
        if (quitting_) return;
        quitting_ = true;
        if (callback_(3)) {
            DestroyWindow(window_);
        } else {
            awaiting_termination_ = true;
            message_ = L"Finishing saved clip\x2026";
            update_icon();
        }
    }

    bool query_end_session() {
        // A query may be canceled by another app. Do not clear the replay
        // buffer or request quit until Windows commits with WM_ENDSESSION.
        if (awaiting_termination_ || !callback_(4)) {
            shutdown_blocked_ = ShutdownBlockReasonCreate(window_,
                L"Optic is finishing a saved clip.") != FALSE;
            return false;
        }
        session_ending_ = true;
        refresh_menu();
        return true;
    }

    void end_session(bool ending) {
        if (shutdown_blocked_) {
            ShutdownBlockReasonDestroy(window_);
            shutdown_blocked_ = false;
        }
        session_ending_ = false;
        if (ending) quit();
        else refresh_menu();
    }

    void open_folder() {
        try {
            const auto folder = optic::recordings_folder();
            const int created = SHCreateDirectoryExW(window_, folder.c_str(), nullptr);
            if (created != ERROR_SUCCESS && created != ERROR_ALREADY_EXISTS && created != ERROR_FILE_EXISTS) {
                report_error("folders", "recordings_folder.create_failed",
                    L"Could not create recordings folder (Windows error " + std::to_wstring(created) + L")");
                return;
            }
            const auto result = reinterpret_cast<INT_PTR>(ShellExecuteW(window_, L"open", folder.c_str(),
                nullptr, nullptr, SW_SHOWNORMAL));
            if (result <= 32) report_error("folders", "recordings_folder.open_failed",
                L"Could not open recordings folder (ShellExecute error " + std::to_wstring(result) + L")");
        } catch (const std::exception& error) {
            report_error("folders", "recordings_folder.open_failed",
                L"Could not open recordings folder: " + from_utf8(error.what()));
        }
    }

    void command(UINT command) {
        switch (command) {
            case kToggleCommand: toggle(); break;
            case kSaveCommand: save(); break;
            case kFolderCommand: open_folder(); break;
            case kQuitCommand: quit(); break;
            default: break;
        }
    }

    HMENU create_menu() const {
        const auto status_text = menu_text(message_);
        HMENU menu = CreatePopupMenu();
        if (!menu) return nullptr;
        AppendMenuW(menu, MF_STRING | MF_DISABLED, 0, status_text.c_str());
        for (const auto& failure : shortcut_failures_) {
            AppendMenuW(menu, MF_STRING | MF_DISABLED, 0, failure.c_str());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, nullptr);
        const bool can_toggle = !quitting_ && !session_ending_ && (state_ == 0 || state_ == 1 || state_ == 2);
        AppendMenuW(menu, MF_STRING | (can_toggle ? MF_ENABLED : MF_DISABLED), kToggleCommand, toggle_title());
        AppendMenuW(menu, MF_STRING | (!quitting_ && !session_ending_ && state_ == 2 ? MF_ENABLED : MF_DISABLED),
            kSaveCommand, L"Save Replay\tCtrl+Alt+S");
        AppendMenuW(menu, MF_SEPARATOR, 0, nullptr);
        AppendMenuW(menu, MF_STRING, kFolderCommand, L"Open Recordings Folder");
        AppendMenuW(menu, MF_SEPARATOR, 0, nullptr);
        AppendMenuW(menu, MF_STRING | (quitting_ ? MF_DISABLED : MF_ENABLED), kQuitCommand, L"Quit Optic");
        return menu;
    }

    void show_menu(POINT point) {
        if (menu_open_) return;
        HMENU menu = create_menu();
        if (!menu) return;
        UINT selected = 0;
        {
            struct MenuScope {
                Application& app;
                HMENU menu;
                ~MenuScope() {
                    app.active_menu_ = nullptr;
                    app.menu_window_ = nullptr;
                    app.menu_open_ = false;
                    DestroyMenu(menu);
                }
            } scope{*this, menu};
            // Foreground ownership ensures clicking elsewhere dismisses the menu.
            SetForegroundWindow(window_);
            menu_open_ = true;
            active_menu_ = menu;
            selected = static_cast<UINT>(TrackPopupMenu(menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, point.x, point.y, 0, window_, nullptr));
        }
        // A quit message can destroy the owner inside the nested menu loop.
        if (!window_) return;
        PostMessageW(window_, WM_NULL, 0, 0);
        auto data = tray_data();
        Shell_NotifyIconW(NIM_SETFOCUS, &data);
        command(selected);
    }

    void destroy() noexcept {
        if (shutdown_blocked_) ShutdownBlockReasonDestroy(window_);
        if (active_menu_) EndMenu();
        {
            std::lock_guard<std::mutex> guard(window_mutex);
            if (application_window == window_) application_window = nullptr;
        }
        // After clearing the published HWND, no producer can enqueue a new
        // payload. Free statuses already queued before GetMessage sees WM_QUIT.
        MSG pending{};
        while (PeekMessageW(&pending, window_, kStatusMessage, kStatusMessage, PM_REMOVE)) {
            delete reinterpret_cast<Status*>(pending.lParam);
        }
        if (session_notifications_) WTSUnRegisterSessionNotification(window_);
        if (toggle_hotkey_) UnregisterHotKey(window_, kToggleCommand);
        if (save_hotkey_) UnregisterHotKey(window_, kSaveCommand);
        KillTimer(window_, kTrayRetryTimer);
        if (tray_added_) {
            NOTIFYICONDATAW data{};
            data.cbSize = sizeof(data);
            data.hWnd = window_;
            data.uID = kTrayId;
            Shell_NotifyIconW(NIM_DELETE, &data);
            tray_added_ = false;
        }
        window_ = nullptr;
        PostQuitMessage(0);
    }

    static LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM wparam, LPARAM lparam) noexcept {
        auto* app = reinterpret_cast<Application*>(GetWindowLongPtrW(window, GWLP_USERDATA));
        if (message == WM_NCCREATE) {
            auto* create = reinterpret_cast<CREATESTRUCTW*>(lparam);
            app = static_cast<Application*>(create->lpCreateParams);
            SetWindowLongPtrW(window, GWLP_USERDATA, reinterpret_cast<LONG_PTR>(app));
        }
        if (!app) return DefWindowProcW(window, message, wparam, lparam);
        try {
            if (app->shutdown_message_ && message == app->shutdown_message_) {
                app->quit();
                return 0;
            }
            if (app->taskbar_created_ && message == app->taskbar_created_) {
                app->tray_added_ = false;
                app->update_icon();
                return 0;
            }
            switch (message) {
#ifdef OPTIC_UI_TEST
                case kTestInvocationMessage: {
                    auto* invocation = reinterpret_cast<TestInvocation*>(lparam);
                    try {
                        invocation->action(*app);
                    } catch (...) {
                        invocation->failure = std::current_exception();
                    }
                    SetEvent(invocation->complete);
                    return 0;
                }
#endif
                case kStatusMessage: {
                    std::unique_ptr<Status> status(reinterpret_cast<Status*>(lparam));
                    if (status) app->set_status(*status);
                    return 0;
                }
                case kStatusErrorMessage:
                    return 0; // The outer message loop reports the pending error.
                case kFinishQuitMessage:
                    if (app->awaiting_termination_) DestroyWindow(window);
                    return 0;
                case kTrayMessage: {
                    const UINT event = app->tray_version_four_ ? LOWORD(lparam) : static_cast<UINT>(lparam);
                    if (event == WM_CONTEXTMENU || event == NIN_SELECT || event == NIN_KEYSELECT
                        || (!app->tray_version_four_ && (event == WM_RBUTTONUP || event == WM_LBUTTONUP))) {
                        POINT point{};
                        if (app->tray_version_four_) {
                            point.x = GET_X_LPARAM(wparam);
                            point.y = GET_Y_LPARAM(wparam);
                        }
                        if (!app->tray_version_four_ || (point.x == -1 && point.y == -1)) GetCursorPos(&point);
                        app->show_menu(point);
                    }
                    return 0;
                }
                case WM_HOTKEY:
                    app->command(static_cast<UINT>(wparam));
                    return 0;
                case WM_COMMAND:
                    app->command(LOWORD(wparam));
                    return 0;
                case WM_ENTERIDLE:
                    if (wparam == MSGF_MENU && app->active_menu_) {
                        app->menu_window_ = reinterpret_cast<HWND>(lparam);
                    }
                    return 0;
                case WM_CLOSE:
                    app->quit();
                    return 0;
                case WM_QUERYENDSESSION:
                    return app->query_end_session() ? TRUE : FALSE;
                case WM_ENDSESSION:
                    app->end_session(wparam != FALSE);
                    return 0;
                case WM_POWERBROADCAST:
                    if (wparam == PBT_APMSUSPEND) optic_capture_interrupt();
                    return TRUE;
                case WM_WTSSESSION_CHANGE:
                    if (wparam == WTS_SESSION_LOCK || wparam == WTS_CONSOLE_DISCONNECT
                        || wparam == WTS_REMOTE_DISCONNECT) optic_capture_interrupt();
                    return 0;
                case WM_DISPLAYCHANGE:
                    optic_capture_interrupt();
                    app->update_icon();
                    return 0;
                case WM_DPICHANGED:
                case WM_SETTINGCHANGE:
                case WM_THEMECHANGED:
                    app->update_icon();
                    return 0;
                case WM_TIMER:
                    if (wparam == kTrayRetryTimer) app->update_icon();
                    return 0;
                case WM_DESTROY:
                    app->destroy();
                    return 0;
                case WM_NCDESTROY:
                    SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                    break;
                default: break;
            }
        } catch (const std::exception& error) {
            report_error("ui", "tray.operation_failed", L"Tray operation failed: " + from_utf8(error.what()));
        } catch (...) {
            report_error("ui", "tray.operation_failed", L"Tray operation failed");
        }
        return DefWindowProcW(window, message, wparam, lparam);
    }
};
}

namespace optic {
std::wstring recordings_folder() {
    PWSTR path = nullptr;
    const HRESULT result = SHGetKnownFolderPath(FOLDERID_Videos, KF_FLAG_DEFAULT, nullptr, &path);
    if (FAILED(result)) {
        if (path) CoTaskMemFree(path);
        throw std::runtime_error("Could not locate your Videos folder (error "
            + std::to_string(static_cast<unsigned long>(result)) + ")");
    }
    std::wstring folder;
    try {
        folder.assign(path);
    } catch (...) {
        CoTaskMemFree(path);
        throw;
    }
    CoTaskMemFree(path);
    if (!folder.empty() && folder.back() != L'\\') folder.push_back(L'\\');
    folder += L"Optic";
    return folder;
}
}

extern "C" bool optic_recordings_folder(wchar_t* path, std::size_t capacity,
    char* error, std::size_t error_capacity) {
    if (path && capacity) path[0] = L'\0';
    optic::copy_error("", error, error_capacity);
    try {
        const auto folder = optic::recordings_folder();
        if (!path || folder.size() >= capacity) {
            optic::copy_error("Recordings folder path buffer is too small", error, error_capacity);
            return false;
        }
        std::copy(folder.begin(), folder.end(), path);
        path[folder.size()] = L'\0';
        return true;
    } catch (const std::exception& exception) {
        optic::copy_error(exception.what(), error, error_capacity);
    } catch (...) {
        optic::copy_error("Could not locate your Videos folder", error, error_capacity);
    }
    return false;
}

extern "C" void optic_run(ActionCallback callback) {
    if (!callback) return;
    const HRESULT com = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    try {
        if (FAILED(com)) throw std::runtime_error("Could not initialize the tray COM apartment (HRESULT "
            + std::to_string(static_cast<unsigned long>(com)) + ")");
        // Setup/uninstall keeps this lease throughout executable replacement.
        // Take it before the instance lock so a launch cannot race shutdown.
        MutexLease maintenance(kMaintenanceMutex);
        const bool can_launch = maintenance.acquire();
        const auto identity = instance_name();
        MutexLease instance(identity.c_str());
        if (!can_launch) {
            write_diagnostic(L"Installation or removal is in progress; try launching Optic again when it finishes.");
        } else if (!instance.acquire()) {
            write_diagnostic(L"Optic is already running in this Windows session.");
        } else {
            Application application(callback);
            if (application.initialize(identity)) {
                maintenance.release();
                instance.hold_until_process_exit();
                application.run();
            } else {
                const std::wstring message = L"Could not start the Optic tray (error "
                    + std::to_wstring(GetLastError()) + L")";
                report_error("ui", "tray.initialization_failed", message);
                MessageBoxW(nullptr, message.c_str(), L"Optic", MB_OK | MB_ICONERROR);
            }
        }
    } catch (const std::exception& error) {
        report_error("ui", "tray.initialization_failed", L"Could not start the Optic tray: " + from_utf8(error.what()));
    } catch (...) {
        report_error("ui", "tray.initialization_failed", L"Could not start the Optic tray");
    }
    if (SUCCEEDED(com)) CoUninitialize();
}

extern "C" void optic_set_error_callback(ErrorCallback callback) {
    error_callback.store(callback);
}

extern "C" bool optic_shutdown(std::uint32_t timeout_ms, char* error, std::size_t error_capacity) {
    optic::copy_error("", error, error_capacity);
    try {
        if (!timeout_ms || timeout_ms > 600000) throw std::runtime_error("Shutdown timeout must be between 1 and 600000 milliseconds");
        const auto identity = instance_name();
        const auto sid = user_sid(GetCurrentProcess());
        Handle instance(OpenMutexW(SYNCHRONIZE | MUTEX_MODIFY_STATE, FALSE, identity.c_str()));
        if (!instance.get()) {
            if (GetLastError() != ERROR_FILE_NOT_FOUND) throw windows_error("Could not contact the running Optic instance");
        }
        const ULONGLONG started = GetTickCount64();
        auto remaining = [started, timeout_ms]() {
            const ULONGLONG elapsed = GetTickCount64() - started;
            return elapsed >= timeout_ms ? DWORD{0} : static_cast<DWORD>(timeout_ms - elapsed);
        };
        DWORD own_session = 0;
        if (!ProcessIdToSessionId(GetCurrentProcessId(), &own_session)) throw windows_error("Could not identify this Windows session");
        const UINT shutdown_message = RegisterWindowMessageW(kShutdownMessageName);
        if (!shutdown_message) throw windows_error("Could not prepare the Optic shutdown request");
        while (instance.get()) {
            const DWORD lock_result = WaitForSingleObject(instance.get(), 0);
            if (lock_result == WAIT_OBJECT_0 || lock_result == WAIT_ABANDONED) {
                ReleaseMutex(instance.get());
                break;
            }
            if (lock_result != WAIT_TIMEOUT) throw windows_error("Could not check the running Optic instance");
            const HWND window = FindWindowW(kWindowClass, identity.c_str());
            if (window) {
                if (!shutdown_window(window, shutdown_message, sid, own_session, remaining())) {
                    throw std::runtime_error("The Optic shutdown endpoint belongs to another user or session");
                }
                break;
            }
            // A launch holds the instance lock before publishing its window.
            // Cover that interval without treating a still-starting app as absent.
            const DWORD budget = remaining();
            if (!budget) throw std::runtime_error("Timed out waiting for the running Optic instance to respond; the process was left running");
            Sleep(std::min(DWORD{25}, budget));
        }
        // Portable releases before this protocol have no instance mutex. They
        // still publish the old tray class/title and already defer WM_CLOSE
        // until a pending save completes. Close every such instance for this
        // user before the installer replaces or launches the app.
        HWND previous = nullptr;
        while (const HWND legacy = FindWindowExW(nullptr, previous, kWindowClass, kLegacyWindowTitle)) {
            const bool closed = shutdown_window(legacy, WM_CLOSE, sid, own_session, remaining());
            previous = closed ? nullptr : legacy;
        }
        return true;
    } catch (const std::exception& exception) {
        optic::copy_error(exception.what(), error, error_capacity);
    } catch (...) {
        optic::copy_error("Could not shut down Optic", error, error_capacity);
    }
    return false;
}

extern "C" void optic_set_status(std::int32_t state, const char* message) {
    try {
        auto status = std::make_unique<Status>(Status{state, from_utf8(message)});
        std::lock_guard<std::mutex> guard(window_mutex);
        if (application_window) {
#ifdef OPTIC_UI_TEST
            const bool synthetic_failure = fail_next_status_post.exchange(false);
            if (synthetic_failure) SetLastError(ERROR_NOT_ENOUGH_QUOTA);
#else
            constexpr bool synthetic_failure = false;
#endif
            if (!synthetic_failure && PostMessageW(application_window, kStatusMessage,
                0, reinterpret_cast<LPARAM>(status.get()))) {
                status.release();
            } else {
                queue_status_failure(GetLastError());
            }
        }
    } catch (...) {
        try {
            std::lock_guard<std::mutex> guard(window_mutex);
            queue_status_failure(ERROR_NOT_ENOUGH_MEMORY);
        } catch (...) {
            OutputDebugStringW(L"[optic] Could not post status update\n");
        }
    }
}

extern "C" void optic_finish_quit() {
    std::lock_guard<std::mutex> guard(window_mutex);
    if (application_window) PostMessageW(application_window, kFinishQuitMessage, 0, 0);
}

extern "C" void optic_prepare_console() {
    // GUI-subsystem binaries have no console on ordinary launch. For CLI use,
    // attach to the caller's console without opening another window, preserving
    // stdout/stderr/stdin already supplied by a pipeline or redirection.
    const DWORD identifiers[] = {STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE};
    HANDLE inherited[3]{};
    for (std::size_t index = 0; index < std::size(identifiers); ++index) {
        HANDLE handle = GetStdHandle(identifiers[index]);
        if (!handle || handle == INVALID_HANDLE_VALUE) continue;
        SetLastError(ERROR_SUCCESS);
        const DWORD type = GetFileType(handle);
        if (type != FILE_TYPE_UNKNOWN || GetLastError() == ERROR_SUCCESS) inherited[index] = handle;
    }
    if (!AttachConsole(ATTACH_PARENT_PROCESS)) return;
    for (std::size_t index = 0; index < std::size(identifiers); ++index) {
        if (inherited[index]) SetStdHandle(identifiers[index], inherited[index]);
    }
}
