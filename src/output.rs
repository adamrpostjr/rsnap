//! Output actions for a finalized capture: clipboard copy, native save
//! dialog, and autosave to a default folder.

use std::path::{Path, PathBuf};

use image::RgbaImage;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::config::ScreenshotFormat;

/// Local wall-clock date/time, via Win32 `GetLocalTime` rather than pulling
/// in a `chrono`-style crate just for this. Used for both the autosave
/// folder layout (`year/month`) and the filename timestamp.
fn local_now() -> SYSTEMTIME {
    unsafe { GetLocalTime() }
}

/// Default filename for save-as/autosave: `image-YYYYMMDD_HHMMSS.{ext}`.
pub fn default_filename(format: ScreenshotFormat) -> String {
    let st = local_now();
    format!(
        "image-{:04}{:02}{:02}_{:02}{:02}{:02}.{}",
        st.wYear,
        st.wMonth,
        st.wDay,
        st.wHour,
        st.wMinute,
        st.wSecond,
        format.extension()
    )
}

/// Writes `image` to `path` in `format`. JPEG has no alpha channel, so that
/// path flattens onto RGB first (PNG keeps the alpha as-is).
fn write_image(image: &RgbaImage, path: &Path, format: ScreenshotFormat, jpeg_quality: u8) -> Result<(), String> {
    match format {
        ScreenshotFormat::Png => image.save(path).map_err(|e| e.to_string()),
        ScreenshotFormat::Jpeg => {
            let rgb = image::DynamicImage::ImageRgba8(image.clone()).into_rgb8();
            let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, jpeg_quality);
            encoder.encode_image(&rgb).map_err(|e| e.to_string())
        }
    }
}

/// Copy the image to the system clipboard.
pub fn copy_to_clipboard(image: &RgbaImage) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let img_data = arboard::ImageData {
        width: image.width() as usize,
        height: image.height() as usize,
        bytes: std::borrow::Cow::Borrowed(image.as_raw()),
    };
    clipboard.set_image(img_data).map_err(|e| e.to_string())
}

/// Show a native "Save As" dialog on its own dedicated thread and write the
/// image to wherever the user picks.
///
/// The dialog runs on a separate thread deliberately: Windows' native file
/// dialogs pump their own modal message loop while open, the same way
/// `TrackPopupMenu` does for the tray icon's context menu — and sharing a
/// thread with winit's own event loop caused exactly that combination to
/// hang/lag (see the tray icon fix). Keeping the dialog off the winit thread
/// avoids the same failure mode here. This is fire-and-forget: the caller
/// should hide the overlay immediately rather than waiting on the result.
pub fn save_via_dialog(
    image: RgbaImage,
    default_dir: Option<PathBuf>,
    default_name: String,
    format: ScreenshotFormat,
    jpeg_quality: u8,
) {
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new()
            .add_filter(format.label(), &[format.extension()])
            .set_file_name(&default_name);
        if let Some(dir) = default_dir {
            dialog = dialog.set_directory(dir);
        }

        let Some(path) = dialog.save_file() else {
            return;
        };
        if let Err(e) = write_image(&image, &path, format, jpeg_quality) {
            crate::logging::log_error(format!("Failed to save {}: {e}", path.display()));
        }
    });
}

/// Write the image to the autosave folder
/// (`{base}\rsnap\[year]\[month]\image-[date]_[time].{ext}`, `base`
/// defaulting to the user's Pictures folder when `base_dir` is `None` — the
/// user-configurable save location set in Settings), creating any missing
/// directories. Synchronous — this is just a file write, not a dialog, so no
/// thread isolation is needed.
pub fn autosave(
    image: &RgbaImage,
    base_dir: Option<&Path>,
    format: ScreenshotFormat,
    jpeg_quality: u8,
) -> Result<PathBuf, String> {
    let st = local_now();

    let mut dir = match base_dir {
        Some(dir) => dir.to_path_buf(),
        None => dirs::picture_dir().ok_or("no Pictures directory available")?,
    };
    dir.push("rsnap");
    dir.push(format!("{:04}", st.wYear));
    dir.push(format!("{:02}", st.wMonth));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let path = dir.join(default_filename(format));
    write_image(image, &path, format, jpeg_quality)?;
    Ok(path)
}

