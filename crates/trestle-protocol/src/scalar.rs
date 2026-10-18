//! Portable scalar formats (ADR-022): decimals, dates, instants and
//! durations travel as JSON strings with one fixed syntax each, so no
//! client rounds a number or guesses a time zone. The browser validator
//! mirrors these functions exactly; `fixtures/contracts/scalars.json`
//! proves they agree.

/// Why a decimal string is not acceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecimalError {
    /// Not `-?digits[.digits]` (no exponent, no `+`, no leading zeros).
    Syntax,
    /// More digits before or after the point than `precision`/`scale` allow.
    Digits,
}

/// Checks `-?(0|[1-9][0-9]*)(\.[0-9]+)?` against `precision` significant
/// digits, `scale` of them after the point.
pub fn check_decimal(text: &str, precision: u8, scale: u8) -> Result<(), DecimalError> {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (unsigned, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || (whole.len() > 1 && whole.starts_with('0')) {
        return Err(DecimalError::Syntax);
    }
    if let Some(fraction) = fraction
        && !digits(fraction)
    {
        return Err(DecimalError::Syntax);
    }
    let whole_digits = if whole == "0" { 0 } else { whole.len() };
    let fraction_digits = fraction.map_or(0, str::len);
    if whole_digits > usize::from(precision - scale) || fraction_digits > usize::from(scale) {
        return Err(DecimalError::Digits);
    }
    Ok(())
}

/// The JSON Schema `pattern` equivalent to [`check_decimal`]. The browser
/// recognizes exactly these patterns and no others.
pub fn decimal_pattern(precision: u8, scale: u8) -> String {
    let whole = match precision - scale {
        0 => "0".to_owned(),
        n => format!("(0|[1-9][0-9]{{0,{}}})", n - 1),
    };
    let fraction = match scale {
        0 => String::new(),
        s => format!("(\\.[0-9]{{1,{s}}})?"),
    };
    format!("^-?{whole}{fraction}$")
}

/// `P[nD][T[nH][nM][nS]]` with at least one part. Written without
/// look-ahead so every JSON Schema engine compiles it.
pub const DURATION_PATTERN: &str = "^P([0-9]+D(T([0-9]+H([0-9]+M)?([0-9]+S)?|[0-9]+M([0-9]+S)?|[0-9]+S))?|T([0-9]+H([0-9]+M)?([0-9]+S)?|[0-9]+M([0-9]+S)?|[0-9]+S))$";

fn number(text: &str) -> Option<u32> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap(year) => 29,
        _ => 28,
    }
}

/// `(year, month, day)` of an RFC 3339 `full-date`, years 0000–9999.
pub fn parse_date(text: &str) -> Option<(i64, u32, u32)> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = i64::from(number(&text[0..4])?);
    let month = number(&text[5..7])?;
    let day = number(&text[8..10])?;
    ((1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day))
        .then_some((year, month, day))
}

/// Days since 1970-01-01 (proleptic Gregorian).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Parses an RFC 3339 `date-time` (offset required; `T`/`Z` in either case)
/// and returns it in UTC as `YYYY-MM-DDTHH:MM:SS[.fff…]Z`, keeping the
/// given fractional digits (PostgreSQL stores microseconds and rounds
/// beyond). `None` when it is not a real time with an offset.
pub fn normalize_instant(text: &str) -> Option<String> {
    if text.len() < 20 || !text.is_ascii() {
        return None;
    }
    let (date, rest) = text.split_at(10);
    let (year, month, day) = parse_date(date)?;
    let rest = rest.strip_prefix(['T', 't'])?;
    let bytes = rest.as_bytes();
    if bytes.len() < 9 || bytes[2] != b':' || bytes[5] != b':' {
        return None;
    }
    let hour = number(&rest[0..2])?;
    let minute = number(&rest[3..5])?;
    let second = number(&rest[6..8])?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut tail = &rest[8..];
    let mut fraction = "";
    if let Some(after) = tail.strip_prefix('.') {
        let end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if end == 0 {
            return None;
        }
        fraction = &after[..end];
        tail = &after[end..];
    }
    let offset_minutes: i64 = if tail.eq_ignore_ascii_case("z") {
        0
    } else {
        let sign = match tail.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return None,
        };
        let offset = &tail[1..];
        if offset.len() != 5 || offset.as_bytes()[2] != b':' {
            return None;
        }
        let hours = number(&offset[0..2])?;
        let minutes = number(&offset[3..5])?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        sign * i64::from(hours * 60 + minutes)
    };
    let local =
        days_from_civil(year, month, day) * 86_400 + i64::from(hour * 3600 + minute * 60 + second);
    let utc = local - offset_minutes * 60;
    let (y, m, d) = civil_from_days(utc.div_euclid(86_400));
    if !(0..=9999).contains(&y) {
        return None;
    }
    let seconds = utc.rem_euclid(86_400);
    let fraction = if fraction.is_empty() {
        String::new()
    } else {
        format!(".{fraction}")
    };
    Some(format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{fraction}Z",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    ))
}

