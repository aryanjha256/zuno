//! Common HTTP request header names, and matching a partially-typed one.
//!
//! In core and pure, for `fuzzy` and `version`'s reason: the matching is something a unit test
//! can hold, while the dropdown it feeds is a paint nothing headless can observe.
//!
//! **Not the IANA registry.** That is hundreds of names, most of which are response-only or
//! belong to one protocol extension, and a list you have to scroll past is worse than typing.
//! This is the set someone actually sends, kept short enough to read.
//!
//! **Prefix before substring, and never fuzzy.** `fuzzy.rs` scores subsequences, which is right
//! for a palette where you half-remember a command name and wrong here: `cte` would match
//! `Content-Type` and a dozen others, and a list that reorders unpredictably as you type is one
//! you stop trusting. Prefix matches first, then substring, each in the table's own order.

/// Sorted, which is what makes "alphabetical within each group" free rather than a sort at
/// every keystroke. `the_table_is_sorted_and_unique` holds it.
pub const COMMON: &[&str] = &[
    "Accept",
    "Accept-Charset",
    "Accept-Encoding",
    "Accept-Language",
    "Authorization",
    "Cache-Control",
    "Connection",
    "Content-Disposition",
    "Content-Encoding",
    "Content-Language",
    "Content-Length",
    "Content-Type",
    "Cookie",
    "Date",
    "ETag",
    "Expect",
    "Forwarded",
    "From",
    "Host",
    "Idempotency-Key",
    "If-Match",
    "If-Modified-Since",
    "If-None-Match",
    "If-Range",
    "If-Unmodified-Since",
    "Origin",
    "Pragma",
    "Prefer",
    "Range",
    "Referer",
    "TE",
    "Trailer",
    "Transfer-Encoding",
    "Upgrade",
    "User-Agent",
    "Via",
    "X-Api-Key",
    "X-Correlation-Id",
    "X-Csrf-Token",
    "X-Forwarded-For",
    "X-Forwarded-Host",
    "X-Forwarded-Proto",
    "X-Request-Id",
    "X-Requested-With",
];

/// What to offer for a partially-typed header name.
///
/// Empty input offers everything — that is the combobox half, and it is the whole value for
/// someone who does not know what headers exist. An input matching nothing offers nothing
/// rather than falling back to the full list, because a custom `X-Trace-Id` is an ordinary
/// thing to type and a list reappearing under it is noise.
pub fn suggestions(typed: &str) -> Vec<&'static str> {
    rank(COMMON, typed)
}

/// Values worth offering, per header. **Only headers with a small vocabulary** — a
/// `User-Agent` or an `If-Match` is whatever you need it to be, and a list under it would be
/// noise. Most-used first rather than alphabetical: `application/json` is the answer far more
/// often than anything that sorts before it.
///
/// `multipart/form-data` is deliberately missing from `Content-Type`: a multipart body writes
/// that header itself, boundary included, over anything typed — so the only place a typed one
/// takes effect is on some other body, where it has no boundary and no server can read it.
const VALUES: &[(&str, &[&str])] = &[
    (
        "Accept",
        &["application/json", "*/*", "text/plain", "text/html", "application/xml", "text/event-stream"],
    ),
    ("Accept-Encoding", &["gzip, deflate, br", "gzip", "br", "deflate", "zstd", "identity"]),
    ("Accept-Language", &["en-US,en;q=0.9", "en", "*"]),
    ("Authorization", &["Bearer {{token}}", "Basic "]),
    ("Cache-Control", &["no-cache", "no-store", "max-age=0"]),
    ("Connection", &["keep-alive", "close"]),
    ("Content-Encoding", &["gzip", "br", "deflate", "zstd"]),
    (
        "Content-Type",
        &[
            "application/json",
            "application/x-www-form-urlencoded",
            "text/plain",
            "application/xml",
            "text/xml",
            "text/html",
            "application/octet-stream",
            "application/graphql",
        ],
    ),
    ("Expect", &["100-continue"]),
    ("Pragma", &["no-cache"]),
    ("Prefer", &["return=representation", "return=minimal", "respond-async"]),
    ("TE", &["trailers"]),
    ("Transfer-Encoding", &["chunked"]),
    ("X-Requested-With", &["XMLHttpRequest"]),
];

/// What to offer for a partially-typed value of the header `name` — `suggestions`' rules, over
/// that header's own values. Nothing for a header with no table.
pub fn value_suggestions(name: &str, typed: &str) -> Vec<&'static str> {
    let name = name.trim();
    VALUES
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, values)| rank(values, typed))
        .unwrap_or_default()
}

