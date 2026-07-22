//! The full-virtual-desktop overlay: a single borderless, transparent,
//! always-on-top window spanning the entire virtual-desktop bounding box at
//! its true (possibly negative) origin. This is the app's central piece —
//! [`OverlayApp`] owns region-select, markup, output (copy/save/autosave/pin),
//! and screen recording.
//!
//! Roughly, in the order a capture happens:
//! - Showing the overlay snapshots each monitor's already-warm capture
//!   session (`crate::capture`) into a per-monitor "frozen" buffer and
//!   uploads it as a texture, so the overlay displays a frozen, pixel-aligned
//!   copy of the live desktop rather than a live view.
//! - Dragging out a region (clamped to real monitor rects, dead zones
//!   excluded) selects it; releasing composites that rect out of the frozen
//!   per-monitor frames (`capture::crop_virtual_desktop`).
//! - Once a selection is finalized, an in-place toolbar (`egui::Area`,
//!   painted in this same window, never a separate OS window) appears:
//!   markup tools, Copy/Save/Autosave, Pin, and Record. Drags *within* the
//!   selection perform whatever tool is active instead of adjusting the
//!   selection; drags *outside* it start a fresh selection (clearing any
//!   markup). Annotations are an append-only `Vec` with an active-count
//!   index, so undo/redo is just moving that index — see `crate::annotate`.

use std::collections::HashMap;
use std::path::PathBuf;

use crossbeam_channel::Receiver;
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::MenuEvent;
use windows::Win32::Foundation::HWND;

use crate::annotate::{self, Annotation, Tool};
use crate::capture::{self, MonitorCapture, SharedFrame};
use crate::config::{self, Config};
use crate::icons::Icon;
use crate::monitors::{DisplayLayout, PxRect};
use crate::output;
use crate::recording::{self, Recorder};
use crate::recording_border;
use crate::win32;

/// What an in-progress drag on the capture area is currently doing. Decided
/// once at `drag_started` (based on where the press landed and which tool is
/// active) and consulted on every subsequent frame of that same drag.
enum Gesture {
    None,
    /// Dragging out the initial (or a replacement) selection rectangle.
    /// Carries whatever window (if any) was under the cursor at the moment
    /// the press started, since `hover_window` itself goes stale the instant
    /// a selection exists — a plain click (negligible movement before
    /// release) snaps to this window instead of the drag's own
    /// (near-zero-size) rect.
    Selecting {
        hovered_at_press: Option<PxRect>,
    },
    /// Draw or Highlight: accumulating points along the drag path.
    DrawStroke(Vec<egui::Pos2>),
    /// Box, Circle, Arrow, Blur, or Invert Colors: only the start point
    /// matters until release, at which point start+current commits the
    /// shape.
    ShapeDrag(egui::Pos2),
    /// Erase: continuously hit-tests annotations against the pointer for as
    /// long as the drag continues.
    Erasing,
    /// No tool selected, drag started on a resize handle: `original` is the
    /// selection rect as it was before this drag began, so each frame can
    /// recompute the rect from that fixed reference rather than accumulating
    /// drift.
    ResizeSelection {
        handle: ResizeHandle,
        original: PxRect,
    },
    /// No tool selected, drag started inside the selection body (not on a
    /// handle): `grab_offset` is the press position relative to the
    /// selection's top-left, `size` is its (constant, while moving)
    /// width/height.
    MoveSelection {
        grab_offset: (i32, i32),
        size: (i32, i32),
    },
}

/// One of the 8 handles drawn around a finalized selection. Dragging a
/// corner moves both edges meeting there; dragging an edge midpoint moves
/// only that one edge, leaving the other three fixed.
#[derive(Clone, Copy)]
enum ResizeHandle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl ResizeHandle {
    /// The 8 handle points for `sel`, in virtual-desktop coordinates, in the
    /// same order as the variants above — used both for hit-testing on press
    /// and for drawing the handles in `ui()`.
    fn points(sel: PxRect) -> [(i32, i32); 8] {
        let mid_x = (sel.left + sel.right) / 2;
        let mid_y = (sel.top + sel.bottom) / 2;
        [
            (sel.left, sel.top),
            (mid_x, sel.top),
            (sel.right, sel.top),
            (sel.right, mid_y),
            (sel.right, sel.bottom),
            (mid_x, sel.bottom),
            (sel.left, sel.bottom),
            (sel.left, mid_y),
        ]
    }

    /// Which handle (if any) `(vx, vy)` is within `tolerance` pixels of.
    fn hit_test(sel: PxRect, vx: i32, vy: i32, tolerance: i32) -> Option<ResizeHandle> {
        const ALL: [ResizeHandle; 8] = [
            ResizeHandle::TopLeft,
            ResizeHandle::Top,
            ResizeHandle::TopRight,
            ResizeHandle::Right,
            ResizeHandle::BottomRight,
            ResizeHandle::Bottom,
            ResizeHandle::BottomLeft,
            ResizeHandle::Left,
        ];
        Self::points(sel)
            .iter()
            .zip(ALL)
            .find(|((hx, hy), _)| (hx - vx).abs() <= tolerance && (hy - vy).abs() <= tolerance)
            .map(|(_, handle)| handle)
    }

    /// Given the selection rect as it was before this resize gesture began,
    /// and the current pointer position, produces a new (start, current)
    /// corner pair reflecting the drag — reusing the same two-arbitrary-
    /// corners representation `drag_start`/`drag_current` already use for the
    /// initial selection-drag, rather than needing a distinct rect type.
    fn apply(self, original: PxRect, pointer: (i32, i32)) -> ((i32, i32), (i32, i32)) {
        let (px, py) = pointer;
        match self {
            ResizeHandle::TopLeft => ((px, py), (original.right, original.bottom)),
            ResizeHandle::Top => ((original.left, py), (original.right, original.bottom)),
            ResizeHandle::TopRight => ((original.left, py), (px, original.bottom)),
            ResizeHandle::Right => ((original.left, original.top), (px, original.bottom)),
            ResizeHandle::BottomRight => ((original.left, original.top), (px, py)),
            ResizeHandle::Bottom => ((original.left, original.top), (original.right, py)),
            ResizeHandle::BottomLeft => ((px, original.top), (original.right, py)),
            ResizeHandle::Left => ((px, original.top), (original.right, original.bottom)),
        }
    }

    fn cursor_icon(self) -> egui::CursorIcon {
        match self {
            ResizeHandle::TopLeft | ResizeHandle::BottomRight => egui::CursorIcon::ResizeNwSe,
            ResizeHandle::TopRight | ResizeHandle::BottomLeft => egui::CursorIcon::ResizeNeSw,
            ResizeHandle::Top | ResizeHandle::Bottom => egui::CursorIcon::ResizeVertical,
            ResizeHandle::Left | ResizeHandle::Right => egui::CursorIcon::ResizeHorizontal,
        }
    }
}

/// Which of the recording dock's three states is showing, derived each frame
/// from `(recording, recording_finished)` — see `render_recording_dock`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DockState {
    Recording,
    Paused,
    Stopped,
}

/// A button press on the recording dock, collected inside the viewport closure
/// and acted on afterward (so the closure doesn't need `&mut self`).
enum DockAction {
    Resume,
    Pause,
    Stop,
    Copy,
    Save,
    Close,
}

/// A screenshot pinned to a separate always-on-top floating window,
/// independent of the main editor overlay.
struct PinnedWindow {
    id: egui::ViewportId,
    image: image::RgbaImage,
    texture: Option<egui::TextureHandle>,
}

/// The `eframe::App` for the single full-virtual-desktop overlay window —
/// see this module's doc comment for the overall capture/markup/output flow.
/// Owns everything from region-select through annotations, pinned windows,
/// and screen recording; there is exactly one instance for the process
/// lifetime, constructed once in `main` and handed to `eframe::run_native`.
pub struct OverlayApp {
    layout: DisplayLayout,
    captures: Vec<MonitorCapture>,
    /// Snapshot of each monitor's latest frame, taken at the moment the
    /// overlay was last shown. Indexed the same as `layout.monitors`.
    frozen: Vec<Option<SharedFrame>>,
    /// One egui texture per monitor, updated in place from `frozen` rather
    /// than recreated every show.
    textures: Vec<Option<egui::TextureHandle>>,
    /// Physical pixel placement has been asserted via raw SetWindowPos at least
    /// once since the window became visible.
    placed_since_shown: bool,
    visible: bool,
    /// Counts down to 0 after a final action (Copy/Save/Pin/Cancel): while
    /// nonzero, `ui()` renders nothing (a blank transparent frame) instead of
    /// hiding immediately. Without this, hiding right after drawing the last
    /// frame's content leaves that content sitting in the GPU swapchain —
    /// and since the window isn't destroyed between shows, the *next* show
    /// briefly flashes that stale content before the fresh capture replaces
    /// it. Rendering a blank frame or two first means the stale flash is
    /// just a transparent blip instead of last session's screenshot.
    closing_frames_remaining: u8,
    drag_start: Option<(i32, i32)>,
    drag_current: Option<(i32, i32)>,
    menu_events: Receiver<MenuEvent>,
    quit_menu_id: tray_icon::menu::MenuId,
    show_menu_id: tray_icon::menu::MenuId,
    stop_recording_menu_id: tray_icon::menu::MenuId,
    settings_menu_id: tray_icon::menu::MenuId,
    /// Fires on the global Ctrl+Shift+S hotkey (registered on the tray
    /// thread — see `main::spawn_tray_icon`), draining the exact same
    /// show-overlay path as clicking the tray menu's item.
    hotkey_events: Receiver<()>,
    /// Persisted user settings (`%APPDATA%\rsnap\config.toml`) — loaded once
    /// at startup, updated whenever the Settings window is saved.
    config: Config,
    /// A working copy being edited in the Settings window, if it's open —
    /// `None` when closed. Edits apply to this copy, not `config` directly,
    /// so Cancel can discard them.
    settings_draft: Option<Config>,
    /// Set while the Settings window is waiting for the next key combo to
    /// assign to whichever shortcut was just clicked "Change..." for.
    settings_capturing: Option<ShortcutTarget>,
    /// When on, every finalized selection is written to the default autosave
    /// folder immediately, in addition to whatever the user does with
    /// Copy/Save As. Initialized from `config.autosave_enabled`, changed
    /// live when Settings is saved.
    autosave_enabled: bool,
    /// Active screen recording, if any — outlives the selection/toolbar
    /// itself (the overlay hides immediately on "Record" so the desktop
    /// stays interactive), so this lives at the app level rather than as
    /// part of the M4 markup/gesture state. Stopped via the tray menu's
    /// "Stop Recording" item.
    recording: Option<Recorder>,
    /// The native marching-ants border indicator for the active recording,
    /// if any — see `recording_border` for why this is a raw Win32 window
    /// rather than an egui viewport. Kept alive through the `Stopped` dock
    /// state (frozen, showing "STOPPED") so the region stays outlined until
    /// the dock is dismissed.
    recording_border: Option<recording_border::BorderHandle>,
    /// The finalized recording file after Stop, while the dock is still open
    /// in its "Stopped" state offering Copy/Save. `None` while actively
    /// recording (the file isn't finished yet) and once the dock is
    /// dismissed. Together with `recording`, this drives the dock's state:
    /// `recording` Some = Recording/Paused, else this Some = Stopped.
    recording_finished: Option<PathBuf>,
    /// The region being (or last) recorded, in physical virtual-desktop
    /// pixels. Remembered for the whole dock lifetime so the dock can anchor
    /// itself under the region even after Stop (when `recording` is gone), and
    /// so "Resume" from the Stopped state can start a fresh recording of the
    /// same region. Cleared when the dock is dismissed.
    recording_rect: Option<PxRect>,

