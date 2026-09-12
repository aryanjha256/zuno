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
    let typed = typed.trim();
    if typed.is_empty() {
        return COMMON.to_vec();
    }
    let needle = typed.to_ascii_lowercase();

    let mut prefix = Vec::new();
    let mut contains = Vec::new();
    for name in COMMON {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with(&needle) {
            prefix.push(*name);
        } else if lower.contains(&needle) {
            contains.push(*name);
        }
    }

    // Exactly one match and you have already typed it: there is nothing left to offer, and a
    // one-row list under a finished word is a thing to dismiss rather than a thing to use.
    if prefix.len() == 1 && contains.is_empty() && prefix[0].eq_ignore_ascii_case(typed) {
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
