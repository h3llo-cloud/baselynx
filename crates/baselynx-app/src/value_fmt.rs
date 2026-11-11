//! How the record panel presents values. Pure, no UI.

use std::time::{SystemTime, UNIX_EPOCH};

/// JSON pretty-printed with two-space indents; `None` when the text is not JSON.
pub fn pretty_json(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    serde_json::to_string_pretty(&v).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Seconds since the epoch for PostgreSQL's `timestamptz` output in ISO style:
/// `2026-02-01 10:00:00+08`, `2026-02-01 10:00:00.123+05:30`.
pub fn parse_timestamptz(text: &str) -> Option<i64> {
    let t = text.trim();
    let (date, rest) = t.split_once(' ')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let m: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    // The offset starts at the last + or - of the time part.
    let sign_at = rest.rfind(['+', '-'])?;
    let (time, offset) = rest.split_at(sign_at);
    let mut tp = time.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let s: f64 = tp.next()?.parse().ok()?;
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let mut op = offset[1..].split(':');
    let oh: i64 = op.next()?.parse().ok()?;
    let om: i64 = op.next().map(|v| v.parse().ok()).unwrap_or(Some(0))?;
    let os: i64 = op.next().map(|v| v.parse().ok()).unwrap_or(Some(0))?;
    let local = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s as i64;
    Some(local - sign * (oh * 3600 + om * 60 + os))
}

/// `2026-02-01 02:00:00 UTC`.
pub fn utc_text(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// `3 hours ago`, `in 2 days`, `just now`.
pub fn relative(epoch: i64, now: i64) -> String {
    let diff = now - epoch;
    let past = diff >= 0;
    let a = diff.unsigned_abs();
    let (n, unit) = match a {
        0..=44 => return "just now".into(),
        45..=5399 => (a.div_ceil(60).max(1), "minute"),
        5400..=129_599 => ((a + 1800) / 3600, "hour"),
        129_600..=3_887_999 => ((a + 43_200) / 86_400, "day"),
        3_888_000..=47_303_999 => ((a + 1_296_000) / 2_592_000, "month"),
        _ => ((a + 15_768_000) / 31_536_000, "year"),
    };
    let plural = if n == 1 { "" } else { "s" };
    if past {
        format!("{n} {unit}{plural} ago")
    } else {
        format!("in {n} {unit}{plural}")
    }
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `bytea` in hex output (`\x0102…`): size and the first bytes.
pub fn bytea_summary(text: &str) -> Option<String> {
    let hex = text.strip_prefix("\\x")?;
    let n = hex.len() / 2;
    let first: Vec<&str> = (0..n.min(16)).filter_map(|i| hex.get(i * 2..i * 2 + 2)).collect();
    let more = if n > 16 { " …" } else { "" };
    let size = if n == 1 {
        "1 byte".to_string()
    } else {
        format!("{n} bytes")
    };
    Some(format!("{size} · {}{more}", first.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json() {
        assert_eq!(
            pretty_json(r#"{"n": 1, "a": [true]}"#).unwrap(),
            "{\n  \"n\": 1,\n  \"a\": [\n    true\n  ]\n}"
        );
        assert!(pretty_json("not json").is_none());
    }

    #[test]
    fn timestamps() {
        let t = parse_timestamptz("2026-02-01 10:00:00+08").unwrap();
        assert_eq!(utc_text(t), "2026-02-01 02:00:00 UTC");
        let t = parse_timestamptz("2026-02-01 10:00:00.5-05:30").unwrap();
        assert_eq!(utc_text(t), "2026-02-01 15:30:00 UTC");
        assert_eq!(parse_timestamptz("1970-01-01 00:00:00+00"), Some(0));
        assert!(parse_timestamptz("infinity").is_none());
        assert_eq!(relative(0, 30), "just now");
        assert_eq!(relative(0, 3 * 3600), "3 hours ago");
        assert_eq!(relative(0, 90), "2 minutes ago");
        assert_eq!(relative(2 * 86_400, 0), "in 2 days");
    }

    #[test]
    fn bytea() {
        assert_eq!(bytea_summary("\\x0102ff").unwrap(), "3 bytes · 01 02 ff");
        assert_eq!(bytea_summary("\\x").unwrap(), "0 bytes · ");
        assert!(bytea_summary("plain").is_none());
    }
}