    // --- Markup state ---
    gesture: Gesture,
    /// `None` = no tool selected: dragging inside the selection moves it,
    /// dragging a handle resizes it. Starts `None` after every finalized
    /// capture — the user has to explicitly pick a tool before markup drags
    /// do anything, matching how most screenshot tools default to "just let
    /// me adjust the region" rather than immediately drawing.
    tool: Option<Tool>,
    /// Append-only; `annotations[..undo_index]` are the active ones.
    /// Undo/redo just moves `undo_index`; a new annotation truncates
    /// anything past it first (standard "editing after undo clears redo").
    annotations: Vec<Annotation>,
    undo_index: usize,
    annotation_textures: Vec<Option<egui::TextureHandle>>,
    active_color: egui::Color32,
    stroke_width: f32,
    /// Pending inline text-entry popup: (anchor position, text so far).
    text_edit: Option<(egui::Pos2, String)>,
    pins: Vec<PinnedWindow>,
    next_pin_id: u64,
    /// Lucide icons (https://lucide.dev/icons), rasterized once at startup —
    /// see `crate::icons`.
    icons: HashMap<Icon, egui::TextureHandle>,
    /// Live preview while dragging Blur/Invert/Magic Erase: the actual
    /// processed pixels (not just an outline), reused in place each frame
    /// via `TextureHandle::set` rather than allocating a fresh GPU texture.
    /// `preview_rect` is the local-space rect it was last computed for, so
    /// we only recompute when the drag has moved meaningfully instead of on
    /// every single frame.
    shape_preview_tex: Option<egui::TextureHandle>,
    shape_preview_rect: Option<egui::Rect>,
    /// Sampled once when a Magic Erase stroke begins (the pixel color under
    /// the press point) and reused for every point in that stroke, so the
    /// brush paints a single flat color rather than continuously resampling
    /// whatever's under the cursor as it moves.
    magic_erase_color: Option<egui::Color32>,
    /// The window under the cursor, recomputed every frame while no
    /// selection exists yet — lets a plain click (rather than a drag) grab
    /// that whole window as the capture region instead of a manual rect.
    hover_window: Option<PxRect>,
}

impl OverlayApp {
    /// Builds the app's initial state. Called once, from the `eframe::run_native`
    /// closure in `main`, after `capture::start_all` has already warmed up
    /// every monitor's capture session and the tray/hotkey plumbing exists —
    /// this just wires those together with fresh, empty markup/recording/pin
    /// state and the loaded `config`.
    pub fn new(
        layout: DisplayLayout,
        captures: Vec<MonitorCapture>,
        quit_menu_id: tray_icon::menu::MenuId,
        show_menu_id: tray_icon::menu::MenuId,
        stop_recording_menu_id: tray_icon::menu::MenuId,
        settings_menu_id: tray_icon::menu::MenuId,
        icons: HashMap<Icon, egui::TextureHandle>,
        hotkey_events: Receiver<()>,
        config: Config,
    ) -> Self {
        let monitor_count = layout.monitors.len();
        let [r, g, b] = config.default_color;
        Self {
            layout,
            captures,
            frozen: vec![None; monitor_count],
            textures: (0..monitor_count).map(|_| None).collect(),
            placed_since_shown: false,
            visible: false,
            closing_frames_remaining: 0,
            drag_start: None,
            drag_current: None,
            menu_events: MenuEvent::receiver().clone(),
            quit_menu_id,
            show_menu_id,
            stop_recording_menu_id,
            settings_menu_id,
            hotkey_events,
            autosave_enabled: config.autosave_enabled,
            recording: None,
            recording_border: None,
            recording_finished: None,
            recording_rect: None,
            gesture: Gesture::None,
            tool: config.default_tool,
            annotations: Vec::new(),
            undo_index: 0,
            annotation_textures: Vec::new(),
            active_color: egui::Color32::from_rgb(r, g, b),
            stroke_width: config.default_stroke_width,
            text_edit: None,
            pins: Vec::new(),
            next_pin_id: 0,
            icons,
            shape_preview_tex: None,
            shape_preview_rect: None,
            magic_erase_color: None,
            hover_window: None,
            settings_draft: None,
            settings_capturing: None,
            config,
        }
    }

    /// Snapshot each monitor's current warm capture into `frozen`, and push it
    /// into that monitor's egui texture. Called once when the overlay
    /// transitions from hidden to shown, so the overlay displays a frozen
    /// frame rather than continuously updating like a live view.
    fn freeze_and_upload(&mut self, ctx: &egui::Context) {
        for i in 0..self.captures.len() {
            let Ok(guard) = self.captures[i].slot.lock() else {
                // Poisoned lock — treat like "no frame yet" for this monitor
                // rather than propagating the panic into the UI thread; the
                // slot's writer thread is presumably in a bad state, but the
                // rest of the overlay can carry on.
                continue;
            };
            let Some(frame) = guard.clone() else {
                continue;
            };
            drop(guard);

            if capture::looks_suspiciously_blank(&frame) {
                // WGC occasionally hands us a spurious near-blank frame (DWM
                // fade transitions, secure-desktop prompts, monitor
                // sleep/wake races). Rather than freeze that and show a
                // broken white screen, keep whatever we last successfully
                // froze for this monitor, if any — that's the still-valid
                // GPU texture from a prior show even if `frozen[i]` itself
                // was cleared to free memory when the overlay last closed
                // (see `finish_capture`). Only if there's neither a frozen
                // buffer nor an existing texture is there truly nothing to
                // fall back to, in which case `ui()`'s debug tint covers for
                // it instead of a broken white screen.
                continue;
            }

            let color_image =
                egui::ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &frame.rgba);
            match &mut self.textures[i] {
                Some(tex) => tex.set(color_image, egui::TextureOptions::LINEAR),
                None => {
                    self.textures[i] =
                        Some(ctx.load_texture(format!("monitor-{i}"), color_image, egui::TextureOptions::LINEAR));
                }
            }
            self.frozen[i] = Some(frame);
        }
    }

    /// The raw `HWND` for this window, via `raw-window-handle` — needed for
    /// the Win32 placement/topmost/capture-exclusion calls that bypass
    /// winit's own APIs (see `win32`).
    fn hwnd(&self, frame: &eframe::Frame) -> Option<HWND> {
        match frame.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => Some(HWND(isize::from(h.hwnd) as *mut _)),
            _ => None,
        }
    }

    /// Force the window to exactly the virtual-desktop bounding rect in
    /// physical pixels, bypassing winit's own (DPI-aware) placement logic.
    fn assert_placement(&self, hwnd: HWND) {
        let vd = self.layout.virtual_desktop;
        win32::place_window(hwnd, vd.left, vd.top, vd.width(), vd.height());
        // Both idempotent — safe to call on every show, not just the first.
        win32::exclude_from_capture(hwnd);
        win32::disable_show_hide_animation(hwnd);
    }

    /// The current selection as a normalized rect (`drag_start`/`drag_current`
    /// in either order), or `None` if no drag has happened yet this show.
    fn selection_rect(&self) -> Option<PxRect> {
        let (sx, sy) = self.drag_start?;
        let (cx, cy) = self.drag_current?;
        Some(PxRect {
            left: sx.min(cx),
            top: sy.min(cy),
            right: sx.max(cx),
            bottom: sy.max(cy),
        })
    }

    /// Window-local point -> virtual-desktop point, matching how the
    /// selection/annotation coordinates relate to the physical window
    /// placement (see `ui()`'s `to_local`).
    fn to_virtual(&self, p: egui::Pos2) -> (i32, i32) {
        let vd = self.layout.virtual_desktop;
        (p.x as i32 + vd.left, p.y as i32 + vd.top)
    }

    /// Composite `sel` out of the frozen per-monitor frames into one image.
    /// `None` if the selection is degenerate (zero-size) or nothing has been
    /// frozen yet for the monitors it covers.
    fn crop_selection(&self, sel: PxRect) -> Option<image::RgbaImage> {
        if sel.width() <= 1 || sel.height() <= 1 {
            return None;
        }
        capture::crop_virtual_desktop(&self.layout.monitors, &self.frozen, sel)
    }

    /// Reads the single pixel color at `pos` (local window coordinates) out
    /// of the frozen frame — used by the Magic Erase tool, which fills its
    /// whole dragged region with whatever color was under the drag's start
    /// point rather than doing any real content-aware fill.
    fn sample_pixel(&self, pos: egui::Pos2) -> egui::Color32 {
        let (vx, vy) = self.to_virtual(pos);
        let sel = PxRect {
            left: vx,
            top: vy,
            right: vx + 2,
            bottom: vy + 2,
        };
        self.crop_selection(sel)
            .map(|img| {
                let p = img.get_pixel(0, 0);
                egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3])
            })
            .unwrap_or(egui::Color32::BLACK)
    }

    /// Same as `crop_selection`, but with every active annotation baked in
    /// as real pixels — what Copy/Save/Autosave/Pin actually use, so the
    /// markup goes out with the screenshot rather than just being an
    /// on-screen-only overlay.
    fn composite_selection(&self, sel: PxRect) -> Option<image::RgbaImage> {
        let mut img = self.crop_selection(sel)?;
        let vd = self.layout.virtual_desktop;
        let origin = egui::vec2((sel.left - vd.left) as f32, (sel.top - vd.top) as f32);
        annotate::bake_annotations(
            &mut img,
            &self.annotations,
            self.undo_index,
            origin,
            self.config.smooth_strokes,
        );
        Some(img)
    }

    /// Commits a new annotation: truncates any redo-able tail past
    /// `undo_index` (standard "editing after undo clears redo"), appends
    /// `ann`, and advances `undo_index` past it. Also enforces
    /// `Config::undo_history_limit` — see the comment below.
    fn push_annotation(&mut self, ann: Annotation) {
        self.annotations.truncate(self.undo_index);
        self.annotation_textures.truncate(self.undo_index);
        self.annotations.push(ann);
        self.annotation_textures.push(None);
        self.undo_index += 1;

        // Bound memory on a very long markup session: once past the
        // configured limit, permanently drop the oldest annotations rather
        // than just leaving them unreachable past `undo_index`. Each
        // annotation's geometry is stored in absolute coordinates (not
        // relative to any other), so dropping the oldest is always safe.
        let limit = self.config.undo_history_limit;
        if limit > 0 && self.annotations.len() > limit {
            let excess = self.annotations.len() - limit;
            self.annotations.drain(0..excess);
            self.annotation_textures.drain(0..excess);
            self.undo_index -= excess;
        }
    }

    /// Drops all markup for the current selection — called when a fresh
    /// selection is started (or the current one is deselected), since
    /// annotation geometry only makes sense relative to the selection it was
    /// drawn on.
    fn clear_annotations(&mut self) {
        self.annotations.clear();
        self.annotation_textures.clear();
        self.undo_index = 0;
    }

    /// Removes the topmost annotation under `pos`, if any. Note this is an
    /// immediate mutation, not itself an undoable "erase" step — undo/redo
    /// still works for everything drawn up to that point, but the erase
    /// itself can't be redone. A reasonable v1 tradeoff given a proper
    /// "erase as its own undo step" model would need annotations to support
    /// tombstoning rather than a flat active-count index.
    fn erase_at(&mut self, pos: egui::Pos2) {
        if let Some(idx) = self.annotations[..self.undo_index]
            .iter()
            .rposition(|a| a.hit_test(pos, 6.0))
        {
            self.annotations.remove(idx);
            self.annotation_textures.remove(idx);
            self.undo_index -= 1;
        }
    }

    /// Steps `undo_index` back by one, hiding the most recently drawn
    /// annotation without discarding it (see `push_annotation`).
    fn undo(&mut self) {
        if self.undo_index > 0 {
            self.undo_index -= 1;
        }
    }

    /// Steps `undo_index` forward by one, restoring the next undone
    /// annotation, if any.
    fn redo(&mut self) {
        if self.undo_index < self.annotations.len() {
            self.undo_index += 1;
        }
    }

    /// Flips the overlay to visible with a clean slate — shared by the tray
    /// menu's "Show Overlay" item and the global Ctrl+Shift+S hotkey.
    fn show_overlay(&mut self, ctx: &egui::Context) {
        self.visible = true;
        self.placed_since_shown = false;
        self.closing_frames_remaining = 0;
        self.drag_start = None;
        self.drag_current = None;
        self.tool = None;
        self.clear_annotations();
        ctx.request_repaint();
    }

    /// Clears the selection and hides the overlay — the standard "we're done
    /// with this capture" endpoint after Copy, Save As, Pin, or Cancel.
    ///
    /// Releases the monitor textures (freeing their GPU memory between
    /// shows) and renders a couple of blank frames before actually hiding,
    /// rather than hiding immediately — see `closing_frames_remaining`'s doc
    /// comment for why.
    fn finish_capture(&mut self, ctx: &egui::Context) {
        self.drag_start = None;
        self.drag_current = None;
        self.gesture = Gesture::None;
        self.tool = None;
        self.text_edit = None;
        self.clear_annotations();
        // Free the CPU-side frame buffers (the bulk of the memory — 4
        // monitors' worth of raw RGBA pixels) but deliberately leave the GPU
        // texture *handles* in `self.textures` alone: clearing those too
        // forced a brand new texture to be created on the next show, and a
        // texture's pixel data isn't actually uploaded to the GPU until the
        // end of the frame it's created on — so the very first frame that
        // painted it sampled a blank/default (white) texture instead,
        // flashing white right as the overlay reappeared. Reusing the
        // existing handle via `tex.set(...)` next time (already what
        // `freeze_and_upload` does) updates it in place with no such gap.
        self.frozen.iter_mut().for_each(|f| *f = None);
        self.closing_frames_remaining = 2;
        ctx.request_repaint();
    }
}

