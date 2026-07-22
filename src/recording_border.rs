//! Native Win32 marching-ants recording border.
//!
//! This deliberately does NOT go through egui/eframe. Two rounds of trying
//! got it transparent that way: first an AMD OpenGL driver crash from wgpu's
//! default backend list including `GL` (fixed separately by restricting
//! `wgpu::Backends` to `PRIMARY`), then — even with that fixed — the border
//! viewport rendered as a solid opaque box instead of a transparent
//! click-through overlay, which survived dropping `.with_mouse_passthrough`
//! from the viewport builder too. Rather than keep guessing at the
//! egui/wgpu/winit multi-viewport transparency stack, this draws the border
//! as a small dedicated `WS_EX_LAYERED` window updated via
//! `UpdateLayeredWindow`, the same reliable mechanism real screen-recording
//! overlays use — bypassing Direct3D/OpenGL/Vulkan entirely for this piece.
//! Matches this codebase's existing pattern of dropping to raw Win32 for
//! things a GUI framework doesn't do well (see `win32.rs`, and the tray
//! icon's own dedicated thread + message loop in `main.rs`).

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
    PeekMessageW, RegisterClassExW, SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage, ULW_ALPHA, UpdateLayeredWindow,
    WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::annotate;
use crate::monitors::PxRect;
use crate::win32;

const CLASS_NAME: &str = "RsnapRecordingBorder";

/// What the border is currently indicating. `Recording` marches the ants and
/// runs the timer; `Paused`/`Stopped` both freeze the ants and the timer and
/// swap the label text — the only difference is the word shown.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BorderState {
    Recording,
    Paused,
    Stopped,
}

impl BorderState {
    /// Anything other than `Recording` freezes the animation + timer.
    fn is_frozen(self) -> bool {
        !matches!(self, BorderState::Recording)
    }
}

/// Cross-thread instruction sent from `BorderHandle` to the border's own
/// message loop (`run`).
enum BorderCommand {
    SetState(BorderState),
    Close,
}

/// Handle to a running border indicator — dropping this without calling
/// `close()` leaks the thread (it'll just run until the process exits), so
/// `close()` should always be called when the border is no longer needed.
pub struct BorderHandle {
    tx: Sender<BorderCommand>,
}

impl BorderHandle {
    /// Update what the border shows (marching/frozen + label). Ignored if the
    /// thread has already exited.
    pub fn set_state(&self, state: BorderState) {
        let _ = self.tx.send(BorderCommand::SetState(state));
    }

    /// Tear the border window down and end its thread.
    pub fn close(self) {
        let _ = self.tx.send(BorderCommand::Close);
    }
}

/// Spawns the border on its own thread, sized/positioned to `rect`
/// (physical, virtual-desktop pixels), starting in the `Recording` state.
/// Fully independent of eframe's `logic()`/repaint cycle — it animates and
/// tears itself down on its own.
pub fn start(rect: PxRect) -> BorderHandle {
    let (tx, rx) = channel();
    std::thread::spawn(move || run(rect, rx));
    BorderHandle { tx }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `RegisterClassExW`'s `lpfnWndProc` needs an `extern "system"` fn pointer;
/// `windows::Win32::UI::WindowsAndMessaging::DefWindowProcW` is a plain
/// `unsafe fn` wrapper around the real extern, so it can't be passed
/// directly — this just forwards to it with the right ABI.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Body of the border's dedicated thread: creates the layered window, then
/// pumps its own message loop — driving the marching-ants animation timer
/// and draining `cmd_rx` for state/close commands from `BorderHandle` —
/// until a `Close` command or the window is destroyed.
fn run(rect: PxRect, cmd_rx: Receiver<BorderCommand>) {
    let class_name = wide(CLASS_NAME);
    let window_name = wide("rsnap-recording-border");

    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();

        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        // Errors ignored: harmless if a previous recording in this same
        // process already registered the class.
        let _ = RegisterClassExW(&wc);

        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_name.as_ptr()),
            WS_POPUP,
            rect.left,
            rect.top,
            rect.width().max(1),
            rect.height().max(1),
            None,
            None,
            Some(hinstance.into()),
            None,
        ) else {
            return;
        };

        win32::exclude_from_capture(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let started_at = Instant::now();
        // Mirror `Recorder`'s pause accounting so the border's timer + ants
        // freeze in lockstep with the actual recording: `frozen_total` is
        // completed frozen spans, `frozen_since` an open one. Effective
        // elapsed subtracts both.
        let mut state = BorderState::Recording;
        let mut frozen_total = Duration::ZERO;
        let mut frozen_since: Option<Instant> = None;
        let mut closing = false;

        loop {
            // Drain any messages Windows delivers to this window/thread —
            // we don't act on individual messages (teardown is handled
            // below via `DestroyWindow`), this just keeps the window from
            // looking hung to the OS.
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    BorderCommand::Close => closing = true,
                    BorderCommand::SetState(new_state) => {
                        // Opening a frozen span (Recording -> Paused/Stopped).
                        if !state.is_frozen() && new_state.is_frozen() {
                            frozen_since = Some(Instant::now());
                        }
                        // Closing one (Paused -> Recording).
                        if state.is_frozen()
                            && !new_state.is_frozen()
                            && let Some(since) = frozen_since.take()
                        {
                            frozen_total += since.elapsed();
                        }
                        state = new_state;
                    }
                }
            }
            if closing {
                break;
            }

            let open_frozen = frozen_since.map(|s| s.elapsed()).unwrap_or_default();
            let elapsed = started_at
                .elapsed()
                .saturating_sub(frozen_total)
                .saturating_sub(open_frozen);
            render_frame(hwnd, rect, state, elapsed);
            std::thread::sleep(Duration::from_millis(33));
        }

        let _ = DestroyWindow(hwnd);
    }
}

