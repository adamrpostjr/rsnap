//! Monitor enumeration & DPI caching.
//!
//! Everything here lives in **physical pixels**, relative to the virtual-desktop
//! origin (which can be negative). This is enumerated once at startup and cached.
//! There's no window anywhere in this process set up to receive
//! `WM_DISPLAYCHANGE` (the overlay's own winit window doesn't hook it, and the
//! tray thread's hotkey queue is a message-only queue, which never gets it
//! either), so `overlay.rs`'s `maybe_refresh_display_layout` instead polls
//! `DisplayLayout::enumerate` + `PartialEq` on a slow (~1s) cadence, well off
//! the hotkey hot path, and swaps in a fresh layout/capture-session set the
//! moment it observes a change.

use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};
use windows::core::BOOL;

/// A physical-pixel rectangle. `right`/`bottom` are exclusive, matching Win32 RECT
/// semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl PxRect {
    /// Width in physical pixels.
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    /// Height in physical pixels.
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    /// True if `(x, y)` falls within this rect (right/bottom exclusive).
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// Clamp a point to lie within this rect.
    pub fn clamp_point(&self, x: i32, y: i32) -> (i32, i32) {
        (
            x.clamp(self.left, self.right.saturating_sub(1).max(self.left)),
            y.clamp(self.top, self.bottom.saturating_sub(1).max(self.top)),
        )
    }

    fn from_win32(r: RECT) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

/// One physical monitor's geometry, DPI, and identity, as enumerated at
/// startup (or on `WM_DISPLAYCHANGE`).
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    /// Win32 device name, e.g. `\\.\DISPLAY1`.
    pub name: String,
    /// Physical-pixel rect in virtual-desktop coordinates.
    pub rect: PxRect,
    pub dpi_x: u32,
    pub dpi_y: u32,
    pub is_primary: bool,
    /// Raw HMONITOR handle (as isize so the struct stays plain-data/Send), kept
    /// around so `windows_capture::monitor::Monitor::from_raw_hmonitor` can
    /// reconstruct a capture target for this exact monitor at session-startup.
    pub hmonitor: isize,
}

impl MonitorInfo {
    /// This monitor's DPI scale relative to the 96-DPI (100%) baseline, e.g.
    /// `1.5` for 150% scaling.
    pub fn scale_factor(&self) -> f32 {
        self.dpi_x as f32 / 96.0
    }
}

/// The full enumerated desktop: every real monitor plus the virtual-desktop
/// bounding box that contains them all.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayLayout {
    pub monitors: Vec<MonitorInfo>,
    /// Bounding box of the full virtual desktop (may include dead zones not
    /// covered by any monitor, e.g. the arms of a T-shaped layout).
    pub virtual_desktop: PxRect,
}

impl DisplayLayout {
    /// Enumerate all monitors and cache their physical rects + DPI. Called once
    /// at startup, and again by `overlay.rs`'s periodic layout-change poll.
    pub fn enumerate() -> Self {
        let monitors = enumerate_monitors();
        let virtual_desktop = virtual_desktop_rect();
        Self {
            monitors,
            virtual_desktop,
        }
    }

    /// True if the point falls on some real monitor (i.e. not in a dead zone).
    pub fn point_on_real_monitor(&self, x: i32, y: i32) -> bool {
        self.monitors.iter().any(|m| m.rect.contains_point(x, y))
    }

    /// Clamp an arbitrary point to the nearest real-monitor-covered pixel by
    /// clamping into whichever monitor rect is closest. Used to keep selection
    /// drags out of dead zones.
    pub fn clamp_to_real_monitor(&self, x: i32, y: i32) -> (i32, i32) {
        if self.monitors.is_empty() {
            return (x, y);
        }
        // If already on a real monitor, no change needed.
        if let Some(m) = self.monitors.iter().find(|m| m.rect.contains_point(x, y)) {
            return m.rect.clamp_point(x, y);
        }
        // Otherwise pick the monitor whose clamped point is nearest.
        self.monitors
            .iter()
            .map(|m| m.rect.clamp_point(x, y))
            .min_by_key(|&(cx, cy)| {
                let dx = (cx - x) as i64;
                let dy = (cy - y) as i64;
                dx * dx + dy * dy
            })
            .unwrap_or((x, y))
    }
}

fn virtual_desktop_rect() -> PxRect {
    unsafe {
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        PxRect {
            left,
            top,
            right: left + width,
            bottom: top + height,
        }
    }
}

unsafe extern "system" fn enum_monitor_proc(hmonitor: HMONITOR, _hdc: HDC, _rect: *mut RECT, lparam: LPARAM) -> BOOL {
    let monitors = unsafe { &mut *(lparam.0 as *mut Vec<MonitorInfo>) };

    let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

    let ok = unsafe { GetMonitorInfoW(hmonitor, &mut info.monitorInfo) };
    if ok.as_bool() {
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        unsafe {
            // Best-effort: if this fails we keep the 96 DPI fallback rather than
            // aborting enumeration.
            let _ = GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        }

        let name_len = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        let name = String::from_utf16_lossy(&info.szDevice[..name_len]);

        let is_primary = (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0;

        monitors.push(MonitorInfo {
            name,
            rect: PxRect::from_win32(info.monitorInfo.rcMonitor),
            dpi_x,
            dpi_y,
            is_primary,
            hmonitor: hmonitor.0 as isize,
        });
    }

    BOOL::from(true)
}

fn enumerate_monitors() -> Vec<MonitorInfo> {
    let mut monitors: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitor_proc),
            LPARAM(&mut monitors as *mut _ as isize),
        );
    }
    monitors
}
