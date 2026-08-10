//! Timestamps without a date dependency.
//!
//! Everything on the wire is RFC3339 UTC with millisecond precision so the
//! iOS client can decode with a single `ISO8601DateFormatter` configuration.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds on the host's monotonic clock — one clock for every process
/// on this machine, immune to wall-time steps. This is the clock deadlines
/// cross a process boundary on: a wall stamp can jump backward and grant a
/// request more time than its sender will wait.
///
/// `None` when the clock cannot be read. Both callers fail closed on it:
/// the daemon refuses to issue a send it cannot bound, and the supervisor
/// refuses a stamped send it cannot clock — neither substitutes a budget.
pub fn now_monotonic_ms() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ok = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if ok != 0 {
        return None;
    }
    Some((ts.tv_sec as u64) * 1_000 + (ts.tv_nsec as u64) / 1_000_000)
}

/// Milliseconds since the Unix epoch. Saturates at 0 for pre-epoch clocks
/// (a Mac whose clock is that wrong has bigger problems than a negative ts).
pub fn now_unix_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis().min(i64::MAX as u128) as i64,
        Err(_) => 0,
    }
}

/// RFC3339 UTC, millisecond precision, e.g. `2026-07-30T16:36:58.412Z`.
pub fn rfc3339_from_unix_ms(ms: i64) -> String {
    let (secs, millis) = if ms >= 0 {
        (ms / 1000, (ms % 1000) as u32)
    } else {
        // Floor division so the millisecond part stays in [0, 1000).
        let s = (ms - 999) / 1000;
        (s, (ms - s * 1000) as u32)
    };
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
        millis
    )
}

pub fn now_rfc3339() -> String {
    rfc3339_from_unix_ms(now_unix_ms())
}

