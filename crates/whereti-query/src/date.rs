//! `dm:` values: today, lastweek, 2024, 2024-03, 2024-03-15, ranges a..b, >x, <x.
//!
//! Times are minutes since 1601-01-01 UTC (the index's `mtime` unit).
//! Day boundaries follow the user's local time zone: with daylight saving
//! rules set (`set_tz_rules`), a summer day searched in winter starts at
//! its own local midnight, not an hour off.

use std::sync::RwLock;

/// Current time and the local time-zone offset, supplied by the caller.
#[derive(Debug, Clone, Copy)]
pub struct DateCtx {
    /// Now, minutes since 1601-01-01 UTC.
    pub now: u32,
    /// Local time minus UTC, in minutes (Romania: +120 in winter, +180 in summer).
    pub tz_offset: i32,
}

impl DateCtx {
    /// Local time minus UTC at `utc` (minutes since 1601): today's offset,
    /// moved by the daylight saving difference when rules are set.
    pub fn offset_at(&self, utc: i64) -> i32 {
        offset_with(self, tz_rules().as_ref(), utc)
    }
}

/// The local time zone's daylight saving rule, as Windows describes it
/// (`GetTimeZoneInformation`, transitions with `wYear` = 0).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TzRules {
    /// Local time minus UTC in standard time: `-(Bias + StandardBias)`.
    pub standard: i32,
    /// Local time minus UTC in daylight time: `-(Bias + DaylightBias)`.
    pub daylight: i32,
    /// When daylight time starts (`DaylightDate`); month 0 = no daylight time.
    pub to_daylight: Transition,
    /// When it ends (`StandardDate`).
    pub to_standard: Transition,
}

/// "The `week`-th `weekday` of `month`, at `minute` local time" (week 5 =
/// the last one), as in a Windows time zone transition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transition {
    pub month: u8,
    pub week: u8,
    /// 0 = Sunday, as Windows counts.
    pub weekday: u8,
    /// Minutes after local midnight.
    pub minute: u16,
}

static RULES: RwLock<Option<TzRules>> = RwLock::new(None);

/// Sets the daylight saving rules every later parse uses. Callers that
/// never set them keep today's offset for every date.
pub fn set_tz_rules(rules: Option<TzRules>) {
    if let Ok(mut r) = RULES.write() {
        *r = rules;
    }
}

fn tz_rules() -> Option<TzRules> {
    RULES.read().ok().and_then(|r| *r)
}

impl TzRules {
    /// Local time minus UTC at `utc` (minutes since 1601).
    pub fn offset_at(&self, utc: i64) -> i32 {
        if self.to_daylight.month == 0 || self.to_standard.month == 0 {
            return self.standard;
        }
        let (y, _, _) = civil_from_day((utc + self.standard as i64).div_euclid(MINUTES_PER_DAY));
        // each change is written in the local time in force just before it
        let start = self.to_daylight.local_minute(y) - self.standard as i64;
        let end = self.to_standard.local_minute(y) - self.daylight as i64;
        // south of the equator summer spans the new year
        let summer = if start < end { (start..end).contains(&utc) } else { utc >= start || utc < end };
        if summer {
            self.daylight
        } else {
            self.standard
        }
    }
}

impl Transition {
    /// The local minute of this change in year `y`, since 1601.
    fn local_minute(&self, y: i64) -> i64 {
        let m = (self.month as i64).clamp(1, 12);
        let first = day_from_civil(y, m, 1);
        // 1601-01-01 was a Monday, so a Sunday-based weekday is day + 1
        let first_weekday = (first + 1).rem_euclid(7);
        let mut d = (self.weekday as i64 - first_weekday).rem_euclid(7) + 7 * (self.week.clamp(1, 5) as i64 - 1);
        while d >= days_in_month(y, m) {
            d -= 7;
        }
        (first + d) * MINUTES_PER_DAY + self.minute as i64
    }
}

/// The offset at `utc`: `ctx.tz_offset` is right for now, the rules say
/// how much more or less it is at that other moment.
fn offset_with(ctx: &DateCtx, rules: Option<&TzRules>, utc: i64) -> i32 {
    match rules {
        Some(r) => ctx.tz_offset + r.offset_at(utc) - r.offset_at(ctx.now as i64),
        None => ctx.tz_offset,
    }
}

