//! Wall-clock helpers (UTC) without extra dependencies.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn now_s() -> i64 {
    now_ms() / 1000
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// (year, month, day) to days since 1970-01-01.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Format seconds since epoch as `YYYYMMDDTHHMMSSZ`.
pub fn stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Parse a `YYYYMMDDTHHMMSSZ` stamp back to seconds since epoch.
pub fn parse_stamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let y = num(0..4)?;
    let mo = num(4..6)? as u32;
    let d = num(6..8)? as u32;
    let h = num(9..11)?;
    let mi = num(11..13)?;
    let se = num(13..15)?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se)
}

/// Index of the UTC day.
pub fn day_index(secs: i64) -> i64 {
    secs.div_euclid(86_400)
}

/// Index of the Monday-based week (1970-01-01 was a Thursday).
pub fn week_index(secs: i64) -> i64 {
    (day_index(secs) + 3).div_euclid(7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for secs in [
            0i64,
            951_782_400,
            1_700_000_000,
            1_794_528_000,
            4_102_444_799,
        ] {
            let s = stamp(secs);
            assert_eq!(parse_stamp(&s), Some(secs), "{s}");
        }
        assert_eq!(stamp(0), "19700101T000000Z");
        assert_eq!(parse_stamp("garbage"), None);
    }

    #[test]
    fn weeks() {
        // 2024-01-01 (Mon) and 2024-01-07 (Sun) share a week; 2024-01-08 does not.
        let mon = days_from_civil(2024, 1, 1) * 86_400;
        let sun = days_from_civil(2024, 1, 7) * 86_400 + 86_399;
        let next = days_from_civil(2024, 1, 8) * 86_400;
        assert_eq!(week_index(mon), week_index(sun));
        assert_eq!(week_index(next), week_index(mon) + 1);
    }
}
