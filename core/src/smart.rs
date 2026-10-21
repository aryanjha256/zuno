//! Values in a response that mean more than their text: timestamps, colours, JWTs.
//!
//! Two halves, priced differently. `classify` runs for every row the response viewer paints, so
//! it only looks at the shape of a value and never allocates for the common miss. `card` builds
//! what the hover card shows, and runs once, when someone hovers.
//!
//! **A number is a timestamp only under a date-like key.** `1792578843` is as likely an order
//! number as a time, and annotating ids with dates is the false positive that makes people turn a
//! feature like this off. A string has no such problem: ISO 8601 is unambiguous on its own.

use crate::history::civil_from_days;
use crate::json::ScalarKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Smart {
    Timestamp { at_ms: i64, form: TimeForm },
    Colour { r: u8, g: u8, b: u8, a: u8 },
    Jwt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeForm {
    Iso,
    UnixSeconds,
    UnixMillis,
}

/// What a value is, if it is one of the kinds worth explaining.
///
/// `key` and `value` are the raw source text, quotes included, as the outline holds them.
pub fn classify(key: Option<&str>, value: &str, kind: ScalarKind) -> Option<Smart> {
    match kind {
        ScalarKind::String => {
            let inner = value.strip_prefix('"')?.strip_suffix('"')?;
            // Escapes mean the raw text is not the value; none of the three shapes needs one.
            if inner.contains('\\') {
                return None;
            }
            colour(inner)
                .or_else(|| jwt(inner).then_some(Smart::Jwt))
                .or_else(|| iso(inner).map(|at_ms| Smart::Timestamp { at_ms, form: TimeForm::Iso }))
        }
        ScalarKind::Number => {
            let key = key?.trim_matches('"');
            if !date_like(key) {
                return None;
            }
            let n: i64 = value.parse().ok()?;
            // 2001-09-09 to 2100-01-01, in seconds then milliseconds. Below the floor a "time" is
            // more likely a duration or a count, and the two ranges cannot overlap.
            const FROM: i64 = 1_000_000_000;
            const TO: i64 = 4_102_444_800;
            if (FROM..TO).contains(&n) {
                Some(Smart::Timestamp { at_ms: n * 1000, form: TimeForm::UnixSeconds })
            } else if (FROM * 1000..TO * 1000).contains(&n) {
                Some(Smart::Timestamp { at_ms: n, form: TimeForm::UnixMillis })
            } else {
                None
            }
        }
        ScalarKind::Bool | ScalarKind::Null => None,
    }
}

fn date_like(key: &str) -> bool {
    const EXACT: &[&str] = &[
        "timestamp", "time", "date", "ts", "exp", "iat", "nbf", "auth_time", "expires",
        "expiry", "created", "updated", "modified", "deleted", "since", "until",
    ];
    let lower = key.to_ascii_lowercase();
    EXACT.contains(&lower.as_str())
        || ["_at", "_time", "_date", "_ts", "timestamp"].iter().any(|end| lower.ends_with(end))
        // camelCase: `createdAt`, `startTime`, `dueDate`.
        || ["At", "Time", "Date"].iter().any(|end| key.len() > end.len() && key.ends_with(end))
}

fn colour(text: &str) -> Option<Smart> {
    let hex = text.strip_prefix('#')?;
    if !matches!(hex.len(), 3 | 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok();
    let pair = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(if hex.len() == 3 {
        let (r, g, b) = (digit(0)?, digit(1)?, digit(2)?);
        Smart::Colour { r: r * 17, g: g * 17, b: b * 17, a: 255 }
    } else {
        Smart::Colour {
            r: pair(0)?,
            g: pair(2)?,
            b: pair(4)?,
            a: if hex.len() == 8 { pair(6)? } else { 255 },
        }
    })
}

/// Three base64url segments whose header is a JSON object naming an `alg`.
///
/// `eyJ` is `{"` encoded, so the prefix rejects nearly every string before anything is decoded.
fn jwt(text: &str) -> bool {
    if !text.starts_with("eyJ") || text.len() > 16 * 1024 {
        return false;
    }
    let mut parts = text.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if payload.is_empty() || ![header, payload, signature].iter().all(|part| part.bytes().all(base64url_char)) {
        return false;
    }
    decode_json(header).is_some_and(|value| value.get("alg").is_some())
}

fn base64url_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn decode_json(segment: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_slice(&base64url(segment)?).ok()? {
        serde_json::Value::Object(map) => Some(map),
        _ => None,
    }
}

fn base64url(segment: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(segment.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for b in segment.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// `YYYY-MM-DD`, optionally followed by a time and a zone. A time without a zone is read as UTC,
/// and the card says so rather than guessing the server's zone.
fn iso(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let s = text.get(range)?;
        s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok())?
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut ms = days_from_civil(year, month, day) * 86_400_000;
    let rest = &text[10..];
    if rest.is_empty() {
        return Some(ms);
    }
    let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
    let rb = rest.as_bytes();
    if rb.len() < 5 || rb[2] != b':' {
        return None;
    }
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let mut at = 5;
    let mut second = 0;
    if rb.get(at) == Some(&b':') {
        second = num(11 + at + 1..11 + at + 3)?;
        at += 3;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut millis = 0;
    if rb.get(at) == Some(&b'.') || rb.get(at) == Some(&b',') {
        let digits = rb[at + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let frac = &rest[at + 1..at + 1 + digits.min(3)];
        millis = frac.parse::<i64>().ok()? * 10_i64.pow(3 - frac.len() as u32);
        at += 1 + digits;
    }
    ms += ((hour * 60 + minute) * 60 + second) * 1000 + millis;
    match &rest[at..] {
        "" | "Z" | "z" => Some(ms),
        zone => {
            let sign = match zone.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let zone = &zone[1..];
            let (h, m) = match zone.len() {
                2 => (zone, "00"),
                4 => (&zone[..2], &zone[2..]),
                5 if zone.as_bytes()[2] == b':' => (&zone[..2], &zone[3..]),
                _ => return None,
            };
            let parse = |s: &str| s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse::<i64>().ok())?;
            let offset = (parse(h)? * 60 + parse(m)?) * 60_000;
            Some(ms - sign * offset)
        }
    }
}

/// The inverse of `civil_from_days`, from the same paper.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Muted,
    Warn,
    Bad,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub label: String,
    pub value: String,
    pub tone: Tone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub title: &'static str,
    pub lines: Vec<Line>,
    pub foot: Option<&'static str>,
}

fn line(label: impl Into<String>, value: impl Into<String>, tone: Tone) -> Line {
    Line { label: label.into(), value: value.into(), tone }
}

/// What the hover card shows. `value` is the raw source text, as for `classify`.
pub fn card(smart: Smart, value: &str, now_ms: i64, offset_secs: i32) -> Card {
    match smart {
        Smart::Timestamp { at_ms, form } => {
            let mut lines = Vec::new();
            if offset_secs != 0 {
                lines.push(line("Local", clock(at_ms, offset_secs), Tone::Plain));
            }
            lines.push(line("UTC", clock(at_ms, 0), Tone::Plain));
            lines.push(line("Relative", relative(at_ms, now_ms), Tone::Plain));
            let zoneless = form == TimeForm::Iso && !has_zone(value.trim_matches('"'));
            lines.push(line(
                "Read as",
                match form {
                    TimeForm::Iso if zoneless => "ISO 8601, no zone, read as UTC",
                    TimeForm::Iso => "ISO 8601",
                    TimeForm::UnixSeconds => "Unix seconds",
                    TimeForm::UnixMillis => "Unix milliseconds",
                },
                Tone::Muted,
            ));
            Card { title: "Timestamp", lines, foot: None }
        }
        Smart::Colour { r, g, b, a } => {
            let (h, s, l) = hsl(r, g, b);
            let mut lines = vec![
                line("Hex", value.trim_matches('"'), Tone::Plain),
                line("RGB", format!("{r}, {g}, {b}"), Tone::Plain),
                line("HSL", format!("{h}°, {s}%, {l}%"), Tone::Plain),
            ];
            if a != 255 {
                lines.push(line("Alpha", format!("{}%", (u32::from(a) * 100 + 127) / 255), Tone::Plain));
            }
            Card { title: "Colour", lines, foot: None }
        }
        Smart::Jwt => {
            let token = value.trim_matches('"');
            let mut parts = token.split('.');
            let header = parts.next().and_then(decode_json).unwrap_or_default();
            let claims = parts.next().and_then(decode_json);
            let mut lines: Vec<Line> = header
                .iter()
                .map(|(k, v)| line(k.clone(), compact(v), Tone::Muted))
                .collect();
            match claims {
                Some(claims) => {
                    for (k, v) in claims.iter().take(16) {
                        let at = matches!(k.as_str(), "exp" | "iat" | "nbf" | "auth_time")
                            .then(|| v.as_i64())
                            .flatten()
                            .map(|secs| secs * 1000);
                        lines.push(match (k.as_str(), at) {
                            ("exp", Some(at)) if at <= now_ms => {
                                line("exp", format!("expired {}", relative(at, now_ms)), Tone::Bad)
                            }
                            ("exp", Some(at)) => line("exp", format!("expires {}", relative(at, now_ms)), Tone::Warn),
                            (_, Some(at)) => line(k.clone(), relative(at, now_ms), Tone::Plain),
                            _ => line(k.clone(), compact(v), Tone::Plain),
                        });
                    }
                }
                None => lines.push(line("claims", "not JSON", Tone::Muted)),
            }
            Card { title: "JWT", lines, foot: Some("Decoded locally. Signature not checked.") }
        }
    }
}

fn has_zone(text: &str) -> bool {
    text.len() > 10 && {
        let time = &text[11..];
        time.ends_with(['Z', 'z']) || time.contains(['+', '-'])
    }
}

fn compact(value: &serde_json::Value) -> String {
    let text = value.to_string();
    crate::request::elide(&text, 48).into_owned()
}

/// `Wed 21 Oct 2026, 10:34:03` in a zone `offset_secs` east of UTC.
fn clock(at_ms: i64, offset_secs: i32) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let local = at_ms.div_euclid(1000) + i64::from(offset_secs);
    let day = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (year, month, date) = civil_from_days(day);
    format!(
        "{} {date} {} {year}, {:02}:{:02}:{:02}",
        WEEKDAYS[day.rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// `3 hours ago`, `in 48 minutes`.
pub fn relative(at_ms: i64, now_ms: i64) -> String {
    let delta = (now_ms - at_ms) / 1000;
    let span = delta.abs();
    if span < 60 {
        return "just now".to_string();
    }
    let (n, unit) = match span {
        ..3_600 => (span / 60, "minute"),
        ..86_400 => (span / 3_600, "hour"),
        ..2_592_000 => (span / 86_400, "day"),
        ..31_536_000 => (span / 2_592_000, "month"),
        _ => (span / 31_536_000, "year"),
    };
    let amount = if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") };
    if delta > 0 { format!("{amount} ago") } else { format!("in {amount}") }
}

fn hsl(r: u8, g: u8, b: u8) -> (u32, u32, u32) {
    let (r, g, b) = (f32::from(r) / 255., f32::from(g) / 255., f32::from(b) / 255.);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.;
    let d = max - min;
    if d == 0. {
        return (0, 0, (l * 100.).round() as u32);
    }
    let s = d / (1. - (2. * l - 1.).abs());
    let h = if max == r {
        60. * ((g - b) / d).rem_euclid(6.)
    } else if max == g {
        60. * ((b - r) / d + 2.)
    } else {
        60. * ((r - g) / d + 4.)
    };
    (h.round() as u32 % 360, (s * 100.).round() as u32, (l * 100.).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_792_579_563_000; // Wed 21 Oct 2026, 10:46:03 UTC

    #[test]
    fn a_number_is_a_time_only_under_a_date_like_key() {
        let at = Some(Smart::Timestamp { at_ms: 1_792_578_843_000, form: TimeForm::UnixSeconds });
        assert_eq!(classify(Some("\"updated_at\""), "1792578843", ScalarKind::Number), at);
        assert_eq!(classify(Some("\"createdAt\""), "1792578843", ScalarKind::Number), at);
        assert_eq!(classify(Some("\"exp\""), "1792578843", ScalarKind::Number), at);
        assert_eq!(classify(Some("\"order_number\""), "1792578843", ScalarKind::Number), None);
        assert_eq!(classify(None, "1792578843", ScalarKind::Number), None);
        assert_eq!(classify(Some("\"expires_at\""), "3600", ScalarKind::Number), None);
        assert_eq!(
            classify(Some("\"ts\""), "1792578843123", ScalarKind::Number),
            Some(Smart::Timestamp { at_ms: 1_792_578_843_123, form: TimeForm::UnixMillis })
        );
    }

    #[test]
    fn iso_strings_parse_with_and_without_zones() {
        let iso_at = |text: &str| match classify(None, &format!("\"{text}\""), ScalarKind::String) {
            Some(Smart::Timestamp { at_ms, .. }) => Some(at_ms),
            _ => None,
        };
        assert_eq!(iso_at("2026-10-21T10:34:03Z"), Some(1_792_578_843_000));
        assert_eq!(iso_at("2026-10-21T16:04:03+05:30"), Some(1_792_578_843_000));
        assert_eq!(iso_at("2026-10-21T10:34:03.5Z"), Some(1_792_578_843_500));
        assert_eq!(iso_at("2026-10-21"), Some(1_792_540_800_000));
        assert_eq!(iso_at("2026-13-21"), None);
        assert_eq!(iso_at("2026-10-21 and more"), None);
        assert_eq!(iso_at("not a date"), None);
    }

    #[test]
    fn colours_and_jwts_are_recognised_by_shape() {
        assert_eq!(
            classify(None, "\"#5b8def\"", ScalarKind::String),
            Some(Smart::Colour { r: 0x5b, g: 0x8d, b: 0xef, a: 255 })
        );
        assert_eq!(
            classify(None, "\"#fff\"", ScalarKind::String),
            Some(Smart::Colour { r: 255, g: 255, b: 255, a: 255 })
        );
        assert_eq!(classify(None, "\"#5b8de\"", ScalarKind::String), None);
        assert_eq!(classify(None, "\"#issue\"", ScalarKind::String), None);

        // {"alg":"HS256","typ":"JWT"} . {"sub":"u1","exp":1792582443} . sig
        let token = "\"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1MSIsImV4cCI6MTc5MjU4MjQ0M30.c2ln\"";
        assert_eq!(classify(None, token, ScalarKind::String), Some(Smart::Jwt));
        let card = card(Smart::Jwt, token, NOW, 0);
        assert!(card.lines.contains(&line("alg", "\"HS256\"", Tone::Muted)));
        assert!(card.lines.contains(&line("sub", "\"u1\"", Tone::Plain)));
        assert!(card.lines.contains(&line("exp", "expires in 48 minutes", Tone::Warn)));
        // Three dotted segments that are not a token.
        assert_eq!(classify(None, "\"eyJ.abc.def\"", ScalarKind::String), None);
    }

    #[test]
    fn times_read_back_as_people_say_them() {
        assert_eq!(relative(NOW - 3 * 3_600_000, NOW), "3 hours ago");
        assert_eq!(relative(NOW + 60_000, NOW), "in 1 minute");
        assert_eq!(relative(NOW, NOW), "just now");
        assert_eq!(clock(1_792_578_843_000, 0), "Wed 21 Oct 2026, 10:34:03");
        assert_eq!(clock(1_792_578_843_000, 19_800), "Wed 21 Oct 2026, 16:04:03");
    }
}
