//! Persisted user settings: `%APPDATA%\rsnap\config.toml`.
//!
//! Loaded once at startup and edited via the Settings window (tray menu).
//! Most fields apply live the moment Settings is saved; the capture hotkey
//! is the one exception — re-registering a global hotkey needs to happen on
//! the tray thread (the thread that originally called `RegisterHotKey`), and
//! wiring up that cross-thread handoff wasn't worth it for a setting nobody
//! changes often, so a hotkey change just takes effect on next launch (the
//! Settings window says so).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::annotate::Tool;

/// A `RegisterHotKey` combo: modifier flags + a virtual-key code. Stored as
/// plain fields (not `windows`-crate types) so this can derive
/// `Serialize`/`Deserialize` without needing to teach serde about foreign
/// newtypes.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct HotkeyConfig {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    /// Virtual-key code, e.g. `0x2C` (`VK_SNAPSHOT`/PrintScreen) or a letter
    /// key's ASCII value (`b'S' as u32`).
    pub vk: u32,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        // Ctrl+Shift+PrintScreen, this app's default capture combo.
        Self {
            ctrl: true,
            shift: true,
            alt: false,
            win: false,
            vk: 0x2C,
        }
    }
}

impl HotkeyConfig {
    /// Human-readable label for the Settings window and tray menu, e.g.
    /// "Ctrl+Shift+PrtScn".
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.win {
            parts.push("Win");
        }
        parts.push(vk_name(self.vk));
        parts.join("+")
    }
}

/// Best-effort virtual-key-code -> display name. Covers the keys a
/// screenshot hotkey realistically uses; falls back to the raw hex code for
/// anything else rather than guessing.
fn vk_name(vk: u32) -> &'static str {
    match vk {
        0x2C => "PrtScn",
        0x70..=0x87 => match vk {
            0x70 => "F1",
            0x71 => "F2",
            0x72 => "F3",
            0x73 => "F4",
            0x74 => "F5",
            0x75 => "F6",
            0x76 => "F7",
            0x77 => "F8",
            0x78 => "F9",
            0x79 => "F10",
            0x7A => "F11",
            0x7B => "F12",
            _ => "Fn",
        },
        0x30..=0x39 => match vk {
            0x30 => "0",
            0x31 => "1",
            0x32 => "2",
            0x33 => "3",
            0x34 => "4",
            0x35 => "5",
            0x36 => "6",
            0x37 => "7",
            0x38 => "8",
            _ => "9",
        },
        0x41..=0x5A => letter_name(vk),
        _ => "?",
    }
}

/// Maps a letter key's virtual-key code (`0x41..=0x5A`) to its display name.
/// Only ever called from `vk_name` with a value already in that range —
/// panics on anything outside it, so don't reuse this without the same guard.
fn letter_name(vk: u32) -> &'static str {
    const LETTERS: [&str; 26] = [
        "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V",
        "W", "X", "Y", "Z",
    ];
    LETTERS[(vk - 0x41) as usize]
}

/// Still-image output format for autosave/Save As. Clipboard copy always
/// hands the raw bitmap to `arboard` regardless of this setting — it only
/// affects files written to disk.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenshotFormat {
    Png,
    Jpeg,
}

impl ScreenshotFormat {
    /// File extension (no leading dot) for this format.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }

    /// Display label for the format picker / save-dialog filter.
    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
        }
    }
}

/// Recording container, both natively supported by `windows-capture`'s Media
/// Foundation writer. MKV/GIF aren't options: the Media Foundation container
/// writer only supports ASF/MP4/AVI/MPEG2/3GP/AMR (no Matroska), and GIF
/// isn't a video container at all — it would need a separate frame-to-GIF
/// encoder entirely outside this writer.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingContainer {
    Mp4,
    Avi,
}

impl RecordingContainer {
    /// File extension (no leading dot) for this container.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Avi => "avi",
        }
    }

    /// Display label for the container picker / save-dialog filter.
    pub fn label(self) -> &'static str {
        match self {
            Self::Mp4 => "MP4",
            Self::Avi => "AVI",
        }
    }
}

