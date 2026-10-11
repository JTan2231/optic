//! Windows tray, per-session instance lifecycle, and shell integration.
//!
//! The window procedure owns no mutable reference across a Win32 call: popup
//! menus and window destruction can dispatch another message synchronously.

#![allow(
    clippy::as_conversions,
    reason = "Win32 stores pointers and bounded message values in integer ABI slots"
)]

use std::{
    cell::{Cell, RefCell},
    ffi::{OsStr, OsString},
    mem::size_of,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    ptr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use windows::{
    core::{w, PCSTR, PCWSTR},
    Win32::{
        Foundation::{
            CloseHandle, GetLastError, LocalFree, SetLastError, ERROR_ALREADY_EXISTS,
            ERROR_CLASS_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE,
            ERROR_INVALID_PARAMETER, ERROR_SUCCESS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
            WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
        },
        Graphics::Gdi::{
            CreateBitmap, CreateDIBSection, DeleteObject, GetSysColor, RedrawWindow, BITMAPINFO,
            BITMAPINFOHEADER, BI_RGB, COLOR_WINDOWTEXT, DIB_RGB_COLORS, RDW_ERASE, RDW_INVALIDATE,
            RDW_UPDATENOW,
        },
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_QUERY,
            TOKEN_USER,
        },
        Storage::FileSystem::{GetFileType, WriteFile, FILE_TYPE_UNKNOWN},
        System::{
            Com::{
                CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
                COINIT_DISABLE_OLE1DDE,
            },
            Console::{
                AttachConsole, GetConsoleMode, GetStdHandle, SetStdHandle, WriteConsoleW,
                ATTACH_PARENT_PROCESS, CONSOLE_MODE, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
                STD_OUTPUT_HANDLE,
            },
            Diagnostics::Debug::OutputDebugStringW,
            LibraryLoader::{GetModuleHandleW, GetProcAddress},
            Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD},
            RemoteDesktop::{
                ProcessIdToSessionId, WTSRegisterSessionNotification,
                WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
            },
            Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy},
            Threading::{
                CreateMutexW, GetCurrentProcess, GetCurrentProcessId, OpenMutexW, OpenProcess,
                OpenProcessToken, ReleaseMutex, WaitForSingleObject, MUTEX_MODIFY_STATE,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
                SYNCHRONIZATION_SYNCHRONIZE,
            },
        },
        UI::{
            Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
            Input::KeyboardAndMouse::{
                RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
            },
            Shell::{
                FOLDERID_Videos, SHCreateDirectoryExW, SHGetKnownFolderPath, ShellExecuteW,
                Shell_NotifyIconW, KF_FLAG_DEFAULT, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP,
                NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETFOCUS, NIM_SETVERSION, NINF_KEY,
                NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
                DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, DrawMenuBar, EndMenu,
                FindWindowExW, FindWindowW, GetCursorPos, GetMessageW, GetSystemMetrics,
                GetWindowLongPtrW, GetWindowThreadProcessId, KillTimer, LoadCursorW, MessageBoxW,
                PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassW,
                RegisterWindowMessageW, SetForegroundWindow, SetMenuItemInfoW, SetTimer,
                SetWindowLongPtrW, SystemParametersInfoW, TrackPopupMenu, TranslateMessage,
                CREATESTRUCTW, GWLP_USERDATA, HICON, HMENU, ICONINFO, IDC_ARROW, MB_ICONERROR,
                MB_OK, MENUITEMINFOW, MFS_DISABLED, MFS_ENABLED, MF_DISABLED, MF_ENABLED,
                MF_SEPARATOR, MF_STRING, MIIM_STATE, MIIM_STRING, MSG, MSGF_MENU, PBT_APMSUSPEND,
                PM_REMOVE, SM_CXSMICON, SPI_GETHIGHCONTRAST, SW_SHOWNORMAL,
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
                WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE,
                WM_DPICHANGED, WM_ENDSESSION, WM_ENTERIDLE, WM_HOTKEY, WM_LBUTTONUP, WM_NCCREATE,
                WM_NCDESTROY, WM_NULL, WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_RBUTTONUP,
                WM_SETTINGCHANGE, WM_THEMECHANGED, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW,
                WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
};

#[cfg(not(test))]
const WINDOW_CLASS: PCWSTR = w!("Optic.TrayWindow");
#[cfg(test)]
const WINDOW_CLASS: PCWSTR = w!("Optic.Test.TrayWindow");
#[cfg(not(test))]
const SHUTDOWN_MESSAGE_NAME: PCWSTR = w!("Optic.Shutdown.v1");
#[cfg(test)]
const SHUTDOWN_MESSAGE_NAME: PCWSTR = w!("Optic.Test.Shutdown.v1");
#[cfg(not(test))]
const INSTANCE_PREFIX: &str = "Local\\Optic.Instance.";
#[cfg(test)]
const INSTANCE_PREFIX: &str = "Local\\Optic.Test.Instance.";
#[cfg(not(test))]
const MAINTENANCE_MUTEX: PCWSTR = w!("Local\\Optic.Maintenance.v1");
#[cfg(test)]
const MAINTENANCE_MUTEX: PCWSTR = w!("Local\\Optic.Test.Maintenance.v1");
#[cfg(not(test))]
const LEGACY_TITLE: PCWSTR = w!("Optic");
#[cfg(test)]
const LEGACY_TITLE: PCWSTR = w!("Optic.Test.Legacy");
const TRAY_MESSAGE: u32 = WM_APP + 1;
const STATUS_MESSAGE: u32 = WM_APP + 2;
const FINISH_QUIT_MESSAGE: u32 = WM_APP + 3;
#[cfg(test)]
const TEST_INVOCATION_MESSAGE: u32 = WM_APP + 4;
const STATUS_ERROR_MESSAGE: u32 = WM_APP + 5;
const TRAY_RETRY_TIMER: usize = 1;
const TOGGLE_COMMAND: u32 = 1;
const SAVE_COMMAND: u32 = 2;
const QUIT_COMMAND: u32 = 3;
const FOLDER_COMMAND: u32 = 4;
const TRAY_ID: u32 = 1;

// HWND is used only as a message destination on producer threads. Keeping the
// integer under a mutex makes publication and status-payload cleanup atomic.
static WINDOW: Mutex<usize> = Mutex::new(0);
static PENDING_STATUS_ERROR: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
static FAIL_NEXT_STATUS_POST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
type ErrorObserver = fn(&str, &str, &str);
#[cfg(test)]
static ERROR_OBSERVER: Mutex<Option<ErrorObserver>> = Mutex::new(None);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(path: &OsStr) -> Vec<u16> {
    path.encode_wide().chain(std::iter::once(0)).collect()
}

fn structure_size<T>() -> u32 {
    u32::try_from(size_of::<T>()).unwrap_or(u32::MAX)
}

fn last_error(operation: &str) -> String {
    // SAFETY: GetLastError reads the calling thread's error value.
    let error = unsafe { GetLastError() };
    format!("{operation} (Windows error {})", error.0)
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: This owner closes its valid kernel handle exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct MutexLease {
    handle: OwnedHandle,
    owned: bool,
}

impl MutexLease {
    fn new(name: PCWSTR) -> Result<Self, String> {
        // SAFETY: The name is a live NUL-terminated string; default security and
        // a non-owned mutex are requested, so no borrowed pointer is retained.
        let handle = unsafe { CreateMutexW(None, false, name) }
            .map_err(|_| last_error("Could not open the Optic launch lock"))?;
        Ok(Self {
            handle: OwnedHandle(handle),
            owned: false,
        })
    }

    fn acquire(&mut self) -> Result<bool, String> {
        // SAFETY: The lease owns a valid mutex handle. This wait is nonblocking.
        let result = unsafe { WaitForSingleObject(self.handle.0, 0) };
        if result == WAIT_TIMEOUT {
            return Ok(false);
        }
        if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
            return Err(last_error("Could not lock the Optic launch"));
        }
        self.owned = true;
        Ok(true)
    }

    fn release(&mut self) {
        if self.owned {
            // SAFETY: acquire established that this thread owns the mutex.
            unsafe {
                let _ = ReleaseMutex(self.handle.0);
            }
            self.owned = false;
        }
    }

    const fn hold_until_process_exit(mut self) {
        // Windows releases this owning thread's mutex at process exit. Window
        // teardown happens before COM/Rust unwind and executable replacement is
        // safe only after that entire process exits.
        self.owned = false;
        std::mem::forget(self);
    }
}

impl Drop for MutexLease {
    fn drop(&mut self) {
        self.release();
    }
}

fn user_sid(process: HANDLE) -> Result<String, String> {
    let mut token = HANDLE::default();
    // SAFETY: process is a live process handle and token points to writable
    // storage for an owned token handle.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }
        .map_err(|_| last_error("Could not identify the Optic user"))?;
    let token = OwnedHandle(token);
    let mut bytes = 0;
    // SAFETY: An empty first query obtains the token information buffer length.
    unsafe {
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &raw mut bytes);
    }
    if bytes == 0 {
        return Err(last_error("Could not read the Optic user"));
    }
    // usize storage provides the alignment required by TOKEN_USER.
    let mut data = vec![0_usize; (bytes as usize).div_ceil(size_of::<usize>())];
    // SAFETY: The buffer is aligned and has at least bytes writable bytes; the
    // owned token remains alive through both API calls.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(data.as_mut_ptr().cast()),
            bytes,
            &raw mut bytes,
        )
    }
    .map_err(|_| last_error("Could not read the Optic user"))?;
    // SAFETY: A successful TokenUser query initialized a TOKEN_USER in data.
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = windows::core::PWSTR::null();
    // SAFETY: TokenUser supplies a valid SID; the API allocates the output.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut sid) }
        .map_err(|_| last_error("Could not identify the Optic user"))?;
    // SAFETY: The successful conversion returns a NUL-terminated UTF-16 string.
    let result = unsafe { sid.to_string() }.map_err(|_| "Invalid Optic user SID".to_owned());
    // SAFETY: ConvertSidToStringSidW allocated this pointer using LocalAlloc.
    unsafe {
        let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(sid.0.cast())));
    }
    result
}

