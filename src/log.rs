//! Minimal append-only logger.
//!
//! On Windows the binary is built with `windows_subsystem = "windows"`, so there is no console
//! and every `eprintln!` in this program is discarded. This file is the only place a failure
//! becomes visible there, which matters a great deal when the symptom being debugged is
//! "the tray icon is quietly wrong".
//!
//! Never log the access token or the `Authorization` header.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_FILE: &str = "log.txt";

/// Truncate the log once it passes this size. Keeps a long-running tray app from
/// slowly filling the home directory.
const MAX_LOG_BYTES: u64 = 256 * 1024;

static LOG_PATH: OnceLock<Mutex<PathBuf>> = OnceLock::new();

/// How much the logger writes.
///
/// `Error` is the quiet default — only failures and non-OK responses. `Info` adds the normal
/// lifecycle narration (startup, the auth flow, update progress, the per-cycle poll heartbeat):
/// what you turn on to understand *why* the icon is doing something, not only *that* it broke. Set
/// through `config.txt`'s `logLevel`.
///
/// Ordered so a smaller discriminant is more severe: a line is written when its level is `<=` the
/// configured threshold.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Error = 0,
    Info = 1,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Info => "INFO",
        }
    }

    /// Parses a `logLevel` value. `None` for anything unrecognised, so a typo keeps the caller's
    /// default rather than silently silencing or flooding the log.
    pub fn parse(value: &str) -> Option<Level> {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "info" => Some(Level::Info),
            _ => None,
        }
    }
}

/// The active threshold. Starts at `Error` — the same default `config.txt` documents — so the few
/// lines emitted before the config is read obey the quiet default too.
static THRESHOLD: AtomicU8 = AtomicU8::new(Level::Error as u8);

/// Raises or lowers how much is written. Called once, right after the config is read.
pub fn set_level(level: Level) {
    THRESHOLD.store(level as u8, Ordering::Relaxed);
}

fn enabled(level: Level) -> bool {
    (level as u8) <= THRESHOLD.load(Ordering::Relaxed)
}

/// Points the logger at `<app_asset_path>/log.txt`. Calls before this are dropped.
pub fn init(app_asset_path: &Path) {
    let _ = LOG_PATH.set(Mutex::new(app_asset_path.join(LOG_FILE)));
}

/// Appends one timestamped, level-tagged line — but only when `level` is at or below the configured
/// threshold, so a quiet default drops the `Info` lines before they ever touch the disk. Never
/// panics and never returns an error: a logger that can take down the caller is worse than none.
pub fn write_line(level: Level, line: &str) {
    if !enabled(level) {
        return;
    }
    let tag = level.tag();

    // Mirror to stderr so anyone running from a terminal (i.e. always on Linux) still sees it.
    #[cfg(not(target_os = "windows"))]
    eprintln!("[{tag}] {line}");

    let Some(path) = LOG_PATH.get() else { return };
    let Ok(path) = path.lock() else { return };

    if std::fs::metadata(&*path).map(|m| m.len()).unwrap_or(0) > MAX_LOG_BYTES {
        let _ = std::fs::write(&*path, "-- log truncated --\n");
    }

    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&*path) {
        let _ = writeln!(file, "{} [{tag}] {}", utc_now(), line);
    }
}

/// Appends an `ERROR` line to `~/.github-trayicon/log.txt`: a failure or non-OK response. Written
/// at every level, including the `error` default.
#[macro_export]
macro_rules! errorln {
    ($($arg:tt)*) => { $crate::log::write_line($crate::log::Level::Error, &format!($($arg)*)) };
}

/// Appends an `INFO` line: normal lifecycle narration. Silent unless `logLevel=info`.
#[macro_export]
macro_rules! infoln {
    ($($arg:tt)*) => { $crate::log::write_line($crate::log::Level::Info, &format!($($arg)*)) };
}

// ── Timestamps ────────────────────────────────────────────────────────────────
// Formatted by hand rather than pulling in `chrono`/`time`: a tray app that polls one
// endpoint does not need a date library for the sake of a log prefix.

fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, enabled, set_level, Level};

    #[test]
    fn parse_reads_the_two_levels_case_and_space_insensitively() {
        assert_eq!(Level::parse("error"), Some(Level::Error));
        assert_eq!(Level::parse("info"), Some(Level::Info));
        assert_eq!(Level::parse("  INFO  "), Some(Level::Info));
        // Anything else is `None`, so the caller keeps its default rather than a typo changing it.
        assert_eq!(Level::parse("debug"), None);
        assert_eq!(Level::parse(""), None);
    }

    #[test]
    fn error_outranks_info_so_a_lower_threshold_hides_more() {
        assert!(Level::Error < Level::Info);
    }

    /// The gate itself. Mutates the process-wide threshold, so it lives in one test rather than two
    /// that could interleave — the store/load is `Relaxed` and there is no ordering to assert across
    /// threads, only the within-test truth table.
    #[test]
    fn the_threshold_admits_only_levels_at_or_below_it() {
        set_level(Level::Error);
        assert!(enabled(Level::Error), "error is always written");
        assert!(!enabled(Level::Info), "info is dropped at the error default");

        set_level(Level::Info);
        assert!(enabled(Level::Error), "error still written when info is on");
        assert!(enabled(Level::Info), "info written once the threshold is raised");

        // Leave the default the rest of the suite would expect.
        set_level(Level::Error);
    }

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        // 2000-02-29 — leap year divisible by 400.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        // Both cross-checked with `date -u -d @<days*86400>`.
        assert_eq!(civil_from_days(20_544), (2026, 4, 1));
        assert_eq!(civil_from_days(20_575), (2026, 5, 2));
    }
}