/// Milliseconds since the epoch for a UTC civil date-time.
///
/// The inverse of [`rfc3339_from_unix_ms`], and the reason it exists: reading a
/// certificate's `notAfter` out of DER means turning `20261029120000Z` back into
/// an instant, and pulling in a date crate for one arithmetic identity would be
/// a poor trade.
pub fn unix_ms_from_civil(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> i64 {
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + (hour as i64) * 3600 + (minute as i64) * 60 + second as i64;
    seconds * 1000
}

/// Parse a timestamp this module produced back into an instant.
///
/// Deliberately narrow: it accepts exactly the shape [`rfc3339_from_unix_ms`]
/// emits (`YYYY-MM-DDTHH:MM:SS.mmmZ`, optionally without the milliseconds) and
/// returns `None` for anything else rather than guessing at an offset or a
/// locale. Its one caller is the schema migration, which turns a stored
/// `created_at` into the timestamp half of a synthesised session uid so migrated
/// runs keep their real order; a `None` there falls back to "now", which costs
/// ordering rather than correctness.
pub fn unix_ms_from_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !(bytes[10] == b'T' || bytes[10] == b't' || bytes[10] == b' ') {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    if !matches!(bytes[19], b'Z' | b'z' | b'.') {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: u32 = text.get(5..7)?.parse().ok()?;
    let day: u32 = text.get(8..10)?.parse().ok()?;
    let hour: u32 = text.get(11..13)?.parse().ok()?;
    let minute: u32 = text.get(14..16)?.parse().ok()?;
    let second: u32 = text.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let millis: i64 = if bytes[19] == b'.' {
        let fraction: String = text[20..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .take(3)
            .collect();
        if fraction.is_empty() {
            return None;
        }
        // "4" means 400ms, not 4ms.
        format!("{fraction:0<3}").parse().ok()?
    } else {
        0
    };
    Some(unix_ms_from_civil(year, month, day, hour, minute, second) + millis)
}

/// Howard Hinnant's `days_from_civil`: (y, m, d) -> days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 -> (y, m, d).
/// Exact for the whole proleptic Gregorian range we care about.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_instants() {
        assert_eq!(rfc3339_from_unix_ms(0), "1970-01-01T00:00:00.000Z");
        // 2026-07-30T16:36:58.412Z
        assert_eq!(
            rfc3339_from_unix_ms(1_785_429_418_412),
            "2026-07-30T16:36:58.412Z"
        );
        // Leap day.
        assert_eq!(
            rfc3339_from_unix_ms(1_709_164_800_000),
            "2024-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn negative_ms_floors_correctly() {
        assert_eq!(rfc3339_from_unix_ms(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(rfc3339_from_unix_ms(-1000), "1969-12-31T23:59:59.000Z");
    }

    #[test]
    fn civil_conversion_round_trips_against_its_own_inverse() {
        // Every day for 60 years, both directions. Cheap, and it makes the
        // hand-rolled calendar arithmetic something we know rather than hope.
        let mut ms = unix_ms_from_civil(1990, 1, 1, 0, 0, 0);
        let end = unix_ms_from_civil(2050, 1, 1, 0, 0, 0);
        while ms < end {
            let text = rfc3339_from_unix_ms(ms);
            let (y, m, d) = (
                text[0..4].parse::<i64>().unwrap(),
                text[5..7].parse::<u32>().unwrap(),
                text[8..10].parse::<u32>().unwrap(),
            );
            assert_eq!(unix_ms_from_civil(y, m, d, 0, 0, 0), ms, "{text}");
            ms += 86_400_000;
        }
    }

    #[test]
    fn known_instants_convert_forwards() {
        assert_eq!(unix_ms_from_civil(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(
            unix_ms_from_civil(2026, 7, 30, 16, 36, 58),
            1_785_429_418_000
        );
        // Leap day, and the day after.
        assert_eq!(
            rfc3339_from_unix_ms(unix_ms_from_civil(2024, 2, 29, 0, 0, 0)),
            "2024-02-29T00:00:00.000Z"
        );
        assert_eq!(
            rfc3339_from_unix_ms(unix_ms_from_civil(2024, 3, 1, 23, 59, 59)),
            "2024-03-01T23:59:59.000Z"
        );
        // Century non-leap year: 1900 is not a leap year, 2000 is.
        assert_eq!(
            rfc3339_from_unix_ms(unix_ms_from_civil(1900, 3, 1, 0, 0, 0)),
            "1900-03-01T00:00:00.000Z"
        );
        assert_eq!(
            rfc3339_from_unix_ms(unix_ms_from_civil(2000, 2, 29, 0, 0, 0)),
            "2000-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn monotonic_format_is_lexicographically_sortable() {
        let a = rfc3339_from_unix_ms(1_785_429_418_412);
        let b = rfc3339_from_unix_ms(1_785_429_418_413);
        assert!(a < b);
    }

    #[test]
    fn parsing_inverts_our_own_formatting() {
        for ms in [
            0i64,
            412,
            1_785_429_418_412,
            1_709_164_800_000,
            unix_ms_from_civil(2050, 12, 31, 23, 59, 59) + 999,
        ] {
            let text = rfc3339_from_unix_ms(ms);
            assert_eq!(unix_ms_from_rfc3339(&text), Some(ms), "{text}");
        }
    }

    #[test]
    fn parsing_accepts_the_variants_a_stored_timestamp_might_have() {
        // Sub-second precision is optional and may be shorter than three digits.
        assert_eq!(
            unix_ms_from_rfc3339("2026-07-30T16:36:58Z"),
            Some(1_785_429_418_000)
        );
        assert_eq!(
            unix_ms_from_rfc3339("2026-07-30T16:36:58.4Z"),
            Some(1_785_429_418_400),
            "a single fractional digit is tenths, not thousandths"
        );
        assert_eq!(
            unix_ms_from_rfc3339("2026-07-30T16:36:58.412345Z"),
            Some(1_785_429_418_412),
            "extra precision is truncated, never misread"
        );
    }

    #[test]
    fn parsing_refuses_what_it_cannot_honestly_read() {
        for bad in [
            "",
            "not a time",
            "2026-07-30",
            "2026-07-30T16:36:58+02:00", // an offset we would have to apply
            "2026-13-30T16:36:58.000Z",  // month 13
            "2026-07-30T25:36:58.000Z",  // hour 25
            "2026-07-30T16:36:58.Z",     // a dot with no digits
            "2026/07/30T16:36:58.000Z",
        ] {
            assert_eq!(unix_ms_from_rfc3339(bad), None, "{bad:?} must not parse");
        }
    }
}
