//! UTC instants at microsecond precision, parsed and printed exactly as the Python release did.
//!
//! The ledger stores `datetime.isoformat()` text with `+00:00` replaced by `Z`, and the Python
//! release accepted anything CPython 3.11+ `datetime.fromisoformat` accepts (once a trailing `Z`
//! became `+00:00`), or finite Unix seconds. Both directions are reproduced here byte for byte so an
//! existing ledger reads and writes the same way. The ISO parser is a port of CPython 3.13's C
//! implementation (`Modules/_datetimemodule.c`), including its tolerance quirks.

use std::time::{SystemTime, UNIX_EPOCH};

pub const SECOND: i64 = 1_000_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// 0001-01-01T00:00:00Z, the earliest value a Python `datetime` can hold.
const MIN_MICROS: i64 = -62_135_596_800 * SECOND;
/// 9999-12-31T23:59:59.999999Z, the latest value a Python `datetime` can hold.
const MAX_MICROS: i64 = 253_402_300_800 * SECOND - 1;
/// Days from 0001-01-01 (ordinal 1) to 1970-01-01.
const UNIX_ORDINAL: i64 = 719_163;

pub const INVALID: &str = "invalid timestamp";
pub const NAIVE: &str = "timestamp must include a UTC offset";

/// A UTC instant in microseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ts(i64);

impl Ts {
    pub const fn from_micros(micros: i64) -> Ts {
        Ts(micros)
    }

    pub const fn micros(self) -> i64 {
        self.0
    }

    /// The current time, truncated to microseconds like `datetime.now(timezone.utc)`.
    pub fn now() -> Ts {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => Ts(elapsed.as_micros() as i64),
            Err(before) => {
                let back = before.duration();
                let micros = back.as_micros() as i64;
                // Floor toward the past for pre-epoch clocks.
                Ts(-micros - i64::from(back.subsec_nanos() % 1000 != 0))
            }
        }
    }

    /// Build from civil UTC fields; used by tests and fixtures.
    pub const fn utc(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> Ts {
        Ts(days_from_civil(year, month, day) * DAY + hour * HOUR + minute * MINUTE + second * SECOND)
    }

    /// `datetime.timestamp()`: seconds since the epoch as a float.
    pub fn timestamp(self) -> f64 {
        self.0 as f64 / 1e6
    }

    /// `datetime.isoformat().replace("+00:00", "Z")`.
    pub fn iso(self) -> String {
        let days = self.0.div_euclid(DAY);
        let rest = self.0.rem_euclid(DAY);
        let (year, month, day) = civil_from_days(days);
        let hour = rest / HOUR;
        let minute = rest % HOUR / MINUTE;
        let second = rest % MINUTE / SECOND;
        let micro = rest % SECOND;
        let mut text = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
        if micro != 0 {
            text.push_str(&format!(".{micro:06}"));
        }
        text.push('Z');
        text
    }

    /// Seconds between two instants as Python's `timedelta.total_seconds()` returns them.
    pub fn seconds_since(self, earlier: Ts) -> f64 {
        (self.0 - earlier.0) as f64 / 1e6
    }

    /// Hours between two instants (`total_seconds() / 3600.0`).
    pub fn hours_since(self, earlier: Ts) -> f64 {
        self.seconds_since(earlier) / 3600.0
    }
}

impl std::ops::Add<i64> for Ts {
    type Output = Ts;
    fn add(self, micros: i64) -> Ts {
        Ts(self.0 + micros)
    }
}

impl std::ops::Sub<i64> for Ts {
    type Output = Ts;
    fn sub(self, micros: i64) -> Ts {
        Ts(self.0 - micros)
    }
}

impl std::ops::Sub<Ts> for Ts {
    type Output = i64;
    fn sub(self, other: Ts) -> i64 {
        self.0 - other.0
    }
}

/// Finite Unix seconds, rounded to microseconds half-to-even like `datetime.fromtimestamp`.
pub fn from_unix_seconds(number: f64) -> Result<Ts, &'static str> {
    if !number.is_finite() || number.abs() > 253_402_300_799.0 {
        return Err(INVALID);
    }
    let mut whole = number.trunc();
    let mut fraction = ((number - whole) * 1e6).round_ties_even();
    if fraction >= 1e6 {
        fraction -= 1e6;
        whole += 1.0;
    } else if fraction < 0.0 {
        fraction += 1e6;
        whole -= 1.0;
    }
    let micros = (whole as i64) * SECOND + fraction as i64;
    if (MIN_MICROS..=MAX_MICROS).contains(&micros) { Ok(Ts(micros)) } else { Err(INVALID) }
}