/// `(days, hours, minutes, seconds)` of a duration in the
/// [`DURATION_PATTERN`] subset.
pub fn parse_duration(text: &str) -> Option<(u64, u64, u64, u64)> {
    let rest = text.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (rest, None),
    };
    let mut parts = [None; 4];
    let mut take = |segment: &str, units: &[(u8, usize)]| -> Option<()> {
        let mut rest = segment;
        let mut last = None;
        while !rest.is_empty() {
            let end = rest.find(|c: char| !c.is_ascii_digit())?;
            if end == 0 {
                return None;
            }
            let unit = rest.as_bytes()[end];
            let index = units.iter().position(|(u, _)| *u == unit)?;
            if last.is_some_and(|l| index <= l) {
                return None;
            }
            last = Some(index);
            parts[units[index].1] = Some(rest[..end].parse::<u64>().ok()?);
            rest = &rest[end + 1..];
        }
        Some(())
    };
    take(date, &[(b'D', 0)])?;
    if let Some(time) = time {
        if time.is_empty() {
            return None;
        }
        take(time, &[(b'H', 1), (b'M', 2), (b'S', 3)])?;
    }
    if parts.iter().all(Option::is_none) {
        return None;
    }
    Some((
        parts[0].unwrap_or(0),
        parts[1].unwrap_or(0),
        parts[2].unwrap_or(0),
        parts[3].unwrap_or(0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_count_digits_on_each_side_of_the_point() {
        for ok in ["0", "-0", "12.5", "99999999.99", "0.01", "-12345678.00"] {
            assert_eq!(check_decimal(ok, 10, 2), Ok(()), "{ok}");
        }
        for (bad, why) in [
            ("", DecimalError::Syntax),
            ("01", DecimalError::Syntax),
            ("+1", DecimalError::Syntax),
            ("1e3", DecimalError::Syntax),
            ("1.", DecimalError::Syntax),
            (".5", DecimalError::Syntax),
            ("123456789.0", DecimalError::Digits),
            ("1.234", DecimalError::Digits),
        ] {
            assert_eq!(check_decimal(bad, 10, 2), Err(why), "{bad}");
        }
        assert_eq!(check_decimal("0.25", 2, 2), Ok(()));
        assert_eq!(check_decimal("1.25", 2, 2), Err(DecimalError::Digits));
        assert_eq!(check_decimal("1.0", 3, 0), Err(DecimalError::Digits));
        assert_eq!(
            decimal_pattern(10, 2),
            "^-?(0|[1-9][0-9]{0,7})(\\.[0-9]{1,2})?$"
        );
        assert_eq!(decimal_pattern(2, 2), "^-?0(\\.[0-9]{1,2})?$");
        assert_eq!(decimal_pattern(3, 0), "^-?(0|[1-9][0-9]{0,2})$");
    }

    #[test]
    fn dates_are_real_calendar_days() {
        assert_eq!(parse_date("2024-02-29"), Some((2024, 2, 29)));
        for bad in [
            "2023-02-29",
            "2026-13-01",
            "2026-00-10",
            "2026-1-01",
            "20260101",
            "2026-04-31",
        ] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
        assert_eq!(civil_from_days(days_from_civil(2000, 3, 1)), (2000, 3, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn instants_normalize_to_utc() {
        let cases = [
            ("2026-10-17T09:30:00Z", "2026-10-17T09:30:00Z"),
            ("2026-10-17t09:30:00z", "2026-10-17T09:30:00Z"),
            ("2026-10-17T09:30:00.123+02:00", "2026-10-17T07:30:00.123Z"),
            ("2026-12-31T23:30:00-01:00", "2027-01-01T00:30:00Z"),
            ("2024-03-01T00:15:00+01:00", "2024-02-29T23:15:00Z"),
        ];
        for (input, utc) in cases {
            assert_eq!(normalize_instant(input).as_deref(), Some(utc), "{input}");
        }
        for bad in [
            "2026-10-17T09:30:00",
            "2026-10-17 09:30:00Z",
            "2026-10-17T24:00:00Z",
            "2026-10-17T09:30:60Z",
            "2026-10-17T09:30:00+2:00",
            "2026-10-17T09:30:00.Z",
        ] {
            assert_eq!(normalize_instant(bad), None, "{bad}");
        }
        assert_eq!(
            normalize_instant("2026-10-17T09:30:00.1234567+01:00").as_deref(),
            Some("2026-10-17T08:30:00.1234567Z")
        );
    }

    #[test]
    fn durations_are_days_and_clock_time() {
        assert_eq!(parse_duration("PT30M"), Some((0, 0, 30, 0)));
        assert_eq!(parse_duration("P1DT2H3M4S"), Some((1, 2, 3, 4)));
        assert_eq!(parse_duration("P7D"), Some((7, 0, 0, 0)));
        for bad in [
            "P", "PT", "P1Y", "P1M", "PT1H1H", "PT1M1H", "P1W", "PT1.5S", "1D", "P-1D",
        ] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }
}
