//! UTC timestamps without a calendar dependency.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Wall-clock seconds since the Unix epoch (Python `time.time()`).
pub fn wall() -> f64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs_f64(),
        Err(error) => -error.duration().as_secs_f64(),
    }
}

/// Monotonic seconds since this process first asked (Python `time.monotonic()`).
pub fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    // Offset so that values are never zero or negative, like a real uptime clock.
    1000.0 + START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
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

/// `YYYY-MM-DDTHH:MM:SS` for whole seconds since the epoch.
fn civil_string(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// ISO 8601 UTC with second precision and a `Z` suffix, like the Python `utc_now()`.
pub fn iso_seconds(epoch: f64) -> Option<String> {
    if !epoch.is_finite() || epoch.abs() > 253_402_300_799.0 || epoch < -62_135_596_800.0 {
        return None;
    }
    Some(format!("{}Z", civil_string(epoch.floor() as i64)))
}

pub fn utc_now() -> String {
    iso_seconds(wall()).unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}

/// `datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")`: microseconds when nonzero.
pub fn utc_now_micro() -> String {
    let now = wall();
    let seconds = now.floor();
    let micros = ((now - seconds) * 1_000_000.0).floor() as i64;
    let base = civil_string(seconds as i64);
    if micros == 0 { format!("{base}Z") } else { format!("{base}.{micros:06}Z") }
}

fn digits(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Parse a Python `datetime.fromisoformat` style string. Returns the epoch seconds when the
/// value carries a UTC offset, `Some(None)` for a valid naive value, and `None` when invalid.
pub fn parse_iso(value: &str) -> Option<Option<f64>> {
    let text = value.trim_end();
    if text.len() < 10 || !text.is_ascii() {
        return None;
    }
    let (date, rest) = text.split_at(10);
    let year = digits(&date[0..4])?;
    let month = digits(&date[5..7])? as u32;
    let day = digits(&date[8..10])? as u32;
    if &date[4..5] != "-" || &date[7..8] != "-" || !(1..=12).contains(&month) || year < 1 {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if day == 0 || day > month_days[month as usize - 1] {
        return None;
    }
    let base = days_from_civil(year, month, day) as f64 * 86_400.0;
    if rest.is_empty() {
        return Some(None);
    }
    let time = &rest[1..];
    if !matches!(&rest[..1], "T" | " " | "t") {
        return None;
    }
    // Split the time from an optional offset.
    let offset_at = time.find(['Z', 'z', '+', '-']);
    let (clock, offset) = match offset_at {
        Some(index) => (&time[..index], Some(&time[index..])),
        None => (time, None),
    };
    let (hms, fraction) = match clock.split_once('.') {
        Some((hms, fraction)) => (hms, Some(fraction)),
        None => (clock, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.len() != 2) {
        return None;
    }
    let hour = digits(parts[0])?;
    let minute = if parts.len() > 1 { digits(parts[1])? } else { 0 };
    let second = if parts.len() > 2 { digits(parts[2])? } else { 0 };
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut seconds = (hour * 3600 + minute * 60 + second) as f64;
    if let Some(fraction) = fraction {
        if parts.len() != 3 || fraction.is_empty() || fraction.len() > 9 || digits(fraction).is_none() {
            return None;
        }
        seconds += format!("0.{fraction}").parse::<f64>().ok()?;
    }
    let Some(offset) = offset else {
        return Some(None);
    };
    let shift = if offset.eq_ignore_ascii_case("z") {
        0.0
    } else {
        let sign = if offset.starts_with('-') { -1.0 } else { 1.0 };
        let body = &offset[1..];
        let (hh, mm) = if let Some((h, m)) = body.split_once(':') {
            (digits(h)?, digits(m.get(..2)?)?)
        } else if body.len() == 4 {
            (digits(&body[..2])?, digits(&body[2..])?)
        } else if body.len() == 2 {
            (digits(body)?, 0)
        } else {
            return None;
        };
        if hh > 23 || mm > 59 {
            return None;
        }
        sign * (hh * 3600 + mm * 60) as f64
    };
    Some(Some(base + seconds - shift))
}

/// Parse an HTTP date (`Retry-After`), accepting IMF-fixdate, RFC 850 and asctime forms.
pub fn parse_http_date(value: &str) -> Option<f64> {
    let cleaned = value.replace([',', '-'], " ");
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let months = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let mut day = None;
    let mut month = None;
    let mut year = None;
    let mut clock = None;
    let mut zone = 0i64;
    for token in tokens {
        let lower = token.to_ascii_lowercase();
        if let Some(index) = months.iter().position(|m| lower.starts_with(m) && lower.len() == 3) {
            month = Some(index as u32 + 1);
        } else if token.contains(':') {
            let parts: Vec<i64> = token.split(':').map(|p| p.parse().unwrap_or(-1)).collect();
            if parts.len() < 2 || parts.iter().any(|p| *p < 0) {
                return None;
            }
            clock = Some(parts[0] * 3600 + parts[1] * 60 + parts.get(2).copied().unwrap_or(0));
        } else if let Some(n) = digits(token) {
            if day.is_none() && token.len() <= 2 {
                day = Some(n as u32);
            } else if year.is_none() {
                year = Some(if token.len() == 2 { if n < 70 { 2000 + n } else { 1900 + n } } else { n });
            }
        } else if (token.starts_with('+') || token.starts_with('-')) && token.len() == 5 {
            let n = digits(&token[1..])?;
            let minutes = (n / 100) * 60 + n % 100;
            zone = if token.starts_with('-') { -minutes * 60 } else { minutes * 60 };
        }
    }
    let (day, month, year, clock) = (day?, month?, year?, clock?);
    if !(1..=31).contains(&day) {
        return None;
    }
    Some((days_from_civil(year, month, day) * 86_400 + clock - zone) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip_and_offsets() {
        assert_eq!(iso_seconds(0.0).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(iso_seconds(1_792_713_600.5).unwrap(), "2026-10-23T00:00:00Z");
        assert_eq!(parse_iso("2026-10-23T00:00:00Z"), Some(Some(1_792_713_600.0)));
        assert_eq!(parse_iso("1970-01-01T00:01:40+00:00"), Some(Some(100.0)));
        assert_eq!(parse_iso("1970-01-01T01:00:00+01:00"), Some(Some(0.0)));
        assert_eq!(parse_iso("1970-01-01T00:00:00.250000Z"), Some(Some(0.25)));
        assert_eq!(parse_iso("2026-10-23T00:00:00"), Some(None));
        assert_eq!(parse_iso("2026-10-23"), Some(None));
        assert_eq!(parse_iso("x"), None);
        assert_eq!(parse_iso("2026-02-30T00:00:00Z"), None);
    }

    #[test]
    fn http_dates() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:02:00 GMT"), Some(120.0));
        assert_eq!(parse_http_date("Thursday, 01-Jan-70 00:02:00 GMT"), Some(120.0));
        assert_eq!(parse_http_date("Thu Jan  1 00:02:00 1970"), Some(120.0));
        assert_eq!(parse_http_date("soon"), None);
    }

    #[test]
    fn civil_conversion_is_inverse() {
        for days in [-1_000_000, -1, 0, 1, 20_000, 2_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
