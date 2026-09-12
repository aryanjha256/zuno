//! Reading a filename out of `Content-Disposition`.
//!
//! When a server says `attachment; filename="report-2026.xlsx"` it has named the file, and that
//! beats anything inferable from the URL and the content type. Download endpoints do this
//! routinely, and it is the difference between `users.xlsx` and `api-v1-export.xlsx`.
//!
//! **Everything here treats the header as hostile**, because it is: it arrives from the far end
//! of the network and its whole purpose is to become a path on the user's disk. RFC 6266 permits
//! a quoted string, which means a filename can contain `/`, `..`, NUL, a newline, or a leading
//! dot — none of which this returns. The parse is the easy half; `sanitize` is the half that
//! matters.

/// Longest name accepted. Comfortably past any real filename and short of the ~255-byte limit
/// every common filesystem imposes, so a name that survives here cannot fail to be written for
/// being too long.
const MAX_LEN: usize = 128;

/// The filename a `Content-Disposition` header asks for, sanitized, if it names one at all.
///
/// `filename*` wins over `filename` when both appear, per RFC 6266 §4.3 — it is the encoded form
/// and therefore the one that can carry a name the plain form had to mangle.
pub fn filename(header: &str) -> Option<String> {
    let params = params(header);

    let extended = params
        .iter()
        .find(|(name, _)| name == "filename*")
        .and_then(|(_, value)| decode_ext(value))
        .and_then(|name| sanitize(&name));
    if extended.is_some() {
        return extended;
    }

    params
        .iter()
        .find(|(name, _)| name == "filename")
        .and_then(|(_, value)| sanitize(value))
}

/// Split a header into its `name=value` parameters.
///
/// Hand-scanned rather than `split(';')` because a quoted value may **contain** a semicolon —
/// `filename="a;b.txt"` is one parameter, not two — and splitting first would cut it in half and
/// leave a fragment that parses as a second parameter with no name.
fn params(header: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;

    for ch in header.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                push_param(&mut out, &current);
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    push_param(&mut out, &current);
    out
}

fn push_param(out: &mut Vec<(String, String)>, raw: &str) {
    let Some((name, value)) = raw.split_once('=') else {
        // `attachment` and `inline` carry no value; they are the disposition type, not a
        // parameter, and nothing here needs them.
        return;
    };
    out.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
}

/// Decode an RFC 5987 `charset'language'percent-encoded` value.
///
/// Only the two charsets the RFC requires are honoured. An unknown one returns `None` rather
/// than a guess: decoding Shift-JIS bytes as UTF-8 produces a name that is wrong in a way nobody
/// can see, and falling back to the plain `filename` parameter is the better answer.
fn decode_ext(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?.to_ascii_lowercase();
    let _language = parts.next()?;
    let encoded = parts.next()?;

    let bytes = percent_decode(encoded)?;
    match charset.as_str() {
        "utf-8" => String::from_utf8(bytes).ok(),
        // Every byte is its own code point, which is what makes this lossless and infallible.
        "iso-8859-1" => Some(bytes.into_iter().map(char::from).collect()),
        _ => None,
    }
}