fn instance_name() -> Result<Vec<u16>, String> {
    // SAFETY: The pseudo-handle is valid throughout the current process.
    let sid = user_sid(unsafe { GetCurrentProcess() })?;
    Ok(wide(&(INSTANCE_PREFIX.to_owned() + &sid)))
}

fn diagnostic(text: &str) {
    let line = format!("[optic] {text}\n");
    let utf16 = wide(&line);
    // SAFETY: The wide string is NUL-terminated and borrowed only for this call.
    unsafe { OutputDebugStringW(PCWSTR(utf16.as_ptr())) };
    // SAFETY: This reads the process's existing standard error handle.
    let Ok(handle) = (unsafe { GetStdHandle(STD_ERROR_HANDLE) }) else {
        return;
    };
    if handle.is_invalid() {
        return;
    }
    let mut mode = CONSOLE_MODE::default();
    let mut written = 0;
    // SAFETY: Both APIs borrow the valid output handle and writable counters;
    // the buffers stay alive until their synchronous writes finish.
    unsafe {
        if GetConsoleMode(handle, &raw mut mode).is_ok() {
            let _ = WriteConsoleW(
                handle,
                &utf16[..utf16.len() - 1],
                Some(&raw mut written),
                None,
            );
        } else {
            let _ = WriteFile(handle, Some(line.as_bytes()), Some(&raw mut written), None);
        }
    }
}

fn report_error(component: &str, event: &str, message: &str) {
    // Never called from set_status or while WINDOW is locked: action callbacks
    // can still hold the Rust recorder lock when posting a status.
    let result = std::panic::catch_unwind(|| {
        #[cfg(test)]
        {
            let observer = *ERROR_OBSERVER
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(observer) = observer {
                observer(component, event, message);
            }
        }
        crate::logging::error(component, event, message, None);
    });
    if result.is_err() {
        diagnostic(message);
    }
}

fn queue_status_failure(window: HWND, error: u32) {
    let error = if error == 0 {
        ERROR_GEN_FAILURE.0
    } else {
        error
    };
    if PENDING_STATUS_ERROR
        .compare_exchange(0, error, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        // SAFETY: WINDOW's guard prevents destruction while posting. Only a
        // bounded integer is retained; logging is deferred to the outer loop.
        unsafe {
            let _ = PostMessageW(Some(window), STATUS_ERROR_MESSAGE, WPARAM(0), LPARAM(0));
        }
    }
}

fn report_pending_status_failure() {
    let error = PENDING_STATUS_ERROR.swap(0, Ordering::AcqRel);
    if error != 0 {
        report_error(
            "ui",
            "tray.status_update_failed",
            &format!("Could not post status update (Windows error {error})"),
        );
    }
}

struct Status {
    state: i32,
    message: String,
}

/// Copies a status and posts it to the window thread without entering logging.
#[allow(
    clippy::significant_drop_tightening,
    reason = "Publication must stay locked until posting completes to prevent window teardown races"
)]
pub fn set_status(state: i32, message: &str) {
    let status = Box::new(Status {
        state,
        message: message.to_owned(),
    });
    let published = WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *published == 0 {
        return;
    }
    let window = HWND(*published as *mut _);
    #[cfg(test)]
    if FAIL_NEXT_STATUS_POST.swap(false, Ordering::AcqRel) {
        queue_status_failure(window, windows::Win32::Foundation::ERROR_NOT_ENOUGH_QUOTA.0);
        return;
    }
    let payload = Box::into_raw(status);
    // SAFETY: The mutex prevents window teardown while posting. Ownership of
    // payload passes to the UI only if posting succeeds.
    let result = unsafe {
        PostMessageW(
            Some(window),
            STATUS_MESSAGE,
            WPARAM(0),
            LPARAM(payload as isize),
        )
    };
    if result.is_err() {
        // SAFETY: Capture the failed post's thread-local code before dropping
        // its allocation, whose allocator can itself call Windows APIs.
        let error = unsafe { GetLastError() }.0;
        // SAFETY: A failed post did not transfer this Box to another thread.
        unsafe { drop(Box::from_raw(payload)) };
        queue_status_failure(window, error);
    }
}

/// Completes a previously deferred quit after its save worker finishes.
pub fn finish_quit() {
    let published = WINDOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *published != 0 {
        // SAFETY: The publication lock keeps the destination valid while
        // posting; this message owns no payload.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(*published as *mut _)),
                FINISH_QUIT_MESSAGE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

const fn state_name(state: i32) -> &'static str {
    match state {
        1 => "Starting…",
        2 => "Buffering",
        3 => "Stopping…",
        _ => "Stopped",
    }
}

fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

fn icon_color() -> u32 {
    let mut contrast = HIGHCONTRASTW {
        cbSize: structure_size::<HIGHCONTRASTW>(),
        ..Default::default()
    };
    // SAFETY: SystemParametersInfoW receives a writable structure of its
    // declared size; GetSysColor returns a process-independent color value.
    unsafe {
        if SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            Some((&raw mut contrast).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
            && contrast.dwFlags.contains(HCF_HIGHCONTRASTON)
        {
            return GetSysColor(COLOR_WINDOWTEXT);
        }
    }
    let mut light = 0_u32;
    let mut bytes = structure_size::<u32>();
    // SAFETY: The registry only writes a DWORD into the matching four-byte
    // buffer, and all key/value strings are static NUL-terminated UTF-16.
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut light).cast()),
            Some(&raw mut bytes),
        );
    }
    if light != 0 {
        0x001e_1e1e
    } else {
        0x00f5_f5f5
    }
}

fn tray_icon_size(window: HWND) -> i32 {
    type GetDpi = unsafe extern "system" fn(HWND) -> u32;
    type GetMetric = unsafe extern "system" fn(i32, u32) -> i32;
    // SAFETY: user32 is already loaded; optional symbol addresses are cast only
    // to their documented signatures. Fallbacks support older Windows 10.
    unsafe {
        let user = GetModuleHandleW(w!("user32.dll")).unwrap_or_default();
        let dpi_fn: Option<GetDpi> = std::mem::transmute(GetProcAddress(
            user,
            PCSTR(c"GetDpiForWindow".as_ptr().cast()),
        ));
        let metric_fn: Option<GetMetric> = std::mem::transmute(GetProcAddress(
            user,
            PCSTR(c"GetSystemMetricsForDpi".as_ptr().cast()),
        ));
        let taskbar = FindWindowW(w!("Shell_TrayWnd"), None).unwrap_or_default();
        let dpi = dpi_fn.map_or(96, |get_dpi| {
            get_dpi(if taskbar.is_invalid() {
                window
            } else {
                taskbar
            })
        });
        let dpi = if dpi == 0 { 96 } else { dpi };
        metric_fn
            .map_or_else(
                || GetSystemMetrics(SM_CXSMICON),
                |get_metric| get_metric(SM_CXSMICON.0, dpi),
            )
            .clamp(16, 64)
    }
}

fn make_icon(size: i32, buffering: bool) -> Result<HICON, String> {
    let pixels_per_row = usize::try_from(size).map_err(|_| "Invalid tray icon dimensions")?;
    if !(16..=64).contains(&pixels_per_row) {
        return Err("Invalid tray icon dimensions".to_owned());
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: structure_size::<BITMAPINFOHEADER>(),
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut storage = ptr::null_mut();
    // SAFETY: The bitmap description has positive bounded dimensions and
    // requests a writable 32-bit DIB. No mapping handle is supplied.
    let bitmap = unsafe {
        CreateDIBSection(
            None,
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut storage,
            None,
            0,
        )
    }
    .map_err(|_| last_error("Could not create the tray icon"))?;
    if storage.is_null() {
        // SAFETY: bitmap was just returned by CreateDIBSection.
        unsafe {
            let _ = DeleteObject(bitmap.into());
        }
        return Err("Could not create the tray icon (empty bitmap storage)".to_owned());
    }
    let outline = icon_color();
    // COLORREF is RGB in low-to-high bytes.
    let center = if buffering { 0x0039_2ddc } else { outline };
    // SAFETY: CreateDIBSection returned size*size writable BGRA pixels.
    let pixels = unsafe {
        std::slice::from_raw_parts_mut(storage.cast::<u32>(), pixels_per_row * pixels_per_row)
    };
    for y in 0..pixels_per_row {
        for x in 0..pixels_per_row {
            let (mut red, mut green, mut blue, mut covered) = (0_u32, 0_u32, 0_u32, 0_u32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let dx = (f64::from(u32::try_from(x).unwrap_or_default())
                        + (f64::from(sx) + 0.5) / 4.0)
                        / f64::from(size)
                        - 0.5;
                    let dy = (f64::from(u32::try_from(y).unwrap_or_default())
                        + (f64::from(sy) + 0.5) / 4.0)
                        / f64::from(size)
                        - 0.5;
                    let radius = dx.hypot(dy);
                    let ring = (0.35..=0.46).contains(&radius);
                    let dot = if buffering {
                        radius <= 0.235
                    } else {
                        (0.16..=0.23).contains(&radius)
                    };
                    if ring || dot {
                        let color = if ring { outline } else { center };
                        red += color & 0xff;
                        green += (color >> 8) & 0xff;
                        blue += (color >> 16) & 0xff;
                        covered += 1;
                    }
                }
            }
            pixels[y * pixels_per_row + x] = ((covered * 255 / 16) << 24)
                | ((red / 16) << 16)
                | ((green / 16) << 8)
                | (blue / 16);
        }
    }
    let mask_bytes = vec![0_u8; pixels_per_row.div_ceil(16) * 2 * pixels_per_row];
    // SAFETY: The mask has the word-aligned stride required by CreateBitmap;
    // both returned bitmap handles are destroyed after the icon copies them.
    unsafe {
        let mask = CreateBitmap(size, size, 1, 1, Some(mask_bytes.as_ptr().cast()));
        let result = if mask.is_invalid() {
            Err(last_error("Could not create the tray icon mask"))
        } else {
            let icon = CreateIconIndirect(&ICONINFO {
                fIcon: true.into(),
                hbmColor: bitmap,
                hbmMask: mask,
                ..Default::default()
            })
            .map_err(|_| last_error("Could not create the tray icon"));
            let _ = DeleteObject(mask.into());
            icon
        };
        let _ = DeleteObject(bitmap.into());
        result
    }
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "Windows session-query cancellation and deferred quit have independent lifecycle flags"
)]
struct UiState {
    state: i32,
    message: String,
    shortcut_failures: Vec<String>,
    quitting: bool,
    awaiting_termination: bool,
    session_ending: bool,
    shutdown_blocked: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            state: 0,
            message: "Stopped".to_owned(),
            shortcut_failures: Vec::new(),
            quitting: false,
            awaiting_termination: false,
            session_ending: false,
            shutdown_blocked: false,
        }
    }
}