/// The text branch of the Python `parse_timestamp`.
pub fn parse_text(value: &str) -> Result<Ts, &'static str> {
    if value.chars().count() > 128 {
        return Err(INVALID);
    }
    let trimmed = value.trim_matches(is_python_space);
    let mut text = trimmed.to_string();
    if text.ends_with('Z') {
        text.pop();
        text.push_str("+00:00");
    }
    let (local, offset) = fromisoformat(&text).ok_or(INVALID)?;
    let offset = offset.ok_or(NAIVE)?;
    let utc = local - offset;
    if (MIN_MICROS..=MAX_MICROS).contains(&utc) { Ok(Ts(utc)) } else { Err(INVALID) }
}

fn is_python_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { y + 1 } else { y }, month, day)
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// Byte at `index`, or NUL past the end, mirroring C string access.
fn at(s: &[u8], index: usize) -> u8 {
    s.get(index).copied().unwrap_or(0)
}

fn is_digit(c: u8) -> bool {
    c.is_ascii_digit()
}

fn parse_digits(s: &[u8], start: usize, count: usize) -> Option<(usize, i64)> {
    let mut value = 0i64;
    for index in start..start + count {
        let digit = at(s, index).wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        value = value * 10 + i64::from(digit);
    }
    Some((start + count, value))
}

/// `_find_isoformat_datetime_separator`; `None` stands for the C code's -1.
fn find_separator(s: &[u8]) -> Option<usize> {
    let len = s.len();
    if len == 7 {
        return Some(7);
    }
    if s[4] == b'-' {
        if s[5] == b'W' {
            if len < 8 {
                return None;
            }
            if len > 8 && s[8] == b'-' {
                if len == 9 {
                    return None;
                }
                if len > 10 && is_digit(s[10]) {
                    return Some(8);
                }
                return Some(10);
            }
            return Some(8);
        }
        return Some(10);
    }
    if s[4] == b'W' {
        let mut index = 7;
        while index < len && is_digit(s[index]) {
            index += 1;
        }
        if index < 9 {
            return Some(index);
        }
        return Some(if index % 2 == 0 { 7 } else { 8 });
    }
    Some(8)
}

fn weekday_of_jan1(year: i64) -> i64 {
    (days_from_civil(year, 1, 1) + UNIX_ORDINAL + 6) % 7
}

fn iso_to_ymd(year: i64, week: i64, day: i64) -> Option<(i64, i64, i64)> {
    if !(1..=9999).contains(&year) {
        return None;
    }
    if week <= 0 || week >= 53 {
        let first = weekday_of_jan1(year);
        if !(week == 53 && (first == 3 || (first == 2 && is_leap(year)))) {
            return None;
        }
    }
    if day <= 0 || day >= 8 {
        return None;
    }
    let first_day = days_from_civil(year, 1, 1);
    let first_weekday = weekday_of_jan1(year);
    let mut monday = first_day - first_weekday;
    if first_weekday > 3 {
        monday += 7;
    }
    Some(civil_from_days(monday + (week - 1) * 7 + day - 1))
}

/// `parse_isoformat_date`; `len` is the separator position.
fn parse_date(s: &[u8], len: usize) -> Option<(i64, i64, i64)> {
    let (mut p, year) = parse_digits(s, 0, 4)?;
    let uses_separator = at(s, p) == b'-';
    if uses_separator {
        p += 1;
    }
    if at(s, p) == b'W' {
        p += 1;
        let (next, week) = parse_digits(s, p, 2)?;
        p = next;
        let mut day = 1;
        if p < len {
            if uses_separator {
                let c = at(s, p);
                p += 1;
                if c != b'-' {
                    return None;
                }
            }
            day = parse_digits(s, p, 1)?.1;
        }
        return iso_to_ymd(year, week, day);
    }
    let (next, month) = parse_digits(s, p, 2)?;
    p = next;
    if uses_separator {
        let c = at(s, p);
        p += 1;
        if c != b'-' {
            return None;
        }
    }
    let (_, day) = parse_digits(s, p, 2)?;
    Some((year, month, day))
}

/// `parse_hh_mm_ss_ff`: `Some((trailing, [h, m, s, us]))`, where `trailing` is the C return value 0/1.
fn parse_hh_mm_ss_ff(s: &[u8], start: usize, end: usize) -> Option<(bool, [i64; 4])> {
    let mut values = [0i64; 4];
    let mut p = start;
    let mut has_separator = true;
    for (index, slot) in values.iter_mut().take(3).enumerate() {
        let (next, value) = parse_digits(s, p, 2)?;
        *slot = value;
        p = next;
        let c = at(s, p);
        p += 1;
        if index == 0 {
            has_separator = c == b':';
        }
        if c == b'.' || c == b',' {
            if p >= end {
                return None;
            }
            break;
        } else if p >= end {
            return Some((c != 0, values));
        } else if has_separator && c == b':' {
            continue;
        } else if !has_separator {
            p -= 1;
        } else {
            return None;
        }
    }
    let remains = end - p;
    let to_parse = remains.min(6);
    let (next, mut micro) = parse_digits(s, p, to_parse)?;
    p = next;
    const CORRECTION: [i64; 5] = [100_000, 10_000, 1_000, 100, 10];
    if to_parse < 6 {
        micro *= CORRECTION[to_parse - 1];
    }
    values[3] = micro;
    while is_digit(at(s, p)) {
        p += 1;
    }
    Some((at(s, p) != 0, values))
}