/// Puts an actual file (not its pixels) on the clipboard as `CF_HDROP`, the
/// same "copied files" format Explorer uses — so pasting into Teams, Explorer,
/// an email, etc. attaches the file itself. Used by the recording dock's Copy
/// button (the finished video), where there's no in-memory image to hand to
/// `copy_to_clipboard`.
///
/// Raw Win32: build a `DROPFILES` header followed by the double-NUL-terminated
/// wide path in a movable global allocation, then hand ownership to the
/// clipboard via `SetClipboardData` (which is why the allocation is *not*
/// freed on the success path — the clipboard owns it after that).
pub fn copy_file_to_clipboard(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::DROPFILES;
    use windows::core::w;

    // File list: the path, NUL-terminated, then a second NUL to end the list.
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide.push(0);

    let header = size_of::<DROPFILES>();
    let total = header + wide.len() * size_of::<u16>();

    // Allocates a movable global block and copies `bytes` into it, for handing
    // to `SetClipboardData` (which takes ownership on success).
    unsafe fn alloc_global(bytes: &[u8]) -> Result<windows::Win32::Foundation::HGLOBAL, String> {
        unsafe {
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| e.to_string())?;
            let ptr = GlobalLock(hglobal);
            if ptr.is_null() {
                let _ = GlobalUnlock(hglobal);
                return Err("GlobalLock failed".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
            let _ = GlobalUnlock(hglobal);
            Ok(hglobal)
        }
    }

    unsafe {
        // Build the DROPFILES block in a byte buffer, then copy it into a
        // global allocation via the shared helper.
        let mut buf = vec![0u8; total];
        let df = DROPFILES {
            pFiles: header as u32,
            pt: windows::Win32::Foundation::POINT { x: 0, y: 0 },
            fNC: false.into(),
            fWide: true.into(),
        };
        std::ptr::write(buf.as_mut_ptr() as *mut DROPFILES, df);
        std::ptr::copy_nonoverlapping(wide.as_ptr(), buf.as_mut_ptr().add(header) as *mut u16, wide.len());
        let hdrop = alloc_global(&buf)?;

        // "Preferred DropEffect" = DROPEFFECT_COPY (1): marks this an explicit
        // *copy* the way Explorer's own Ctrl+C does. Some paste targets (Teams
        // among them) ignore a bare `CF_HDROP` unless this is present.
        let cf_drop_effect = RegisterClipboardFormatW(w!("Preferred DropEffect"));
        let heffect = alloc_global(&1u32.to_ne_bytes())?;

        OpenClipboard(None).map_err(|e| e.to_string())?;
        // From here, always close the clipboard before returning.
        let result = (|| {
            EmptyClipboard().map_err(|e| e.to_string())?;
            SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(hdrop.0))).map_err(|e| e.to_string())?;
            if cf_drop_effect != 0 {
                SetClipboardData(cf_drop_effect, Some(HANDLE(heffect.0))).map_err(|e| e.to_string())?;
            }
            Ok(())
        })();
        let _ = CloseClipboard();

        // On failure the clipboard never took ownership, so the movable
        // global allocations leak — a few hundred bytes, one-time, only on the
        // rare path where the clipboard was locked by another app. Not worth
        // pulling in `GlobalFree` (not exposed by the enabled features) for.
        result
    }
}

/// Show a native "Save As" dialog on its own thread and copy an already-written
/// file (`src`) to wherever the user picks. Used by the recording dock's Save
/// button: the video is already fully encoded on disk at `src` (in the
/// configured Videos folder), so this just copies it out — the container format
/// is fixed at record time, so there's no re-encode. Fire-and-forget, on a
/// separate thread for the same reason as `save_via_dialog`.
pub fn save_file_via_dialog(
    src: PathBuf,
    default_dir: Option<PathBuf>,
    default_name: String,
    label: &'static str,
    ext: &'static str,
) {
    std::thread::spawn(move || {
        let mut dialog = rfd::FileDialog::new()
            .add_filter(label, &[ext])
            .set_file_name(&default_name);
        if let Some(dir) = default_dir {
            dialog = dialog.set_directory(dir);
        }

        let Some(dest) = dialog.save_file() else {
            return;
        };
        if let Err(e) = std::fs::copy(&src, &dest) {
            crate::logging::log_error(format!("Failed to save recording to {}: {e}", dest.display()));
        }
    });
}