const MINUTES_PER_DAY: i64 = 1440;
/// Days from 1601-01-01 to 1970-01-01.
const EPOCH_1601_TO_1970_DAYS: i64 = 134_774;
/// Years a date may name: file times start in 1601; later than 2200 is a typo.
const YEARS: std::ops::RangeInclusive<i64> = 1601..=2200;

/// Parses a `dm:` value into an inclusive range of minutes.
pub fn parse_range(v: &str, ctx: &DateCtx) -> Option<(u32, u32)> {
    parse_range_in(v, ctx, tz_rules().as_ref())
}

fn parse_range_in(v: &str, ctx: &DateCtx, rules: Option<&TzRules>) -> Option<(u32, u32)> {
    let parse_span = |v: &str, ctx: &DateCtx| parse_span(v, ctx, rules);
    let v = v.trim().to_ascii_lowercase();
    if let Some((a, b)) = v.split_once("..") {
        // `2024..` and `..2024` are open ends
        let (a, b) = (a.trim(), b.trim());
        if a.is_empty() && b.is_empty() {
            return None;
        }
        let start = if a.is_empty() { 0 } else { parse_span(a, ctx)?.0 };
        let end = if b.is_empty() { u32::MAX } else { parse_span(b, ctx)?.1 };
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
fn parse_span(v: &str, ctx: &DateCtx, rules: Option<&TzRules>) -> Option<(u32, u32)> {
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
        _ => numeric_span(v, y)?,
    };
    // local midnight -> UTC with the offset of that night (asked twice, so
    // a night next to a daylight saving change settles on the right one)
    let to_utc = |day: i64| {
        let local = day * MINUTES_PER_DAY;
        let mut utc = local - ctx.tz_offset as i64;
        for _ in 0..2 {
            utc = local - offset_with(ctx, rules, utc) as i64;
        }
        utc
    };
    let start = to_utc(first_day).clamp(0, u32::MAX as i64) as u32;
    let end = (to_utc(last_day + 1) - 1).clamp(0, u32::MAX as i64) as u32;
    Some((start, end))
}

/// `2024`, `2024-03`, `2024-03-15`, `15.03.2024`, `15.03.24` -> (first day,
/// last day). Impossible dates (`2024-02-31`) and years outside 1601-2200
/// are not dates (the word is then searched as text).
fn numeric_span(v: &str, this_year: i64) -> Option<(i64, i64)> {
    let parts: Vec<&str> = v.split(['-', '.', '/']).collect();
    let num = |s: &str| {
        if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse::<i64>().ok()
    };
    // `24` is 2024; a year more than ten years ahead is the last century's
    let year = |s: &str| {
        let y = match s.len() {
            4 => num(s)?,
            2 => {
                let y = 2000 + num(s)?;
                if y > this_year + 10 { y - 100 } else { y }
            }
            _ => return None,
        };
        YEARS.contains(&y).then_some(y)
    };
    match parts.as_slice() {
        [y] if y.len() == 4 => {
            let y = year(y)?;
            Some((day_from_civil(y, 1, 1), day_from_civil(y + 1, 1, 1) - 1))
        }
        [y, m] if y.len() == 4 => {
            let (y, m) = (year(y)?, num(m)?);
            if !(1..=12).contains(&m) {
                return None;
            }
            Some((day_from_civil(y, m, 1), day_from_civil(y, m, 1) + days_in_month(y, m) - 1))
        }
        [a, b, c] => {
            // 2024-03-15 or 15.03.2024
            let (y, m, d) = if a.len() == 4 { (year(a)?, num(b)?, num(c)?) } else { (year(c)?, num(b)?, num(a)?) };
            if !(1..=12).contains(&m) || !(1..=days_in_month(y, m)).contains(&d) {
                return None;
            }
            let day = day_from_civil(y, m, d);
            Some((day, day))
        }
        _ => None,
    }
}

fn days_in_month(y: i64, m: i64) -> i64 {
    let next = if m == 12 { day_from_civil(y + 1, 1, 1) } else { day_from_civil(y, m + 1, 1) };
    next - day_from_civil(y, m, 1)
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

    #[test]
    fn open_ended_ranges() {
        let c = ctx();
        assert_eq!(parse_range("2024..", &c), Some((at(2024, 1, 1, 0, 0, 180), u32::MAX)));
        assert_eq!(parse_range("..2024", &c), Some((0, at(2024, 12, 31, 23, 59, 180))));
        assert_eq!(parse_range("2024-03..", &c), Some((at(2024, 3, 1, 0, 0, 180), u32::MAX)));
        assert!(parse_range("..", &c).is_none());
    }

    #[test]
    fn impossible_dates_and_years_are_not_dates() {
        let c = ctx();
        assert!(parse_range("2024-02-31", &c).is_none());
        assert!(parse_range("2023-02-29", &c).is_none());
        assert!(parse_range("31.04.2024", &c).is_none());
        assert!(parse_range("2024-02-29", &c).is_some());
        assert!(parse_range("9999", &c).is_none());
        assert!(parse_range(">9999", &c).is_none());
        assert!(parse_range("1500", &c).is_none());
        assert!(parse_range("0024-01-01", &c).is_none());
        // two-digit years: this century, unless more than ten years ahead
        let (s, _) = parse_range("15.03.24", &c).unwrap();
        assert_eq!(s, at(2024, 3, 15, 0, 0, 180));
        let (s, _) = parse_range("15.03.99", &c).unwrap();
        assert_eq!(s, at(1999, 3, 15, 0, 0, 180));
        let (s, _) = parse_range("15.03.36", &c).unwrap();
        assert_eq!(s, at(2036, 3, 15, 0, 0, 180));
        let (s, _) = parse_range("15.03.37", &c).unwrap();
        assert_eq!(s, at(1937, 3, 15, 0, 0, 180));
    }

    /// Romania as Windows describes it: UTC+2, summer UTC+3 from the last
    /// Sunday of March 03:00 to the last Sunday of October 04:00.
    fn romania() -> TzRules {
        TzRules {
            standard: 120,
            daylight: 180,
            to_daylight: Transition { month: 3, week: 5, weekday: 0, minute: 180 },
            to_standard: Transition { month: 10, week: 5, weekday: 0, minute: 240 },
        }
    }

    #[test]
    fn transitions_fall_on_the_right_sunday() {
        let r = romania();
        assert_eq!(r.to_daylight.local_minute(2026), day_from_civil(2026, 3, 29) * 1440 + 180);
        assert_eq!(r.to_standard.local_minute(2026), day_from_civil(2026, 10, 25) * 1440 + 240);
        assert_eq!(r.to_daylight.local_minute(2024), day_from_civil(2024, 3, 31) * 1440 + 180);
        let second_sunday = Transition { month: 3, week: 2, weekday: 0, minute: 120 };
        assert_eq!(second_sunday.local_minute(2026), day_from_civil(2026, 3, 8) * 1440 + 120);
        assert_eq!(r.offset_at(at(2026, 7, 1, 12, 0, 180) as i64), 180);
        assert_eq!(r.offset_at(at(2026, 12, 1, 12, 0, 120) as i64), 120);
        // the hour before and after the change
        assert_eq!(r.offset_at(at(2026, 3, 29, 2, 59, 120) as i64), 120);
        assert_eq!(r.offset_at(at(2026, 3, 29, 4, 0, 180) as i64), 180);
    }

    #[test]
    fn a_summer_day_searched_in_winter_starts_at_its_own_midnight() {
        let winter = DateCtx { now: at(2026, 12, 10, 12, 0, 120), tz_offset: 120 };
        let r = romania();
        let (s, e) = parse_range_in("2024-07-15", &winter, Some(&r)).unwrap();
        assert_eq!((s, e), (at(2024, 7, 15, 0, 0, 180), at(2024, 7, 15, 23, 59, 180)));
        // without rules: today's offset for every date, as before
        let (s, _) = parse_range_in("2024-07-15", &winter, None).unwrap();
        assert_eq!(s, at(2024, 7, 15, 0, 0, 120));
        // the day summer time starts: midnight in winter time, the next in summer time
        let (s, e) = parse_range_in("2026-03-29", &winter, Some(&r)).unwrap();
        assert_eq!((s, e), (at(2026, 3, 29, 0, 0, 120), at(2026, 3, 29, 23, 59, 180)));
        // in summer, a winter month
        let (s, e) = parse_range_in("2026-01", &ctx(), Some(&r)).unwrap();
        assert_eq!((s, e), (at(2026, 1, 1, 0, 0, 120), at(2026, 1, 31, 23, 59, 120)));
        assert_eq!(winter.offset_at(at(2024, 7, 15, 0, 0, 180) as i64), 120); // no rules set
    }
}
