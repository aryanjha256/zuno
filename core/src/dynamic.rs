//! `{{$guid}}` and its kin: variables whose value is made at send time rather than looked up.
//!
//! **Postman's names, deliberately.** A collection imported from Postman uses exactly these, and a
//! name of Zuno's own would mean nothing to Postman on export — so the five people actually use,
//! under the names they already type. Postman's ~100 fake-data ones (`$randomEmail`, …) are not
//! here yet.
//!
//! **A fresh value per occurrence**, as Postman does: `{{$guid}}` twice in one request is two
//! different ids. The request keeps the placeholder; the Network tab's sent request shows what
//! went out.

use std::time::{SystemTime, UNIX_EPOCH};

/// Every dynamic variable, with what it makes — for completion and the hover.
pub const VARIABLES: [(&str, &str); 5] = [
    ("$guid", "a random UUID (v4)"),
    ("$randomUUID", "a random UUID (v4)"),
    ("$timestamp", "the time of sending, in Unix seconds"),
    ("$isoTimestamp", "the time of sending, as ISO 8601 in UTC"),
    ("$randomInt", "a random whole number from 0 to 1000"),
];

pub fn is_dynamic(name: &str) -> bool {
    VARIABLES.iter().any(|(known, _)| *known == name)
}

pub fn describe(name: &str) -> Option<&'static str> {
    VARIABLES.iter().find(|(known, _)| *known == name).map(|(_, what)| *what)
}

/// A fresh value for `name`, or `None` when it is not a dynamic variable.
pub fn generate(name: &str) -> Option<String> {
    match name {
        "$guid" | "$randomUUID" => Some(uuid_v4()),
        "$timestamp" => Some((now_ms() / 1000).to_string()),
        "$isoTimestamp" => Some(iso_utc(now_ms())),
        "$randomInt" => Some((random_u32() % 1001).to_string()),
        _ => None,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// Random bytes from the OS. A failure here is a broken system rather than a broken request, so it
/// falls back to the clock rather than refusing to send — the value is test data, not a secret.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    if getrandom::fill(&mut bytes).is_err() {
        let seed = now_ms().to_le_bytes();
        for (ix, byte) in bytes.iter_mut().enumerate() {
            *byte = seed[ix % seed.len()].wrapping_add(ix as u8);
        }
    }
    bytes
}

fn random_u32() -> u32 {
    u32::from_le_bytes(random_bytes::<4>())
}

/// RFC 4122 version 4: random, with the version nibble set to 4 and the variant bits to `10`.
fn uuid_v4() -> String {
    let mut b = random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// `2026-10-13T10:20:30.123Z` — Postman's `$isoTimestamp` shape.
fn iso_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (year, month, day) = crate::history::civil_from_days((secs / 86_400) as i64);
    let in_day = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3600,
        (in_day % 3600) / 60,
        in_day % 60,
        ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uuid_is_version_four_and_never_repeats() {
        let a = generate("$guid").unwrap();
        let b = generate("$randomUUID").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4", "version nibble: {a}");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"), "variant bits: {a}");
    }

    /// Checked against `date -u -d @1791374400` — Wed 7 Oct 2026, 12:00 UTC.
    #[test]
    fn timestamps_are_utc_and_iso() {
        assert_eq!(iso_utc(1_791_374_400_123), "2026-10-07T12:00:00.123Z");
        let ts: u64 = generate("$timestamp").unwrap().parse().unwrap();
        assert!(ts > 1_700_000_000);
    }

    #[test]
    fn random_ints_stay_in_range_and_unknown_names_are_none() {
        for _ in 0..200 {
            let n: u32 = generate("$randomInt").unwrap().parse().unwrap();
            assert!(n <= 1000);
        }
        assert_eq!(generate("$nope"), None);
        assert_eq!(generate("guid"), None);
    }
}