impl UiState {
    const fn can_toggle(&self) -> bool {
        !self.quitting && !self.session_ending && matches!(self.state, 0..=2)
    }

    const fn can_save(&self) -> bool {
        !self.quitting && !self.session_ending && self.state == 2
    }

    const fn toggle_title(&self) -> &'static str {
        match self.state {
            0 => "Start Buffering\tCtrl+Alt+B",
            3 => "Stopping…\tCtrl+Alt+B",
            _ => "Stop Buffering\tCtrl+Alt+B",
        }
    }
}

struct Application {
    callback: fn(i32) -> bool,
    window: Cell<HWND>,
    icon: Cell<HICON>,
    taskbar_created: Cell<u32>,
    shutdown_message: Cell<u32>,
    state: RefCell<UiState>,
    tray_added: Cell<bool>,
    icon_failure_reported: Cell<bool>,
    tray_failure_reported: Cell<bool>,
    tray_version_four: Cell<bool>,
    toggle_hotkey: Cell<bool>,
    save_hotkey: Cell<bool>,
    session_notifications: Cell<bool>,
    menu_open: Cell<bool>,
    active_menu: Cell<HMENU>,
    menu_window: Cell<HWND>,
}

impl Application {
    fn new(callback: fn(i32) -> bool) -> Self {
        Self {
            callback,
            window: Cell::new(HWND::default()),
            icon: Cell::new(HICON::default()),
            taskbar_created: Cell::new(0),
            shutdown_message: Cell::new(0),
            state: RefCell::new(UiState::default()),
            tray_added: Cell::new(false),
            icon_failure_reported: Cell::new(false),
            tray_failure_reported: Cell::new(false),
            tray_version_four: Cell::new(false),
            toggle_hotkey: Cell::new(false),
            save_hotkey: Cell::new(false),
            session_notifications: Cell::new(false),
            menu_open: Cell::new(false),
            active_menu: Cell::new(HMENU::default()),
            menu_window: Cell::new(HWND::default()),
        }
    }

