//! Comparing a released version against the running one.
//!
//! In core, and a pure function, for the reason `axis_ticks` and `elide` are: the answer is
//! arithmetic a unit test can hold, and the thing it feeds — a chip in the titlebar — is a
//! paint nothing headless can observe.
//!
//! The trap this exists for is that the obvious implementation is a string compare, and
//! `"0.2.10" < "0.2.9"` lexically. That is wrong exactly once, in public, on the tenth patch
//! release of a line, which is far enough away to ship.

/// A released version, parsed for ordering only.
///
/// Three components, because that is what Zuno tags. A fourth would be ignored rather than
/// refused: an unfamiliar suffix should not make the app decide it is up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    parts: [u64; 3],
    /// `true` for an ordinary release, `false` for a pre-release, so that `0.3.0-rc1` sorts
    /// *below* `0.3.0` — derived `Ord` reads the fields in order, and `false < true`.
    ///
    /// A bool rather than comparing pre-release identifiers: Zuno has never tagged one, and
    /// full semver ordering is a table of rules for a case that does not exist. If one is ever
    /// tagged, this at least refuses to offer it as an upgrade over the release it precedes.
    release: bool,
}

/// Reads `0.2.5`, `v0.2.5`, `0.2`, `0.3.0-rc1`, `0.2.5+build`.
///
/// Returns `None` for anything it cannot read rather than guessing, because every caller
/// treats "unknown" as "say nothing" — a version check that cannot parse an answer must not
/// produce a notice.
pub fn parse(text: &str) -> Option<Version> {
    let text = text.trim();
    let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
    // Build metadata is explicitly not part of precedence, so it is dropped before the
    // pre-release split — `1.0.0+a-b` has build `a-b`, not pre-release `b`.
    let text = text.split('+').next()?;
    let (numbers, release) = match text.split_once('-') {
        Some((numbers, _)) => (numbers, false),
        None => (text, true),
    };

    let mut parts = [0u64; 3];
    let mut seen = 0;
    for (ix, field) in numbers.split('.').enumerate() {
        if ix >= parts.len() {
            break;
        }
        parts[ix] = field.parse().ok()?;
        seen += 1;
    }
    (seen > 0).then_some(Version { parts, release })
}

/// Is `candidate` a release worth telling someone about, given they are running `current`?
///
/// False whenever either side cannot be read. Both directions matter: an unreadable *candidate*
/// must not produce a notice, and an unreadable *current* must not make every check announce an
/// update on a build whose own version is malformed.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse(candidate), parse(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_patch_is_newer() {
        assert!(is_newer("0.2.6", "0.2.5"));
        assert!(is_newer("0.3.0", "0.2.99"));
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    /// The whole reason this module is not a string comparison.
    #[test]
    fn ten_is_newer_than_nine() {
        assert!(is_newer("0.2.10", "0.2.9"));
        assert!(!is_newer("0.2.9", "0.2.10"));
        assert!(is_newer("0.10.0", "0.9.0"));
    }

    #[test]
    fn the_same_version_is_not_newer() {
        assert!(!is_newer("0.2.5", "0.2.5"));
        assert!(!is_newer("v0.2.5", "0.2.5"));
        // A tag carries the `v`; `CARGO_PKG_VERSION` does not. Both sides tolerate it, so
        // which one is which cannot matter.
        assert!(!is_newer("0.2.5", "v0.2.5"));
    }

    #[test]
    fn an_older_version_is_not_newer() {
        assert!(!is_newer("0.2.4", "0.2.5"));
        assert!(!is_newer("0.1.0", "1.0.0"));
    }

    /// Unreadable input says "no update", never "yes". A malformed answer from the network, or
    /// a malformed `CARGO_PKG_VERSION`, must not put a chip in the titlebar.
    #[test]
    fn unreadable_input_is_never_newer() {
        assert!(!is_newer("", "0.2.5"));
        assert!(!is_newer("latest", "0.2.5"));
        assert!(!is_newer("0.2.x", "0.2.5"));
        assert!(!is_newer("<html>", "0.2.5"));
        assert!(!is_newer("0.2.6", "not-a-version"));
    }

    #[test]
    fn a_missing_component_reads_as_zero() {
        assert_eq!(parse("1.0"), parse("1.0.0"));
        assert_eq!(parse("1"), parse("1.0.0"));
        assert!(is_newer("1.1", "1.0.9"));
    }

    /// A pre-release is older than the release it precedes, so tagging `0.3.0-rc1` never offers
    /// itself as an upgrade over `0.3.0`.
    #[test]
    fn a_pre_release_sorts_below_its_release() {
        assert!(is_newer("0.3.0", "0.3.0-rc1"));
        assert!(!is_newer("0.3.0-rc1", "0.3.0"));
        assert!(is_newer("0.3.0-rc1", "0.2.9"));
    }

    /// Build metadata is not part of precedence, and its hyphen must not be read as the start
    /// of a pre-release.
    #[test]
    fn build_metadata_is_ignored() {
        assert_eq!(parse("1.2.3+build-7"), parse("1.2.3"));
        assert!(!is_newer("1.2.3+build", "1.2.3"));
    }
}
