//! Small helpers without a dependency.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Host monotonic time in µs since the first call in this process: one epoch for every log that
/// pairs host and device timestamps (frames.csv, audio.csv).
pub fn host_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// UTC `YYYYmmdd-HHMMSS`, without a date crate.
pub fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem / 60 % 60, rem % 60)
}

pub fn hexdump(base: u32, data: &[u8]) -> String {
    let mut s = String::new();
    for (i, row) in data.chunks(16).enumerate() {
        s.push_str(&format!("{:04x}:", base as usize + i * 16));
        for b in row {
            s.push_str(&format!(" {b:02x}"));
        }
        s.push('\n');
    }
    s
}
