//! rsnap: a native Windows tray app for screenshotting, markup, and screen
//! recording across arbitrary (including mixed-DPI, multi-monitor) desktops.
//!
//! Process shape, top to bottom:
//! - [`spawn_tray_icon`] puts the tray icon, its context menu, and the global
//!   capture hotkey on their own dedicated thread (see that function's doc
//!   for why it can't share the winit/eframe thread).
//! - [`capture::start_all`] starts one persistent Windows Graphics Capture
//!   session per monitor at startup, so the first hotkey press doesn't pay
//!   session-start latency.
//! - The rest of `main` builds a single full-virtual-desktop, borderless,
//!   always-invisible-until-shown [`eframe`] window and hands it to
//!   [`overlay::OverlayApp`], which owns everything from region-select
//!   through annotation, clipboard/save output, and recording.
#![windows_subsystem = "windows"]

mod annotate;
mod capture;
mod config;
mod icons;
mod logging;
mod monitors;
mod output;
mod overlay;
mod recording;
mod recording_border;
mod win32;

use eframe::egui;
use tray_icon::TrayIconBuilder;
use tray_icon::menu::{Menu, MenuId, MenuItem};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, TranslateMessage, WM_HOTKEY};

use config::HotkeyConfig;
use monitors::DisplayLayout;
use overlay::OverlayApp;

/// Only ID in use for now — settings only supports one configurable hotkey
/// (capture). Registered once at startup from `Config::hotkey`; changing it
/// in Settings takes effect on next launch (see `config.rs`'s doc comment).
const CAPTURE_HOTKEY_ID: i32 = 1;

/// Translates a `HotkeyConfig`'s modifier flags into the `RegisterHotKey`
/// bitmask, always including `MOD_NOREPEAT` so holding the combo down
/// doesn't re-fire the capture hotkey on every OS key-repeat tick.
fn hotkey_modifiers(hotkey: &HotkeyConfig) -> HOT_KEY_MODIFIERS {
    let mut mods = MOD_NOREPEAT;
    if hotkey.ctrl {
        mods |= MOD_CONTROL;
    }
    if hotkey.shift {
        mods |= MOD_SHIFT;
    }
    if hotkey.alt {
        mods |= MOD_ALT;
    }
    if hotkey.win {
        mods |= MOD_WIN;
    }
    mods
}

/// Build the tray icon + menu and run its message loop for the lifetime of
/// the process, on its own dedicated thread.
///
/// This has to be isolated from the eframe/winit main thread: showing the
/// tray context menu calls into Win32's `TrackPopupMenu`, which blocks the
/// calling thread and pumps its own nested modal message loop until the menu
/// is dismissed. When that ran on the same thread as winit's own event loop,
/// the two loops fought each other — clicks were either silently swallowed
/// or the popup would hang/lag instead of closing. Putting the tray on its
/// own thread with a plain `GetMessage`/`DispatchMessage` loop gives
/// `TrackPopupMenu` a thread winit never touches. The resulting `MenuEvent`s
/// still arrive via `tray_icon::menu::MenuEvent`'s global channel, which is
/// thread-agnostic, so the main thread's polling code is unchanged.
fn spawn_tray_icon(hotkey: HotkeyConfig) -> (MenuId, MenuId, MenuId, MenuId, crossbeam_channel::Receiver<()>) {
    let (id_tx, id_rx) = std::sync::mpsc::channel::<(MenuId, MenuId, MenuId, MenuId)>();
    let (hotkey_tx, hotkey_rx) = crossbeam_channel::unbounded::<()>();

    std::thread::spawn(move || {
        let menu = Menu::new();
        let show_item = MenuItem::new(format!("Capture ({})", hotkey.label()), true, None);
        let stop_recording_item = MenuItem::new("Stop Recording", true, None);
        let settings_item = MenuItem::new("Settings...", true, None);
        let quit_item = MenuItem::new("Quit", true, None);
        menu.append(&show_item).expect("append show item");
        menu.append(&stop_recording_item).expect("append stop recording item");
        menu.append(&settings_item).expect("append settings item");
        menu.append(&quit_item).expect("append quit item");
        let show_id = show_item.id().clone();
        let stop_recording_id = stop_recording_item.id().clone();
        let settings_id = settings_item.id().clone();
        let quit_id = quit_item.id().clone();

        // Keep the tray icon alive for as long as this thread runs (i.e. for
        // the lifetime of the process) by holding it in this local binding
        // across the message loop below.
        let _tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("rsnap")
            .with_icon(app_icon())
            .build()
            .expect("failed to build tray icon");

        // `RegisterHotKey` ties WM_HOTKEY delivery to whichever thread calls
        // it (with hwnd=None, its message-only queue) — has to happen here,
        // on the same thread that then pumps GetMessage below, not on the
        // eframe/winit thread.
        unsafe {
            let _ = RegisterHotKey(None, CAPTURE_HOTKEY_ID, hotkey_modifiers(&hotkey), hotkey.vk);
        }

        id_tx
            .send((show_id, stop_recording_id, settings_id, quit_id))
            .expect("main thread went away before tray thread could report its menu ids");

        unsafe {
            let mut msg = MSG::default();
            loop {
                let ret = GetMessageW(&mut msg, None, 0, 0);
                if !ret.as_bool() {
                    break;
                }
                if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == CAPTURE_HOTKEY_ID {
                    let _ = hotkey_tx.send(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });

    let (show_id, stop_recording_id, settings_id, quit_id) = id_rx.recv().expect("tray icon thread failed to start");
    (show_id, stop_recording_id, settings_id, quit_id, hotkey_rx)
}

/// Asserts Per-Monitor-V2 DPI awareness in-process, belt-and-suspenders
/// alongside the manifest (`rsnap.exe.manifest`, embedded via `build.rs`,
/// which already declares it). Covers cases where the manifest doesn't take
/// — e.g. running via `cargo run` without the resource compiled in during a
/// given build. Failure here just means the manifest already won, which is
/// fine.
fn init_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Installs a panic hook that logs the panic message/location via
/// `logging::log_error` before the default hook runs. With
/// `#![windows_subsystem = "windows"]` there's no console for a panic's
/// default stderr output to ever reach, so without this a panic is simply a
/// silent crash — indistinguishable from a driver-level fault, and exactly
/// why an intermittent "hotkey press occasionally crashes" report had no
/// trace to go on.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        logging::log_error(format!("panic: {info}"));
        default_hook(info);
    }));
}