impl eframe::App for OverlayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Fully transparent clear so dead zones (and anything we don't paint
        // over) show the real desktop through the window.
        [0.0, 0.0, 0.0, 0.0]
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Runs every pass regardless of window visibility (unlike `ui`, which
        // eframe only calls while the window is actually shown) — this is
        // where tray events must be drained, since the whole point of the
        // tray "Show Overlay" click is to flip an initially-hidden window to
        // visible. If this lived in `ui` instead, it would never run at all
        // while hidden, and the tray menu would appear to do nothing. Pinned
        // windows are rendered here too, for the same reason: they must keep
        // showing even while the main overlay is hidden.

        // Belt-and-suspenders: never let egui/winit auto-detect scaling rescale
        // our physical-pixel coordinate space.
        ctx.set_pixels_per_point(1.0);

        // --- Tray/hotkey event draining ---

        // Drain tray menu events (show/stop-recording/quit) each frame. The
        // "Stop Recording" item always exists in the menu (tray-icon has no
        // runtime add/remove), so it's simply a no-op when `self.recording`
        // is already `None`.
        while let Ok(event) = self.menu_events.try_recv() {
            if event.id == self.quit_menu_id {
                std::process::exit(0);
            } else if event.id == self.show_menu_id {
                self.show_overlay(ctx);
            } else if event.id == self.stop_recording_menu_id {
                self.stop_recording();
            } else if event.id == self.settings_menu_id {
                let mut draft = self.config.clone();
                // Reflect actual registry state rather than trusting the
                // config file alone, in case the Run-key entry was removed
                // some other way (e.g. a third-party startup manager).
                draft.start_with_windows = win32::is_start_with_windows_enabled();
                self.settings_draft = Some(draft);
            }
        }

        // Drain the global Ctrl+Shift+S hotkey the same way — same
        // show-overlay path, just a different trigger.
        while self.hotkey_events.try_recv().is_ok() {
            self.show_overlay(ctx);
        }

        // --- Closing countdown + placement/visibility ---

        // Counting down after a final action (Copy/Save/Pin/Cancel) — see
        // `closing_frames_remaining`'s doc comment. `ui()` renders nothing
        // while this is nonzero; once it reaches 0, actually move off-screen.
        let was_closing = self.closing_frames_remaining > 0;
        if was_closing {
            self.closing_frames_remaining -= 1;
            ctx.request_repaint();
        }
        let should_hide_now = was_closing && self.closing_frames_remaining == 0;

        if let Some(hwnd) = self.hwnd(frame) {
            if self.visible && !self.placed_since_shown {
                self.assert_placement(hwnd);
                self.freeze_and_upload(ctx);
                self.placed_since_shown = true;
            } else if self.visible {
                // Other apps' own topmost windows (notification toasts, chat
                // popups, etc.) can reclaim the top Z-order spot after we set
                // it once — reassert every frame while shown so the overlay
                // can't silently end up behind them.
                win32::keep_on_top(hwnd);
            }
            if should_hide_now {
                // Deliberately not `ViewportCommand::Visible(false)` — see
                // `win32::move_offscreen`'s doc comment for why we never
                // actually hide the window at the OS level at all.
                self.visible = false;
                win32::move_offscreen(hwnd);
            }
        }

        // --- Always-on sub-windows ---
        self.render_pins(ctx);
        self.render_settings_window(ctx);
        self.render_recording_dock(ctx);

        // --- Recording frame sampling ---

        // Sample the live desktop into the active recording, if any — reads
        // straight from the still-warm per-monitor capture sessions rather
        // than `self.frozen` (which only updates when the overlay itself is
        // shown, and a recording keeps running with the overlay hidden).
        if let Some(recorder) = &mut self.recording {
            let live_frames: Vec<Option<SharedFrame>> = self
                .captures
                .iter()
                .map(|c| c.slot.lock().ok().and_then(|guard| guard.clone()))
                .collect();
            recorder.maybe_push_frame(&self.layout.monitors, &live_frames);
        }

        // --- Repaint scheduling ---

        // Keep polling for tray events even while hidden/idle. While visible
        // or recording, repaint at a tighter interval — for `keep_on_top`
        // above, and so the recording sampling loop actually keeps up with
        // its target frame rate instead of being throttled to 10fps.
        // Keep the tight interval while the dock is open too (including its
        // Stopped state, where `recording` is already `None`) so its buttons
        // stay responsive.
        let dock_open = self.recording.is_some() || self.recording_finished.is_some();
        let interval = if self.visible || dock_open { 16 } else { 100 };
        ctx.request_repaint_after(std::time::Duration::from_millis(interval));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Only called while the window is visible (see `logic` above for the
        // always-runs half of the update).

        if self.closing_frames_remaining > 0 || !self.visible {
            // Render nothing (clear_color is fully transparent) — see
            // `closing_frames_remaining`'s doc comment for why. Checking
            // `!self.visible` too, not just the countdown, matters for the
            // exact frame the countdown reaches 0: `logic()` (which always
            // runs first) flips `self.visible` to false and clears the
            // monitor textures to `None` on that same frame, but eframe's
            // own external visibility flag — and so whether `ui()` gets
            // called at all — hasn't caught up to our just-sent hide command
            // yet. Without this, that one frame still ran the normal draw
            // loop against now-empty textures, hitting the debug-tint
            // fallback and flashing a burst of colorful rectangles right
            // before the window actually disappeared.
            return;
        }

        let ctx = ui.ctx().clone();

        // --- Hover-window recompute ---

        // Recompute the hover-highlighted window every frame while no
        // selection exists yet — cheap (one WinAPI call), and drives both
        // the dim-overlay preview below and the click-to-select-window
        // handling further down.
        self.hover_window = if self.selection_rect().is_none() {
            ctx.input(|i| i.pointer.interact_pos()).and_then(|pos| {
                let (vx, vy) = self.to_virtual(pos);
                win32::window_rect_at(vx, vy).map(|(x, y, w, h)| PxRect {
                    left: x,
                    top: y,
                    right: x + w,
                    bottom: y + h,
                })
            })
        } else {
            None
        };

        let vd = self.layout.virtual_desktop;

        // --- Monitor image draw ---

        // `ui` here already spans the whole viewport (eframe hands us a
        // background-layer root Ui sized to the full window) so we paint
        // straight onto it rather than nesting another panel.
        ui.set_clip_rect(ui.max_rect());
        let painter = ui.painter();

        // Window-local origin (0,0) corresponds to virtual-desktop
        // coordinate (vd.left, vd.top) because we placed the window
        // there exactly, in physical pixels, with pixels_per_point=1.0.
        let to_local = |vx: i32, vy: i32| -> egui::Pos2 { egui::pos2((vx - vd.left) as f32, (vy - vd.top) as f32) };

        let palette = [
            egui::Color32::from_rgba_unmultiplied(60, 120, 220, 90),
            egui::Color32::from_rgba_unmultiplied(220, 120, 60, 90),
            egui::Color32::from_rgba_unmultiplied(60, 200, 120, 90),
            egui::Color32::from_rgba_unmultiplied(200, 60, 200, 90),
            egui::Color32::from_rgba_unmultiplied(220, 220, 60, 90),
        ];

        for (i, m) in self.layout.monitors.iter().enumerate() {
            let tl = to_local(m.rect.left, m.rect.top);
            let br = to_local(m.rect.right, m.rect.bottom);
            let rect = egui::Rect::from_min_max(tl, br);

            if let Some(tex) = &self.textures[i] {
                // The real thing: a frozen, pixel-aligned copy of what was
                // actually on screen for this monitor.
                painter.image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                // No frame has arrived from this monitor's capture session
                // yet (e.g. very first show right after startup, or its
                // session failed to start — see `capture::start_all`) — fall
                // back to a solid per-monitor tint so the overlay is never
                // blank.
                let color = palette[i % palette.len()];
                painter.rect_filled(rect, 0.0, color);
                painter.rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::WHITE),
                    egui::StrokeKind::Inside,
                );
            }

            let label = format!(
                "{}{}\n{}x{} @ ({}, {})\nDPI {}x{} (scale {:.2})",
                m.name,
                if m.is_primary { "  [PRIMARY]" } else { "" },
                m.rect.width(),
                m.rect.height(),
                m.rect.left,
                m.rect.top,
                m.dpi_x,
                m.dpi_y,
                m.scale_factor()
            );
            let label_pos = tl + egui::vec2(12.0, 12.0);
            let label_font = egui::FontId::proportional(16.0);
            let galley = painter.layout_no_wrap(label.clone(), label_font.clone(), egui::Color32::WHITE);
            painter.rect_filled(
                egui::Rect::from_min_size(label_pos, galley.size()).expand(4.0),
                4.0,
                egui::Color32::from_black_alpha(160),
            );
            painter.text(
                label_pos,
                egui::Align2::LEFT_TOP,
                label,
                label_font,
                egui::Color32::WHITE,
            );
        }

        // --- Dimming "spotlight" ---

        // Dimming "spotlight" overlay: covers everything except the current
        // selection, so the selection reads as a clear portal through to the
        // crisp frozen image while everything else is dimmed. Before any
        // selection exists, the whole virtual desktop is dimmed uniformly;
        // once one exists, it's framed with up to four non-overlapping rects
        // around the selection "hole" instead of one rect covering it (there's
        // no way to punch a literal hole in a single filled rect).
        let dim_color = egui::Color32::from_black_alpha(140);
        let full_rect = egui::Rect::from_min_max(to_local(vd.left, vd.top), to_local(vd.right, vd.bottom));
        match self.selection_rect() {
            None => {
                // Before any selection exists, hovering a window (see
                // `window_rect_at`) previews it as the click target the same
                // way an in-progress selection previews a drag: a portal
                // hole through the dimming, framed with a highlight border,
                // so it's clear what a click will capture before it happens.
                match self.hover_window {
                    Some(hover) => {
                        let hover_rect = egui::Rect::from_min_max(
                            to_local(hover.left, hover.top),
                            to_local(hover.right, hover.bottom),
                        );
                        paint_dim_with_hole(painter, full_rect, hover_rect, dim_color);
                        painter.rect_stroke(
                            hover_rect,
                            0.0,
                            egui::Stroke::new(2.0, egui::Color32::from_rgb(90, 170, 255)),
                            egui::StrokeKind::Inside,
                        );
                    }
                    None => {
                        painter.rect_filled(full_rect, 0.0, dim_color);
                    }
                }
            }
            Some(sel) => {
                let sel_rect = egui::Rect::from_min_max(to_local(sel.left, sel.top), to_local(sel.right, sel.bottom));
                paint_dim_with_hole(painter, full_rect, sel_rect, dim_color);
            }
        }

        // --- Annotation + live gesture preview painting ---

        // Active markup, drawn on top of the frozen desktop content.
        annotate::paint_annotations(
            &ctx,
            painter,
            &self.annotations,
            self.undo_index,
            &mut self.annotation_textures,
            self.config.smooth_strokes,
        );

        // In-progress freeform stroke, drawn live while dragging.
        if let Gesture::DrawStroke(points) = &self.gesture
            && points.len() >= 2
        {
            let (color, width) = if self.tool == Some(Tool::Highlight) {
                (
                    egui::Color32::from_rgba_unmultiplied(
                        self.active_color.r(),
                        self.active_color.g(),
                        self.active_color.b(),
                        90,
                    ),
                    self.stroke_width * 4.0,
                )
            } else if self.tool == Some(Tool::MagicErase) {
                (
                    self.magic_erase_color.unwrap_or(egui::Color32::BLACK),
                    self.stroke_width,
                )
            } else {
                (self.active_color, self.stroke_width)
            };
            let drawn = if self.config.smooth_strokes {
                annotate::smooth_points(points)
            } else {
                points.clone()
            };
            painter.add(egui::Shape::line(drawn, egui::Stroke::new(width, color)));
        }

        // In-progress shape drag (Box/Circle/Arrow/Blur/Invert Colors),
        // drawn live while dragging so it doesn't feel unresponsive between
        // press and release.
        if let Gesture::ShapeDrag(start) = &self.gesture
            && let Some(pos) = ctx.input(|i| i.pointer.interact_pos())
        {
            let rect = egui::Rect::from_two_pos(*start, pos);
            match self.tool {
                Some(Tool::Box) => {
                    painter.rect_stroke(
                        rect,
                        0.0,
                        egui::Stroke::new(self.stroke_width, self.active_color),
                        egui::StrokeKind::Inside,
                    );
                }
                Some(Tool::Circle) => {
                    painter.add(egui::Shape::ellipse_stroke(
                        rect.center(),
                        rect.size() / 2.0,
                        egui::Stroke::new(self.stroke_width, self.active_color),
                    ));
                }
                Some(Tool::Arrow) => {
                    annotate::draw_arrow(painter, *start, pos, self.active_color, self.stroke_width);
                }
                Some(Tool::Blur) | Some(Tool::InvertColors) => {
                    // Only recompute the actual processed preview when
                    // the rect has moved meaningfully — re-cropping and
                    // re-processing on literally every frame reintroduces
                    // the same per-frame cost problem freeform drawing
                    // had to be fixed for. A stale-by-a-few-pixels
                    // preview is imperceptible; the outline always
                    // tracks the pointer exactly regardless.
                    let stale = match self.shape_preview_rect {
                        Some(prev) => (prev.min - rect.min).length() > 4.0 || (prev.max - rect.max).length() > 4.0,
                        None => true,
                    };
                    if stale && rect.width() >= 2.0 && rect.height() >= 2.0 {
                        let (vx0, vy0) = self.to_virtual(rect.left_top());
                        let (vx1, vy1) = self.to_virtual(rect.right_bottom());
                        let vsel = PxRect {
                            left: vx0.min(vx1),
                            top: vy0.min(vy1),
                            right: vx0.max(vx1),
                            bottom: vy0.max(vy1),
                        };
                        if let Some(cropped) = self.crop_selection(vsel) {
                            let small = downscale_for_preview(&cropped);
                            let preview_src = if self.tool == Some(Tool::Blur) {
                                annotate::blur_image(&small, self.stroke_width)
                            } else {
                                annotate::invert_image(&small)
                            };
                            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                                [preview_src.width() as usize, preview_src.height() as usize],
                                preview_src.as_raw(),
                            );
                            match &mut self.shape_preview_tex {
                                Some(tex) => tex.set(color_image, egui::TextureOptions::LINEAR),
                                None => {
                                    self.shape_preview_tex = Some(ctx.load_texture(
                                        "shape-preview",
                                        color_image,
                                        egui::TextureOptions::LINEAR,
                                    ));
                                }
                            }
                        }
                        self.shape_preview_rect = Some(rect);
                    }
                    if let Some(tex) = &self.shape_preview_tex {
                        painter.image(
                            tex.id(),
                            rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    }
                    painter.rect_stroke(
                        rect,
                        0.0,
                        egui::Stroke::new(1.5, egui::Color32::WHITE),
                        egui::StrokeKind::Inside,
                    );
                }
                _ => {}
            }
        }

        // --- Gesture state machine (press/hold/release) ---

        // The single interact zone driving both selection and markup. Once a
        // selection exists, a drag starting inside it performs the active
        // tool's action; a drag starting outside it (but on a real monitor)
        // starts a brand new selection, clearing any markup.
        //
        // Deliberately NOT using `response.drag_started()/dragged()/
        // drag_stopped()/clicked()` here: egui only promotes a press to
        // "decidedly dragging" once the pointer has moved past a distance
        // threshold *or* `max_click_duration` (0.8s!) has elapsed, since it
        // can't yet tell a click from the start of a drag. That's fine for
        // ordinary buttons, but for a markup tool it meant every stroke had
        // an up-to-800ms hesitation before anything appeared — "I have to
        // hold the mouse down for a moment before I can start drawing".
        // Reading raw pointer press/hold/release state instead starts the
        // gesture on the very first frame of the press, with zero threshold.
        // `is_pointer_button_down_on()` still respects layering, so a press
        // that lands on the toolbar (a higher Order::Foreground layer) is
        // correctly not seen as "ours" here.
        let response = ui.interact(
            ui.max_rect(),
            ui.id().with("overlay_drag"),
            egui::Sense::click_and_drag(),
        );

        let pressed_here = response.is_pointer_button_down_on() && ctx.input(|i| i.pointer.primary_pressed());
        let held_down = ctx.input(|i| i.pointer.primary_down());
        let released = ctx.input(|i| i.pointer.primary_released());
        let pointer_pos = ctx.input(|i| i.pointer.interact_pos());

        if pressed_here && let Some(pos) = pointer_pos {
            let (vx, vy) = self.to_virtual(pos);
            let current_sel = self.selection_rect();
            let inside_selection = current_sel.is_some_and(|s| s.contains_point(vx, vy));
            let handle_hit = current_sel
                .filter(|_| self.tool.is_none())
                .and_then(|s| ResizeHandle::hit_test(s, vx, vy, 10));

            if let (Some(sel), Some(handle)) = (current_sel, handle_hit) {
                // No tool selected, and the press landed on one of the
                // selection's own handles — resize instead of starting
                // new markup or a new selection.
                self.gesture = Gesture::ResizeSelection { handle, original: sel };
            } else if inside_selection && self.tool.is_none() {
                // No tool selected: drag the selection body to move it.
                let sel = current_sel.expect("inside_selection implies a selection exists");
                self.gesture = Gesture::MoveSelection {
                    grab_offset: (vx - sel.left, vy - sel.top),
                    size: (sel.width(), sel.height()),
                };
            } else if inside_selection && self.tool.is_some() {
                let tool = self.tool.expect("just checked is_some");
                self.gesture = match tool {
                    Tool::Draw | Tool::Highlight => Gesture::DrawStroke(vec![pos]),
                    Tool::MagicErase => {
                        self.magic_erase_color = Some(self.sample_pixel(pos));
                        Gesture::DrawStroke(vec![pos])
                    }
                    Tool::Box | Tool::Circle | Tool::Arrow | Tool::Blur | Tool::InvertColors => {
                        self.shape_preview_rect = None;
                        Gesture::ShapeDrag(pos)
                    }
                    Tool::Erase => Gesture::Erasing,
                    Tool::Text | Tool::Number => Gesture::None,
                };
                // Text/Number act immediately on press — there's no
                // drag variant of either, so there's nothing to gain by
                // waiting for release.
                match tool {
                    Tool::Text => self.text_edit = Some((pos, String::new())),
                    Tool::Number => {
                        let n = 1 + self.annotations[..self.undo_index]
                            .iter()
                            .filter(|a| matches!(a, Annotation::Number { .. }))
                            .count() as u32;
                        self.push_annotation(Annotation::Number {
                            pos,
                            n,
                            color: self.active_color,
                        });
                    }
                    _ => {}
                }
            } else if self.layout.point_on_real_monitor(vx, vy) {
                self.gesture = Gesture::Selecting {
                    hovered_at_press: self.hover_window,
                };
                self.drag_start = Some((vx, vy));
                self.drag_current = Some((vx, vy));
                self.tool = None;
                self.clear_annotations();
            }
        }

        if held_down {
            // Computed up front rather than inside the match arms below:
            // some arms bind references into `self.gesture` (e.g. `handle`,
            // `original`), and those stay borrowed for the rest of the arm —
            // calling `self.to_virtual(pos)` (which needs the whole `&self`)
            // partway through would conflict with that still-live borrow.
            let virtual_pos = pointer_pos.map(|p| self.to_virtual(p));

            match &mut self.gesture {
                Gesture::Selecting { .. } => {
                    if let Some((vx, vy)) = virtual_pos {
                        self.drag_current = Some(self.layout.clamp_to_real_monitor(vx, vy));
                    }
                }
                Gesture::DrawStroke(points) => {
                    if let Some(pos) = pointer_pos {
                        // Decimate: only record a point once the cursor has
                        // moved a meaningful distance from the last one.
                        // Without this, a slow/careful stroke racks up a
                        // point per mouse-move event (hundreds to
                        // thousands), and every one of those points gets
                        // fully re-tessellated into a renderable mesh on
                        // *every* frame, for *every* stroke drawn so far —
                        // not just the in-progress one. That's what caused
                        // the "insane lag" while drawing.
                        const MIN_POINT_SPACING: f32 = 2.0;
                        let should_add = points
                            .last()
                            .is_none_or(|&last| last.distance(pos) >= MIN_POINT_SPACING);
                        if should_add {
                            points.push(pos);
                        }
                    }
                }
                Gesture::Erasing => {
                    if let Some(pos) = pointer_pos {
                        self.erase_at(pos);
                    }
                }
                Gesture::ResizeSelection { handle, original } => {
                    if let Some(pointer) = virtual_pos {
                        let (start, current) = handle.apply(*original, pointer);
                        self.drag_start = Some(start);
                        self.drag_current = Some(current);
                    }
                }
                Gesture::MoveSelection { grab_offset, size } => {
                    if let Some((vx, vy)) = virtual_pos {
                        let new_left = vx - grab_offset.0;
                        let new_top = vy - grab_offset.1;
                        self.drag_start = Some((new_left, new_top));
                        self.drag_current = Some((new_left + size.0, new_top + size.1));
                    }
                }
                Gesture::ShapeDrag(_) | Gesture::None => {}
            }
        }

        if released {
            let finished = std::mem::replace(&mut self.gesture, Gesture::None);
            match finished {
                Gesture::Selecting { hovered_at_press } => {
                    // A plain click (negligible movement between press and
                    // release) over a window snaps the whole window in as
                    // the selection, instead of leaving the degenerate
                    // near-zero-size rect the drag itself produced — the
                    // "click a window to capture it" flow most snip tools
                    // support alongside manual dragging.
                    let was_click = match (self.drag_start, self.drag_current) {
                        (Some((sx, sy)), Some((cx, cy))) => (sx - cx).abs() <= 4 && (sy - cy).abs() <= 4,
                        _ => false,
                    };
                    if was_click && let Some(win) = hovered_at_press {
                        self.drag_start = Some((win.left, win.top));
                        self.drag_current = Some((win.right, win.bottom));
                    }
                }
                Gesture::MoveSelection { .. } | Gesture::ResizeSelection { .. } => {}
                Gesture::DrawStroke(points) => {
                    if points.len() >= 2 {
                        let color = if self.tool == Some(Tool::MagicErase) {
                            self.magic_erase_color.unwrap_or(egui::Color32::BLACK)
                        } else {
                            self.active_color
                        };
                        self.push_annotation(Annotation::Stroke {
                            points,
                            color,
                            width: self.stroke_width,
                            highlight: self.tool == Some(Tool::Highlight),
                        });
                    }
                }
                Gesture::ShapeDrag(start) => {
                    if let Some(pos) = pointer_pos {
                        self.commit_shape_drag(start, pos);
                    }
                }
                Gesture::Erasing => {}
                Gesture::None => {}
            }
        }

        // --- Cursor ---

        // Reflect the current gesture/hover state in the cursor: a grab hand
        // over the selection body (or "grabbing" while actively dragging it),
        // the matching directional resize cursor over a handle, and the
        // default arrow everywhere else (over a tool-in-progress, no
        // selection yet, etc).
        let hover_cursor = match &self.gesture {
            Gesture::MoveSelection { .. } => Some(egui::CursorIcon::Grabbing),
            Gesture::ResizeSelection { handle, .. } => Some(handle.cursor_icon()),
            _ if self.tool.is_none() => pointer_pos.and_then(|pos| {
                let (vx, vy) = self.to_virtual(pos);
                let sel = self.selection_rect()?;
                if let Some(handle) = ResizeHandle::hit_test(sel, vx, vy, 10) {
                    Some(handle.cursor_icon())
                } else if sel.contains_point(vx, vy) {
                    Some(egui::CursorIcon::Grab)
                } else {
                    None
                }
            }),
            _ => None,
        };
        if let Some(cursor) = hover_cursor {
            ctx.set_cursor_icon(cursor);
        }

        // --- Text-entry popup ---

        // Inline text-entry popup for the Text tool.
        let mut commit_text: Option<(egui::Pos2, String)> = None;
        let mut cancel_text = false;
        if let Some((pos, text)) = &mut self.text_edit {
            egui::Area::new(egui::Id::new("rsnap_text_edit"))
                .fixed_pos(*pos)
                .order(egui::Order::Foreground)
                .show(&ctx, |ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_black_alpha(220))
                        .inner_margin(4.0)
                        .show(ui, |ui| {
                            let resp = ui.add(egui::TextEdit::singleline(text).desired_width(220.0));
                            resp.request_focus();
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                commit_text = Some((*pos, text.clone()));
                            }
                        });
                });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel_text = true;
            }
        }
        if let Some((pos, text)) = commit_text {
            self.text_edit = None;
            if !text.trim().is_empty() {
                self.push_annotation(Annotation::Text {
                    pos,
                    text,
                    color: self.active_color,
                    size: 20.0,
                });
            }
        } else if cancel_text {
            self.text_edit = None;
        }

        // --- Keyboard shortcuts ---

        // Esc: cancel text entry if that's active, otherwise cancel the
        // whole in-progress selection/capture and hide the overlay.
        if self.text_edit.is_none() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.finish_capture(&ctx);
        }

        // Keyboard shortcuts: Copy/Save (both user-configurable in Settings,
        // defaulting to Ctrl+C/Ctrl+S) plus Ctrl+Z/Ctrl+Y undo/redo.
        // Suppressed while text entry has focus so typing "c"/"s" etc. in a
        // text annotation doesn't accidentally trigger them.
        if self.text_edit.is_none() {
            if let Some(sel) = self.selection_rect() {
                if shortcut_triggered(&ctx, &self.config.copy_shortcut) {
                    self.copy_selection_to_clipboard(sel, &ctx);
                }
                if shortcut_triggered(&ctx, &self.config.save_shortcut) {
                    self.save_selection_via_dialog(sel, &ctx);
                }
            }
            if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Z)) {
                self.undo();
            }
            if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Y)) {
                self.redo();
            }
        }

        // --- Selection outline + toolbar ---

        // Tracks the toolbar's actual on-screen rect for this frame (if it
        // was drawn), so the "click elsewhere clears the selection" check
        // below can tell a click on a toolbar button apart from a click out
        // in empty space. Using `ctx.input(|i| i.pointer.primary_clicked())`
        // alone here is a trap: that flag is true for *any* click anywhere in
        // the window this frame, so it used to fire even when the click
        // landed on a toolbar button — clearing the selection (and hiding the
        // toolbar) before the button's own `.clicked()` handler ever ran.
        let mut toolbar_rect = None;

        if let Some(sel) = self.selection_rect() {
            let tl = to_local(sel.left, sel.top);
            let br = to_local(sel.right, sel.bottom);
            let rect = egui::Rect::from_min_max(tl, br);
            let painter = ui.painter();

            // Dashed selection outline with corner/edge handles — dragging a
            // handle resizes the finalized selection (`ResizeHandle::hit_test`
            // below decides which one, `Gesture::ResizeSelection` carries the
            // drag).
            let corners = [
                rect.left_top(),
                rect.right_top(),
                rect.right_bottom(),
                rect.left_bottom(),
                rect.left_top(),
            ];
            let dash_stroke = egui::Stroke::new(1.5, egui::Color32::WHITE);
            painter.extend(egui::Shape::dashed_line(&corners, dash_stroke, 6.0, 4.0));

            let handle_size = egui::vec2(8.0, 8.0);
            let handle_points = [
                rect.left_top(),
                rect.center_top(),
                rect.right_top(),
                rect.right_center(),
                rect.right_bottom(),
                rect.center_bottom(),
                rect.left_bottom(),
                rect.left_center(),
            ];
            for p in handle_points {
                let handle_rect = egui::Rect::from_center_size(p, handle_size);
                painter.rect_filled(handle_rect, 1.0, egui::Color32::WHITE);
                painter.rect_stroke(
                    handle_rect,
                    1.0,
                    egui::Stroke::new(1.0, egui::Color32::BLACK),
                    egui::StrokeKind::Outside,
                );
            }

            // In-place toolbar: painted as an egui::Area within this same
            // window/coordinate space (never a separate OS window), anchored
            // just below the finalized selection — flipped above it if that
            // would run off the bottom of the virtual desktop. Hidden while
            // actively dragging so it doesn't flicker under the cursor.
            // `response.dragged()` isn't used for this any more: since our
            // gesture logic now bypasses egui's click/drag disambiguation
            // (see the note above `pressed_here`), it can lag behind our own
            // gesture state — checking `self.gesture` directly stays
            // consistent with whatever's actually happening.
            if matches!(self.gesture, Gesture::None) {
                let toolbar_height = 44.0;
                let below = egui::pos2(rect.center().x, br.y) + egui::vec2(0.0, 10.0);
                let above = egui::pos2(rect.center().x, tl.y) - egui::vec2(0.0, toolbar_height + 10.0);
                let toolbar_pos = if below.y + toolbar_height <= vd.height() as f32 {
                    below
                } else if above.y >= 0.0 {
                    above
                } else {
                    // The selection spans (near enough) the full height of
                    // the virtual desktop — neither clear-of-selection
                    // position fits on screen at all, which used to mean the
                    // toolbar rendered entirely off-screen and effectively
                    // vanished. Pin it just inside the top edge instead,
                    // overlapping the selection slightly rather than being
                    // unreachable.
                    egui::pos2(rect.center().x, 10.0)
                };

                let area_response = egui::Area::new(egui::Id::new("rsnap_toolbar"))
                    .fixed_pos(toolbar_pos)
                    .pivot(egui::Align2::CENTER_TOP)
                    .order(egui::Order::Foreground)
                    .show(&ctx, |ui| {
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(28, 28, 30))
                            .corner_radius(20)
                            .inner_margin(egui::Margin::symmetric(10, 6))
                            .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(60)))
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.horizontal(|ui| {
                                    // No explicit "select" button: no tool
                                    // picked is already the default state
                                    // (dragging the selection body moves it,
                                    // dragging a handle resizes it), so
                                    // clicking a tool that's already active
                                    // toggles it back off instead, rather
                                    // than needing a dedicated button just to
                                    // get back to that default.
                                    for tool in Tool::ALL {
                                        let icon_tex = &self.icons[&tool.icon()];
                                        let active = self.tool == Some(tool);
                                        if toolbar_toggle_button(ui, icon_tex, tool.tooltip(), active).clicked() {
                                            self.tool = if active { None } else { Some(tool) };
                                        }
                                    }

                                    ui.add_space(4.0);
                                    ui.separator();
                                    ui.add_space(4.0);
                                    ui.color_edit_button_srgba(&mut self.active_color);
                                    ui.add(
                                        egui::DragValue::new(&mut self.stroke_width)
                                            .range(1.0..=24.0)
                                            .speed(0.1)
                                            .suffix("px"),
                                    );

                                    ui.add_space(4.0);
                                    ui.separator();
                                    ui.add_space(4.0);
                                    if toolbar_icon_button(ui, &self.icons[&Icon::Pin], "Pin screenshot").clicked() {
                                        if let Some(img) = self.composite_selection(sel) {
                                            self.pin_image(img);
                                        }
                                        self.autosave_current_selection();
                                        self.finish_capture(&ctx);
                                    }
                                    if toolbar_icon_button(ui, &self.icons[&Icon::Video], "Record this region")
                                        .clicked()
                                    {
                                        self.start_recording(sel);
                                        // Recording proceeds over the live,
                                        // fully-interactive desktop, not
                                        // under the selection UI — same
                                        // hide-immediately behavior as
                                        // Copy/Save/Pin.
                                        self.finish_capture(&ctx);
                                    }
                                    if toolbar_icon_button(ui, &self.icons[&Icon::Undo2], "Undo").clicked() {
                                        self.undo();
                                    }
                                    if toolbar_icon_button(ui, &self.icons[&Icon::Redo2], "Redo").clicked() {
                                        self.redo();
                                    }

                                    ui.add_space(4.0);
                                    ui.separator();
                                    ui.add_space(4.0);
                                    let copy_tooltip =
                                        format!("Copy to clipboard ({})", self.config.copy_shortcut.label());
                                    if toolbar_icon_button(ui, &self.icons[&Icon::Copy], &copy_tooltip).clicked() {
                                        self.copy_selection_to_clipboard(sel, &ctx);
                                    }
                                    let save_tooltip = format!("Save As... ({})", self.config.save_shortcut.label());
                                    if toolbar_icon_button(ui, &self.icons[&Icon::Save], &save_tooltip).clicked() {
                                        self.save_selection_via_dialog(sel, &ctx);
                                    }
                                });
                            });
                    });
                toolbar_rect = Some(area_response.response.rect);
            }
        }

        // --- Click-to-deselect ---

        // Click-elsewhere-to-deselect: only clears the selection if the click
        // landed outside both the selection rect and the toolbar (see the
        // note above `toolbar_rect` for why this can't just use the global
        // `primary_clicked()` flag on its own).
        if self.text_edit.is_none()
            && ui.input(|i| i.pointer.primary_clicked())
            && matches!(self.gesture, Gesture::None)
            && let Some(sel) = self.selection_rect()
        {
            let selection_rect = egui::Rect::from_min_max(to_local(sel.left, sel.top), to_local(sel.right, sel.bottom));
            let click_pos = ui.input(|i| i.pointer.interact_pos());
            let inside_selection = click_pos.is_some_and(|p| selection_rect.contains(p));
            let inside_toolbar = click_pos.zip(toolbar_rect).is_some_and(|(p, tb)| tb.contains(p));
            if !inside_selection && !inside_toolbar {
                self.drag_start = None;
                self.drag_current = None;
                self.clear_annotations();
            }
        }
    }
}

