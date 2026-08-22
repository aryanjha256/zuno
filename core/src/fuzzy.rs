//! Fuzzy subsequence matching and ranking for the picker.
//!
//! Hand-rolled rather than taking `nucleo` or Zed's `fuzzy` crate, and the reason is
//! scale: this ranks hundreds of requests and a couple of dozen action names, not a
//! kernel checkout. At that size a scored subsequence match is indistinguishable in
//! feel, stays a hundred lines of pure code that unit-tests without a window, and avoids
//! wrapping a matcher whose API we'd have to adapt anyway. Revisit if a collection ever
//! gets big enough that ranking shows up in a profile.
//!
//! **Greedy, not optimal.** Each query character takes the first candidate character it
//! can, so `score("ab", "a-xb-ab")` matches the first `a` and the first following `b`
//! rather than finding the tighter `ab` at the end. Fixing that means searching all
//! alignments, which is where a real matcher earns its complexity. Greedy is predictable,
//! and it never *fails* to match something a human would call a match — it just sometimes
//! scores it lower than the ideal alignment would.
//!
//! Scores are comparable only against each other, and only for the same query. There is
//! no meaningful absolute scale.

/// Awarded once per matched character, so longer matches beat shorter ones.
const MATCH: i32 = 4;
/// Directly after the previous match. The strongest signal that a run is intentional:
/// typing `inv` should rank `invoices` far above `i-n-v`.
///
/// Must stay comfortably above `BOUNDARY`, or a string of separator-separated single
/// characters scores like a genuine run. At 12-vs-10 the two came out *exactly* tied for
/// `users` against `u-s-e-r-s`; the gap is deliberate, not incidental.
const CONSECUTIVE: i32 = 14;
/// At the start, or just after a separator, or at a camelCase hump — the places a human
/// reads as "the beginning of a word".
const BOUNDARY: i32 = 8;
/// Per candidate character skipped between two matches, capped so a late match in a long
/// string isn't ranked below noise.
const GAP: i32 = -1;
const MAX_GAP_PENALTY: i32 = -12;
/// Per trailing candidate character. Breaks ties toward the shorter, more specific name:
/// `users` should beat `users-archived` for the query `users`.
const TRAILING: i32 = -1;

/// Score `candidate` against `query`, or `None` if the query isn't a subsequence of it.
///
/// Case-insensitive. An empty query matches everything at 0, which is what makes a picker
/// list its whole set before you type.
pub fn score(query: &str, candidate: &str) -> Option<i32> {
    score_against(&query.chars().collect::<Vec<_>>(), candidate)
}

/// `score`, with the query already split into characters.
///
/// Split out so `rank` can collect the query **once** for a whole ranking pass instead of once per
/// candidate — it is the same string every time, and a picker re-ranks its entire list on every
/// keystroke.
///
/// The empty-query rule lives here rather than in `score` so both entry points share it. That
/// matters more than it looks: falling through to the loop below with no needles scores `-trailing`,
/// which would sort an unfiltered list by length and quietly discard the order the caller
/// assembled.
fn score_against(needles: &[char], candidate: &str) -> Option<i32> {
    if needles.is_empty() {
        return Some(0);
    }

    // Collected once rather than repeatedly indexing a &str: matching walks with
    // lookbehind, which char_indices makes awkward, and these are short strings.
    let haystack: Vec<char> = candidate.chars().collect();

    let mut total = 0;
    let mut at = 0;
    let mut previous_match: Option<usize> = None;

    for &needle in needles {
        let found = haystack[at..]
            .iter()
            .position(|candidate_char| eq_ignore_case(*candidate_char, needle))
            .map(|offset| at + offset)?;

        total += MATCH;

        if previous_match == Some(found.wrapping_sub(1)) {
            total += CONSECUTIVE;
        } else if is_boundary(&haystack, found) {
            total += BOUNDARY;
        }

        if let Some(previous) = previous_match {
            let skipped = found.saturating_sub(previous + 1) as i32;
            total += (skipped * GAP).max(MAX_GAP_PENALTY);
        } else {
            // Leading characters before the first match: same idea, so an early match
            // outranks a late one.
            total += (found as i32 * GAP).max(MAX_GAP_PENALTY);
        }

        previous_match = Some(found);
        at = found + 1;
    }

    let trailing = haystack.len().saturating_sub(at) as i32;
    total += trailing * TRAILING;

    Some(total)
}

/// ASCII-fast, Unicode-correct. `to_lowercase` allocates for the general case, so the
/// common path avoids it.
fn eq_ignore_case(a: char, b: char) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(&b)
    } else {
        a.to_lowercase().eq(b.to_lowercase())
    }
}

/// Whether `index` reads as the start of a word.
///
/// Covers separators (`/users`, `api-key`, `first_name`, `api.example.com`) and camelCase
/// humps (`anchorsForUser`), because request names in the wild use both.
fn is_boundary(haystack: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }

    let previous = haystack[index - 1];
    if matches!(previous, '/' | '-' | '_' | '.' | ' ' | ':' | '?' | '&' | '=') {
        return true;
    }

    // A lowercase-to-uppercase transition. Guarded on the current char being uppercase so
    // that a digit after a letter (`v1`) isn't treated as a new word.
    previous.is_lowercase() && haystack[index].is_uppercase()
}