    fn initialize(&self, identity: PCWSTR) -> Result<(), String> {
        // SAFETY: All strings and structures live through each API call. self
        // is in a stable Box until window destruction completes. The ordinary
        // hidden top-level window receives shell/display/power broadcasts.
        unsafe {
            let instance = GetModuleHandleW(None)
                .map_err(|_| last_error("Could not locate the Optic executable"))?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: HINSTANCE(instance.0),
                lpszClassName: WINDOW_CLASS,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            if RegisterClassW(&raw const class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS
            {
                return Err(last_error("Could not register the Optic tray window"));
            }
            self.taskbar_created
                .set(RegisterWindowMessageW(w!("TaskbarCreated")));
            self.shutdown_message
                .set(RegisterWindowMessageW(SHUTDOWN_MESSAGE_NAME));
            if self.taskbar_created.get() == 0 || self.shutdown_message.get() == 0 {
                return Err(last_error("Could not register the Optic window messages"));
            }
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                WINDOW_CLASS,
                identity,
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(HINSTANCE(instance.0)),
                Some(ptr::from_ref(self).cast()),
            )
            .map_err(|_| last_error("Could not create the Optic tray window"))?;
            self.window.set(window);
            *WINDOW
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = window.0 as usize;
            self.session_notifications
                .set(WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION).is_ok());
            if !self.session_notifications.get() {
                report_error(
                    "ui",
                    "tray.session_notifications_failed",
                    &last_error("Could not register session notifications"),
                );
            }
            self.toggle_hotkey.set(
                RegisterHotKey(
                    Some(window),
                    TOGGLE_COMMAND.cast_signed(),
                    MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                    u32::from(b'B'),
                )
                .is_ok(),
            );
            if !self.toggle_hotkey.get() {
                self.shortcut_failure("Ctrl+Alt+B", GetLastError().0);
            }
            self.save_hotkey.set(
                RegisterHotKey(
                    Some(window),
                    SAVE_COMMAND.cast_signed(),
                    MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                    u32::from(b'S'),
                )
                .is_ok(),
            );
            if !self.save_hotkey.get() {
                self.shortcut_failure("Ctrl+Alt+S", GetLastError().0);
            }
        }
        self.update_icon();
        Ok(())
    }

    fn message_loop() {
        let mut message = MSG::default();
        loop {
            // SAFETY: The message buffer is writable. The main thread runs the
            // tray apartment and owns every window dispatched by this loop.
            let result = unsafe { GetMessageW(&raw mut message, None, 0, 0) }.0;
            if result <= 0 {
                let error = if result == -1 {
                    Some(last_error("Windows message loop failed"))
                } else {
                    None
                };
                report_pending_status_failure();
                if let Some(error) = error {
                    report_error("ui", "tray.message_loop_failed", &error);
                }
                break;
            }
            // SAFETY: GetMessage initialized message. Dispatch may re-enter
            // Application methods, so no RefCell borrow is held here.
            unsafe {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
            report_pending_status_failure();
        }
    }

    fn shortcut_failure(&self, shortcut: &str, error: u32) {
        let text = format!("Shortcut unavailable: {shortcut} (error {error})");
        self.state
            .borrow_mut()
            .shortcut_failures
            .insert(0, text.clone());
        report_error("shortcuts", "shortcut.registration_failed", &text);
    }

    fn refresh_menu(&self) {
        let menu = self.active_menu.get();
        if menu.is_invalid() {
            return;
        }
        let (status, toggle, can_toggle, can_save, can_quit) = {
            let state = self.state.borrow();
            (
                wide(&menu_text(&state.message)),
                wide(state.toggle_title()),
                state.can_toggle(),
                state.can_save(),
                !state.quitting,
            )
        };
        let update = |item, by_position, text: Option<&[u16]>, enabled| {
            let mut info = MENUITEMINFOW {
                cbSize: structure_size::<MENUITEMINFOW>(),
                fMask: MIIM_STATE,
                fState: if enabled { MFS_ENABLED } else { MFS_DISABLED },
                ..Default::default()
            };
            if let Some(text) = text {
                info.fMask |= MIIM_STRING;
                info.dwTypeData = windows::core::PWSTR(text.as_ptr().cast_mut());
            }
            // SAFETY: The menu is alive through this call. The API only reads
            // the string pointer despite its mutable Win32 declaration.
            unsafe {
                let _ = SetMenuItemInfoW(menu, item, by_position, &raw const info);
            }
        };
        update(0, true, Some(&status), false);
        update(TOGGLE_COMMAND, false, Some(&toggle), can_toggle);
        update(SAVE_COMMAND, false, None, can_save);
        update(QUIT_COMMAND, false, None, can_quit);
        // SAFETY: These operations redraw the owner and any currently tracked
        // popup. No mutable state borrow is held while callbacks may dispatch.
        unsafe {
            let _ = DrawMenuBar(self.window.get());
            if !self.menu_window.get().is_invalid() {
                let _ = RedrawWindow(
                    Some(self.menu_window.get()),
                    None,
                    None,
                    RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW,
                );
            }
        }
    }

    fn tray_data(&self) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: structure_size::<NOTIFYICONDATAW>(),
            hWnd: self.window.get(),
            uID: TRAY_ID,
            uCallbackMessage: TRAY_MESSAGE,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            hIcon: self.icon.get(),
            ..Default::default()
        };
        let tip = wide(&format!("Optic: {}", self.state.borrow().message));
        let length = (tip.len() - 1).min(data.szTip.len() - 1);
        data.szTip[..length].copy_from_slice(&tip[..length]);
        data
    }

    fn update_icon(&self) {
        self.refresh_menu();
        let buffering = {
            let state = self.state.borrow();
            state.state == 2 && !state.quitting
        };
        match make_icon(tray_icon_size(self.window.get()), buffering) {
            Ok(icon) => {
                self.icon_failure_reported.set(false);
                let previous = self.icon.replace(icon);
                if !previous.is_invalid() {
                    // SAFETY: The previous icon is owned by this application.
                    unsafe {
                        let _ = DestroyIcon(previous);
                    }
                }
            }
            Err(error) if !self.icon_failure_reported.replace(true) => {
                report_error("ui", "tray.icon_failed", &error);
            }
            Err(_) => {}
        }
        let mut data = self.tray_data();
        // SAFETY: The data and icon remain alive through each shell call. The
        // callback destination is the currently owned hidden tray window.
        unsafe {
            if self.tray_added.get() {
                if Shell_NotifyIconW(NIM_MODIFY, &raw const data).as_bool() {
                    self.tray_failure_reported.set(false);
                    return;
                }
                self.tray_added.set(false);
            }
            if Shell_NotifyIconW(NIM_ADD, &raw const data).as_bool() {
                self.tray_failure_reported.set(false);
                self.tray_added.set(true);
                data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                self.tray_version_four
                    .set(Shell_NotifyIconW(NIM_SETVERSION, &raw const data).as_bool());
                let _ = KillTimer(Some(self.window.get()), TRAY_RETRY_TIMER);
            } else {
                let error = last_error("Could not add the tray icon");
                if !self.tray_failure_reported.replace(true) {
                    report_error("ui", "tray.registration_failed", &error);
                }
                SetTimer(Some(self.window.get()), TRAY_RETRY_TIMER, 2000, None);
            }
        }
    }

    fn apply_status(&self, status: Status) {
        if self.state.borrow().quitting {
            return;
        }
        if self.state.borrow().shutdown_blocked && (self.callback)(4) {
            // SAFETY: The owner window is live; this removes its shutdown reason.
            unsafe {
                let _ = ShutdownBlockReasonDestroy(self.window.get());
            }
            self.state.borrow_mut().shutdown_blocked = false;
        }
        {
            let mut state = self.state.borrow_mut();
            state.state = status.state;
            state.message = if status.message.is_empty() {
                state_name(status.state).to_owned()
            } else {
                status.message
            };
        }
        self.update_icon();
    }

    fn toggle(&self) {
        {
            let mut state = self.state.borrow_mut();
            if !state.can_toggle() {
                return;
            }
            state.state = if state.state == 0 { 1 } else { 3 };
            state_name(state.state).clone_into(&mut state.message);
        }
        self.update_icon();
        (self.callback)(1);
    }

    fn save(&self) {
        let can_save = self.state.borrow().can_save();
        if can_save {
            (self.callback)(2);
        }
    }

    fn quit(&self) {
        {
            let mut state = self.state.borrow_mut();
            if state.quitting {
                return;
            }
            state.quitting = true;
        }
        if (self.callback)(3) {
            // SAFETY: This is the owning window thread. Destruction may re-enter
            // window_proc, so no mutable reference or state borrow is held.
            unsafe {
                let _ = DestroyWindow(self.window.get());
            }
        } else {
            {
                let mut state = self.state.borrow_mut();
                state.awaiting_termination = true;
                "Finishing saved clip…".clone_into(&mut state.message);
            }
            self.update_icon();
        }
    }

    fn query_end_session(&self) -> bool {
        let awaiting_termination = self.state.borrow().awaiting_termination;
        if awaiting_termination || !(self.callback)(4) {
            // SAFETY: The current hidden top-level window owns the reason and
            // the API copies the static UTF-16 text before returning.
            let blocked = unsafe {
                ShutdownBlockReasonCreate(self.window.get(), w!("Optic is finishing a saved clip."))
            }
            .is_ok();
            self.state.borrow_mut().shutdown_blocked = blocked;
            return false;
        }
        self.state.borrow_mut().session_ending = true;
        self.refresh_menu();
        true
    }

    fn end_session(&self, ending: bool) {
        if self.state.borrow().shutdown_blocked {
            // SAFETY: This window owns any previously registered shutdown reason.
            unsafe {
                let _ = ShutdownBlockReasonDestroy(self.window.get());
            }
        }
        {
            let mut state = self.state.borrow_mut();
            state.shutdown_blocked = false;
            state.session_ending = false;
        }
        if ending {
            self.quit();
        } else {
            self.refresh_menu();
        }
    }

    fn open_folder(&self) {
        let folder = match recordings_folder() {
            Ok(folder) => wide_path(folder.as_os_str()),
            Err(error) => {
                report_error(
                    "folders",
                    "recordings_folder.open_failed",
                    &format!("Could not open recordings folder: {error}"),
                );
                return;
            }
        };
        // SAFETY: The path remains NUL-terminated and alive through both shell
        // calls. ShellExecute copies it and launches Explorer asynchronously.
        unsafe {
            let created =
                SHCreateDirectoryExW(Some(self.window.get()), PCWSTR(folder.as_ptr()), None);
            if created != ERROR_SUCCESS.0.cast_signed()
                && created != ERROR_ALREADY_EXISTS.0.cast_signed()
                && created != ERROR_FILE_EXISTS.0.cast_signed()
            {
                report_error(
                    "folders",
                    "recordings_folder.create_failed",
                    &format!("Could not create recordings folder (Windows error {created})"),
                );
                return;
            }
            let result = ShellExecuteW(
                Some(self.window.get()),
                w!("open"),
                PCWSTR(folder.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            );
            if result.0 as isize <= 32 {
                report_error(
                    "folders",
                    "recordings_folder.open_failed",
                    &format!(
                        "Could not open recordings folder (ShellExecute error {})",
                        result.0 as isize
                    ),
                );
            }
        }
    }

    fn command(&self, command: u32) {
        match command {
            TOGGLE_COMMAND => self.toggle(),
            SAVE_COMMAND => self.save(),
            FOLDER_COMMAND => self.open_folder(),
            QUIT_COMMAND => self.quit(),
            _ => {}
        }
    }

    fn create_menu(&self) -> Result<HMENU, String> {
        let (message, failures, toggle, can_toggle, can_save, can_quit) = {
            let state = self.state.borrow();
            (
                menu_text(&state.message),
                state.shortcut_failures.clone(),
                state.toggle_title(),
                state.can_toggle(),
                state.can_save(),
                !state.quitting,
            )
        };
        // SAFETY: Each string lives through AppendMenuW, which copies it. The
        // caller takes ownership of the returned menu handle.
        unsafe {
            let menu =
                CreatePopupMenu().map_err(|_| last_error("Could not create the tray menu"))?;
            let append = |text: &str, flags, id| {
                let text = wide(text);
                let _ = AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr()));
            };
            append(&message, MF_STRING | MF_DISABLED, 0);
            for failure in failures {
                append(&failure, MF_STRING | MF_DISABLED, 0);
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            append(
                toggle,
                MF_STRING | if can_toggle { MF_ENABLED } else { MF_DISABLED },
                TOGGLE_COMMAND as usize,
            );
            append(
                "Save Replay\tCtrl+Alt+S",
                MF_STRING | if can_save { MF_ENABLED } else { MF_DISABLED },
                SAVE_COMMAND as usize,
            );
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            append("Open Recordings Folder", MF_STRING, FOLDER_COMMAND as usize);
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            append(
                "Quit Optic",
                MF_STRING | if can_quit { MF_ENABLED } else { MF_DISABLED },
                QUIT_COMMAND as usize,
            );
            Ok(menu)
        }
    }

    fn show_menu(&self, point: windows::Win32::Foundation::POINT) {
        if self.menu_open.get() {
            return;
        }
        let Ok(menu) = self.create_menu() else {
            return;
        };
        self.menu_open.set(true);
        self.active_menu.set(menu);
        // SAFETY: The menu and owner are live. TrackPopupMenu runs a nested
        // message loop; self has shared access only and no state borrow is held.
        let selected = unsafe {
            let _ = SetForegroundWindow(self.window.get());
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                Some(0),
                self.window.get(),
                None,
            )
            .0
            .cast_unsigned()
        };
        self.active_menu.set(HMENU::default());
        self.menu_window.set(HWND::default());
        self.menu_open.set(false);
        // SAFETY: The popup is no longer tracked, so its owned handle can close.
        unsafe {
            let _ = DestroyMenu(menu);
        }
        if self.window.get().is_invalid() {
            return;
        }
        let data = self.tray_data();
        // SAFETY: The owner survived the nested loop. The empty wakeup and tray
        // focus request retain no pointers after their calls.
        unsafe {
            let _ = PostMessageW(Some(self.window.get()), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = Shell_NotifyIconW(NIM_SETFOCUS, &raw const data);
        }
        self.command(selected);
    }

    fn destroy(&self) {
        let window = self.window.get();
        // SAFETY: Called on the owner thread for WM_DESTROY. Clear publication
        // before draining status Boxes so producers cannot enqueue more. Each
        // removed message transferred exactly one allocation to the UI.
        unsafe {
            if self.state.borrow().shutdown_blocked {
                let _ = ShutdownBlockReasonDestroy(window);
            }
            if !self.active_menu.get().is_invalid() {
                let _ = EndMenu();
            }
            {
                let mut published = WINDOW
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *published == window.0 as usize {
                    *published = 0;
                }
            }
            let mut pending = MSG::default();
            while PeekMessageW(
                &raw mut pending,
                Some(window),
                STATUS_MESSAGE,
                STATUS_MESSAGE,
                PM_REMOVE,
            )
            .as_bool()
            {
                if pending.lParam.0 != 0 {
                    drop(Box::from_raw(pending.lParam.0 as *mut Status));
                }
            }
            if self.session_notifications.get() {
                let _ = WTSUnRegisterSessionNotification(window);
            }
            if self.toggle_hotkey.get() {
                let _ = UnregisterHotKey(Some(window), TOGGLE_COMMAND.cast_signed());
            }
            if self.save_hotkey.get() {
                let _ = UnregisterHotKey(Some(window), SAVE_COMMAND.cast_signed());
            }
            let _ = KillTimer(Some(window), TRAY_RETRY_TIMER);
            if self.tray_added.replace(false) {
                let data = NOTIFYICONDATAW {
                    cbSize: structure_size::<NOTIFYICONDATAW>(),
                    hWnd: window,
                    uID: TRAY_ID,
                    ..Default::default()
                };
                let _ = Shell_NotifyIconW(NIM_DELETE, &raw const data);
            }
            self.window.set(HWND::default());
            PostQuitMessage(0);
        }
    }

    fn tray_event(&self, wparam: WPARAM, lparam: LPARAM) {
        let event = if self.tray_version_four.get() {
            u32::from(u16::try_from(lparam.0.cast_unsigned() & 0xffff).unwrap_or_default())
        } else {
            u32::try_from(lparam.0).unwrap_or_default()
        };
        if event != WM_CONTEXTMENU
            && event != NIN_SELECT
            && event != (NIN_SELECT | NINF_KEY)
            && (self.tray_version_four.get() || (event != WM_RBUTTONUP && event != WM_LBUTTONUP))
        {
            return;
        }
        let mut point = windows::Win32::Foundation::POINT::default();
        if self.tray_version_four.get() {
            point.x = i32::from(
                u16::try_from(wparam.0 & 0xffff)
                    .unwrap_or_default()
                    .cast_signed(),
            );
            point.y = i32::from(
                u16::try_from((wparam.0 >> 16) & 0xffff)
                    .unwrap_or_default()
                    .cast_signed(),
            );
        }
        if !self.tray_version_four.get() || (point.x == -1 && point.y == -1) {
            // SAFETY: The cursor API writes the supplied POINT.
            unsafe {
                let _ = GetCursorPos(&raw mut point);
            }
        }
        self.show_menu(point);
    }

    fn handle_message(
        &self,
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        if self.shutdown_message.get() != 0 && message == self.shutdown_message.get() {
            self.quit();
            return Some(LRESULT(0));
        }
        if self.taskbar_created.get() != 0 && message == self.taskbar_created.get() {
            self.tray_added.set(false);
            self.update_icon();
            return Some(LRESULT(0));
        }
        match message {
            #[cfg(test)]
            TEST_INVOCATION_MESSAGE => {
                // SAFETY: invoke transfers exactly one boxed invocation after a
                // successful post and the UI consumes that allocation once.
                let tests::Invocation { action, complete } =
                    *unsafe { Box::from_raw(lparam.0 as *mut tests::Invocation) };
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(self)));
                let _ = complete.send(result);
            }
            STATUS_MESSAGE => {
                if lparam.0 != 0 {
                    // SAFETY: Every status message carries exactly one owned Box.
                    let status = unsafe { Box::from_raw(lparam.0 as *mut Status) };
                    self.apply_status(*status);
                }
            }
            STATUS_ERROR_MESSAGE => {} // Logging belongs to the outer loop.
            FINISH_QUIT_MESSAGE => {
                let awaiting = self.state.borrow().awaiting_termination;
                if awaiting {
                    // SAFETY: The owner thread destroys its live window. Shared
                    // access tolerates synchronous WM_DESTROY reentry.
                    unsafe {
                        let _ = DestroyWindow(window);
                    }
                }
            }
            TRAY_MESSAGE => self.tray_event(wparam, lparam),
            WM_HOTKEY => self.command(u32::try_from(wparam.0).unwrap_or_default()),
            WM_COMMAND => self.command(u32::from(
                u16::try_from(wparam.0 & 0xffff).unwrap_or_default(),
            )),
            WM_ENTERIDLE => {
                if wparam.0 == MSGF_MENU as usize && !self.active_menu.get().is_invalid() {
                    self.menu_window.set(HWND(lparam.0 as *mut _));
                }
            }
            WM_CLOSE => self.quit(),
            WM_QUERYENDSESSION => return Some(LRESULT(isize::from(self.query_end_session()))),
            WM_ENDSESSION => self.end_session(wparam.0 != 0),
            WM_POWERBROADCAST => {
                if wparam.0 == PBT_APMSUSPEND as usize {
                    interrupt_capture(super::capture::Interruption::Expected);
                }
                return Some(LRESULT(1));
            }
            WM_WTSSESSION_CHANGE => {
                if matches!(wparam.0, 7 | 2 | 4) {
                    interrupt_capture(super::capture::Interruption::Expected);
                }
            }
            WM_DISPLAYCHANGE => {
                interrupt_capture(super::capture::Interruption::DisplayChanged);
                self.update_icon();
            }
            WM_DPICHANGED | WM_SETTINGCHANGE | WM_THEMECHANGED => self.update_icon(),
            WM_TIMER => {
                if wparam.0 == TRAY_RETRY_TIMER {
                    self.update_icon();
                }
            }
            WM_DESTROY => self.destroy(),
            WM_NCDESTROY => {
                // SAFETY: This clears the borrowed pointer before the window
                // ceases to exist; its boxed Application still remains alive.
                unsafe {
                    SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                }
                return None;
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

impl Drop for Application {
    fn drop(&mut self) {
        // Window cleanup is explicit before Drop: DestroyWindow re-enters the
        // callback, which must never alias this destructor's mutable reference.
        // SAFETY: The icon is solely owned and destroying it doesn't dispatch
        // window callbacks or retain a pointer to this Application.
        unsafe {
            if !self.icon.get().is_invalid() {
                let _ = DestroyIcon(self.icon.get());
            }
        }
    }
}

fn interrupt_capture(reason: super::capture::Interruption) {
    #[cfg(not(test))]
    super::capture::interrupt(reason);
    #[cfg(test)]
    {
        tests::INTERRUPTIONS.fetch_add(1, Ordering::AcqRel);
        if reason == super::capture::Interruption::DisplayChanged {
            tests::DISPLAY_INTERRUPTION.fetch_add(1, Ordering::AcqRel);
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: The window stores only the stable Box pointer supplied at create.
    // WM_NCCREATE receives Windows' live CREATESTRUCTW for the current call.
    let mut app = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const Application;
    if message == WM_NCCREATE {
        // SAFETY: Windows documents lParam as a CREATESTRUCTW for WM_NCCREATE.
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        app = create.lpCreateParams.cast();
        // SAFETY: Creation supplied a stable shared pointer that lives until
        // DestroyWindow returns and WM_NCDESTROY clears this slot.
        unsafe {
            SetWindowLongPtrW(window, GWLP_USERDATA, app as isize);
        }
    }
    if !app.is_null() {
        // SAFETY: This pointer is borrowed from the Box owned by run. Shared
        // interior-mutability access permits synchronous callback reentry.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: run owns this stable Box until the window is destroyed;
            // only a shared reference crosses synchronous callback reentry.
            unsafe { &*app }.handle_message(window, message, wparam, lparam)
        }));
        match result {
            Ok(Some(result)) => return result,
            Ok(None) => {}
            Err(_) => report_error("ui", "tray.operation_failed", "Tray operation failed"),
        }
    }
    // SAFETY: Unhandled Windows messages use their default window procedure.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// Runs the hidden tray on the main STA thread, starting with capture stopped.
pub fn run(callback: fn(i32) -> bool) {
    // SAFETY: The executable's main thread owns this matching COM apartment.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let result = if com.is_err() {
        Err(format!(
            "Could not initialize the tray COM apartment (HRESULT {})",
            com.0.cast_unsigned()
        ))
    } else {
        run_initialized(callback)
    };
    if let Err(error) = result {
        report_error(
            "ui",
            "tray.initialization_failed",
            &format!("Could not start the Optic tray: {error}"),
        );
    }
    if com.is_ok() {
        // SAFETY: The successful initialization above is balanced on its thread.
        unsafe {
            CoUninitialize();
        }
    }
}