/// `parse_isoformat_time`: time fields plus `Some(offset_micros)` when an offset was given.
fn parse_time(s: &[u8], start: usize, end: usize) -> Option<([i64; 4], Option<i64>)> {
    let mut tz = start;
    loop {
        let c = at(s, tz);
        if c == b'Z' || c == b'+' || c == b'-' {
            break;
        }
        tz += 1;
        if tz >= end {
            break;
        }
    }
    let (trailing, values) = parse_hh_mm_ss_ff(s, start, tz)?;
    if tz == end {
        return if trailing { None } else { Some((values, None)) };
    }
    if at(s, tz) == b'Z' {
        return if at(s, tz + 1) != 0 { None } else { Some((values, Some(0))) };
    }
    let sign = if at(s, tz) == b'-' { -1 } else { 1 };
    let (tz_trailing, tz_values) = parse_hh_mm_ss_ff(s, tz + 1, end)?;
    if tz_trailing {
        return None;
    }
    let seconds = sign * (tz_values[0] * 3600 + tz_values[1] * 60 + tz_values[2]);
    let micros = sign * tz_values[3];
    if seconds == 0 && micros == 0 {
        return Some((values, Some(0)));
    }
    let offset = seconds * SECOND + micros;
    if offset <= -DAY || offset >= DAY {
        return None;
    }
    Some((values, Some(offset)))
}