impl OverlayApp {
    /// Turns a finished `ShapeDrag` gesture (Box/Circle/Arrow/Blur/Invert
    /// Colors) into a committed annotation.
    fn commit_shape_drag(&mut self, start: egui::Pos2, end: egui::Pos2) {
        let rect = egui::Rect::from_two_pos(start, end);
        match self.tool {
            Some(Tool::Box) => self.push_annotation(Annotation::Box {
                rect,
                color: self.active_color,
                width: self.stroke_width,
            }),
            Some(Tool::Circle) => self.push_annotation(Annotation::Circle {
                rect,
                color: self.active_color,
                width: self.stroke_width,
            }),
            Some(Tool::Arrow) => self.push_annotation(Annotation::Arrow {
                from: start,
                to: end,
                color: self.active_color,
                width: self.stroke_width,
            }),
            Some(Tool::Blur) | Some(Tool::InvertColors) => {
                let (vx0, vy0) = self.to_virtual(rect.left_top());
                let (vx1, vy1) = self.to_virtual(rect.right_bottom());
                let vsel = PxRect {
                    left: vx0.min(vx1),
                    top: vy0.min(vy1),
                    right: vx0.max(vx1),
                    bottom: vy0.max(vy1),
                };
                if let Some(cropped) = self.crop_selection(vsel) {
                    let processed = if self.tool == Some(Tool::Blur) {
                        annotate::blur_image(&cropped, self.stroke_width)
                    } else {
                        annotate::invert_image(&cropped)
                    };
                    self.push_annotation(Annotation::Processed { rect, image: processed });
                }
            }
            _ => {}
        }
    }

