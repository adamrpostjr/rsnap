//! Minimal error logging for a packaged, console-less build.
//!
//! With `#![windows_subsystem = "windows"]` the process has no console and no
//! stdout/stderr anyone will ever see, so the various fire-and-forget
//! background operations (autosave, save-as, recording finalize, clipboard
//! copy, ...) need somewhere to record failures. This appends a timestamped
//! line per failure to `%APPDATA%\rsnap\rsnap.log`, right next to
//! `config.toml` (see `config::app_dir`). Deliberately best-effort: a logging
//! failure is never allowed to become a second error, let alone a panic.

use std::io::Write;

use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::config::app_dir;

/// Appends `message` to the log file, prefixed with a local timestamp.
/// Silently does nothing if the config directory or the file can't be
/// reached (e.g. a locked-down profile) — logging is a diagnostic nicety,
/// not something worth surfacing a second failure over.
pub fn log_error(message: impl AsRef<str>) {
    let Some(dir) = app_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    let st = unsafe { GetLocalTime() };
    let line = format!(
        "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}] {}\n",
        st.wYear,
        st.wMonth,
        st.wDay,
        st.wHour,
        st.wMinute,
        st.wSecond,
        message.as_ref()
    );

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("rsnap.log"))
    {
        let _ = file.write_all(line.as_bytes());
    }
}