/// The full set of persisted, user-configurable settings — hotkeys, output
/// locations/formats, default markup style, and recording parameters. See
/// this module's doc comment for load/save/persistence semantics.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// Global capture hotkey (`RegisterHotKey`) — changes take effect on
    /// next launch, see this module's doc comment.
    pub hotkey: HotkeyConfig,
    /// In-app shortcuts (plain egui key events, not `RegisterHotKey` — only
    /// active while the overlay itself has focus) — these apply live, no
    /// restart needed.
    pub copy_shortcut: HotkeyConfig,
    pub save_shortcut: HotkeyConfig,
    /// Whether a finalized selection is written to disk automatically (in
    /// addition to whatever the user does manually via copy/save).
    pub autosave_enabled: bool,
    /// Overrides the default `Pictures` folder for screenshot autosave/Save
    /// As when set.
    pub screenshot_save_dir: Option<PathBuf>,
    /// Overrides the default `Videos` folder for recordings when set.
    pub video_save_dir: Option<PathBuf>,
    /// Still-image format used for autosave and Save As.
    pub screenshot_format: ScreenshotFormat,
    /// Only meaningful when `screenshot_format` is `Jpeg`.
    pub jpeg_quality: u8,
    /// Default stroke/fill color for new annotations, as `[r, g, b]`.
    pub default_color: [u8; 3],
    /// Default stroke width (in points) for new annotations.
    pub default_stroke_width: f32,
    /// Which tool (if any) is pre-selected right after a new selection is
    /// made. Independent of the "no tool = move/resize" default *within* an
    /// existing selection — this only affects the moment a fresh selection
    /// is drawn.
    pub default_tool: Option<Tool>,
    /// Caps how many annotations (across all tools, whole session) are kept
    /// for undo — once exceeded, the oldest are dropped permanently rather
    /// than just becoming unreachable, to bound memory on a very long
    /// markup session. `0` means unlimited.
    pub undo_history_limit: usize,
    /// Lightly smooths freehand Draw/Highlight/Magic-Erase strokes (a
    /// 3-point moving average) instead of painting the raw decimated point
    /// path — softens the usual mouse-drawn jitter at a small cost to sharp
    /// corners.
    pub smooth_strokes: bool,
    /// Registers rsnap to launch at Windows sign-in (via the current user's
    /// `Run` registry key — see `win32::set_start_with_windows`).
    pub start_with_windows: bool,
    /// Target frame rate for new recordings.
    pub recording_fps: u32,
    /// Target video bitrate for new recordings, in megabits/second.
    pub recording_bitrate_mbps: u32,
    /// Container format for new recordings.
    pub recording_container: RecordingContainer,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: HotkeyConfig::default(),
            copy_shortcut: HotkeyConfig {
                ctrl: true,
                shift: false,
                alt: false,
                win: false,
                vk: 0x43, // 'C'
            },
            save_shortcut: HotkeyConfig {
                ctrl: true,
                shift: false,
                alt: false,
                win: false,
                vk: 0x53, // 'S'
            },
            autosave_enabled: true,
            screenshot_save_dir: None,
            video_save_dir: None,
            screenshot_format: ScreenshotFormat::Png,
            jpeg_quality: 90,
            default_color: [230, 50, 45],
            default_stroke_width: 3.0,
            default_tool: None,
            undo_history_limit: 0,
            smooth_strokes: true,
            start_with_windows: false,
            recording_fps: 30,
            recording_bitrate_mbps: 15,
            recording_container: RecordingContainer::Mp4,
        }
    }
}

/// `%APPDATA%\rsnap` — the app's own subdirectory under the OS config dir,
/// shared by `config.toml` (this module) and the error log (`crate::logging`).
pub fn app_dir() -> Option<PathBuf> {
    let mut dir = dirs::config_dir()?;
    dir.push("rsnap");
    Some(dir)
}

fn config_path() -> Option<PathBuf> {
    Some(app_dir()?.join("config.toml"))
}

/// Loads the config, falling back to defaults if the file doesn't exist yet
/// or fails to parse (e.g. from a future version with fields this build
/// doesn't know about) — never treated as fatal.
pub fn load() -> Config {
    config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persists `config` to `config.toml`, creating the containing directory if
/// needed. Called whenever the Settings window is saved.
pub fn save(config: &Config) -> Result<(), String> {
    let path = config_path().ok_or("no config directory available")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = toml::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}
