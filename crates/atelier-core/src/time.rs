//! Tiny RFC 3339 helper. PROVISIONAL — replaced once the harness pulls
//! in a real time crate (chrono / time / jiff) for sub-second precision
//! and timezone support. For now this matches the historical
//! `now_rfc3339` helpers that lived in three different crates (runner,
//! atelier-gui, atelier-tui) byte-for-byte. v57 lifted the helper here
//! so every caller agrees by construction.

/// Returns the current wall-clock time as an RFC 3339 string with
/// second precision and a `Z` suffix (e.g. `2026-05-17T15:30:42Z`).
/// On clock-skew situations where `SystemTime::now()` is *before* the
/// UNIX epoch — possible mid-boot before NTP steps the clock — falls
/// back to `1970-01-01T00:00:00Z`, the original three implementations'
/// behaviour. Callers that need a stricter contract should use a real
/// monotonic clock; this helper is for human-readable timestamps the
/// runner / UIs already accept as "best effort."
pub fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_for_unix_seconds(now)
}

/// Format a UNIX-epoch second count as RFC 3339. Exposed for tests so
/// the formatting is exercisable without depending on the current
/// wall-clock time.
pub fn rfc3339_for_unix_seconds(now: u64) -> String {
    let secs_in_day = 86_400u64;
    let day = (now / secs_in_day) as i64;
    let sod = now % secs_in_day;
    let (h, m, s) = (
        (sod / 3600) as u32,
        ((sod / 60) % 60) as u32,
        (sod % 60) as u32,
    );
    let (y, mo, d) = days_to_ymd(day);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Civil-from-days, Howard Hinnant's algorithm. Translates a count of
/// days since 1970-01-01 into (year, month, day).
fn days_to_ymd(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_renders_as_1970_01_01() {
        assert_eq!(rfc3339_for_unix_seconds(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_dates_round_trip() {
        // 1970-01-02 00:00:00 UTC = 86_400 (one day past epoch — the
        // simplest sanity case for the Howard Hinnant date algorithm
        // without depending on a date calculator).
        assert_eq!(rfc3339_for_unix_seconds(86_400), "1970-01-02T00:00:00Z");
        // 2000-03-01 00:00:00 UTC = 951_868_800 (leap-year boundary;
        // exercises the era/yoe arms of the algorithm).
        assert_eq!(
            rfc3339_for_unix_seconds(951_868_800),
            "2000-03-01T00:00:00Z"
        );
    }

    #[test]
    fn intra_day_seconds_format_with_leading_zeros() {
        // 1970-01-01 03:04:05 UTC = 3*3600 + 4*60 + 5 = 11_045.
        assert_eq!(rfc3339_for_unix_seconds(11_045), "1970-01-01T03:04:05Z");
    }

    #[test]
    fn now_rfc3339_returns_z_suffixed_iso_form() {
        let s = now_rfc3339();
        assert!(
            s.ends_with('Z') && s.len() == 20,
            "expected RFC 3339 Z-suffix, got {s:?}"
        );
    }
}
