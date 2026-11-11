//! Numbers and durations in chrome.

use std::time::Duration;

/// `1 234 567` with a thin space as the thousands separator.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{2009}');
        }
        out.push(ch);
    }
    out
}

/// `512 B`, `12.3 MB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// A schedule's interval: `24 h`, `90 min`, `2 d`, `1 h 30 min`.
pub fn every(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60);
    match (d, h, m) {
        (d, 0, 0) if d > 0 => format!("{d} d"),
        (0, h, 0) if h > 0 => format!("{h} h"),
        (0, 0, m) => format!("{m} min"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, h, _) => format!("{d} d {h} h"),
    }
}

/// `just now`, `12 min ago`, `3 h ago`, `2 d ago`.
pub fn ago(t: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
    let s = now.signed_duration_since(t).num_seconds().max(0);
    match s {
        0..60 => "just now".into(),
        60..3_600 => format!("{} min ago", s / 60),
        3_600..86_400 => format!("{} h ago", s / 3_600),
        _ => format!("{} d ago", s / 86_400),
    }
}

/// `1 row`, `12 rows`.
pub fn rows(n: u64) -> String {
    if n == 1 {
        "1 row".into()
    } else {
        format!("{} rows", count(n))
    }
}

/// `12 ms`, `1.4 s`, `2 min 05 s`.
pub fn duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", d.as_secs_f64())
    } else {
        let s = d.as_secs();
        format!("{} min {:02} s", s / 60, s % 60)
    }
}

/// First `max` characters of a statement on one line, for the Messages log.
pub fn statement_preview(sql: &str, max: usize) -> String {
    let one_line: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let cut: String = one_line.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Shortens a type name for the column header: `character varying(20)` → `varchar(20)`.
pub fn short_type(type_name: &str) -> String {
    type_name
        .replace("character varying", "varchar")
        .replace("timestamp with time zone", "timestamptz")
        .replace("timestamp without time zone", "timestamp")
        .replace("time with time zone", "timetz")
        .replace("time without time zone", "time")
        .replace("double precision", "float8")
        .replace("integer", "int4")
        .replace("bigint", "int8")
        .replace("smallint", "int2")
        .replace("boolean", "bool")
        .replace("character", "char")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_and_ages() {
        assert_eq!(every(86_400), "1 d");
        assert_eq!(every(12 * 3_600), "12 h");
        assert_eq!(every(5_400), "1 h 30 min");
        assert_eq!(every(90 * 60), "1 h 30 min");
        assert_eq!(every(45 * 60), "45 min");
        let now = chrono::Utc::now();
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now - chrono::Duration::minutes(12), now), "12 min ago");
        assert_eq!(ago(now - chrono::Duration::hours(3), now), "3 h ago");
        assert_eq!(ago(now - chrono::Duration::days(2), now), "2 d ago");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(3 * 1024 * 1024 / 2), "1.5 MB");
    }

    #[test]
    fn counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1\u{2009}000");
        assert_eq!(count(1_234_567), "1\u{2009}234\u{2009}567");
        assert_eq!(rows(1), "1 row");
        assert_eq!(rows(1000), "1\u{2009}000 rows");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(Duration::from_millis(12)), "12 ms");
        assert_eq!(duration(Duration::from_millis(1400)), "1.4 s");
        assert_eq!(duration(Duration::from_secs(125)), "2 min 05 s");
    }

    #[test]
    fn previews_and_types() {
        assert_eq!(statement_preview("select\n  1", 80), "select 1");
        assert_eq!(statement_preview("abcdef", 3), "abc…");
        assert_eq!(short_type("character varying(20)"), "varchar(20)");
        assert_eq!(short_type("timestamp with time zone"), "timestamptz");
        assert_eq!(short_type("integer[]"), "int4[]");
    }
}
