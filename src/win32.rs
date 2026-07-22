//! Raw Win32 window placement helpers.
//!
//! We bypass winit's logical-point placement math entirely for the overlay: it's
//! placed and sized directly via `SetWindowPos` in physical pixels. This is the
//! belt-and-suspenders half of the DPI fix (the other half is forcing
//! `pixels_per_point = 1.0` in the egui context) — between the two, nothing in
//! the pipeline is allowed to silently rescale coordinates.

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_TRANSITIONS_FORCEDISABLED, DwmGetWindowAttribute,
    DwmSetWindowAttribute,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, HWND_TOPMOST, IsWindowVisible,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow,
    SetWindowDisplayAffinity, SetWindowPos, WDA_EXCLUDEFROMCAPTURE,
};
use windows::core::{BOOL, PCWSTR};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

const RUN_KEY_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE_NAME: &str = "rsnap";

/// Adds or removes this exe from the per-user "start with Windows" registry
/// key (`HKCU\...\Run`) — the standard mechanism most lightweight tray apps
/// use, rather than a scheduled task or a Startup-folder shortcut.
pub fn set_start_with_windows(enabled: bool) -> Result<(), String> {
    unsafe {
        let subkey = wide(RUN_KEY_SUBKEY);
        let mut hkey = HKEY::default();
        let err = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        );
        if err != ERROR_SUCCESS {
            return Err(format!("RegCreateKeyExW failed: {err:?}"));
        }

        let value_name = wide(RUN_VALUE_NAME);
        let result = if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let quoted = format!("\"{}\"", exe.display());
            let data = wide(&quoted);
            let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
            let err = RegSetValueExW(hkey, PCWSTR(value_name.as_ptr()), None, REG_SZ, Some(bytes));
            if err == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!("RegSetValueExW failed: {err:?}"))
            }
        } else {
            // Not present is success, not an error — deleting a value that's
            // already gone shouldn't fail the whole operation.
            let err = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr()));
            if err == ERROR_SUCCESS || err == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(format!("RegDeleteValueW failed: {err:?}"))
            }
        };

        let _ = RegCloseKey(hkey);
        result
    }
}

/// Whether this exe is currently registered to start with Windows — used to
/// initialize the Settings window's checkbox from actual system state
/// rather than trusting the config file alone (in case the registry entry
/// was removed some other way).
pub fn is_start_with_windows_enabled() -> bool {
    unsafe {
        let subkey = wide(RUN_KEY_SUBKEY);
        let mut hkey = HKEY::default();
        let err = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE,
            None,
            &mut hkey,
            None,
        );
        if err != ERROR_SUCCESS {
            return false;
        }

        let value_name = wide(RUN_VALUE_NAME);
        let err = RegQueryValueExW(hkey, PCWSTR(value_name.as_ptr()), None, None, None, None);
        let _ = RegCloseKey(hkey);
        err == ERROR_SUCCESS
    }
}

/// Move + resize a window to an exact physical-pixel rect in virtual-desktop
/// coordinates (origin may be negative), and ensure it's topmost + visible.
///
/// Deliberately NOT passing `SWP_NOACTIVATE`: this is called once, right when
/// the overlay transitions from hidden to shown, and it needs real keyboard
/// focus at that point — without it, Ctrl+C/Ctrl+S/Ctrl+Z/Escape all
/// silently do nothing, since Windows only delivers key messages to whatever
/// window currently has focus, and hit-testing (which is all mouse input
/// needs) doesn't require it. `keep_on_top`, called every frame afterward
/// just to reassert Z-order, is the one that stays `SWP_NOACTIVATE` — we
/// don't want to re-steal focus 60 times a second while the user is typing
/// into a text annotation.
pub fn place_window(hwnd: HWND, x: i32, y: i32, width: i32, height: i32) {
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, width, height, SWP_SHOWWINDOW);
    }
    force_foreground(hwnd);
}

/// Forces real keyboard focus onto `hwnd`, working around Windows' normal
/// foreground-lock restriction that silently ignores `SetForegroundWindow`
/// calls from a background process/thread that didn't just handle a
/// qualifying input event itself. Our overlay is shown from a background
/// thread (a global `RegisterHotKey` or tray-menu callback, not the window's
/// own thread), so a plain `SetForegroundWindow` there was being ignored —
/// `place_window` looked like it worked (topmost, visible, no error) but
/// keyboard shortcuts (Ctrl+C/S/Z/Y, Escape) never actually reached the
/// window. The standard workaround: temporarily attach this thread's input
/// queue to whichever thread currently owns the real foreground window, so
/// Windows treats the two as one input source and allows the handoff, then
/// detach again immediately after.
fn force_foreground(hwnd: HWND) {
    unsafe {
        let current_thread = GetCurrentThreadId();
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };

        let attached = foreground_thread != 0 && foreground_thread != current_thread && {
            AttachThreadInput(current_thread, foreground_thread, true).as_bool()
        };

        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));

        if attached {
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        }
    }
}