/// `datetime.fromisoformat`: local microseconds plus an optional UTC offset in microseconds.
fn fromisoformat(text: &str) -> Option<(i64, Option<i64>)> {
    if text.chars().count() < 7 {
        return None;
    }
    let s = text.as_bytes();
    let len = s.len();
    let separator = find_separator(s)?;
    let (year, month, day) = parse_date(s, separator)?;
    let mut time = [0i64; 4];
    let mut offset = None;
    if len > separator {
        let lead = s[separator];
        let width = if lead & 0x80 == 0 {
            1
        } else {
            match lead & 0xf0 {
                0xe0 => 3,
                0xf0 => 4,
                _ => 2,
            }
        };
        let start = separator + width;
        (time, offset) = parse_time(s, start, len.max(start))?;
    }
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let [hour, minute, second, micro] = time;
    if !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
        || !(0..SECOND).contains(&micro)
    {
        return None;
    }
    let local = days_from_civil(year, month, day) * DAY + hour * HOUR + minute * MINUTE + second * SECOND + micro;
    Some((local, offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case below was run through CPython 3.13's `parse_timestamp` from the Python release.
    #[test]
    fn text_matches_cpython() {
        let accepted = [
            ("2026-10-23T12:00:00Z", "2026-10-23T12:00:00Z"),
            ("2026-10-23T12:00:00+00:00", "2026-10-23T12:00:00Z"),
            ("2026-10-23T12:00Z", "2026-10-23T12:00:00Z"),
            ("2026-10-23T12Z", "2026-10-23T12:00:00Z"),
            ("2026-10-23 12:00:00Z", "2026-10-23T12:00:00Z"),
            ("2026-10-23x12:00:00Z", "2026-10-23T12:00:00Z"),
            ("20261023T120000Z", "2026-10-23T12:00:00Z"),
            ("2026-10-23T120000Z", "2026-10-23T12:00:00Z"),
            ("20261023T12:00:00Z", "2026-10-23T12:00:00Z"),
            ("2026-10-23T12:00:00.1Z", "2026-10-23T12:00:00.100000Z"),
            ("2026-10-23T12:00:00.1234567Z", "2026-10-23T12:00:00.123456Z"),
            ("2026-10-23T12:00:00,5Z", "2026-10-23T12:00:00.500000Z"),
            ("2026-10-23T12:00:00.123456789Z", "2026-10-23T12:00:00.123456Z"),
            ("2026-W43-5T12:00:00Z", "2026-10-23T12:00:00Z"),
            ("2026W435T12:00Z", "2026-10-23T12:00:00Z"),
            ("2026-10-23T12:00:00+0530", "2026-10-23T06:30:00Z"),
            ("2026-10-23T12:00:00+05", "2026-10-23T07:00:00Z"),
            ("2026-10-23T12:00:00+05:30:15", "2026-10-23T06:29:45Z"),
            ("2026-10-23T12:00:00+05:30:15.5", "2026-10-23T06:29:44.500000Z"),
            ("2026-10-23T12:00:00-00:00", "2026-10-23T12:00:00Z"),
            (" 2026-10-23T12:00:00Z ", "2026-10-23T12:00:00Z"),
            ("2026-10-23T12:00:00 Z", "2026-10-23T12:00:00Z"),
            ("2026-10-23T12:00:00.000Z", "2026-10-23T12:00:00Z"),
            ("0001-01-01T00:00:00Z", "0001-01-01T00:00:00Z"),
            ("2026-10-23T12:00:00.12Z", "2026-10-23T12:00:00.120000Z"),
            ("2026-10-23T1200Z", "2026-10-23T12:00:00Z"),
            ("2026-10-23T12:00:00+23:59", "2026-10-22T12:01:00Z"),
            ("2026-10-23T12:00:00.1234Z", "2026-10-23T12:00:00.123400Z"),
            ("2026-10-23T12:00:00.123+05:00", "2026-10-23T07:00:00.123000Z"),
            ("2026-10-23T12:00:00,123456+05:00", "2026-10-23T07:00:00.123456Z"),
            ("2026-10-23T12:00:00.123456Z", "2026-10-23T12:00:00.123456Z"),
            ("2026-10-23T12:00:00.1234560Z", "2026-10-23T12:00:00.123456Z"),
            ("2026-10-23T12:00:00.1234567890123+00:00", "2026-10-23T12:00:00.123456Z"),
            ("2026-10-23T12:00:00+00", "2026-10-23T12:00:00Z"),
            ("2026-10-23T12+0000", "2026-10-23T12:00:00Z"),
            ("2026-10-23T12:00:00.5-03:00", "2026-10-23T15:00:00.500000Z"),
        ];
        for (input, expected) in accepted {
            assert_eq!(parse_text(input).map(Ts::iso), Ok(expected.to_string()), "{input:?}");
        }
        let rejected = [
            ("2026-10-23T24:00:00Z", INVALID),
            ("2026-10-23T23:59:60Z", INVALID),
            ("2026-10-23", NAIVE),
            ("2026-10-23T12:00:00", NAIVE),
            ("2026-10-23T12:00:00z", INVALID),
            ("2026-10-23T12:00:00.Z", INVALID),
            ("0001-01-01T00:00:00+01:00", INVALID),
            ("9999-12-31T23:59:59-01:00", INVALID),
            ("2026-10-23T12:0Z", INVALID),
            ("2026-1-23T12:00Z", INVALID),
            ("+2026-10-23T12:00Z", INVALID),
            ("2026-10-23T12:00:00+24:00", INVALID),
            ("2026-10-23T12:00:00UTC", INVALID),
            ("2026-02-30T12:00Z", INVALID),
            ("2026-10-23T12:00:00+05:3", INVALID),
            ("2026-10-23T12:00:00Z+00:00", INVALID),
            ("2026-10-23T12:00:00ZZ", INVALID),
            ("2026-10-23TT12:00Z", INVALID),
            ("", INVALID),
            ("nonsense", INVALID),
        ];
        for (input, error) in rejected {
            assert_eq!(parse_text(input), Err(error), "{input:?}");
        }
        assert_eq!(parse_text(&"1".repeat(129)), Err(INVALID));
    }

    #[test]
    fn unix_seconds_match_cpython() {
        let cases: [(f64, Option<&str>); 9] = [
            (0.0, Some("1970-01-01T00:00:00Z")),
            (-1.0, Some("1969-12-31T23:59:59Z")),
            (1.5, Some("1970-01-01T00:00:01.500000Z")),
            (1e10, Some("2286-11-20T17:46:40Z")),
            (253_402_300_799.0, Some("9999-12-31T23:59:59Z")),
            (-62_135_596_800.0, Some("0001-01-01T00:00:00Z")),
            (-62_135_596_801.0, None),
            (1_761_220_800.000_000_5, Some("2025-10-23T12:00:00Z")),
            (1_761_220_800.999_999_5, Some("2025-10-23T12:00:01Z")),
        ];
        for (number, expected) in cases {
            assert_eq!(from_unix_seconds(number).ok().map(Ts::iso).as_deref(), expected, "{number}");
        }
        assert_eq!(from_unix_seconds(f64::NAN), Err(INVALID));
        assert_eq!(from_unix_seconds(f64::INFINITY), Err(INVALID));
        assert_eq!(from_unix_seconds(1e300), Err(INVALID));
    }

    #[test]
    fn civil_round_trip() {
        for days in [-719_162, -1, 0, 1, 20_384, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(Ts::utc(2026, 10, 23, 12, 0, 0).iso(), "2026-10-23T12:00:00Z");
        assert_eq!((Ts::utc(1969, 12, 31, 23, 59, 59) + 1).iso(), "1969-12-31T23:59:59.000001Z");
    }
}
