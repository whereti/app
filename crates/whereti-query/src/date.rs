//! `dm:` values: today, lastweek, 2024, 2024-03, 2024-03-15, ranges a..b, >x, <x.
//!
//! Times are minutes since 1601-01-01 UTC (the index's `mtime` unit).
//! Day boundaries follow the user's local time zone.

/// Current time and the local time-zone offset, supplied by the caller.
#[derive(Debug, Clone, Copy)]
pub struct DateCtx {
    /// Now, minutes since 1601-01-01 UTC.
    pub now: u32,
    /// Local time minus UTC, in minutes (Romania: +120 in winter, +180 in summer).
    pub tz_offset: i32,
}

const MINUTES_PER_DAY: i64 = 1440;
/// Days from 1601-01-01 to 1970-01-01.
const EPOCH_1601_TO_1970_DAYS: i64 = 134_774;

/// Parses a `dm:` value into an inclusive range of minutes.
pub fn parse_range(v: &str, ctx: &DateCtx) -> Option<(u32, u32)> {
    let v = v.trim().to_ascii_lowercase();
    if let Some((a, b)) = v.split_once("..") {
        let (start, _) = parse_span(a, ctx)?;
        let (_, end) = parse_span(b, ctx)?;
        return (start <= end).then_some((start, end));
    }
    for (op, f) in [
        (">=", 0u8),
        ("<=", 1),
        (">", 2),
        ("<", 3),
    ] {
        if let Some(rest) = v.strip_prefix(op) {
            let (s, e) = parse_span(rest, ctx)?;
            return Some(match f {
                0 => (s, u32::MAX),
                1 => (0, e),
                2 => (e.saturating_add(1), u32::MAX),
                _ => (0, s.saturating_sub(1)),
            });
        }
    }
    parse_span(&v, ctx)
}

/// One named or numeric period -> (first minute, last minute).
fn parse_span(v: &str, ctx: &DateCtx) -> Option<(u32, u32)> {
    let local_now = ctx.now as i64 + ctx.tz_offset as i64;
    let today = local_now.div_euclid(MINUTES_PER_DAY); // local day number since 1601
    let (y, m, _) = civil_from_day(today);
    let weekday = today.rem_euclid(7); // 1601-01-01 was a Monday -> 0 = Monday
    let (first_day, last_day) = match v {
        "today" => (today, today),
        "yesterday" => (today - 1, today - 1),
        "thisweek" => (today - weekday, today),
        "lastweek" => (today - weekday - 7, today - weekday - 1),
        "thismonth" => (day_from_civil(y, m, 1), today),
        "lastmonth" => {
            let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
            (day_from_civil(py, pm, 1), day_from_civil(y, m, 1) - 1)
        }
        "thisyear" => (day_from_civil(y, 1, 1), today),
        "lastyear" => (day_from_civil(y - 1, 1, 1), day_from_civil(y, 1, 1) - 1),
        _ => numeric_span(v)?,
    };
    let to_utc = |day: i64| day * MINUTES_PER_DAY - ctx.tz_offset as i64;
    let start = to_utc(first_day).clamp(0, u32::MAX as i64) as u32;
    let end = (to_utc(last_day + 1) - 1).clamp(0, u32::MAX as i64) as u32;
    Some((start, end))
}

fn numeric_span(v: &str) -> Option<(i64, i64)> {
    let parts: Vec<&str> = v.split(['-', '.', '/']).collect();
    let num = |s: &str| s.parse::<i64>().ok();
    match parts.as_slice() {
        [y] if y.len() == 4 => {
            let y = num(y)?;
            Some((day_from_civil(y, 1, 1), day_from_civil(y + 1, 1, 1) - 1))
        }
        [y, m] if y.len() == 4 => {
            let (y, m) = (num(y)?, num(m)?);
            if !(1..=12).contains(&m) {
                return None;
            }
            let next = if m == 12 { day_from_civil(y + 1, 1, 1) } else { day_from_civil(y, m + 1, 1) };
            Some((day_from_civil(y, m, 1), next - 1))
        }
        [a, b, c] => {
            // 2024-03-15 or 15.03.2024
            let (y, m, d) = if a.len() == 4 { (num(a)?, num(b)?, num(c)?) } else { (num(c)?, num(b)?, num(a)?) };
            if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
                return None;
            }
            let day = day_from_civil(y, m, d);
            Some((day, day))
        }
        _ => None,
    }
}

/// Day number since 1601-01-01 for a civil date (proleptic Gregorian).
pub fn day_from_civil(y: i64, m: i64, d: i64) -> i64 {
    // Howard Hinnant's days_from_civil, shifted to 1601.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468 + EPOCH_1601_TO_1970_DAYS
}

/// Inverse of [`day_from_civil`].
pub fn civil_from_day(day: i64) -> (i64, i64, i64) {
    let z = day - EPOCH_1601_TO_1970_DAYS + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-24 15:00 local, Romania summer time (UTC+3).
    fn ctx() -> DateCtx {
        let day = day_from_civil(2026, 10, 24);
        DateCtx { now: (day * 1440 + 15 * 60 - 180) as u32, tz_offset: 180 }
    }

    fn at(y: i64, m: i64, d: i64, hh: i64, mm: i64, tz: i64) -> u32 {
        (day_from_civil(y, m, d) * 1440 + hh * 60 + mm - tz) as u32
    }

    #[test]
    fn civil_round_trip_and_epoch() {
        assert_eq!(day_from_civil(1601, 1, 1), 0);
        assert_eq!(day_from_civil(1970, 1, 1), EPOCH_1601_TO_1970_DAYS);
        for day in [0, 1, 59, 60, 365, 140_000, 155_000] {
            let (y, m, d) = civil_from_day(day);
            assert_eq!(day_from_civil(y, m, d), day);
        }
    }

    #[test]
    fn today_uses_local_midnight() {
        let (s, e) = parse_range("today", &ctx()).unwrap();
        assert_eq!(s, at(2026, 10, 24, 0, 0, 180));
        assert_eq!(e, at(2026, 10, 24, 23, 59, 180));
    }

    #[test]
    fn calendar_periods() {
        let c = ctx(); // Saturday
        let (s, _) = parse_range("thisweek", &c).unwrap();
        assert_eq!(s, at(2026, 10, 19, 0, 0, 180)); // Monday
        let (s, e) = parse_range("lastweek", &c).unwrap();
        assert_eq!((s, e), (at(2026, 10, 12, 0, 0, 180), at(2026, 10, 18, 23, 59, 180)));
        let (s, e) = parse_range("lastmonth", &c).unwrap();
        assert_eq!((s, e), (at(2026, 9, 1, 0, 0, 180), at(2026, 9, 30, 23, 59, 180)));
    }

    #[test]
    fn numeric_and_ranges() {
        let c = ctx();
        let (s, e) = parse_range("2024-01..2024-06", &c).unwrap();
        assert_eq!((s, e), (at(2024, 1, 1, 0, 0, 180), at(2024, 6, 30, 23, 59, 180)));
        let (s, e) = parse_range("15.03.2024", &c).unwrap();
        assert_eq!((s, e), (at(2024, 3, 15, 0, 0, 180), at(2024, 3, 15, 23, 59, 180)));
        let (s, e) = parse_range(">2025", &c).unwrap();
        assert_eq!((s, e), (at(2026, 1, 1, 0, 0, 180), u32::MAX));
        assert!(parse_range("2024-13", &c).is_none());
        assert!(parse_range("soon", &c).is_none());
    }
}