/// Rank `candidates` against `query`, best first, dropping non-matches.
///
/// Returns indices into `candidates` so the caller keeps ownership of whatever the
/// candidates actually are — the picker's items carry more than a label.
///
/// Ties break toward the earlier candidate, which keeps the order stable: the caller
/// sorts its candidates meaningfully (open buffers before files, then by path), and an
/// unstable sort here would scramble that for equal scores.
pub fn rank(query: &str, candidates: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<usize> {
    // Collected once for the pass, not once per candidate.
    let needles: Vec<char> = query.chars().collect();

    let mut scored: Vec<(usize, i32)> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(ix, candidate)| {
            score_against(&needles, candidate.as_ref()).map(|score| (ix, score))
        })
        .collect();

    scored.sort_by(|(ix_a, score_a), (ix_b, score_b)| score_b.cmp(score_a).then(ix_a.cmp(ix_b)));
    scored.into_iter().map(|(ix, _)| ix).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ranked labels, which is what a test actually wants to assert about.
    fn ranked<'a>(query: &str, candidates: &[&'a str]) -> Vec<&'a str> {
        rank(query, candidates)
            .into_iter()
            .map(|ix| candidates[ix])
            .collect()
    }

    #[test]
    fn a_non_subsequence_does_not_match() {
        assert!(score("xyz", "users").is_none());
        // Right characters, wrong order — subsequence, not bag-of-characters.
        assert!(score("resu", "users").is_none());
        assert!(score("usersx", "users").is_none());
    }

    #[test]
    fn an_empty_query_matches_everything() {
        // What makes the picker show the full list before you type.
        assert_eq!(score("", "anything"), Some(0));
        assert_eq!(ranked("", &["a", "b", "c"]), ["a", "b", "c"]);
    }

    #[test]
    fn an_empty_query_keeps_the_callers_order_regardless_of_length() {
        // The picker lists its whole set before you type, in the order the caller assembled it —
        // open buffers before saved files, environments in scan order. Letting an empty query fall
        // through to the scoring loop would give every candidate `-trailing`, sorting the list by
        // length and silently throwing that order away. Guards the shortcut in `score_against`.
        assert_eq!(
            ranked("", &["a-very-long-name", "b", "medium-name"]),
            ["a-very-long-name", "b", "medium-name"]
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(score("USERS", "users").is_some());
        assert!(score("users", "USERS").is_some());
        assert!(score("anchorsforuser", "anchorsForUser").is_some());
    }

    #[test]
    fn consecutive_beats_scattered() {
        let tight = score("inv", "invoices").expect("match");
        let loose = score("inv", "i-n-v-oices").expect("match");
        assert!(tight > loose, "tight {tight} should beat scattered {loose}");
    }

    #[test]
    fn a_word_boundary_outranks_the_middle_of_a_word() {
        // Typing `inv` should find the path segment, not letters buried in a longer word.
        assert_eq!(
            ranked("inv", &["reinvented", "v1/invoices"]),
            ["v1/invoices", "reinvented"]
        );
    }

    #[test]
    fn camel_case_humps_count_as_boundaries() {
        // Same characters at the same offsets in strings of the same length, differing
        // only in case — so gap and length terms cancel and this isolates the hump bonus.
        // An earlier version of this test compared against `aaaafuuu`, which conflated the
        // hump bonus with the consecutive bonus and asserted a ranking a real matcher
        // would not agree with.
        let humps = score("afu", "anchorsForUser").expect("match");
        let flat = score("afu", "anchorsforuser").expect("match");
        assert!(humps > flat, "humps {humps} should beat flat {flat}");
    }

    #[test]
    fn the_shorter_of_two_matches_wins() {
        // Both contain `users` at the same offset; the more specific name should lead.
        assert_eq!(
            ranked("users", &["users-archived-v2", "users"]),
            ["users", "users-archived-v2"]
        );
    }

    #[test]
    fn an_early_match_outranks_a_late_one() {
        assert_eq!(
            ranked("api", &["v1/internal/legacy/api", "api/v1"]),
            ["api/v1", "v1/internal/legacy/api"]
        );
    }

    #[test]
    fn a_long_prefix_of_junk_does_not_sink_a_real_match() {
        // The gap penalty is capped for exactly this: a genuine match late in a long
        // string must still outrank a scattered near-miss.
        let deep = score("users", &format!("{}/users", "x".repeat(400))).expect("match");
        let scattered = score("users", "u-s-e-r-s").expect("match");
        assert!(deep > scattered, "deep {deep} vs scattered {scattered}");
    }

    #[test]
    fn ties_keep_the_callers_order() {
        // The picker lists open buffers before files and relies on that surviving.
        assert_eq!(ranked("x", &["x", "x", "x"]), ["x", "x", "x"]);
        assert_eq!(rank("x", ["x", "x", "x"]), [0, 1, 2]);
    }

    #[test]
    fn non_ascii_matches_case_insensitively() {
        // Folding has to work in both directions for multi-byte characters, not just
        // ASCII — `eq_ignore_case` takes the cheap ASCII path first and must not let the
        // general path rot.
        assert!(score("é", "École").is_some(), "lowercase query, uppercase text");
        assert!(score("É", "école").is_some(), "uppercase query, lowercase text");
        assert!(score("cole", "École").is_some());
        // Multi-byte characters must not panic or split — this walks chars, not bytes.
        assert!(score("請", "請求書").is_some());
        assert!(score("書", "請求書").is_some(), "a match at the last char");
    }

    #[test]
    fn a_realistic_collection_ranks_sensibly() {
        let collection = [
            "v1/invoices",
            "v1/invoices/{id}/void",
            "v2/users",
            "anchorsForUser",
            "health",
        ];

        // Exact segment first.
        assert_eq!(ranked("invoices", &collection)[0], "v1/invoices");
        // Initials of a camelCase name.
        assert_eq!(ranked("afu", &collection)[0], "anchorsForUser");
        // A query matching nothing yields nothing rather than everything.
        assert!(ranked("zzz", &collection).is_empty());
    }
}