    /// The folder Save As dialogs should default to — the user's configured
    /// override (Settings) if set, otherwise the Pictures folder.
    fn default_save_dir(&self) -> Option<std::path::PathBuf> {
        self.config.screenshot_save_dir.clone().or_else(dirs::picture_dir)
    }

    /// Composites `sel`, copies it to the clipboard, autosaves, and hides the
    /// overlay — the shared "Copy" action for both the Ctrl+C shortcut and
    /// the toolbar's Copy button.
    fn copy_selection_to_clipboard(&mut self, sel: PxRect, ctx: &egui::Context) {
        if let Some(img) = self.composite_selection(sel)
            && let Err(e) = output::copy_to_clipboard(&img)
        {
            crate::logging::log_error(format!("Copy to clipboard failed: {e}"));
        }
        self.autosave_current_selection();
        self.finish_capture(ctx);
    }

    /// Composites `sel`, opens a native Save As dialog for it, autosaves, and
    /// hides the overlay — the shared "Save" action for both the Ctrl+S
    /// shortcut and the toolbar's Save button.
    fn save_selection_via_dialog(&mut self, sel: PxRect, ctx: &egui::Context) {
        if let Some(img) = self.composite_selection(sel) {
            output::save_via_dialog(
                img,
                self.default_save_dir(),
                output::default_filename(self.config.screenshot_format),
                self.config.screenshot_format,
                self.config.jpeg_quality,
            );
        }
        self.autosave_current_selection();
        self.finish_capture(ctx);
    }