fn percent_decode(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut ix = 0;
    while ix < bytes.len() {
        if bytes[ix] == b'%' {
            let hex = bytes.get(ix + 1..ix + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            ix += 3;
        } else {
            out.push(bytes[ix]);
            ix += 1;
        }
    }
    Some(out)
}

/// Reduce a server-supplied name to something safe to suggest as a path.
///
/// **The order matters.** Path separators are stripped *first*, so `../../.ssh/config` becomes
/// `config` rather than being rejected outright — a hostile header should degrade to a harmless
/// name, not to no name, because rejecting it silently falls back to the URL-derived label and
/// hides that the server said anything. Leading dots go last, after the segment is chosen, so
/// `/etc/.profile` cannot come back as a hidden file.
fn sanitize(raw: &str) -> Option<String> {
    // Both separators, whatever platform this runs on: a Windows-shaped name reaching a Unix box
    // still must not be read as a path if the file is later opened somewhere else.
    let base = raw.rsplit(['/', '\\']).next()?;

    let cleaned: String = base
        .chars()
        .filter(|ch| {
            // Control characters, and the ASCII set that is special to some filesystem or shell.
            !ch.is_control() && !matches!(ch, '\0' | '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
        .collect();

    let cleaned = cleaned.trim();
    // A name that is only dots is `.` or `..`, which name directories rather than files.
    let cleaned = cleaned.trim_start_matches('.');
    let cleaned = cleaned.trim();

    if cleaned.is_empty() {
        return None;
    }

    let truncated: String = cleaned.chars().take(MAX_LEN).collect();
    let truncated = truncated.trim_end().to_string();
    (!truncated.is_empty()).then_some(truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_filename_is_read() {
        assert_eq!(
            filename(r#"attachment; filename="report-2026.xlsx""#).as_deref(),
            Some("report-2026.xlsx")
        );
    }

    #[test]
    fn an_unquoted_token_is_read() {
        assert_eq!(
            filename("attachment; filename=export.csv").as_deref(),
            Some("export.csv")
        );
    }

    #[test]
    fn inline_counts_too() {
        // The disposition *type* says how to display it, not whether it has a name.
        assert_eq!(
            filename(r#"inline; filename="preview.png""#).as_deref(),
            Some("preview.png")
        );
    }

    #[test]
    fn a_header_with_no_filename_names_nothing() {
        assert_eq!(filename("attachment"), None);
        assert_eq!(filename(""), None);
        assert_eq!(filename("attachment; size=42"), None);
    }

    /// RFC 6266 §4.3: the encoded form is the one that can carry a name the plain form mangled,
    /// so it wins wherever both are sent — which is the common shape, sent for old clients.
    #[test]
    fn the_extended_form_wins_over_the_plain_one() {
        let header = "attachment; filename=\"naive.txt\"; filename*=UTF-8''na%C3%AFve.txt";
        assert_eq!(filename(header).as_deref(), Some("naïve.txt"));
    }

    #[test]
    fn an_extended_value_is_percent_decoded() {
        assert_eq!(
            filename("attachment; filename*=UTF-8''report%20%282026%29.pdf").as_deref(),
            Some("report (2026).pdf")
        );
        assert_eq!(
            filename("attachment; filename*=iso-8859-1'en'%A3-rates.csv").as_deref(),
            Some("£-rates.csv")
        );
    }

    /// Guessing a charset produces a name that is wrong in a way nobody can see, so an unknown
    /// one falls back to the plain parameter instead.
    #[test]
    fn an_unknown_charset_falls_back_rather_than_guessing() {
        let header = "attachment; filename=\"safe.txt\"; filename*=shift_jis''%82%A0.txt";
        assert_eq!(filename(header).as_deref(), Some("safe.txt"));
    }

    /// A quoted value may contain the very character a naive `split(';')` would cut on.
    #[test]
    fn a_semicolon_inside_quotes_does_not_split_the_parameter() {
        assert_eq!(
            filename(r#"attachment; filename="a;b.txt""#).as_deref(),
            Some("a;b.txt")
        );
    }

    #[test]
    fn a_backslash_escape_inside_quotes_is_unescaped() {
        assert_eq!(
            filename(r#"attachment; filename="quote\"d.txt""#).as_deref(),
            Some("quoted.txt"),
            "the quote itself is stripped as a filesystem-special character"
        );
    }

    /// **The reason this module treats the header as hostile.** It is remote input whose entire
    /// purpose is to become a path.
    #[test]
    fn a_traversal_attempt_degrades_to_a_harmless_name() {
        for (header, expected) in [
            (r#"attachment; filename="../../.ssh/config""#, "config"),
            (r#"attachment; filename="/etc/passwd""#, "passwd"),
            (r#"attachment; filename="..\\..\\windows\\system32\\evil.dll""#, "evil.dll"),
            (r#"attachment; filename="C:\\Users\\me\\secret.txt""#, "secret.txt"),
        ] {
            let got = filename(header);
            assert_eq!(got.as_deref(), Some(expected), "{header}");
            let got = got.unwrap();
            assert!(!got.contains('/') && !got.contains('\\'), "{got:?}");
        }
    }

    /// Degrading rather than rejecting is deliberate: a rejected header falls back to the
    /// URL-derived label, which hides that the server said anything at all.
    #[test]
    fn a_name_that_is_only_dots_or_separators_names_nothing() {
        assert_eq!(filename(r#"attachment; filename="..""#), None);
        assert_eq!(filename(r#"attachment; filename=".""#), None);
        assert_eq!(filename(r#"attachment; filename="/""#), None);
        assert_eq!(filename(r#"attachment; filename="""#), None);
        assert_eq!(filename(r#"attachment; filename="   ""#), None);
    }

    #[test]
    fn a_leading_dot_cannot_make_a_hidden_file() {
        assert_eq!(
            filename(r#"attachment; filename=".bashrc""#).as_deref(),
            Some("bashrc")
        );
    }

    #[test]
    fn control_characters_are_removed() {
        let got = filename("attachment; filename=\"re\nport\t.csv\"").expect("a name");
        assert_eq!(got, "report.csv");
        assert!(!got.chars().any(char::is_control), "{got:?}");
    }

    #[test]
    fn an_absurdly_long_name_is_cut_to_something_writable() {
        let long = "a".repeat(500);
        let got = filename(&format!("attachment; filename=\"{long}.csv\"")).expect("a name");
        assert!(got.len() <= MAX_LEN, "{} chars", got.len());
    }

    #[test]
    fn the_parameter_name_is_case_insensitive() {
        assert_eq!(
            filename(r#"Attachment; FileName="a.txt""#).as_deref(),
            Some("a.txt")
        );
    }
}