/// Prefix matches, then substring matches, each in the table's own order.
fn rank(table: &[&'static str], typed: &str) -> Vec<&'static str> {
    let typed = typed.trim();
    if typed.is_empty() {
        return table.to_vec();
    }
    let needle = typed.to_ascii_lowercase();

    let mut prefix = Vec::new();
    let mut contains = Vec::new();
    for entry in table {
        let lower = entry.to_ascii_lowercase();
        if lower.starts_with(&needle) {
            prefix.push(*entry);
        } else if lower.contains(&needle) {
            contains.push(*entry);
        }
    }

    // Exactly one match and you have already typed it: there is nothing left to offer, and a
    // one-row list under a finished word is a thing to dismiss rather than a thing to use.
    // Trimmed, so a finished `Basic ` counts as typed.
    if prefix.len() == 1 && contains.is_empty() && prefix[0].trim().eq_ignore_ascii_case(typed) {
        return Vec::new();
    }

    prefix.extend(contains);
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_and_unique() {
        let mut sorted = COMMON.to_vec();
        sorted.sort_by_key(|name| name.to_ascii_lowercase());
        assert_eq!(COMMON.to_vec(), sorted, "COMMON must stay alphabetical");

        let mut seen: Vec<String> = COMMON.iter().map(|n| n.to_ascii_lowercase()).collect();
        seen.sort();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "COMMON has a duplicate");
    }

    /// Values follow the header whatever case its name was typed in, filter by what is typed,
    /// and a header with no vocabulary offers nothing rather than something irrelevant.
    #[test]
    fn values_are_offered_for_the_header_they_belong_to() {
        assert_eq!(value_suggestions("content-type", "").first(), Some(&"application/json"));
        assert_eq!(value_suggestions("Content-Type", "form"), ["application/x-www-form-urlencoded"]);
        assert_eq!(value_suggestions("Accept", "json"), ["application/json"]);
        assert!(value_suggestions("Content-Type", "application/json").is_empty());
        assert!(value_suggestions("Authorization", "basic").is_empty(), "finished, trimmed");
        assert!(value_suggestions("User-Agent", "").is_empty());
        assert!(value_suggestions("", "").is_empty());
    }

    /// A header in the value table must be one the name list offers, or its values sit behind a
    /// name nobody is prompted to type — and every value must be something the engine can send.
    #[test]
    fn every_value_belongs_to_a_known_header_and_is_sendable() {
        for (name, values) in VALUES {
            assert!(COMMON.contains(name), "{name} has values but is not a suggested name");
            for value in *values {
                assert!(
                    http::HeaderValue::from_str(value).is_ok(),
                    "{name}: {value:?} is not a legal header value"
                );
            }
        }
    }

    /// Every name has to be a legal header name, or the dropdown offers something the engine
    /// refuses. Catches a stray space or a non-ASCII character pasted into the table.
    #[test]
    fn every_name_is_a_legal_header_name() {
        for name in COMMON {
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "{name} is not a legal header name"
            );
        }
    }

    #[test]
    fn nothing_typed_offers_everything() {
        assert_eq!(suggestions("").len(), COMMON.len());
        assert_eq!(suggestions("   ").len(), COMMON.len());
    }

    #[test]
    fn a_prefix_matches() {
        let out = suggestions("auth");
        assert_eq!(out, vec!["Authorization"]);
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(suggestions("AUTH"), suggestions("auth"));
        assert_eq!(suggestions("CoNtEnT-T"), suggestions("content-t"));
    }

    /// The ordering rule: everything starting with the text comes before anything merely
    /// containing it. Break it and a substring match can outrank the name you were typing.
    #[test]
    fn prefix_matches_come_before_substring_matches() {
        let out = suggestions("encoding");
        assert!(
            out.contains(&"Accept-Encoding") && out.contains(&"Content-Encoding"),
            "{out:?}"
        );

        let out = suggestions("con");
        let first_prefix = out.iter().position(|n| n.starts_with("Con")).expect("a prefix match");
        let first_other = out.iter().position(|n| !n.starts_with("Con"));
        if let Some(other) = first_other {
            assert!(
                first_prefix < other,
                "a substring match outranked a prefix match: {out:?}"
            );
        }
    }

    /// A custom header is ordinary. Offering the whole table under it would put a list in the
    /// way of the most common reason to be typing at all.
    #[test]
    fn an_unknown_name_offers_nothing() {
        assert!(suggestions("X-Trace-Id").is_empty());
        assert!(suggestions("zzz").is_empty());
    }

    /// Once the word is finished there is nothing left to choose.
    #[test]
    fn a_finished_name_offers_nothing() {
        assert!(suggestions("Authorization").is_empty());
        assert!(suggestions("authorization").is_empty());
        // But a finished name that is also a prefix of others still offers those.
        assert!(suggestions("Accept").len() > 1);
    }
}
