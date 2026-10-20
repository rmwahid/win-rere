//! A small append-only log.
//!
//! The release binary has no console, so failures have to go somewhere the
//! user can find later. Everything is best effort: a logging problem must never
//! take the widget down.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_LOCK: Mutex<()> = Mutex::new(());

/// The log is rotated once it passes this size.
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// Points the log at its file and rotates an oversized file out of the way.
pub fn init(path: PathBuf) {
    if let Ok(metadata) = fs::metadata(&path) {
        if metadata.len() > MAX_LOG_BYTES {
            let previous = path.with_extension("log.old");
            let _ = fs::remove_file(&previous);
            let _ = fs::rename(&path, previous);
        }
    }
    let _ = LOG_PATH.set(path);
}

/// Writes one line, prefixed with a UTC timestamp.
pub fn line(message: &str) {
    #[cfg(debug_assertions)]
    eprintln!("win-rere: {message}");

    let Some(path) = LOG_PATH.get() else {
        return;
    };
    let _guard = LOG_LOCK.lock();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {message}", now_iso8601());
    }
}

/// The current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now_iso8601() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60
    )
}

/// Converts days since the Unix epoch into a civil date.
///
/// This is Howard Hinnant's `civil_from_days`, with the epoch shifted to
/// 1970-01-01. It avoids pulling a date crate in for one log prefix.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> String {
        let days = seconds.div_euclid(86_400);
        let remainder = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            remainder / 3600,
            (remainder % 3600) / 60,
            remainder % 60
        )
    }

    #[test]
    fn formats_the_epoch() {
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(at(86_400), "1970-01-02T00:00:00Z");
    }

    #[test]
    fn formats_a_modern_timestamp() {
        assert_eq!(at(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
