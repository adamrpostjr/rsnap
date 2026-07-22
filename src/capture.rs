//! Persistent per-monitor still-capture sessions.
//!
//! One Windows Graphics Capture session per monitor is started at process
//! startup via `start_free_threaded` and kept alive for the process lifetime —
//! this is the "expensive setup happens once, hotkey handler only touches
//! already-warm objects" half of the performance model. Each session runs on
//! its own background thread and continuously overwrites a shared
//! `FrameSlot` with the latest frame; showing the overlay just reads whatever
//! is currently cached there instead of kicking off a new capture.

use std::sync::{Arc, Mutex};

use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};

use crate::monitors::{MonitorInfo, PxRect};

type HandlerError = Box<dyn std::error::Error + Send + Sync>;

/// The latest captured frame for one monitor: RGBA8, no row padding.
#[derive(Clone)]
pub struct SharedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Shared cell holding a monitor's latest captured frame, written by that
/// monitor's capture thread and read by the UI thread when the overlay
/// freezes a new composite. `None` until the first frame arrives.
pub type FrameSlot = Arc<Mutex<Option<SharedFrame>>>;

/// Heuristic guard against Windows Graphics Capture occasionally delivering
/// a spurious near-blank (often pure white) frame as if it were valid
/// content — observed around DWM fade transitions, secure-desktop prompts
/// (UAC, lock screen), and races when a monitor wakes from sleep. Rather
/// than try to pin down the exact trigger, this samples a small grid of
/// pixels across the frame and flags it if almost all of them are
/// near-white, so the caller can fall back to the last known-good frame
/// instead of freezing/showing the broken one.
pub fn looks_suspiciously_blank(frame: &SharedFrame) -> bool {
    const GRID: u32 = 8;
    const WHITE_THRESHOLD: u8 = 250;
    const BLANK_FRACTION: f32 = 0.9;

    if frame.width == 0 || frame.height == 0 {
        return true;
    }

    let mut blank = 0u32;
    let mut total = 0u32;
    for gy in 0..GRID {
        for gx in 0..GRID {
            let x = (gx * frame.width) / GRID;
            let y = (gy * frame.height) / GRID;
            let idx = ((y * frame.width + x) * 4) as usize;
            let Some(&r) = frame.rgba.get(idx) else {
                continue;
            };
            let Some(&g) = frame.rgba.get(idx + 1) else {
                continue;
            };
            let Some(&b) = frame.rgba.get(idx + 2) else {
                continue;
            };
            total += 1;
            if r >= WHITE_THRESHOLD && g >= WHITE_THRESHOLD && b >= WHITE_THRESHOLD {
                blank += 1;
            }
        }
    }

    total > 0 && (blank as f32 / total as f32) >= BLANK_FRACTION
}

struct MonitorFrameHandler {
    slot: FrameSlot,
}

impl GraphicsCaptureApiHandler for MonitorFrameHandler {
    type Flags = FrameSlot;
    type Error = HandlerError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { slot: ctx.flags })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let buffer = frame.buffer()?;
        let width = buffer.width();
        let height = buffer.height();
        let mut scratch = Vec::new();
        let mut rgba = buffer.as_nopadding_buffer(&mut scratch).to_vec();

        // Force every pixel fully opaque. A captured desktop frame has no
        // legitimate concept of transparency, but WGC has been observed
        // handing back frames with a non-255 alpha channel for some
        // monitors — and since our own overlay window is itself
        // alpha-transparent, any non-opaque pixel in the captured texture
        // blends with the real (live) desktop underneath instead of fully
        // covering it, which looks exactly like a semi-transparent/blended
        // overlay rather than a clean frozen screenshot.
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }

        // A poisoned lock (some other thread panicked while holding it) would
        // otherwise take this capture thread down too; recover the guard and
        // keep going rather than propagating the panic — a dropped frame is
        // harmless, this slot gets overwritten again on the next tick.
        match self.slot.lock() {
            Ok(mut guard) => *guard = Some(SharedFrame { width, height, rgba }),
            Err(poisoned) => *poisoned.into_inner() = Some(SharedFrame { width, height, rgba }),
        }
        Ok(())
    }
}