/// Renders the dashed marching-ants outline + "REC mm:ss" label into an
/// `image::RgbaImage` (reusing the same `image`/`imageproc`/`ab_glyph`
/// pipeline the rest of the app already uses for baking annotations), then
/// pushes it to the window as premultiplied-alpha BGRA via
/// `UpdateLayeredWindow`.
fn render_frame(hwnd: HWND, rect: PxRect, state: BorderState, elapsed: Duration) {
    let w = rect.width().max(1) as u32;
    let h = rect.height().max(1) as u32;

    let mut img = image::RgbaImage::new(w, h);
    // A frozen state keeps `elapsed` constant, so the phase (and thus the
    // ants) stop advancing on their own — no extra branch needed here.
    let phase = (elapsed.as_secs_f32() * 30.0) % 16.0;
    draw_marching_ants(&mut img, phase);

    let secs = elapsed.as_secs();
    let (word, text_color) = match state {
        BorderState::Recording => ("REC", image::Rgba([255, 80, 80, 255])),
        BorderState::Paused => ("PAUSED", image::Rgba([255, 200, 60, 255])),
        BorderState::Stopped => ("STOPPED", image::Rgba([200, 200, 200, 255])),
    };
    let label = format!("{word} {:02}:{:02}", secs / 60, secs % 60);
    imageproc::drawing::draw_filled_rect_mut(
        &mut img,
        imageproc::rect::Rect::at(4, 4).of_size((label.len() as u32 * 8).max(70), 22),
        image::Rgba([0, 0, 0, 200]),
    );
    if let Some(font) = annotate::cached_font() {
        imageproc::drawing::draw_text_mut(&mut img, text_color, 10, 8, ab_glyph::PxScale::from(14.0), font, &label);
    }

    unsafe {
        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // negative == top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(hbitmap) = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0) else {
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            return;
        };
        let old_obj = SelectObject(mem_dc, HGDIOBJ::from(hbitmap));

        // Premultiply + swap to BGRA — the same conversion `recording.rs`
        // already does for the video encoder's raw-buffer path, just with
        // premultiplication added since `ULW_ALPHA`/`AC_SRC_ALPHA` require it
        // (top-down here, unlike the encoder's bottom-up requirement).
        let dst = std::slice::from_raw_parts_mut(bits_ptr as *mut u8, (w * h * 4) as usize);
        for (src, dst) in img.pixels().zip(dst.chunks_exact_mut(4)) {
            let [r, g, b, a] = src.0;
            dst[0] = (b as u16 * a as u16 / 255) as u8;
            dst[1] = (g as u16 * a as u16 / 255) as u8;
            dst[2] = (r as u16 * a as u16 / 255) as u8;
            dst[3] = a;
        }

        let size = SIZE {
            cx: w as i32,
            cy: h as i32,
        };
        let src_pt = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            Some(screen_dc),
            None,
            Some(&size),
            Some(mem_dc),
            Some(&src_pt),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        SelectObject(mem_dc, old_obj);
        let _ = DeleteObject(HGDIOBJ::from(hbitmap));
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
}

/// Draws a dashed rectangle outline around the full image, offsetting every
/// dash by `phase` pixels so repeated calls with an increasing phase produce
/// the "marching" animation.
fn draw_marching_ants(img: &mut image::RgbaImage, phase: f32) {
    const DASH_LEN: f32 = 8.0;
    let (w, h) = img.dimensions();
    // Inset by 1px: valid pixel coordinates only go up to `w - 1`/`h - 1`,
    // and `imageproc`'s line drawing silently clips (draws nothing) for a
    // segment sitting exactly on the out-of-bounds far edge — which is why
    // only the top and left edges were ever showing up.
    let (w, h) = ((w - 1) as f32, (h - 1) as f32);
    // Classic two-tone marching ants: a solid dark line under the whole
    // outline, with white dashes marching over it. The dark base shows through
    // the gaps, so the outline stays visible on both light and dark
    // backgrounds (plain white dashes vanished on a white page).
    let base = image::Rgba([0, 0, 0, 255]);
    let dash = image::Rgba([255, 255, 255, 255]);

    let edges = [
        ((0.0, 0.0), (w, 0.0)),
        ((w, 0.0), (w, h)),
        ((w, h), (0.0, h)),
        ((0.0, h), (0.0, 0.0)),
    ];

    // Pass 1: solid dark base along every edge.
    for (start, end) in edges {
        imageproc::drawing::draw_line_segment_mut(img, start, end, base);
    }

    // Pass 2: white dashes marching over the base.
    for (start, end) in edges {
        let (sx, sy) = start;
        let (ex, ey) = end;
        let len = ((ex - sx).powi(2) + (ey - sy).powi(2)).sqrt();
        let (dx, dy) = ((ex - sx) / len, (ey - sy) / len);

        let mut t = -(phase % (DASH_LEN * 2.0));
        while t < len {
            let seg_start = t.max(0.0);
            let seg_end = (t + DASH_LEN).min(len);
            if seg_end > seg_start {
                let a = (sx + dx * seg_start, sy + dy * seg_start);
                let b = (sx + dx * seg_end, sy + dy * seg_end);
                imageproc::drawing::draw_line_segment_mut(img, a, b, dash);
            }
            t += DASH_LEN * 2.0;
        }
    }
}