fn run_initialized(callback: fn(i32) -> bool) -> Result<(), String> {
    let mut maintenance = MutexLease::new(MAINTENANCE_MUTEX)?;
    let can_launch = maintenance.acquire()?;
    let identity = instance_name()?;
    let mut instance = MutexLease::new(PCWSTR(identity.as_ptr()))?;
    if !can_launch {
        diagnostic(
            "Installation or removal is in progress; try launching Optic again when it finishes.",
        );
    } else if !instance.acquire()? {
        diagnostic("Optic is already running in this Windows session.");
    } else {
        let application = Box::new(Application::new(callback));
        match application.initialize(PCWSTR(identity.as_ptr())) {
            Ok(()) => {
                maintenance.release();
                instance.hold_until_process_exit();
                Application::message_loop();
            }
            Err(error) => {
                let message = format!("Could not start the Optic tray: {error}");
                report_error("ui", "tray.initialization_failed", &message);
                let message = wide(&message);
                // SAFETY: The synchronous message box borrows live static/title
                // and NUL-terminated message strings until it is dismissed.
                unsafe {
                    MessageBoxW(
                        None,
                        PCWSTR(message.as_ptr()),
                        w!("Optic"),
                        MB_OK | MB_ICONERROR,
                    );
                }
            }
        }
        if !application.window.get().is_invalid() {
            // SAFETY: Clean up any window after message-loop or initialization
            // failure before dropping the Box, under shared access only.
            unsafe {
                let _ = DestroyWindow(application.window.get());
            }
        }
    }
    Ok(())
}

/// Locates the user's known Videos folder without losing Unicode path units.
pub fn recordings_folder() -> Result<PathBuf, String> {
    // SAFETY: The shell allocates a NUL-terminated known-folder path, and no
    // caller-owned references are retained by this lookup.
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None) }.map_err(
        |error| {
            format!(
                "Could not locate your Videos folder (error {})",
                error.code().0.cast_unsigned()
            )
        },
    )?;
    // SAFETY: A successful known-folder lookup returns a valid UTF-16 string.
    let units = unsafe { path.as_wide() };
    let result = PathBuf::from(OsString::from_wide(units)).join("Optic");
    // SAFETY: SHGetKnownFolderPath allocated path using the COM task allocator.
    unsafe {
        CoTaskMemFree(Some(path.0.cast()));
    }
    Ok(result)
}

/// Attaches CLI invocations to their parent's console, preserving redirection.
pub fn prepare_console() {
    let identifiers = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    let mut inherited = [HANDLE::default(); 3];
    // SAFETY: Standard handle APIs access this process's existing handles. Only
    // valid inherited handles are restored after console attachment replaces
    // defaults; no additional console window is opened.
    unsafe {
        for (index, identifier) in identifiers.iter().enumerate() {
            if let Ok(handle) = GetStdHandle(*identifier) {
                if !handle.is_invalid() {
                    SetLastError(ERROR_SUCCESS);
                    if GetFileType(handle) != FILE_TYPE_UNKNOWN || GetLastError() == ERROR_SUCCESS {
                        inherited[index] = handle;
                    }
                }
            }
        }
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        for (identifier, handle) in identifiers.into_iter().zip(inherited) {
            if !handle.is_invalid() {
                let _ = SetStdHandle(identifier, handle);
            }
        }
    }
}