/// A single monitor's warm capture session. Holding this alive keeps the
/// background capture thread running; nothing needs to be done with it beyond
/// keeping it in scope and reading `slot`.
///
/// `_control` is `None` when this monitor's session failed to start (see
/// `start_all`) — `slot` then simply never receives a frame, and callers that
/// treat "no frame yet" as "nothing to show for this monitor" (which they
/// already have to, for the pre-first-frame case) handle it for free.
pub struct MonitorCapture {
    pub slot: FrameSlot,
    _control: Option<CaptureControl<MonitorFrameHandler, HandlerError>>,
}

/// Start one persistent capture session per monitor, in the same order as
/// `monitors` — the returned `Vec` is always the same length, so callers can
/// keep zipping it against `monitors` by index (see `overlay.rs`'s
/// `freeze_and_upload`, which does exactly that).
///
/// A monitor whose capture session fails to start (e.g. a transient WGC
/// hiccup, or a monitor that's mid-disconnect) gets a `MonitorCapture` whose
/// slot simply never receives a frame, rather than aborting the whole
/// process — the overlay still comes up and works normally for every other
/// monitor.
pub fn start_all(monitors: &[MonitorInfo]) -> Vec<MonitorCapture> {
    monitors
        .iter()
        .map(|m| {
            let slot: FrameSlot = Arc::new(Mutex::new(None));
            let target = Monitor::from_raw_hmonitor(m.hmonitor as *mut std::ffi::c_void);

            let settings = Settings::new(
                target,
                CursorCaptureSettings::WithoutCursor,
                DrawBorderSettings::WithoutBorder,
                SecondaryWindowSettings::Include,
                MinimumUpdateIntervalSettings::Default,
                DirtyRegionSettings::Default,
                ColorFormat::Rgba8,
                slot.clone(),
            );

            let control = match MonitorFrameHandler::start_free_threaded(settings) {
                Ok(control) => Some(control),
                Err(e) => {
                    crate::logging::log_error(format!(
                        "Failed to start capture session for monitor {} ({}): {e}",
                        m.name, m.hmonitor
                    ));
                    None
                }
            };

            MonitorCapture {
                slot,
                _control: control,
            }
        })
        .collect()
}

/// Composite a virtual-desktop-space selection rect out of each monitor's
/// frozen frame. Monitors that don't overlap the selection are skipped;
/// monitors that partially overlap contribute only their overlapping rows.
pub fn crop_virtual_desktop(
    monitors: &[MonitorInfo],
    frames: &[Option<SharedFrame>],
    sel: PxRect,
) -> Option<image::RgbaImage> {
    let width = sel.width();
    let height = sel.height();
    if width <= 0 || height <= 0 {
        return None;
    }

    let mut out = image::RgbaImage::new(width as u32, height as u32);

    for (m, frame) in monitors.iter().zip(frames.iter()) {
        let Some(frame) = frame else { continue };

        let ix_left = m.rect.left.max(sel.left);
        let ix_top = m.rect.top.max(sel.top);
        let ix_right = m.rect.right.min(sel.right);
        let ix_bottom = m.rect.bottom.min(sel.bottom);
        if ix_left >= ix_right || ix_top >= ix_bottom {
            continue;
        }

        let row_px = (ix_right - ix_left) as usize;
        let src_x0 = (ix_left - m.rect.left) as u32;
        let dst_x0 = (ix_left - sel.left) as u32;

        let out_width = out.width();
        let buf: &mut [u8] = &mut out;

        for y in ix_top..ix_bottom {
            let src_y = (y - m.rect.top) as u32;
            let dst_y = (y - sel.top) as u32;

            let src_start = (src_y * frame.width + src_x0) as usize * 4;
            let src_end = src_start + row_px * 4;
            if src_end > frame.rgba.len() {
                continue;
            }

            let dst_start = (dst_y * out_width + dst_x0) as usize * 4;
            let dst_end = dst_start + row_px * 4;
            buf[dst_start..dst_end].copy_from_slice(&frame.rgba[src_start..src_end]);
        }
    }

    Some(out)
}
