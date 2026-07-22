//! Screen recording: feeds live per-monitor frames into `windows-capture`'s
//! Media Foundation encoder. Video-only — audio is explicitly out of scope;
//! reuses the same persistent per-monitor WGC sessions the still-capture
//! path already keeps warm, rather than starting a separate capture session.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, ContainerSettingsSubType, VideoEncoder, VideoSettingsBuilder,
};

use crate::capture::{self, SharedFrame};
use crate::config::RecordingContainer;
use crate::monitors::{MonitorInfo, PxRect};

/// An in-progress screen recording: owns the Media Foundation encoder, the
/// virtual-desktop rect being recorded, and the pause/resume bookkeeping
/// described on the fields below. Created by `start`, fed frames via
/// `maybe_push_frame`, and finalized by `stop`.
pub struct Recorder {
    encoder: Option<VideoEncoder>,
    rect: PxRect,
    fps: u32,
    /// Output file this recording is being written to — kept so the dock's
    /// Copy/Save-As actions have a path to operate on once recording stops.
    path: PathBuf,
    started_at: Instant,
    last_pushed_at: Option<Instant>,
    /// Emulated pause. Media Foundation's encoder has no native pause, so
    /// pausing just stops pushing frames (`maybe_push_frame` early-returns)
    /// and freezes the timestamp clock: `paused_total` accumulates every
    /// completed pause span, and `paused_at` marks an open one. Frame
    /// timestamps and `elapsed()` both subtract this, so the output video has
    /// no frozen-frame gap across a pause — it simply resumes where it left
    /// off.
    paused: bool,
    paused_at: Option<Instant>,
    paused_total: Duration,
    /// Last known-good frame per monitor (same index as `monitors`) — WGC
    /// occasionally delivers a spurious blank/white frame (the same issue
    /// worked around for still captures via `looks_suspiciously_blank`), and
    /// `maybe_push_frame` reads straight from the live capture slots with no
    /// equivalent protection of its own, so it needs to fall back to
    /// whatever last looked real rather than encoding the blank frame
    /// as-is.
    last_good_frames: Vec<Option<SharedFrame>>,
}

impl Recorder {
    /// `fps`/`bitrate_mbps`/`container` come from Settings
    /// (`Config::recording_fps`/`recording_bitrate_mbps`/
    /// `recording_container`) — user-configurable rather than fixed
    /// defaults.
    pub fn start(
        rect: PxRect,
        monitor_count: usize,
        path: &Path,
        fps: u32,
        bitrate_mbps: u32,
        container: RecordingContainer,
    ) -> Result<Self, String> {
        let width = rect.width() as u32;
        let height = rect.height() as u32;
        let container_sub_type = match container {
            RecordingContainer::Mp4 => ContainerSettingsSubType::MPEG4,
            RecordingContainer::Avi => ContainerSettingsSubType::AVI,
        };
        let encoder = VideoEncoder::new(
            VideoSettingsBuilder::new(width, height)
                .frame_rate(fps)
                .bitrate(bitrate_mbps.saturating_mul(1_000_000)),
            AudioSettingsBuilder::new().disabled(true),
            ContainerSettingsBuilder::new().sub_type(container_sub_type),
            path,
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            encoder: Some(encoder),
            rect,
            fps,
            path: path.to_path_buf(),
            started_at: Instant::now(),
            last_pushed_at: None,
            paused: false,
            paused_at: None,
            paused_total: Duration::ZERO,
            last_good_frames: vec![None; monitor_count],
        })
    }

    /// The file this recording is being written to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Recording time so far, excluding any paused spans (so a paused
    /// recording shows a frozen timer).
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed().saturating_sub(self.total_paused())
    }

    /// Whether the recording is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Total time spent paused, including the currently-open pause span if
    /// paused right now.
    fn total_paused(&self) -> Duration {
        match self.paused_at {
            Some(at) => self.paused_total + at.elapsed(),
            None => self.paused_total,
        }
    }

    /// Pauses the recording: `maybe_push_frame` stops encoding new frames and
    /// `elapsed()`'s timer freezes until `resume` is called. A no-op if
    /// already paused.
    pub fn pause(&mut self) {
        if !self.paused {
            self.paused = true;
            self.paused_at = Some(Instant::now());
        }
    }

    /// Resumes a paused recording. A no-op if not currently paused.
    pub fn resume(&mut self) {
        if self.paused {
            if let Some(at) = self.paused_at.take() {
                self.paused_total += at.elapsed();
            }
            self.paused = false;
        }
    }

    /// Crops the current live frames to the recording rect and pushes them
    /// to the encoder, throttled to the configured frame rate — called
    /// every `logic()` pass (which can run far more often than that), so
    /// most calls are a no-op check against the last push time rather than
    /// doing any real work.
    pub fn maybe_push_frame(&mut self, monitors: &[MonitorInfo], frames: &[Option<SharedFrame>]) {
        // While paused, push nothing — the encoder simply gets no frames for
        // that span, and the timestamp math below (which subtracts paused
        // time) makes the next real frame continue seamlessly.
        if self.paused {
            return;
        }

        let interval = Duration::from_secs_f64(1.0 / self.fps as f64);
        if let Some(last) = self.last_pushed_at
            && last.elapsed() < interval
        {
            return;
        }
        self.last_pushed_at = Some(Instant::now());

        // Swap in the last known-good frame for any monitor whose latest
        // capture looks like one of WGC's spurious blank/white frames,
        // rather than encoding that directly. `crop_virtual_desktop` itself
        // has no opinion on frame quality — it just copies whatever bytes
        // it's given.
        let sanitized: Vec<Option<SharedFrame>> = frames
            .iter()
            .enumerate()
            .map(|(i, frame)| match frame {
                Some(f) if !capture::looks_suspiciously_blank(f) => {
                    self.last_good_frames[i] = Some(f.clone());
                    Some(f.clone())
                }
                _ => self.last_good_frames.get(i).cloned().flatten(),
            })
            .collect();

        let Some(cropped) = capture::crop_virtual_desktop(monitors, &sanitized, self.rect) else {
            return;
        };
        let bgra = to_bgra_bottom_up(&cropped);
        // 100ns ticks, matching `TimeSpan`'s unit (see the encoder's own
        // audio-clock math, which uses the same 10_000_000-per-second base).
        // Use paused-adjusted elapsed so the encoded timeline has no gap
        // across a pause (see `paused_total`).
        let timestamp = self.elapsed().as_micros() as i64 * 10;

        if let Some(encoder) = &mut self.encoder
            && let Err(e) = encoder.send_frame_buffer(&bgra, timestamp)
        {
            crate::logging::log_error(format!("Recording frame send failed: {e}"));
        }
    }

    /// Finalizes the video file. Consuming `self` mirrors `VideoEncoder`'s
    /// own `finish(self)` — there's nothing meaningful to do with a
    /// recorder after this.
    pub fn stop(mut self) -> Result<(), String> {
        match self.encoder.take() {
            Some(encoder) => encoder.finish().map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }
}