fn shutdown_window(
    window: HWND,
    message: u32,
    sid: &str,
    own_session: u32,
    timeout: u32,
) -> Result<bool, String> {
    let mut process_id = 0;
    // SAFETY: The window may disappear; the API writes a process ID only for a
    // valid window. OpenProcess returns an independently owned process handle.
    let thread = unsafe { GetWindowThreadProcessId(window, Some(&raw mut process_id)) };
    if thread == 0 {
        return Ok(true);
    }
    // SAFETY: Query and synchronization permissions suffice to validate the
    // endpoint and wait for graceful process exit; termination isn't requested.
    let process = match unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            process_id,
        )
    } {
        Ok(process) => OwnedHandle(process),
        // SAFETY: This thread's failed OpenProcess sets its last-error value.
        Err(_) if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER => return Ok(true),
        Err(_) => return Err(last_error("Could not wait for Optic to close")),
    };
    // SAFETY: The process handle is valid for the duration of these waits.
    if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0 {
        return Ok(true);
    }
    let mut session = 0;
    // SAFETY: The output is writable and the process ID was verified above.
    unsafe { ProcessIdToSessionId(process_id, &raw mut session) }
        .map_err(|_| last_error("Could not identify the running Optic session"))?;
    if session != own_session || user_sid(process.0)? != sid {
        return Ok(false);
    }
    // SAFETY: The copied message owns no data and reaches only the validated
    // same-user/session window. Process ownership protects its exit wait.
    if unsafe { PostMessageW(Some(window), message, WPARAM(0), LPARAM(0)) }.is_err() {
        // SAFETY: A concurrent graceful exit can explain the vanished window.
        if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0 {
            return Ok(true);
        }
        return Err(last_error("Could not request Optic shutdown"));
    }
    // SAFETY: This waits for process exit without ever terminating an active save.
    let result = unsafe { WaitForSingleObject(process.0, timeout) };
    if result == WAIT_OBJECT_0 {
        return Ok(true);
    }
    if result != WAIT_TIMEOUT {
        return Err(last_error("Could not wait for Optic to close"));
    }
    Err("Timed out waiting for Optic to finish its saved clip and close; the process was left running".to_owned())
}