    /// Autosaves the current selection (with whatever's been drawn so far),
    /// when autosave is on. Called only from the "final" actions — Copy,
    /// Save As, Pin — not after every intermediate markup edit; autosave is
    /// meant to back up what you actually walked away with, not create a
    /// new file on disk for every single stroke/shape/erase along the way.
    fn autosave_current_selection(&self) {
        if !self.autosave_enabled {
            return;
        }
        let Some(sel) = self.selection_rect() else {
            return;
        };
        let Some(img) = self.composite_selection(sel) else {
            return;
        };
        // PNG-encoding and writing to disk on every single stroke/shape used
        // to happen synchronously right here, blocking the UI thread — which
        // is what caused a noticeable stall (and lost pointer movement)
        // right after finishing one stroke and starting the next. The crop
        // + annotation baking above stays on this thread (cheap, memory
        // only); only the actual file write moves off it, the same way
        // `output::save_via_dialog` already keeps its blocking work off the
        // main thread.
        let save_dir = self.config.screenshot_save_dir.clone();
        let format = self.config.screenshot_format;
        let jpeg_quality = self.config.jpeg_quality;
        std::thread::spawn(move || {
            if let Err(e) = output::autosave(&img, save_dir.as_deref(), format, jpeg_quality) {
                crate::logging::log_error(format!("Autosave failed: {e}"));
            }
        });
    }

    /// Opens `image` in a new always-on-top floating pin window, independent
    /// of the main overlay (see `PinnedWindow`, `render_pins`).
    fn pin_image(&mut self, image: image::RgbaImage) {
        let id = egui::ViewportId::from_hash_of(("rsnap-pin", self.next_pin_id));
        self.next_pin_id += 1;
        self.pins.push(PinnedWindow {
            id,
            image,
            texture: None,
        });
    }

    /// Finalizes the active recording (if any) and moves the dock into its
    /// Stopped state: the file is closed and remembered for Copy/Save, and the
    /// border is frozen showing "STOPPED" (kept alive so the region stays
    /// outlined until the dock is dismissed). Shared by the tray "Stop
    /// Recording" item and the dock's Stop button. If finalizing fails there's
    /// nothing to offer, so the whole dock/border is torn down instead.
    fn stop_recording(&mut self) {
        let mut finished = None;
        if let Some(recorder) = self.recording.take() {
            let path = recorder.path().to_path_buf();
            match recorder.stop() {
                Ok(()) => finished = Some(path),
                Err(e) => crate::logging::log_error(format!("Failed to finalize recording: {e}")),
            }
        }

        if let Some(path) = finished {
            if let Some(border) = &self.recording_border {
                border.set_state(recording_border::BorderState::Stopped);
            }
            self.recording_finished = Some(path);
        } else {
            if let Some(border) = self.recording_border.take() {
                border.close();
            }
            self.recording_finished = None;
            self.recording_rect = None;
        }
    }

    /// Fully dismisses the recording dock: stops first if still recording,
    /// then tears down the border and clears all recording state.
    fn close_recording_dock(&mut self) {
        if self.recording.is_some() {
            self.stop_recording();
        }
        if let Some(border) = self.recording_border.take() {
            border.close();
        }
        self.recording_finished = None;
        self.recording_rect = None;
    }

    /// Starts a fresh recording of `rect`. Shared by the toolbar's Record
    /// button (starting from no prior recording) and the dock's Resume
    /// button from the Stopped state (re-recording the same region into a
    /// new file) — in the latter case, any frozen "STOPPED" border is closed
    /// first so its timer restarts from zero for the new take; closing a
    /// border that doesn't exist yet is simply a no-op.
    fn start_recording(&mut self, rect: PxRect) {
        match recording::default_video_path(self.config.video_save_dir.as_deref(), self.config.recording_container) {
            Ok(path) => match Recorder::start(
                rect,
                self.layout.monitors.len(),
                &path,
                self.config.recording_fps,
                self.config.recording_bitrate_mbps,
                self.config.recording_container,
            ) {
                Ok(recorder) => {
                    self.recording = Some(recorder);
                    if let Some(border) = self.recording_border.take() {
                        border.close();
                    }
                    self.recording_border = Some(recording_border::start(rect));
                    self.recording_finished = None;
                    self.recording_rect = Some(rect);
                }
                Err(e) => crate::logging::log_error(format!("Failed to start recording: {e}")),
            },
            Err(e) => crate::logging::log_error(format!("Failed to determine recording output path: {e}")),
        }
    }

    /// The floating recording action dock: an opaque, always-on-top,
    /// undecorated egui viewport anchored under the recording region, rendered
    /// from `logic()` (like pins/Settings) so it persists while the main
    /// overlay is hidden. Transport (Resume/Pause/Stop) + Copy/Save on the
    /// finished file. A no-op unless a recording is active or just finished.
    ///
    /// Unlike the marching-ants border this needs neither transparency nor
    /// click-through, so the ordinary opaque secondary-viewport path (proven
    /// fine for pins and Settings) works — none of the graphics-stack trouble
    /// that pushed the border to native Win32.
    fn render_recording_dock(&mut self, ctx: &egui::Context) {
        let state = match (&self.recording, &self.recording_finished) {
            (Some(rec), _) if rec.is_paused() => DockState::Paused,
            (Some(_), _) => DockState::Recording,
            (None, Some(_)) => DockState::Stopped,
            (None, None) => return,
        };

        // Status line: live timer while recording/paused, filename once saved.
        let status = match state {
            DockState::Recording | DockState::Paused => {
                let secs = self.recording.as_ref().map(|r| r.elapsed().as_secs()).unwrap_or(0);
                let word = if state == DockState::Paused { "PAUSED" } else { "REC" };
                format!("{word}  {:02}:{:02}", secs / 60, secs % 60)
            }
            // Keep it short — the border already shows "STOPPED" over the
            // region, and the full filename overflowed the bar.
            DockState::Stopped => "Saved".to_owned(),
        };

        // Best-effort anchor under the region's bottom-center (physical px;
        // exact on a 100% monitor, possibly slightly off on a scaled one — the
        // dock is draggable to compensate).
        const DOCK_W: f32 = 360.0;
        const DOCK_H: f32 = 48.0;
        let pos = self.recording_rect.map(|r| {
            let cx = r.left as f32 + r.width() as f32 / 2.0;
            egui::pos2(cx - DOCK_W / 2.0, r.top as f32 + r.height() as f32 + 8.0)
        });

        // Clone the icon handles the closure needs (cheap Arc clones) so it
        // doesn't have to borrow `self`.
        let play = self.icons[&Icon::CirclePlay].clone();
        let pause = self.icons[&Icon::CirclePause].clone();
        let stop = self.icons[&Icon::CircleStop].clone();
        let copy = self.icons[&Icon::Copy].clone();
        let save = self.icons[&Icon::Save].clone();
        let close = self.icons[&Icon::X].clone();

        let mut builder = egui::ViewportBuilder::default()
            .with_title("rsnap - recording")
            .with_always_on_top()
            .with_inner_size(egui::vec2(DOCK_W, DOCK_H))
            .with_resizable(false)
            .with_decorations(false);
        if let Some(pos) = pos {
            builder = builder.with_position(pos);
        }

        let mut action: Option<DockAction> = None;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("rsnap-recording-dock"),
            builder,
            |ctx, _class| {
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(28, 28, 30))
                            .inner_margin(egui::Margin::symmetric(10, 6)),
                    )
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;

                            let icon_btn = |ui: &mut egui::Ui, tex: &egui::TextureHandle, enabled: bool, tip: &str| {
                                let btn = egui::Button::image(egui::Image::new((tex.id(), egui::vec2(20.0, 20.0))))
                                    .corner_radius(8)
                                    .min_size(egui::vec2(36.0, 32.0));
                                ui.add_enabled(enabled, btn).on_hover_text(tip.to_owned())
                            };

                            // Resume: enabled while Paused (continue) or Stopped
                            // (record the region again).
                            let can_resume = matches!(state, DockState::Paused | DockState::Stopped);
                            let can_pause = state == DockState::Recording;
                            let can_stop = matches!(state, DockState::Recording | DockState::Paused);
                            let can_files = state == DockState::Stopped;

                            if icon_btn(ui, &play, can_resume, "Resume / record again").clicked() {
                                action = Some(DockAction::Resume);
                            }
                            if icon_btn(ui, &pause, can_pause, "Pause").clicked() {
                                action = Some(DockAction::Pause);
                            }
                            if icon_btn(ui, &stop, can_stop, "Stop").clicked() {
                                action = Some(DockAction::Stop);
                            }

                            ui.add_space(4.0);
                            ui.separator();
                            ui.add_space(4.0);

                            if icon_btn(ui, &copy, can_files, "Copy video file").clicked() {
                                action = Some(DockAction::Copy);
                            }
                            if icon_btn(ui, &save, can_files, "Save As...").clicked() {
                                action = Some(DockAction::Save);
                            }