/// `VideoEncoder::send_frame_buffer`'s raw-buffer path requires BGRA8,
/// bottom-to-top row order — our own capture pipeline produces top-down
/// RGBA (the natural order for `image::RgbaImage`), so every frame needs
/// its channels reordered and its rows reversed before being handed off.
fn to_bgra_bottom_up(img: &image::RgbaImage) -> Vec<u8> {
    let (w, h) = img.dimensions();
    let row_bytes = (w * 4) as usize;
    let src = img.as_raw();
    let mut out = vec![0u8; src.len()];

    for y in 0..h as usize {
        let src_row = &src[y * row_bytes..(y + 1) * row_bytes];
        let dst_y = h as usize - 1 - y;
        let dst_row = &mut out[dst_y * row_bytes..(dst_y + 1) * row_bytes];
        for x in 0..w as usize {
            let s = &src_row[x * 4..x * 4 + 4];
            dst_row[x * 4] = s[2];
            dst_row[x * 4 + 1] = s[1];
            dst_row[x * 4 + 2] = s[0];
            dst_row[x * 4 + 3] = s[3];
        }
    }
    out
}

fn local_now() -> SYSTEMTIME {
    unsafe { GetLocalTime() }
}

/// Default recording output path, mirroring `output::autosave`'s layout:
/// `{base}\rsnap\[year]\[month]\video-[date]_[time].{ext}`, `base` defaulting
/// to the user's Videos folder when `base_dir` is `None` (the
/// user-configurable save location set in Settings).
pub fn default_video_path(base_dir: Option<&Path>, container: RecordingContainer) -> Result<PathBuf, String> {
    let st = local_now();
    let mut dir = match base_dir {
        Some(dir) => dir.to_path_buf(),
        None => dirs::video_dir().ok_or("no Videos directory available")?,
    };
    dir.push("rsnap");
    dir.push(format!("{:04}", st.wYear));
    dir.push(format!("{:02}", st.wMonth));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let name = format!(
        "video-{:04}{:02}{:02}_{:02}{:02}{:02}.{}",
        st.wYear,
        st.wMonth,
        st.wDay,
        st.wHour,
        st.wMinute,
        st.wSecond,
        container.extension()
    );
    Ok(dir.join(name))
}
