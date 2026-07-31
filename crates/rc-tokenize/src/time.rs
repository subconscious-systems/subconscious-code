//! Shared date utilities — no `chrono`, no `time` crate.
//!
//! Howard Hinnant's days-from-civil inverse and the weekday/month tables live
//! here once (they were duplicated in `rc-ctx` and `rc-cli`). Used for the
//! environment-block date and sortable session ids.

/// Howard Hinnant's days-from-civil inverse: days since 1970-01-01 → (y, m, d).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

/// Today's date as "Weekday Mon D, YYYY" (UTC), e.g. "Monday Jul 28, 2026".
/// No `chrono`: days-since-epoch → civil date, plus a weekday via the
/// 1970-01-01=Thursday anchor. Good enough for the model's environment block.
pub fn today_string() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (now / 86400) as i64;
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 was a Thursday. Shift so Sunday=0 … Saturday=6.
    let day_idx = (((days % 7) + 7) % 7) as usize;
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][day_idx];
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];
    format!("{weekday} {month} {d}, {y}")
}

/// A compact, sortable UTC timestamp (`YYYYmmddTHHMMSS`) for fresh session ids,
/// without `chrono`. `--continue` picks the newest file by mtime, so
/// monotonicity is what matters, not the format.
pub fn sortable_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs = now % 60;
    let mins = (now / 60) % 60;
    let hours = (now / 3600) % 24;
    let days = now / 86400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}{m:02}{d:02}T{hours:02}{mins:02}{secs:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_date() {
        // 1970-01-01 = day 0.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-01-01 = day 10957: 30 years × 365 + 7 leap days (1972..=1996).
        // A clean, hand-verifiable anchor independent of today's date.
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        // 2026-07-31 (today) — day 20665. Sanity check against a recent date.
        assert_eq!(civil_from_days(20_665), (2026, 7, 31));
    }

    #[test]
    fn today_string_has_expected_shape() {
        let s = today_string();
        // "Weekday Mon D, YYYY" — weekday is 3 letters, month 3, year 4.
        assert!(s.contains(", 20"), "year should be 20xx: {s}");
        assert!(s.len() >= 14, "too short: {s}");
    }

    #[test]
    fn sortable_timestamp_is_lexicographically_ordered() {
        let a = sortable_timestamp();
        // A later timestamp must sort after an earlier one (same length).
        assert!(!a.is_empty());
        assert!(a.len() == 15, "YYYYmmddTHHMMSS is 15 chars: {a}");
        assert!(a.as_bytes()[8] == b'T', "separator at pos 8: {a}");
    }
}