                            ui.add_space(4.0);
                            ui.separator();
                            ui.add_space(4.0);
                            ui.label(&status);

                            // Close pinned to the far right; the status label
                            // fills the gap between it and the buttons.
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if icon_btn(ui, &close, true, "Close").clicked() {
                                    action = Some(DockAction::Close);
                                }
                            });
                        });
                    });
                if ctx.input(|i| i.viewport().close_requested()) {
                    action = Some(DockAction::Close);
                }
            },
        );

        match action {
            Some(DockAction::Pause) => {
                if let Some(rec) = &mut self.recording {
                    rec.pause();
                }
                if let Some(border) = &self.recording_border {
                    border.set_state(recording_border::BorderState::Paused);
                }
            }
            Some(DockAction::Resume) => match state {
                DockState::Paused => {
                    if let Some(rec) = &mut self.recording {
                        rec.resume();
                    }
                    if let Some(border) = &self.recording_border {
                        border.set_state(recording_border::BorderState::Recording);
                    }
                }
                DockState::Stopped => {
                    if let Some(rect) = self.recording_rect {
                        self.start_recording(rect);
                    }
                }
                DockState::Recording => {}
            },
            Some(DockAction::Stop) => self.stop_recording(),
            Some(DockAction::Copy) => {
                if let Some(path) = self.recording_finished.clone()
                    && let Err(e) = output::copy_file_to_clipboard(&path)
                {
                    crate::logging::log_error(format!("Copy video to clipboard failed: {e}"));
                }
            }
            Some(DockAction::Save) => {
                if let Some(path) = self.recording_finished.clone() {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    output::save_file_via_dialog(
                        path,
                        self.config.video_save_dir.clone(),
                        name,
                        self.config.recording_container.label(),
                        self.config.recording_container.extension(),
                    );
                }
            }
            Some(DockAction::Close) => self.close_recording_dock(),
            None => {}
        }
    }

    /// Renders every pinned window as its own always-on-top OS viewport,
    /// independent of the main editor overlay — called from `logic()` so
    /// pins keep showing even while the main overlay is hidden.
    fn render_pins(&mut self, ctx: &egui::Context) {
        let mut closed = Vec::new();
        for (i, pin) in self.pins.iter_mut().enumerate() {
            let image = &pin.image;
            let tex = pin
                .texture
                .get_or_insert_with(|| {
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [image.width() as usize, image.height() as usize],
                        image.as_raw(),
                    );
                    ctx.load_texture(format!("pin-{i}"), color_image, egui::TextureOptions::LINEAR)
                })
                .clone();
            let size = egui::vec2(pin.image.width() as f32, pin.image.height() as f32);

            let mut want_close = false;
            ctx.show_viewport_immediate(
                pin.id,
                egui::ViewportBuilder::default()
                    .with_title("rsnap - pinned")
                    .with_always_on_top()
                    .with_inner_size(size)
                    .with_resizable(false)
                    .with_decorations(false),
                |ctx, _class| {
                    let mut want_start_drag = false;
                    egui::CentralPanel::default()
                        .frame(egui::Frame::new().inner_margin(0.0))
                        .show(ctx, |ui| {
                            ui.image((tex.id(), size));

                            // No decorations means no OS title bar to grab,
                            // so dragging anywhere on the image moves the
                            // window instead — `StartDrag` hands off to the
                            // OS's own window-move behavior. Called on raw
                            // pointer press (not egui's click/drag-decided
                            // response) both because `StartDrag`'s own docs
                            // say it needs to be called right as the button
                            // goes down, and because we've already seen
                            // egui's click/drag disambiguation add a
                            // noticeable hesitation (see the note on
                            // `pressed_here` above) — not something a window
                            // drag should have either.
                            let response =
                                ui.interact(ui.max_rect(), ui.id().with("pin_drag"), egui::Sense::click_and_drag());
                            if response.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
                                want_start_drag = true;
                            }
                            // No title bar means no OS close button either —
                            // right-click closes it instead.
                            if response.secondary_clicked() {
                                want_close = true;
                            }
                        });
                    if want_start_drag {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) || ctx.input(|i| i.viewport().close_requested())
                    {
                        want_close = true;
                    }
                },
            );
            if want_close {
                closed.push(i);
            }
        }
        for i in closed.into_iter().rev() {
            self.pins.remove(i);
        }
    }

    /// Renders the Settings window as its own decorated, opaque viewport —
    /// unlike the recording border, this one doesn't need transparency or
    /// click-through, so the usual egui/wgpu viewport path (already proven
    /// fine for pins) works without any of the graphics-stack issues that
    /// forced the border to go native. A no-op when the window isn't open
    /// (`self.settings_draft.is_none()`).
    fn render_settings_window(&mut self, ctx: &egui::Context) {
        if self.settings_draft.is_none() {
            return;
        }

        if let Some(target) = self.settings_capturing {
            // `egui::Key` has no modifier-only variants (Ctrl/Shift/Alt live
            // in `Modifiers`, alongside the event), so any `Key` event that
            // `key_to_vk` recognizes is a real, capturable key — no
            // modifier-exclusion filtering needed here.
            let captured = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => key_to_vk(*key).map(|vk| (vk, *modifiers)),
                    _ => None,
                })
            });
            if let Some((vk, modifiers)) = captured {
                if let Some(draft) = &mut self.settings_draft {
                    let combo = crate::config::HotkeyConfig {
                        ctrl: modifiers.ctrl,
                        shift: modifiers.shift,
                        alt: modifiers.alt,
                        win: modifiers.mac_cmd || (modifiers.command && !modifiers.ctrl),
                        vk,
                    };
                    match target {
                        ShortcutTarget::Capture => draft.hotkey = combo,
                        ShortcutTarget::Copy => draft.copy_shortcut = combo,
                        ShortcutTarget::Save => draft.save_shortcut = combo,
                    }
                }
                self.settings_capturing = None;
            }
        }

        let mut want_close = false;
        let mut want_save = false;
        let id = egui::ViewportId::from_hash_of("rsnap-settings");
        ctx.show_viewport_immediate(
            id,
            egui::ViewportBuilder::default()
                .with_title("rsnap Settings")
                .with_inner_size(egui::vec2(400.0, 470.0))
                .with_min_inner_size(egui::vec2(400.0, 470.0))
                .with_resizable(false),
            |ctx, _class| {
                egui::Panel::bottom("settings_actions")
                    .frame(
                        egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(16, 12))
                            .fill(ctx.style().visuals.window_fill),
                    )
                    .show(ctx, |ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let save_button = egui::Button::new(egui::RichText::new("Save").strong())
                                .fill(egui::Color32::from_rgb(60, 120, 220))
                                .min_size(egui::vec2(72.0, 28.0));
                            if ui.add(save_button).clicked() {
                                want_save = true;
                            }
                            if ui
                                .add_sized(egui::vec2(72.0, 28.0), egui::Button::new("Cancel"))
                                .clicked()
                            {
                                want_close = true;
                            }
                        });
                    });

                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .inner_margin(16.0)
                            .fill(ctx.style().visuals.window_fill),
                    )
                    .show(ctx, |ui| {
                        let Some(draft) = &mut self.settings_draft else { return };
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            settings_section(ui, "Shortcuts", |ui| {
                                settings_row(ui, "Capture", |ui| {
                                    let label = if self.settings_capturing == Some(ShortcutTarget::Capture) {
                                        "Press a key combo...".to_string()
                                    } else {
                                        draft.hotkey.label()
                                    };
                                    if ui.button(label).clicked() {
                                        self.settings_capturing = Some(ShortcutTarget::Capture);
                                    }
                                });
                                ui.label(
                                    egui::RichText::new("Changing this takes effect after restarting rsnap.")
                                        .small()
                                        .color(egui::Color32::from_gray(150)),
                                );
                                ui.add_space(4.0);
                                settings_row(ui, "Copy", |ui| {
                                    let label = if self.settings_capturing == Some(ShortcutTarget::Copy) {
                                        "Press a key combo...".to_string()
                                    } else {
                                        draft.copy_shortcut.label()
                                    };
                                    if ui.button(label).clicked() {
                                        self.settings_capturing = Some(ShortcutTarget::Copy);
                                    }
                                });
                                settings_row(ui, "Save", |ui| {
                                    let label = if self.settings_capturing == Some(ShortcutTarget::Save) {
                                        "Press a key combo...".to_string()
                                    } else {
                                        draft.save_shortcut.label()
                                    };
                                    if ui.button(label).clicked() {
                                        self.settings_capturing = Some(ShortcutTarget::Save);
                                    }
                                });
                                ui.label(
                                    egui::RichText::new("Copy/Save apply immediately, while the overlay has focus.")
                                        .small()
                                        .color(egui::Color32::from_gray(150)),
                                );
                            });

                            settings_section(ui, "Output", |ui| {
                                settings_row(ui, "Autosave every capture", |ui| {
                                    ui.checkbox(&mut draft.autosave_enabled, "");
                                });
                                ui.add_space(4.0);
                                settings_row(ui, "Screenshot format", |ui| {
                                    egui::ComboBox::new("screenshot_format_combo", "")
                                        .selected_text(draft.screenshot_format.label())
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut draft.screenshot_format,
                                                config::ScreenshotFormat::Png,
                                                "PNG",
                                            );
                                            ui.selectable_value(
                                                &mut draft.screenshot_format,
                                                config::ScreenshotFormat::Jpeg,
                                                "JPEG",
                                            );
                                        });
                                });
                                if draft.screenshot_format == config::ScreenshotFormat::Jpeg {
                                    settings_row(ui, "JPEG quality", |ui| {
                                        ui.add(egui::Slider::new(&mut draft.jpeg_quality, 1..=100));
                                    });
                                }
                                ui.add_space(4.0);
                                settings_row(ui, "Screenshots", |ui| {
                                    if draft.screenshot_save_dir.is_some() && ui.button("Reset to default").clicked() {
                                        draft.screenshot_save_dir = None;
                                    }
                                    if ui.button("Choose folder...").clicked()
                                        && let Some(dir) = rfd::FileDialog::new().pick_folder()
                                    {
                                        draft.screenshot_save_dir = Some(dir);
                                    }
                                });
                                let screenshot_shown = draft
                                    .screenshot_save_dir
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "Default (Pictures)".to_string());
                                settings_row(ui, "", |ui| {
                                    ui.label(
                                        egui::RichText::new(screenshot_shown)
                                            .small()
                                            .color(egui::Color32::from_gray(160)),
                                    );
                                });
                                ui.add_space(6.0);
                                settings_row(ui, "Recordings", |ui| {
                                    if draft.video_save_dir.is_some() && ui.button("Reset to default").clicked() {
                                        draft.video_save_dir = None;
                                    }
                                    if ui.button("Choose folder...").clicked()
                                        && let Some(dir) = rfd::FileDialog::new().pick_folder()
                                    {
                                        draft.video_save_dir = Some(dir);
                                    }
                                });
                                let video_shown = draft
                                    .video_save_dir
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "Default (Videos)".to_string());
                                settings_row(ui, "", |ui| {
                                    ui.label(
                                        egui::RichText::new(video_shown)
                                            .small()
                                            .color(egui::Color32::from_gray(160)),
                                    );
                                });
                            });

                            settings_section(ui, "Recording quality", |ui| {
                                settings_row(ui, "Container", |ui| {
                                    egui::ComboBox::new("recording_container_combo", "")
                                        .selected_text(draft.recording_container.label())
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut draft.recording_container,
                                                config::RecordingContainer::Mp4,
                                                "MP4",
                                            );
                                            ui.selectable_value(
                                                &mut draft.recording_container,
                                                config::RecordingContainer::Avi,
                                                "AVI",
                                            );
                                        });
                                });
                                settings_row(ui, "Frame rate", |ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut draft.recording_fps)
                                            .range(10..=60)
                                            .suffix(" fps"),
                                    );
                                });
                                settings_row(ui, "Bitrate", |ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut draft.recording_bitrate_mbps)
                                            .range(2..=50)
                                            .suffix(" Mbps"),
                                    );
                                });
                            });

                            settings_section(ui, "Markup defaults", |ui| {
                                settings_row(ui, "Color", |ui| {
                                    let mut color = egui::Color32::from_rgb(
                                        draft.default_color[0],
                                        draft.default_color[1],
                                        draft.default_color[2],
                                    );
                                    if ui.color_edit_button_srgba(&mut color).changed() {
                                        draft.default_color = [color.r(), color.g(), color.b()];
                                    }
                                });
                                settings_row(ui, "Stroke width", |ui| {
                                    ui.add(
                                        egui::DragValue::new(&mut draft.default_stroke_width)
                                            .range(1.0..=24.0)
                                            .speed(0.1)
                                            .suffix("px"),
                                    );
                                });
                                settings_row(ui, "Default tool", |ui| {
                                    let current_label =
                                        draft.default_tool.map(|t| t.tooltip()).unwrap_or("None (Select)");
                                    egui::ComboBox::new("default_tool_combo", "")
                                        .selected_text(current_label)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut draft.default_tool, None, "None (Select)");
                                            for tool in Tool::ALL {
                                                ui.selectable_value(
                                                    &mut draft.default_tool,
                                                    Some(tool),
                                                    tool.tooltip(),
                                                );
                                            }
                                        });
                                });
                                ui.add_space(4.0);
                                settings_row(ui, "Smooth freehand strokes", |ui| {
                                    ui.checkbox(&mut draft.smooth_strokes, "");
                                });
                                ui.add_space(4.0);
                                settings_row(ui, "Undo history", |ui| {
                                    let mut unlimited = draft.undo_history_limit == 0;
                                    let changed = ui.checkbox(&mut unlimited, "").changed();
                                    ui.label("Unlimited");
                                    if changed {
                                        draft.undo_history_limit = if unlimited { 0 } else { 200 };
                                    }
                                });
                                if draft.undo_history_limit > 0 {
                                    settings_row(ui, "", |ui| {
                                        ui.add(
                                            egui::DragValue::new(&mut draft.undo_history_limit)
                                                .range(10..=2000)
                                                .suffix(" annotations"),
                                        );
                                    });
                                }
                            });

                            settings_section(ui, "Startup", |ui| {
                                settings_row(ui, "Start rsnap with Windows", |ui| {
                                    ui.checkbox(&mut draft.start_with_windows, "");
                                });
                            });
                        }); // ScrollArea
                    });

                if ctx.input(|i| i.viewport().close_requested()) {
                    want_close = true;
                }
            },
        );

        if want_save {
            if let Some(draft) = self.settings_draft.take() {
                if let Err(e) = win32::set_start_with_windows(draft.start_with_windows) {
                    crate::logging::log_error(format!("Failed to update start-with-Windows setting: {e}"));
                }
                if let Err(e) = crate::config::save(&draft) {
                    crate::logging::log_error(format!("Failed to save settings: {e}"));
                }
                self.autosave_enabled = draft.autosave_enabled;
                let [r, g, b] = draft.default_color;
                self.active_color = egui::Color32::from_rgb(r, g, b);
                self.stroke_width = draft.default_stroke_width;
                self.config = draft;
            }
            self.settings_capturing = None;
        } else if want_close {
            self.settings_draft = None;
            self.settings_capturing = None;
        }
    }
}

