//! Substring search over a response body.
//!
//! Deliberately over the **source bytes**, not over the rendered rows. The viewer shows a
//! `JsonOutline`, where a key keeps its quotes and a folded container renders as
//! `{ … 3 items }`, and the raw-text fallback truncates any line past
//! `lines::MAX_DISPLAY_LINE`. Searching what is drawn would therefore mean a match count
//! that depends on the fold state and silently misses anything past the display cut. The
//! bytes are the one answer that doesn't move, so this finds every occurrence and leaves
//! *reaching* it to the caller — which is why `json` and `lines` own the offset-to-row
//! mapping rather than this module.
//!
//! One pass, no allocation beyond the result. **Background executor only** on a body of
//! any size (architecture.md §1, rule 2).

/// How many matches are collected before the search gives up.
///
/// Searching a 10MB body for `"` finds about two million occurrences. Nobody navigates two
/// million hits, and the `Vec` alone would cost 8MB, so the scan stops here and reports that
/// it did. **The caller has to say so in the UI** — a count that silently means "at least
/// this many" reads exactly like a count that means "this many", which is the same trust bug
/// as a body truncated without a notice.
pub const MAX_MATCHES: usize = 5_000;

/// Every match, and whether the scan hit `MAX_MATCHES` before the end of the body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hits {
    /// Byte offsets into the source, ascending. `u32` to match `Span`, which caps a
    /// searchable body at 4GB — far past where any of this is a good idea.
    pub offsets: Vec<u32>,
    pub truncated: bool,
}

impl Hits {
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }
}

/// Find every non-overlapping occurrence of `needle`.
///
/// **Smart case:** an all-lowercase needle matches case-insensitively, and one uppercase
/// character anywhere makes the whole search case-sensitive. That beats a fixed default in
/// both directions — `id` finding `Id` is what you want when skimming keys, and `ID` *not*
/// matching `id` is what you want once you've typed a distinction on purpose. Rejected: a
/// case toggle, which is a second control to find and set for a rule people already expect
/// from every editor.
///
/// Folding is **ASCII-only**, so `É` does not match `é`. Doing it properly means decoding
/// UTF-8 per candidate position, and a response body is not guaranteed to be UTF-8 at all
/// (invariant 4) — the bytes have to stay searchable either way.
///
/// Non-overlapping: a match consumes its own length, so `aa` in `aaaa` is two hits, not
/// three. That's what "next match" means to a person pressing it repeatedly.
pub fn find(source: &[u8], needle: &str) -> Hits {
    let needle = needle.as_bytes();
    let mut hits = Hits::default();

    // An empty needle matches everywhere, which is the same as nowhere for a search UI.
    if needle.is_empty() || needle.len() > source.len() {
        return hits;
    }

    let fold = !needle.iter().any(u8::is_ascii_uppercase);
    let first = fold_byte(needle[0], fold);
    let last_start = source.len() - needle.len();

    let mut ix = 0usize;
    while ix <= last_start {
        // Skip on the first byte alone before paying for a full compare. A naive
        // window-by-window comparison is O(n·m) on a body that repeats the first byte;
        // this makes the common case one pass over the source.
        if fold_byte(source[ix], fold) != first {
            ix += 1;
            continue;
        }

        if matches_at(source, ix, needle, fold) {
            hits.offsets.push(ix as u32);
            if hits.offsets.len() >= MAX_MATCHES {
                // `ix` is the *start* of the last match, so there is more body left
                // unscanned unless this match ended exactly at the end.
                hits.truncated = ix + needle.len() < source.len();
                return hits;
            }
            ix += needle.len();
        } else {
            ix += 1;
        }
    }

    hits
}

fn matches_at(source: &[u8], at: usize, needle: &[u8], fold: bool) -> bool {
    source[at..at + needle.len()]
        .iter()
        .zip(needle)
        .all(|(found, wanted)| fold_byte(*found, fold) == fold_byte(*wanted, fold))
}

fn fold_byte(byte: u8, fold: bool) -> u8 {
    if fold {
        byte.to_ascii_lowercase()
    } else {
        byte
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offsets(source: &str, needle: &str) -> Vec<u32> {
        find(source.as_bytes(), needle).offsets
    }

    #[test]
    fn finds_every_occurrence_in_order() {
        assert_eq!(offsets("a-b-a-c-a", "a"), vec![0, 4, 8]);
    }

    #[test]
    fn matches_do_not_overlap() {
        // Three overlapping positions exist in "aaaa"; a person pressing "next" wants two.
        assert_eq!(offsets("aaaa", "aa"), vec![0, 2]);
    }

    #[test]
    fn a_lowercase_needle_ignores_case() {
        assert_eq!(offsets(r#"{"userId":1,"USERID":2}"#, "userid"), vec![2, 13]);
    }

    #[test]
    fn one_uppercase_character_makes_the_search_case_sensitive() {
        // The discriminating half: the lowercase spelling is present and must be skipped.
        assert_eq!(offsets(r#"{"userId":1,"userid":2}"#, "userId"), vec![2]);
    }

    #[test]
    fn non_ascii_is_matched_bytewise_without_folding() {
        // Folding is ASCII-only and documented as such: the exact bytes still match.
        assert_eq!(offsets("héllo", "héllo"), vec![0]);
        assert!(offsets("HÉLLO", "héllo").is_empty(), "no non-ASCII folding");
    }

    #[test]
    fn an_empty_needle_finds_nothing() {
        assert!(find(b"anything", "").is_empty());
    }

    #[test]
    fn a_needle_longer_than_the_body_finds_nothing() {
        assert!(find(b"ab", "abc").is_empty());
    }

    #[test]
    fn nothing_is_found_when_nothing_matches() {
        assert!(find(b"abcdef", "xyz").is_empty());
    }

    #[test]
    fn a_match_at_the_very_end_is_found() {
        // The `ix <= last_start` bound is exactly where an off-by-one would hide.
        assert_eq!(offsets("abcxyz", "xyz"), vec![3]);
    }

    #[test]
    fn invalid_utf8_is_searchable() {
        // Response bodies are bytes, not strings (invariant 4). A needle still has to be
        // findable in a body that isn't valid UTF-8.
        let source = [0xffu8, b'k', b'e', b'y', 0xfe];
        let hits = find(&source, "key");
        assert_eq!(hits.offsets, vec![1]);
    }

    #[test]
    fn the_scan_stops_at_the_cap_and_says_so() {
        let source = "x".repeat(MAX_MATCHES * 2);
        let hits = find(source.as_bytes(), "x");

        assert_eq!(hits.len(), MAX_MATCHES);
        assert!(
            hits.truncated,
            "hitting the cap with body left over must be reported, not silently dropped"
        );
    }

    #[test]
    fn exactly_the_cap_with_nothing_left_over_is_not_truncated() {
        // The distinction the `ix + needle.len() < source.len()` check exists to make:
        // finding every match and *happening* to stop at the cap is a complete result.
        let source = "x".repeat(MAX_MATCHES);
        let hits = find(source.as_bytes(), "x");

        assert_eq!(hits.len(), MAX_MATCHES);
        assert!(!hits.truncated, "a complete result must not claim truncation");
    }
}