fn main() -> eframe::Result {
    install_panic_hook();
    init_dpi_awareness();

    let config = config::load();
    let layout = DisplayLayout::enumerate();

    // Start all per-monitor capture sessions now, once, so they're already
    // warm by the time the hotkey/tray "show" handler runs.
    let captures = capture::start_all(&layout.monitors);

    let (show_id, stop_recording_id, settings_id, quit_id, hotkey_events) = spawn_tray_icon(config.hotkey);

    let vd = layout.virtual_desktop;
    // Deliberately NOT setting `.with_always_on_top()` here: an always-on-top
    // window at the OS level can interfere with the tray icon's
    // TrackPopupMenu/SetForegroundWindow handshake, silently swallowing menu
    // clicks before they ever become WM_COMMAND. Topmost is asserted manually
    // via raw SetWindowPos(HWND_TOPMOST) in `win32::place_window`, called
    // every time the overlay is actually shown — so we lose nothing.
    //
    // `.with_visible(true)` here is deliberate too, not a bug: the window is
    // "shown" at the OS level for the entire process lifetime and is instead
    // made invisible by moving it far off-screen (see
    // `win32::move_offscreen`'s doc comment) — actually hiding and re-showing
    // it via `ShowWindow` turned out to cause a brief white flash on every
    // show after the first, which nothing on our own render-pipeline side
    // could account for.
    let viewport = egui::ViewportBuilder::default()
        .with_decorations(false)
        .with_resizable(false)
        .with_transparent(true)
        .with_taskbar(false)
        .with_visible(true)
        .with_position(egui::pos2(win32::OFFSCREEN_POS as f32, win32::OFFSCREEN_POS as f32))
        .with_inner_size(egui::vec2(vd.width() as f32, vd.height() as f32));

    // Explicitly exclude the `GL` backend from wgpu's default backend set
    // (`PRIMARY | GL`), which otherwise leaves wgpu free to fall back to
    // OpenGL on some systems. On one test machine (an AMD laptop GPU, only
    // reproduced once undocked from its usual multi-monitor desk setup),
    // wgpu falling onto the OpenGL backend crashed hard inside the AMD
    // driver itself (`atio6axx.dll`, `STATUS_ACCESS_VIOLATION`) — before the
    // overlay ever even showed. Restricting to `PRIMARY` (Vulkan/DX12/Metal)
    // forces a modern backend and avoids that driver's GL path entirely.
    let mut wgpu_setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    wgpu_setup.instance_descriptor.backends = wgpu::Backends::PRIMARY;
    let native_options = eframe::NativeOptions {
        viewport,
        wgpu_options: egui_wgpu::WgpuConfiguration {
            wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(wgpu_setup),
            ..Default::default()
        },
        ..Default::default()
    };

    eframe::run_native(
        "rsnap-overlay",
        native_options,
        Box::new(move |cc| {
            let icons = icons::load_all(&cc.egui_ctx, 20);
            Ok(Box::new(OverlayApp::new(
                layout,
                captures,
                quit_id,
                show_id,
                stop_recording_id,
                settings_id,
                icons,
                hotkey_events,
                config,
            )))
        }),
    )
}

/// The project logo, embedded into the binary at compile time (rather than
/// read from disk at runtime, since we don't want the app to depend on
/// `logo.png` existing next to the exe wherever it ends up installed).
/// Resized down to a standard tray icon size — the source asset is a large
/// (1024x1024) square PNG, too big to hand to the tray directly.
fn app_icon() -> tray_icon::Icon {
    let bytes = include_bytes!("../logo.png");
    let img = image::load_from_memory(bytes)
        .expect("logo.png should decode")
        .resize_exact(32, 32, image::imageops::FilterType::Lanczos3);
    let rgba = img.to_rgba8();
    tray_icon::Icon::from_rgba(rgba.into_raw(), 32, 32).expect("valid icon data")
}
