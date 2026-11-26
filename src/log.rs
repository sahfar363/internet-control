//! Tiny dependency-free file logger.
//!
//! Writes `internet-control.log` next to the exe (falling back to the
//! temp dir when the exe directory is not writable). The exe is built
//! with `windows_subsystem = "windows"`, so there is no console and
//! `eprintln!` output is invisible — this file is the only way to see
//! what happened on Sahand's PC when the window is white or the app
//! misbehaves. Every line is timestamped (UTC) and flushed, so the
//! log survives the process ending.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Initialise the log next to the exe, or in the temp dir as a
/// fallback. Safe to call more than once; later calls are no-ops.
pub fn init() {
    let path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("internet-control.log")))
        .filter(|p| {
            // Only use the exe dir if we can actually create the file
            // there (e.g. not a read-only folder).
            OpenOptions::new().create(true).append(true).open(p).is_ok()
        })
        .or_else(|| Some(std::env::temp_dir().join("internet-control.log")));
    if let Some(path) = path {
        init_at(&path);
    }
}

/// Initialise the log at an explicit path (used by tests). Returns
/// the path when the file could be opened.
pub fn init_at(path: &PathBuf) -> Option<PathBuf> {
    if LOG_FILE.get().is_some() {
        return LOG_PATH.get().cloned();
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let _ = writeln!(
        file,
        "=== Internet Control v{} started at {} ===",
        env!("CARGO_PKG_VERSION"),
        format_timestamp(now_secs())
    );
    let _ = file.flush();
    let _ = LOG_PATH.set(path.clone());
    let _ = LOG_FILE.set(Mutex::new(Some(file)));
    Some(path.clone())
}

/// Path of the log file, if initialisation succeeded.
pub fn path() -> Option<PathBuf> {
    LOG_PATH.get().cloned()
}

/// Write one timestamped line, flushed immediately. No-op when the
/// log was never initialised or the file is gone.
pub fn log(msg: &str) {
    let Some(lock) = LOG_FILE.get() else {
        return;
    };
    if let Ok(mut guard) = lock.lock() {
        if let Some(file) = guard.as_mut() {
            let _ = writeln!(file, "[{}] {msg}", format_timestamp(now_secs()));
            let _ = file.flush();
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM:SS UTC` for a Unix timestamp. Pure std — no
/// chrono. (Civil-from-days algorithm.)
pub fn format_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_known_dates() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_timestamp(86_400), "1970-01-02 00:00:00 UTC");
        assert_eq!(format_timestamp(1_700_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(format_timestamp(1_795_651_200), "2026-11-26 00:00:00 UTC");
        // Leap day.
        assert_eq!(format_timestamp(1_709_164_800), "2024-02-29 00:00:00 UTC");
    }

    #[test]
    fn log_writes_timestamped_lines_to_file() {
        let dir = std::env::temp_dir().join(format!(
            "ic-log-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("internet-control.log");
        // init_at is global-once: this test may run after another
        // test initialised the logger, so only assert when we won
        // the race — otherwise the file may already exist from the
        // shared logger. To stay deterministic, write through the
        // public API and check the file we asked for when it is
        // the one in use.
        let got = init_at(&path);
        if got == Some(path.clone()) {
            log("hello from the test");
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("hello from the test"), "{text}");
            assert!(text.contains("UTC"), "{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