/// Which shortcut the Settings window is currently waiting for a key combo
/// to assign to, after clicking one of its "Change..." buttons.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShortcutTarget {
    Capture,
    Copy,
    Save,
}

/// Whether `shortcut` (an in-app Copy/Save binding, checked against the
/// current frame's input) was just triggered.
///
/// Ctrl+C needs special handling: egui's own input layer hardcodes Ctrl+C
/// (and Ctrl+V/X/A) as clipboard commands at the OS-event-translation stage,
/// converting the key-*down* into `Event::Copy` instead of a normal `Key`
/// press — only the key-*up* ever surfaces as an ordinary `Key` event, so
/// `key_pressed` never fires for it. Any *other* combo (including a
/// user-rebound Copy shortcut that isn't literally Ctrl+C) doesn't get this
/// treatment and can be checked the normal way.
fn shortcut_triggered(ctx: &egui::Context, shortcut: &crate::config::HotkeyConfig) -> bool {
    if shortcut.ctrl && !shortcut.shift && !shortcut.alt && shortcut.vk == 0x43 {
        return ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
    }
    let Some(key) = vk_to_key(shortcut.vk) else {
        return false;
    };
    ctx.input(|i| {
        i.modifiers.ctrl == shortcut.ctrl
            && i.modifiers.shift == shortcut.shift
            && i.modifiers.alt == shortcut.alt
            && i.key_pressed(key)
    })
}

/// A visually-grouped block of settings: a heading plus its rows inside a
/// subtly-shaded rounded card, matching the toolbar's own dark-pill styling
/// rather than a flat list of ungrouped labels.
fn settings_section(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    let full_width = ui.available_width();
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(32, 32, 35))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(55)))
        .show(ui, |ui| {
            ui.set_width(full_width - 2.0 * (12.0 + 1.0)); // account for inner_margin + stroke
            ui.label(egui::RichText::new(title).strong().size(15.0));
            ui.add_space(6.0);
            add_contents(ui);
        });
    ui.add_space(10.0);
}

/// A label + control pair on one line, with the label given a fixed width so
/// multiple rows in the same section line their controls up.
fn settings_row(ui: &mut egui::Ui, label: &str, add_control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add_control);
    });
}

fn key_to_vk(key: egui::Key) -> Option<u32> {
    use egui::Key::*;
    Some(match key {
        A => 0x41,
        B => 0x42,
        C => 0x43,
        D => 0x44,
        E => 0x45,
        F => 0x46,
        G => 0x47,
        H => 0x48,
        I => 0x49,
        J => 0x4A,
        K => 0x4B,
        L => 0x4C,
        M => 0x4D,
        N => 0x4E,
        O => 0x4F,
        P => 0x50,
        Q => 0x51,
        R => 0x52,
        S => 0x53,
        T => 0x54,
        U => 0x55,
        V => 0x56,
        W => 0x57,
        X => 0x58,
        Y => 0x59,
        Z => 0x5A,
        Num0 => 0x30,
        Num1 => 0x31,
        Num2 => 0x32,
        Num3 => 0x33,
        Num4 => 0x34,
        Num5 => 0x35,
        Num6 => 0x36,
        Num7 => 0x37,
        Num8 => 0x38,
        Num9 => 0x39,
        F1 => 0x70,
        F2 => 0x71,
        F3 => 0x72,
        F4 => 0x73,
        F5 => 0x74,
        F6 => 0x75,
        F7 => 0x76,
        F8 => 0x77,
        F9 => 0x78,
        F10 => 0x79,
        F11 => 0x7A,
        F12 => 0x7B,
        _ => return None,
    })
}

/// Inverse of `key_to_vk` — used to check `key_pressed` against a stored
/// shortcut's virtual-key code.
fn vk_to_key(vk: u32) -> Option<egui::Key> {
    use egui::Key::*;
    Some(match vk {
        0x41 => A,
        0x42 => B,
        0x43 => C,
        0x44 => D,
        0x45 => E,
        0x46 => F,
        0x47 => G,
        0x48 => H,
        0x49 => I,
        0x4A => J,
        0x4B => K,
        0x4C => L,
        0x4D => M,
        0x4E => N,
        0x4F => O,
        0x50 => P,
        0x51 => Q,
        0x52 => R,
        0x53 => S,
        0x54 => T,
        0x55 => U,
        0x56 => V,
        0x57 => W,
        0x58 => X,
        0x59 => Y,
        0x5A => Z,
        0x30 => Num0,
        0x31 => Num1,
        0x32 => Num2,
        0x33 => Num3,
        0x34 => Num4,
        0x35 => Num5,
        0x36 => Num6,
        0x37 => Num7,
        0x38 => Num8,
        0x39 => Num9,
        0x70 => F1,
        0x71 => F2,
        0x72 => F3,
        0x73 => F4,
        0x74 => F5,
        0x75 => F6,
        0x76 => F7,
        0x77 => F8,
        0x78 => F9,
        0x79 => F10,
        0x7A => F11,
        0x7B => F12,
        _ => return None,
    })
}

/// A single icon-styled toolbar button: fixed pill size, a Lucide icon
/// (https://lucide.dev/icons, rasterized once at startup — see
/// `crate::icons`), hover tooltip.
fn toolbar_icon_button(ui: &mut egui::Ui, tex: &egui::TextureHandle, tooltip: &str) -> egui::Response {
    let response = ui.add_sized(
        egui::vec2(36.0, 32.0),
        egui::Button::image(egui::Image::new((tex.id(), egui::vec2(20.0, 20.0)))).corner_radius(8),
    );
    response.on_hover_text(tooltip)
}

/// Same as `toolbar_icon_button`, but stays visibly highlighted while
/// `active` is true (used for tool selection and the Autosave toggle).
fn toolbar_toggle_button(ui: &mut egui::Ui, tex: &egui::TextureHandle, tooltip: &str, active: bool) -> egui::Response {
    let button = egui::Button::image(egui::Image::new((tex.id(), egui::vec2(20.0, 20.0))))
        .corner_radius(8)
        .selected(active);
    let response = ui.add_sized(egui::vec2(36.0, 32.0), button);
    response.on_hover_text(tooltip)
}

/// Dims everything in `full_rect` except `hole` (left unpainted, punched out
/// via four non-overlapping strips around it — there's no way to punch a
/// literal hole in one filled rect). Shared by the selection spotlight and
/// the window-hover preview, which both need the exact same "portal through
/// the dimming" treatment around a rect.
fn paint_dim_with_hole(painter: &egui::Painter, full_rect: egui::Rect, hole: egui::Rect, dim_color: egui::Color32) {
    // Top strip (full width, above the hole).
    painter.rect_filled(
        egui::Rect::from_min_max(full_rect.min, egui::pos2(full_rect.max.x, hole.min.y)),
        0.0,
        dim_color,
    );
    // Bottom strip (full width, below the hole).
    painter.rect_filled(
        egui::Rect::from_min_max(egui::pos2(full_rect.min.x, hole.max.y), full_rect.max),
        0.0,
        dim_color,
    );
    // Left strip (only alongside the hole's own height).
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(full_rect.min.x, hole.min.y),
            egui::pos2(hole.min.x, hole.max.y),
        ),
        0.0,
        dim_color,
    );
    // Right strip (only alongside the hole's own height).
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(hole.max.x, hole.min.y),
            egui::pos2(full_rect.max.x, hole.max.y),
        ),
        0.0,
        dim_color,
    );
}

/// Shrinks `img` so its longest side is at most 160px, if it isn't already
/// smaller — used to keep the Blur/Invert/Magic Erase live drag preview
/// cheap regardless of how large a region the user is dragging over. The
/// result gets stretched back up to the full selection rect by the GPU when
/// painted, which for a live in-progress preview (replaced by a full-quality
/// bake on release) is an imperceptible tradeoff.
fn downscale_for_preview(img: &image::RgbaImage) -> image::RgbaImage {
    const MAX_DIM: u32 = 160;
    let (w, h) = img.dimensions();
    let longest = w.max(h).max(1);
    if longest <= MAX_DIM {
        return img.clone();
    }
    let scale = MAX_DIM as f32 / longest as f32;
    let new_w = ((w as f32 * scale).round().max(1.0)) as u32;
    let new_h = ((h as f32 * scale).round().max(1.0)) as u32;
    image::imageops::resize(img, new_w, new_h, image::imageops::FilterType::Triangle)
}