/// Re-assert topmost without touching position/size — call every frame while
/// the overlay is visible. Other apps' own topmost windows (notification
/// toasts, chat popups, etc.) can and do reclaim the very top Z-order spot
/// after we set it once; without continuously reasserting, the overlay can
/// silently end up behind them while still technically "topmost" in name.
///
/// Deliberately does NOT pass `SWP_NOZORDER` — that flag tells Windows to
/// ignore the `hWndInsertAfter` (`HWND_TOPMOST`) argument entirely and keep
/// the current Z order, which would make this call a no-op.
pub fn keep_on_top(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}

/// Excludes this window from any screen capture (Windows Graphics Capture,
/// DXGI Desktop Duplication, etc.) — the window still renders normally for a
/// human looking at the screen, but capture APIs see straight through it to
/// whatever's underneath.
///
/// Applied to our own overlay window specifically to close off a feedback
/// loop: the overlay is a topmost, full-screen window showing a frozen
/// screenshot whenever it's visible, so without this, a capture session
/// could plausibly end up capturing *our own frozen content* instead of the
/// live desktop — which would explain a report of the overlay only ever
/// refreshing once, on the very first show after app startup, and never
/// again until the whole process was restarted.
pub fn exclude_from_capture(hwnd: HWND) {
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
}

/// Disables DWM's default show/hide/restore transition animation for this
/// window. Without this, Windows can apply its own fade/flash transition
/// when a layered window like ours becomes visible again after being
/// hidden — invisible to our own render pipeline entirely, since it's DWM
/// compositing the effect, not us — which is what a report of "a flash of
/// white when clicking Show Overlay" turned out to be once the app's own
/// texture-reuse and blank-frame-on-close issues were already ruled out.
pub fn disable_show_hide_animation(hwnd: HWND) {
    let disabled = BOOL::from(true);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &disabled as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<BOOL>() as u32,
        );
    }
}

/// Far outside any realistic virtual-desktop bounds, in either direction —
/// used to make the overlay invisible without ever calling `ShowWindow`.
pub const OFFSCREEN_POS: i32 = -32000;

/// Moves the window to a position far off any monitor, without touching its
/// size or ever calling `ShowWindow(SW_HIDE)`. The window stays "visible" at
/// the OS level the entire time the app is resident — it's simply not
/// anywhere a human (or a screen capture) can see it.
///
/// This exists because actually hiding and re-showing the window via
/// `ShowWindow`/`ViewportCommand::Visible` — even with DWM's transition
/// animation explicitly disabled and the app's own texture/blank-frame
/// handling correct — still produced a brief white flash specifically on the
/// *second and later* shows (never the very first). That pattern points at
/// Windows resetting some part of the window's backing/redirection surface
/// specifically on a hide-then-show cycle, which is outside anything our own
/// render pipeline controls. Never hiding at all sidesteps that class of
/// issue entirely: there's no hide-then-show transition for Windows to reset
/// anything around.
pub fn move_offscreen(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            OFFSCREEN_POS,
            OFFSCREEN_POS,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
        );
    }
}

/// Finds the topmost *other* window at `(x, y)` (virtual-desktop physical
/// pixels) and returns its true visible bounds as `(left, top, width,
/// height)` — the "click a window to select it as the capture region" flow
/// in `overlay.rs`.
///
/// Deliberately NOT `WindowFromPoint`: while the overlay is showing, it's
/// itself a topmost window spanning the entire virtual desktop, so
/// `WindowFromPoint` at any location just hits *us* — every query would
/// resolve to our own HWND and get filtered out by the own-process check
/// below, meaning nothing would ever be found. Walking the real Z-order with
/// `EnumWindows` (top to bottom) and skipping our own windows finds whatever
/// real window is actually at that point underneath the overlay instead.
pub fn window_rect_at(x: i32, y: i32) -> Option<(i32, i32, i32, i32)> {
    struct Search {
        x: i32,
        y: i32,
        own_pid: u32,
        found: Option<(i32, i32, i32, i32)>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let search = &mut *(lparam.0 as *mut Search);

            let mut owner_pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut owner_pid));
            if owner_pid == search.own_pid || !IsWindowVisible(hwnd).as_bool() {
                return BOOL::from(true);
            }

            // Minimized/suspended UWP apps keep reporting their old, often
            // offscreen, rect via `GetWindowRect`/DWM while cloaked.
            let mut cloaked: u32 = 0;
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            );
            if cloaked != 0 {
                return BOOL::from(true);
            }

            // `GetWindowRect` includes the invisible resize-border padding
            // Windows 10/11 leave around most top-level windows; DWM's
            // extended frame bounds match what's actually drawn. Fall back
            // to `GetWindowRect` only if DWM has nothing for it.
            let mut rect = RECT::default();
            let got_extended = DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut rect as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_ok();
            if !got_extended && GetWindowRect(hwnd, &mut rect).is_err() {
                return BOOL::from(true);
            }
            if rect.right <= rect.left || rect.bottom <= rect.top {
                return BOOL::from(true);
            }

            if search.x >= rect.left && search.x < rect.right && search.y >= rect.top && search.y < rect.bottom {
                search.found = Some((rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top));
                return BOOL::from(false); // stop at the first (topmost) match
            }

            BOOL::from(true)
        }
    }

    let mut search = Search {
        x,
        y,
        own_pid: unsafe { GetCurrentProcessId() },
        found: None,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut search as *mut _ as isize));
    }
    search.found
}
