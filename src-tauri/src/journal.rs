//! Event log for support: one line per decision or failure, UTC (so two devices' logs line up), in
//! `app_data_dir/logs/loka.log`, rotated at 1 MiB to `loka.1.log`; its tail goes into `diagnostics`.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX: u64 = 1 << 20;

struct Log {
    file: File,
    path: PathBuf,
    len: u64,
}

static LOG: Mutex<Option<Log>> = Mutex::new(None);

/// `journal!("...", args)`: one line in the log (and stderr).
macro_rules! journal {
    ($($t:tt)*) => { $crate::journal::write(&format!($($t)*)) };
}

/// Opens the log in `dir/logs` and logs panics there before the default hook runs.
pub fn init(dir: &Path) {
    let dir = dir.join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("loka.log");
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
        let len = file.metadata().map_or(0, |m| m.len());
        *lock() = Some(Log { file, path, len });
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write(&format!("PANIC {info}"));
        prev(info);
    }));
}

fn lock() -> std::sync::MutexGuard<'static, Option<Log>> {
    LOG.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn write(line: &str) {
    eprintln!("{line}");
    let line = format!("{} {line}\n", stamp(SystemTime::now()));
    let mut g = lock();
    let Some(log) = g.as_mut() else { return };
    if log.len + line.len() as u64 > MAX {
        let _ = std::fs::rename(&log.path, log.path.with_file_name("loka.1.log"));
        match OpenOptions::new().create(true).append(true).open(&log.path) {
            Ok(f) => (log.file, log.len) = (f, 0),
            Err(_) => return,
        }
    }
    if log.file.write_all(line.as_bytes()).is_ok() {
        log.len += line.len() as u64;
    }
}

/// The last `n` lines of the log (across the rotation).
pub fn tail(n: usize) -> String {
    let Some(path) = lock().as_ref().map(|l| l.path.clone()) else { return "(no log)".into() };
    let text = [path.with_file_name("loka.1.log"), path].iter().filter_map(|p| std::fs::read_to_string(p).ok()).collect::<String>();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// `2026-05-03 14:03:12.345Z`
fn stamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (secs, ms) = (d.as_secs(), d.subsec_millis());
    let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
    // civil from days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{ms:03}Z", rest / 3600, rest / 60 % 60, rest % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stamps_utc() {
        assert_eq!(stamp(UNIX_EPOCH), "1970-01-01 00:00:00.000Z");
        // 2024-02-29 23:59:59.5 (leap day)
        assert_eq!(stamp(UNIX_EPOCH + Duration::from_millis(1_709_251_199_500)), "2024-02-29 23:59:59.500Z");
        assert_eq!(stamp(UNIX_EPOCH + Duration::from_secs(1_709_251_200)), "2024-03-01 00:00:00.000Z");
    }
}