/// Requests this user's session instance to finish saving and exit gracefully.
pub fn shutdown(timeout_ms: u32) -> Result<(), String> {
    if timeout_ms == 0 || timeout_ms > 600_000 {
        return Err("Shutdown timeout must be between 1 and 600000 milliseconds".to_owned());
    }
    let identity = instance_name()?;
    // SAFETY: The current process pseudo-handle is valid for SID lookup.
    let sid = user_sid(unsafe { GetCurrentProcess() })?;
    // SAFETY: The identity string is live and NUL-terminated; the mutex name
    // isolates both SID and Windows Local session namespace.
    let instance = match unsafe {
        OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE,
            false,
            PCWSTR(identity.as_ptr()),
        )
    } {
        Ok(handle) => Some(OwnedHandle(handle)),
        // SAFETY: OpenMutexW sets this thread's last-error on failure.
        Err(_) if unsafe { GetLastError() } == ERROR_FILE_NOT_FOUND => None,
        Err(_) => return Err(last_error("Could not contact the running Optic instance")),
    };
    let started = Instant::now();
    let remaining = || {
        timeout_ms.saturating_sub(u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX))
    };
    let mut own_session = 0;
    // SAFETY: These APIs query this process and register a copied static message.
    let message = unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &raw mut own_session)
            .map_err(|_| last_error("Could not identify this Windows session"))?;
        RegisterWindowMessageW(SHUTDOWN_MESSAGE_NAME)
    };
    if message == 0 {
        return Err(last_error("Could not prepare the Optic shutdown request"));
    }
    if let Some(instance) = instance {
        loop {
            // SAFETY: The opened mutex is owned until this operation finishes.
            let result = unsafe { WaitForSingleObject(instance.0, 0) };
            if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED {
                // SAFETY: A successful wait gave this thread mutex ownership.
                unsafe {
                    let _ = ReleaseMutex(instance.0);
                }
                break;
            }
            if result != WAIT_TIMEOUT {
                return Err(last_error("Could not check the running Optic instance"));
            }
            // SAFETY: Both class and identity strings are valid during lookup.
            if let Ok(window) = unsafe { FindWindowW(WINDOW_CLASS, PCWSTR(identity.as_ptr())) } {
                if !shutdown_window(window, message, &sid, own_session, remaining())? {
                    return Err(
                        "The Optic shutdown endpoint belongs to another user or session".to_owned(),
                    );
                }
                break;
            }
            let budget = remaining();
            if budget == 0 {
                return Err("Timed out waiting for the running Optic instance to respond; the process was left running".to_owned());
            }
            // A launch holds the instance mutex before publishing its window.
            std::thread::sleep(Duration::from_millis(u64::from(budget.min(25))));
        }
    }
    let mut previous = None;
    loop {
        // SAFETY: Legacy lookup uses the stable old title/class; every resulting
        // endpoint is separately checked for SID and session before signaling.
        let Ok(legacy) = (unsafe { FindWindowExW(None, previous, WINDOW_CLASS, LEGACY_TITLE) })
        else {
            break;
        };
        let closed = shutdown_window(legacy, WM_CLOSE, &sid, own_session, remaining())?;
        previous = if closed { None } else { Some(legacy) };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::too_many_lines,
        reason = "Integration assertions and bounded child-process setup use descriptive failures"
    )]

    use super::*;
    use std::{
        fs,
        os::windows::process::CommandExt,
        process::{Child, Command},
        sync::{
            atomic::{AtomicBool, AtomicUsize},
            mpsc,
        },
        thread,
    };
    use windows::Win32::{
        Foundation::ERROR_HOTKEY_ALREADY_REGISTERED,
        Storage::FileSystem::ReadFile,
        System::{
            Pipes::CreatePipe,
            Threading::{
                CreateEventW, OpenEventW, ResetEvent, SetEvent, CREATE_NO_WINDOW,
                EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
            },
        },
        UI::WindowsAndMessaging::{
            GetMenuState, GetMenuStringW, SendMessageTimeoutW, ENDSESSION_CLOSEAPP,
            MENU_ITEM_FLAGS, MF_BYCOMMAND, MF_BYPOSITION, MF_GRAYED, SMTO_ABORTIFHUNG,
        },
    };

    static TOGGLES: AtomicUsize = AtomicUsize::new(0);
    static SAVES: AtomicUsize = AtomicUsize::new(0);
    static QUITS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static INTERRUPTIONS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static DISPLAY_INTERRUPTION: AtomicUsize = AtomicUsize::new(0);
    static DEFER_QUIT: AtomicBool = AtomicBool::new(true);
    static SAVING: AtomicBool = AtomicBool::new(false);
    static ERRORS: Mutex<Vec<(String, String, String)>> = Mutex::new(Vec::new());

    pub(super) struct Invocation {
        pub(super) action: Box<dyn FnOnce(&Application) + Send>,
        pub(super) complete: mpsc::Sender<thread::Result<()>>,
    }

    fn action(command: i32) -> bool {
        match command {
            1 => {
                TOGGLES.fetch_add(1, Ordering::AcqRel);
                true
            }
            2 => {
                SAVES.fetch_add(1, Ordering::AcqRel);
                true
            }
            3 => {
                QUITS.fetch_add(1, Ordering::AcqRel);
                !DEFER_QUIT.load(Ordering::Acquire)
            }
            4 => !SAVING.load(Ordering::Acquire),
            _ => true,
        }
    }

    fn observe_error(component: &str, event: &str, message: &str) {
        // A deferred status error must reach logging only after publication's
        // mutex was released. Taking that same mutex catches a regression.
        let _window_guard = WINDOW.lock().unwrap();
        ERRORS
            .lock()
            .unwrap()
            .push((component.to_owned(), event.to_owned(), message.to_owned()));
    }

    fn invoke(window: HWND, action: impl FnOnce(&Application) + Send + 'static) {
        let (complete, receive) = mpsc::channel();
        let payload = Box::into_raw(Box::new(Invocation {
            action: Box::new(action),
            complete,
        }));
        // SAFETY: Successful posting transfers this boxed invocation to the UI;
        // every captured value is owned, even if the receiver times out.
        let result = unsafe {
            PostMessageW(
                Some(window),
                TEST_INVOCATION_MESSAGE,
                WPARAM(0),
                LPARAM(payload as isize),
            )
        };
        if result.is_err() {
            // SAFETY: Failed posting leaves sole ownership with this sender.
            unsafe {
                drop(Box::from_raw(payload));
            }
        }
        result.expect("Could not post the UI integration invocation");
        let completion = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("UI integration invocation timed out");
        if let Err(failure) = completion {
            std::panic::resume_unwind(failure);
        }
    }

    fn post(window: HWND, message: u32, wparam: usize, lparam: isize) {
        // SAFETY: The known test window receives only copied numeric messages.
        unsafe { PostMessageW(Some(window), message, WPARAM(wparam), LPARAM(lparam)) }
            .expect("Could not post the test window message");
    }

    fn menu_title(menu: HMENU, item: u32, flags: MENU_ITEM_FLAGS) -> String {
        let mut text = [0_u16; 2048];
        // SAFETY: The menu is owned by the UI and text is a writable UTF-16 buffer.
        let length = unsafe { GetMenuStringW(menu, item, Some(&mut text), flags) };
        assert!(length > 0, "Could not inspect tray menu title");
        String::from_utf16_lossy(&text[..usize::try_from(length).unwrap()])
    }

    fn menu_enabled(menu: HMENU, command: u32) -> bool {
        // SAFETY: The test inspects its live menu handle on the UI thread.
        let state = unsafe { GetMenuState(menu, command, MF_BYCOMMAND) };
        assert_ne!(state, u32::MAX, "Could not inspect tray menu state");
        state & (MF_DISABLED.0 | MF_GRAYED.0) == 0
    }

    fn check_menu(app: &Application, title: &str, toggle: bool, save: bool, quit: bool) {
        let owned = app.active_menu.get().is_invalid();
        let menu = if owned {
            app.create_menu().unwrap()
        } else {
            app.active_menu.get()
        };
        assert_eq!(menu_title(menu, TOGGLE_COMMAND, MF_BYCOMMAND), title);
        assert_eq!(
            menu_title(menu, SAVE_COMMAND, MF_BYCOMMAND),
            "Save Replay\tCtrl+Alt+S"
        );
        assert_eq!(
            menu_title(menu, FOLDER_COMMAND, MF_BYCOMMAND),
            "Open Recordings Folder"
        );
        assert_eq!(menu_title(menu, QUIT_COMMAND, MF_BYCOMMAND), "Quit Optic");
        assert_eq!(
            menu_title(menu, 0, MF_BYPOSITION),
            menu_text(&app.state.borrow().message)
        );
        assert_eq!(menu_enabled(menu, TOGGLE_COMMAND), toggle);
        assert_eq!(menu_enabled(menu, SAVE_COMMAND), save);
        assert_eq!(menu_enabled(menu, QUIT_COMMAND), quit);
        assert!(menu_enabled(menu, FOLDER_COMMAND));
        if owned {
            // SAFETY: This helper created and owns the untracked temporary menu.
            unsafe { DestroyMenu(menu) }.unwrap();
        }
    }

    fn wait_for_window() -> HWND {
        let identity = instance_name().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // SAFETY: Class/title strings identify only the test namespace.
            if let Ok(window) = unsafe { FindWindowW(WINDOW_CLASS, PCWSTR(identity.as_ptr())) } {
                return window;
            }
            assert!(
                Instant::now() < deadline,
                "The test tray window wasn't published"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn check_ui(window: HWND) {
        invoke(window, |app| {
            assert_eq!(app.state.borrow().state, 0);
            assert_eq!(app.state.borrow().message, "Stopped");
            assert!(!app.toggle_hotkey.get());
            assert!(app
                .state
                .borrow()
                .shortcut_failures
                .iter()
                .any(|text| text.contains("Ctrl+Alt+B")));
            app.active_menu.set(app.create_menu().unwrap());
            check_menu(app, "Start Buffering\tCtrl+Alt+B", true, false, true);
            FAIL_NEXT_STATUS_POST.store(true, Ordering::Release);
        });
        set_status(2, "Synthetic dropped status");
        invoke(window, |app| {
            assert_eq!(app.state.borrow().state, 0);
            assert_eq!(app.state.borrow().message, "Stopped");
            let errors = ERRORS.lock().unwrap();
            assert_eq!(
                errors
                    .iter()
                    .filter(|(_, event, message)| event == "tray.status_update_failed"
                        && message.contains(
                            &windows::Win32::Foundation::ERROR_NOT_ENOUGH_QUOTA
                                .0
                                .to_string()
                        ))
                    .count(),
                1
            );
        });
        post(window, WM_HOTKEY, SAVE_COMMAND as usize, 0);
        post(window, WM_COMMAND, TOGGLE_COMMAND as usize, 0);
        invoke(window, |app| {
            assert_eq!(TOGGLES.load(Ordering::Acquire), 1);
            assert_eq!(SAVES.load(Ordering::Acquire), 0);
            assert_eq!(app.state.borrow().state, 1);
            check_menu(app, "Stop Buffering\tCtrl+Alt+B", true, false, true);
        });
        let mut message = "Buffering · last 30 seconds & ready".to_owned();
        set_status(2, &message);
        message.clear();
        invoke(window, |app| {
            assert_eq!(app.state.borrow().state, 2);
            assert_eq!(
                app.state.borrow().message,
                "Buffering · last 30 seconds & ready"
            );
            check_menu(app, "Stop Buffering\tCtrl+Alt+B", true, true, true);
        });
        post(window, WM_HOTKEY, SAVE_COMMAND as usize, 0);
        post(window, WM_HOTKEY, TOGGLE_COMMAND as usize, 0);
        post(window, WM_HOTKEY, SAVE_COMMAND as usize, 0);
        post(window, WM_HOTKEY, TOGGLE_COMMAND as usize, 0);
        invoke(window, |app| {
            assert_eq!(TOGGLES.load(Ordering::Acquire), 2);
            assert_eq!(SAVES.load(Ordering::Acquire), 1);
            assert_eq!(app.state.borrow().state, 3);
            check_menu(app, "Stopping…\tCtrl+Alt+B", false, false, true);
        });
        set_status(0, "Stopped");
        invoke(window, |app| {
            check_menu(app, "Start Buffering\tCtrl+Alt+B", true, false, true);
        });
        post(window, WM_HOTKEY, TOGGLE_COMMAND as usize, 0);
        set_status(2, "");
        invoke(window, |app| {
            assert_eq!(TOGGLES.load(Ordering::Acquire), 3);
            assert_eq!(app.state.borrow().message, "Buffering");
        });
        post(window, WM_POWERBROADCAST, PBT_APMSUSPEND as usize, 0);
        post(window, WM_WTSSESSION_CHANGE, 7, 0);
        post(window, WM_DISPLAYCHANGE, 0, 0);
        invoke(window, |_| {
            assert_eq!(INTERRUPTIONS.load(Ordering::Acquire), 3);
            assert_eq!(DISPLAY_INTERRUPTION.load(Ordering::Acquire), 1);
        });
        SAVING.store(true, Ordering::Release);
        let mut query_result = 1;
        // SAFETY: SendMessageTimeout copies a query to the known test window;
        // query_result is writable until its bounded synchronous call returns.
        unsafe {
            SendMessageTimeoutW(
                window,
                WM_QUERYENDSESSION,
                WPARAM(0),
                LPARAM(isize::try_from(ENDSESSION_CLOSEAPP).unwrap()),
                SMTO_ABORTIFHUNG,
                1000,
                Some(&raw mut query_result),
            )
        };
        assert_eq!(query_result, 0);
        invoke(window, |app| {
            assert!(!app.state.borrow().quitting);
            assert_eq!(QUITS.load(Ordering::Acquire), 0);
        });
        SAVING.store(false, Ordering::Release);
        // SAFETY: This bounded idle query targets the same owned test window.
        unsafe {
            SendMessageTimeoutW(
                window,
                WM_QUERYENDSESSION,
                WPARAM(0),
                LPARAM(isize::try_from(ENDSESSION_CLOSEAPP).unwrap()),
                SMTO_ABORTIFHUNG,
                1000,
                Some(&raw mut query_result),
            )
        };
        assert_eq!(query_result, 1);
        post(window, WM_HOTKEY, TOGGLE_COMMAND as usize, 0);
        post(window, WM_HOTKEY, SAVE_COMMAND as usize, 0);
        post(
            window,
            WM_ENDSESSION,
            0,
            isize::try_from(ENDSESSION_CLOSEAPP).unwrap(),
        );
        invoke(window, |app| {
            assert_eq!(QUITS.load(Ordering::Acquire), 0);
            assert_eq!(TOGGLES.load(Ordering::Acquire), 3);
            assert_eq!(SAVES.load(Ordering::Acquire), 1);
            assert!(!app.state.borrow().session_ending);
            check_menu(app, "Stop Buffering\tCtrl+Alt+B", true, true, true);
        });
        post(
            window,
            WM_ENDSESSION,
            1,
            isize::try_from(ENDSESSION_CLOSEAPP).unwrap(),
        );
        post(window, WM_COMMAND, QUIT_COMMAND as usize, 0);
        set_status(0, "Ignored during quitting");
        post(window, WM_HOTKEY, TOGGLE_COMMAND as usize, 0);
        post(window, WM_HOTKEY, SAVE_COMMAND as usize, 0);
        invoke(window, |app| {
            assert_eq!(QUITS.load(Ordering::Acquire), 1);
            assert_eq!(TOGGLES.load(Ordering::Acquire), 3);
            assert_eq!(SAVES.load(Ordering::Acquire), 1);
            assert!(app.state.borrow().awaiting_termination);
            assert_eq!(app.state.borrow().state, 2);
            assert_eq!(app.state.borrow().message, "Finishing saved clip…");
            check_menu(app, "Stop Buffering\tCtrl+Alt+B", false, false, false);
            // SAFETY: This test menu is untracked and solely owned by the test.
            unsafe { DestroyMenu(app.active_menu.replace(HMENU::default())) }.unwrap();
        });
        finish_quit();
        for _ in 0..32 {
            set_status(2, "Ignored queued status");
        }
    }

    fn check_console() {
        // SAFETY: The test saves/restores this process's standard handles, and
        // each pipe is locally owned and never shared with another process.
        unsafe {
            let original_out = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or_default();
            let original_err = GetStdHandle(STD_ERROR_HANDLE).unwrap_or_default();
            let (mut out_read, mut out_write, mut err_read, mut err_write) = (
                HANDLE::default(),
                HANDLE::default(),
                HANDLE::default(),
                HANDLE::default(),
            );
            CreatePipe(&raw mut out_read, &raw mut out_write, None, 0).unwrap();
            CreatePipe(&raw mut err_read, &raw mut err_write, None, 0).unwrap();
            let handles = [
                OwnedHandle(out_read),
                OwnedHandle(out_write),
                OwnedHandle(err_read),
                OwnedHandle(err_write),
            ];
            SetStdHandle(STD_OUTPUT_HANDLE, out_write).unwrap();
            SetStdHandle(STD_ERROR_HANDLE, err_write).unwrap();
            prepare_console();
            let preserved = GetStdHandle(STD_OUTPUT_HANDLE).unwrap() == out_write
                && GetStdHandle(STD_ERROR_HANDLE).unwrap() == err_write;
            let mut valid_bytes = true;
            for (read, write) in [(out_read, out_write), (err_read, err_write)] {
                let mut written = 0;
                let mut count = 0;
                let mut bytes = [0_u8; 10];
                valid_bytes &=
                    WriteFile(write, Some(b"redirected"), Some(&raw mut written), None).is_ok();
                valid_bytes &= ReadFile(read, Some(&mut bytes), Some(&raw mut count), None).is_ok();
                valid_bytes &= written == 10 && count == 10 && &bytes == b"redirected";
            }
            SetStdHandle(STD_OUTPUT_HANDLE, original_out).unwrap();
            SetStdHandle(STD_ERROR_HANDLE, original_err).unwrap();
            drop(handles);
            assert!(
                preserved && valid_bytes,
                "Console attachment lost redirected output"
            );
        }
    }

    fn wait_child(child: &mut Child, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "Lifecycle child failed: {status}");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Lifecycle child didn't exit on time"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn start_child(executable: &std::path::Path, mode: &str, ready: &str, finish: &str) -> Child {
        Command::new(executable)
            .args([
                "--exact",
                "windows::app::tests::lifecycle_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("OPTIC_UI_TEST_CHILD", mode)
            .env("OPTIC_UI_TEST_READY", ready)
            .env("OPTIC_UI_TEST_FINISH", finish)
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .expect("Could not start the lifecycle child")
    }

    fn check_lifecycle() {
        shutdown(1000).expect("Shutdown of an absent instance failed");
        let executable = std::env::current_exe().unwrap();
        let directory =
            std::env::temp_dir().join(format!("Optic-rust-shell-lifecycle-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let copied = directory.join("Optic-copy.exe");
        fs::copy(&executable, &copied).unwrap();
        let ready_name = format!("Local\\Optic.Test.Ready.{}", std::process::id());
        let finish_name = format!("Local\\Optic.Test.Finish.{}", std::process::id());
        let ready_text = wide(&ready_name);
        let finish_text = wide(&finish_name);
        // SAFETY: These named manual-reset events are isolated to this test PID.
        let ready = OwnedHandle(
            unsafe { CreateEventW(None, true, false, PCWSTR(ready_text.as_ptr())) }.unwrap(),
        );
        // SAFETY: The copied string lives through creation, which retains no pointer.
        let finish = OwnedHandle(
            unsafe { CreateEventW(None, true, false, PCWSTR(finish_text.as_ptr())) }.unwrap(),
        );
        {
            let mut maintenance = MutexLease::new(MAINTENANCE_MUTEX).unwrap();
            assert!(maintenance.acquire().unwrap());
            let mut blocked = start_child(&copied, "probe", &ready_name, &finish_name);
            wait_child(&mut blocked, Duration::from_secs(5));
        }
        for mode in ["running", "legacy"] {
            let mut running = start_child(
                if mode == "legacy" {
                    &copied
                } else {
                    &executable
                },
                mode,
                &ready_name,
                &finish_name,
            );
            // SAFETY: ready is an owned live event; this wait is bounded.
            assert_eq!(unsafe { WaitForSingleObject(ready.0, 5000) }, WAIT_OBJECT_0);
            if mode == "running" {
                let mut duplicate = start_child(&copied, "probe", &ready_name, &finish_name);
                wait_child(&mut duplicate, Duration::from_secs(5));
            }
            let timeout = shutdown(80).expect_err("A pending synthetic save should defer shutdown");
            assert!(timeout.contains("Timed out"));
            assert!(running.try_wait().unwrap().is_none());
            // SAFETY: Only the test completion event is signaled; the child then
            // performs the same deferred quit path as a real save worker.
            unsafe { SetEvent(finish.0) }.unwrap();
            shutdown(5000).expect("Deferred completion didn't finish process shutdown");
            wait_child(&mut running, Duration::from_secs(5));
            shutdown(1000).unwrap();
            // SAFETY: No child is using these event states between test cases.
            unsafe {
                ResetEvent(ready.0).unwrap();
                ResetEvent(finish.0).unwrap();
            }
        }
        fs::remove_file(copied).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn lifecycle_child() {
        let Ok(mode) = std::env::var("OPTIC_UI_TEST_CHILD") else {
            return;
        };
        *ERROR_OBSERVER.lock().unwrap() = Some(observe_error);
        if mode == "probe" {
            DEFER_QUIT.store(false, Ordering::Release);
            let (complete, wait) = mpsc::channel();
            let watchdog = thread::spawn(move || {
                if wait.recv_timeout(Duration::from_secs(2)).is_err() {
                    let published = *WINDOW.lock().unwrap();
                    if published != 0 {
                        post(HWND(published as *mut _), WM_CLOSE, 0, 0);
                    }
                }
            });
            run(action);
            let _ = complete.send(());
            watchdog.join().unwrap();
            assert!(
                ERRORS.lock().unwrap().is_empty(),
                "Routine launch refusal was logged as a failure"
            );
            return;
        }
        assert!(mode == "running" || mode == "legacy");
        let ready = wide(&std::env::var("OPTIC_UI_TEST_READY").unwrap());
        let finish = wide(&std::env::var("OPTIC_UI_TEST_FINISH").unwrap());
        // SAFETY: Test-owned named events exist until the parent finishes waiting.
        let ready = OwnedHandle(
            unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(ready.as_ptr())) }.unwrap(),
        );
        // SAFETY: Only synchronization permission is needed to wait for completion.
        let finish = OwnedHandle(
            unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(finish.as_ptr())) }
                .unwrap(),
        );
        let (ready_handle, finish_handle) = (ready.0 .0 as usize, finish.0 .0 as usize);
        SAVING.store(true, Ordering::Release);
        let completion = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while *WINDOW.lock().unwrap() == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            // SAFETY: Parent/child event owners live through this thread join.
            unsafe {
                SetEvent(HANDLE(ready_handle as *mut _)).unwrap();
                let _ = WaitForSingleObject(HANDLE(finish_handle as *mut _), 10_000);
            }
            SAVING.store(false, Ordering::Release);
            DEFER_QUIT.store(false, Ordering::Release);
            finish_quit();
            let published = *WINDOW.lock().unwrap();
            if published != 0 {
                post(HWND(published as *mut _), WM_CLOSE, 0, 0);
            }
        });
        if mode == "legacy" {
            let application = Box::new(Application::new(action));
            application.initialize(LEGACY_TITLE).unwrap();
            Application::message_loop();
        } else {
            run(action);
        }
        completion.join().unwrap();
        assert_eq!(QUITS.load(Ordering::Acquire), 1);
    }

    #[test]
    fn windows_tray_lifecycle_and_callbacks() {
        *ERROR_OBSERVER.lock().unwrap() = Some(observe_error);
        report_error("ui", "test.failed", "Native failure · (Windows error 5)");
        let workers: Vec<_> = (0..4)
            .map(|_| {
                thread::spawn(|| {
                    for _ in 0..8 {
                        report_error(
                            "ui",
                            "test.concurrent_failed",
                            "Synthetic concurrent failure",
                        );
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(ERRORS.lock().unwrap().len(), 33);
        assert_eq!(
            ERRORS.lock().unwrap()[0].2,
            "Native failure · (Windows error 5)"
        );
        ERRORS.lock().unwrap().clear();
        set_status(0, "Stopped");
        assert!(shutdown(0).is_err());
        assert!(
            ERRORS.lock().unwrap().is_empty(),
            "Returned errors and normal statuses were logged twice"
        );
        let folder = recordings_folder().unwrap();
        assert_eq!(folder.file_name(), Some(OsStr::new("Optic")));
        let unicode = OsString::from_wide(&[0x00e9, 0xd83c, 0xdfa5, 0xd800]);
        let units = wide_path(&unicode);
        assert_eq!(OsString::from_wide(&units[..units.len() - 1]), unicode);
        check_console();
        check_lifecycle();
        // SAFETY: This process intentionally owns a conflicting shortcut only
        // during its synthetic tray test and releases it afterwards.
        let conflict = unsafe {
            RegisterHotKey(
                None,
                77,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                u32::from(b'B'),
            )
        }
        .is_ok();
        // SAFETY: Failed registration reports its error on the same test thread.
        assert!(conflict || unsafe { GetLastError() } == ERROR_HOTKEY_ALREADY_REGISTERED);
        let worker = thread::spawn(|| {
            let window = wait_for_window();
            let result = std::panic::catch_unwind(|| check_ui(window));
            if result.is_err() {
                DEFER_QUIT.store(false, Ordering::Release);
                finish_quit();
                post(window, WM_CLOSE, 0, 0);
            }
            result
        });
        run(action);
        let result = worker.join().unwrap();
        if conflict {
            // SAFETY: This test thread owns the intentionally conflicting key.
            unsafe { UnregisterHotKey(None, 77) }.unwrap();
        }
        if let Err(failure) = result {
            std::panic::resume_unwind(failure);
        }
        assert_eq!(*WINDOW.lock().unwrap(), 0);
        assert_eq!(QUITS.load(Ordering::Acquire), 1);
        assert_eq!(
            ERRORS
                .lock()
                .unwrap()
                .iter()
                .filter(|(component, event, message)| component == "shortcuts"
                    && event == "shortcut.registration_failed"
                    && message.contains("Ctrl+Alt+B")
                    && message.contains("error "))
                .count(),
            1
        );
        set_status(0, "Post-termination status");
        finish_quit();
        *ERROR_OBSERVER.lock().unwrap() = None;
    }
}
