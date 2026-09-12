//! Pulling the readable text out of an HTML response.
//!
//! **This is extraction, not rendering, and the distinction is the whole scope.** Nothing here
//! lays anything out: no CSS, no boxes, no images. What it answers is "what does this page
//! *say*", which is the question an API client is actually holding when a framework returns a
//! 500 page instead of JSON — the traceback is in there, under 40KB of markup. "What does it
//! *look* like" is a different question and belongs in a real browser, which is why the pane
//! offers a button rather than a renderer.
//!
//! **`html2text` does the work, and `nanohtml2text` was tried first and rejected.** The small
//! crate is one dependency to this one's thirty and twelve times faster, and it passes `<pre>`
//! contents through **raw** — entities undecoded, nested tags left as literal markup. Every
//! framework worth naming puts its traceback in a `<pre>`, so the cheap option was broken in
//! precisely the case it was being bought for. That is not a judgement call: swapping the crate
//! back makes `a_framework_error_page_reduces_to_its_message` fail on the entity.
//!
//! `TrivialDecorator` and `no_table_borders` turn off the half of `html2text` we do not want.
//! Its default output is shaped for a terminal — `#` before a heading, backticks around `<code>`,
//! box-drawing rules around tables — and decoration invented by the viewer is indistinguishable
//! from decoration that was in the response, which is the one thing a debugging tool must not do.

use bytes::Bytes;

/// Above this, the body is left as markup.
///
/// Lower than the JSON viewer's `MAX_AUTO_PARSE` because the work is dearer: `html2text` builds
/// a real document with html5ever rather than scanning, measured at roughly **100ms per
/// megabyte**, so this cap is about a fifth of a second on a background thread. It is also the
/// point past which the feature has stopped being itself — an error page is kilobytes, and a
/// multi-megabyte HTML body is not a thing anyone is reading for a message.
pub const MAX_EXTRACT_BYTES: usize = 2 * 1024 * 1024;

/// Column to wrap at.
///
/// `html2text` must be given a width; there is no "do not wrap". Fixed rather than taken from
/// the pane, because the pane is resizable and re-extracting on every drag would put a
/// hundred-millisecond job behind a live gesture. 120 is chosen to sit above a typical traceback
/// line, so the lines that matter most come through unwrapped, and the viewer's own horizontal
/// scroll covers whatever still runs long.
const WRAP_COLUMNS: usize = 120;

/// Content-Type first, then sniff — the mirror of `looks_like_json`.
///
/// Sniffing is deliberately **narrow** here, where the JSON check is broad. A body opening with
/// `<` is as likely to be XML, SVG or an RSS feed as HTML, and running those through a text
/// extractor throws away the only thing in them worth reading. So a bare `<` is not enough: the
/// document has to announce itself.
pub fn looks_like_html(body: &[u8], content_type: Option<&str>) -> bool {
    if let Some(content_type) = content_type {
        let content_type = content_type.to_ascii_lowercase();
        if content_type.contains("html") {
            return true;
        }
        // An explicit non-HTML type is respected, exactly as the JSON side respects one.
        if content_type.contains("json")
            || content_type.contains("xml")
            || content_type.starts_with("image/")
            || content_type.starts_with("audio/")
            || content_type.starts_with("video/")
            || content_type.contains("javascript")
            || content_type.contains("css")
        {
            return false;
        }
    }

    let head = &body[..body.len().min(512)];
    let Ok(head) = std::str::from_utf8(head) else {
        // A truncated multi-byte character at the 512-byte edge is not evidence of anything, so
        // fall back to the bytes that are definitely whole rather than to a guess.
        let cut = head
            .iter()
            .rposition(|byte| byte.is_ascii())
            .map(|ix| ix + 1)
            .unwrap_or(0);
        return std::str::from_utf8(&head[..cut])
            .is_ok_and(|head| announces_html(head));
    };
    announces_html(head)
}

fn announces_html(head: &str) -> bool {
    let head = head.trim_start().to_ascii_lowercase();
    head.starts_with("<!doctype html")
        || head.starts_with("<html")
        || head.starts_with("<head")
        || head.starts_with("<body")
}

/// The text of an HTML document, or `None` if it is too large or not UTF-8.
///
/// `None` rather than a best-effort string: a partial extraction of a page you are reading to
/// find out what went wrong is worse than the markup, because it looks complete.
pub fn to_text(body: &Bytes) -> Option<String> {
    if body.len() > MAX_EXTRACT_BYTES {
        return None;
    }
    // Validated rather than lossily converted: `html2text` would accept the bytes either way,
    // and a page of replacement characters is not a readable page.
    std::str::from_utf8(body).ok()?;
    let text = html2text::config::with_decorator(html2text::render::TrivialDecorator::new())
        .no_table_borders()
        .string_from_read(body.as_ref(), WRAP_COLUMNS)
        .ok()?;

    // A blank line per block element means a page with any structure comes out double- and
    // triple-spaced, and on a viewer drawing one row per line that is most of the screen spent on
    // nothing. Runs collapse to a single separator, and the leading and trailing ones go.
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0usize;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blanks > 0 {
                out.push('\n');
            }
        }
        blanks = 0;
        out.push_str(line);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DJANGO: &str = "<!DOCTYPE html><html><head><title>ValueError at /api/users/</title>\
<style>body{font-family:sans-serif}</style><script>var x=1;</script></head><body>\
<h1>ValueError at /api/users/</h1>\
<pre class=\"exception_value\">invalid literal for int() with base 10: &#39;abc&#39;</pre>\
</body></html>";

    #[test]
    fn a_framework_error_page_reduces_to_its_message() {
        let text = to_text(&Bytes::from_static(DJANGO.as_bytes())).expect("extracted");
        assert!(text.contains("ValueError at /api/users/"), "{text:?}");
        assert!(
            text.contains("invalid literal for int() with base 10: 'abc'"),
            "the entity should be decoded: {text:?}"
        );
    }

    /// **The test that chose the crate.** `<pre>` is where Django, Flask and Rails each put the
    /// traceback, and `nanohtml2text` passes its contents through untouched — entities raw,
    /// nested tags literal, and no separation from the block before it. A traceback is also the
    /// one place where *indentation is the structure*, so an extractor that collapses it has
    /// removed the thing being read for.
    #[test]
    fn a_traceback_in_a_pre_block_keeps_its_text_and_its_indentation() {
        let html = "<html><body><h1>ValueError</h1>\
<pre>Traceback (most recent call last):\n  File &quot;views.py&quot;, line 42, in get\n    \
return int(pk)\nValueError: invalid literal</pre></body></html>";

        let text = to_text(&Bytes::from_static(html.as_bytes())).expect("extracted");

        assert!(
            text.contains(r#"File "views.py", line 42, in get"#),
            "entities inside <pre> must be decoded: {text:?}"
        );
        assert!(
            text.contains("\n  File"),
            "the frame's two-space indent is its structure: {text:?}"
        );
        assert!(
            text.contains("\n    return int(pk)"),
            "and the source line's four-space indent: {text:?}"
        );
        assert!(
            !text.contains("ValueErrorTraceback"),
            "the heading must not run into the block after it: {text:?}"
        );
    }

    /// The viewer draws what it is given, so anything the extractor invents is indistinguishable
    /// from something the server sent — which for a debugging tool is the worst kind of wrong.
    #[test]
    fn nothing_is_decorated_with_terminal_markup() {
        let html = "<html><body><h1>Heading</h1><p>text with <code>inline()</code></p>\
<table><tr><td>a</td><td>b</td></tr></table></body></html>";

        let text = to_text(&Bytes::from_static(html.as_bytes())).expect("extracted");

        assert!(!text.contains('#'), "no markdown heading marker: {text:?}");
        assert!(!text.contains('`'), "no backticks around code: {text:?}");
        assert!(
            !text.chars().any(|c| matches!(c, '\u{2500}'..='\u{257F}')),
            "no box-drawing table rules: {text:?}"
        );
        assert!(text.contains("Heading") && text.contains("inline()"), "{text:?}");
    }

    /// The contents of `<script>` and `<style>` are not prose — leaving them in buries the
    /// message under the very thing this exists to remove.
    #[test]
    fn script_and_style_contents_are_dropped() {
        let text = to_text(&Bytes::from_static(DJANGO.as_bytes())).expect("extracted");
        assert!(!text.contains("font-family"), "css leaked: {text:?}");
        assert!(!text.contains("var x"), "js leaked: {text:?}");
    }

    /// One row per line in the viewer, so a run of blank lines is a run of blank rows.
    #[test]
    fn blank_runs_are_collapsed_to_one() {
        let html = "<p>one</p><div></div><div></div><div></div><p>two</p>";
        let text = to_text(&Bytes::from_static(html.as_bytes())).expect("extracted");
        assert!(
            !text.contains("\n\n\n"),
            "no more than one blank line should survive: {text:?}"
        );
        assert!(text.contains("one") && text.contains("two"), "{text:?}");
    }

    #[test]
    fn there_is_no_leading_or_trailing_blank_line() {
        let html = "<html><body>\n\n  <p>hello</p>\n\n</body></html>";
        let text = to_text(&Bytes::from_static(html.as_bytes())).expect("extracted");
        assert_eq!(text, "hello", "got {text:?}");
    }

    #[test]
    fn an_oversized_body_is_refused_rather_than_half_extracted() {
        let big = Bytes::from(format!("<p>{}</p>", "x".repeat(MAX_EXTRACT_BYTES)));
        assert!(to_text(&big).is_none());
    }

    #[test]
    fn a_body_that_is_not_utf8_is_refused() {
        assert!(to_text(&Bytes::from_static(&[0xff, 0xfe, b'<'])).is_none());
    }

    #[test]
    fn the_content_type_decides_when_it_says_anything() {
        assert!(looks_like_html(b"whatever", Some("text/html; charset=utf-8")));
        assert!(!looks_like_html(DJANGO.as_bytes(), Some("application/json")));
        assert!(!looks_like_html(b"<rss><channel/></rss>", Some("application/xml")));
    }

    /// The sniff is narrow on purpose: `<` alone is as likely to open XML, SVG or RSS, and
    /// extracting text from those discards the only content they have.
    #[test]
    fn a_bare_angle_bracket_is_not_enough_to_sniff_html() {
        assert!(!looks_like_html(b"<rss version=\"2.0\"><channel/></rss>", None));
        assert!(!looks_like_html(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>", None));
        assert!(!looks_like_html(b"<?xml version=\"1.0\"?><note/>", None));

        assert!(looks_like_html(b"<!DOCTYPE html><html></html>", None));
        assert!(looks_like_html(b"\n  <html lang=\"en\">", None));
        assert!(looks_like_html(b"<HTML>", None), "the tag name is case-insensitive");
    }

    /// A multi-byte character straddling the 512-byte sniff window must not be read as evidence.
    #[test]
    fn a_split_character_at_the_sniff_edge_does_not_panic() {
        let mut body = b"<!DOCTYPE html><p>".to_vec();
        while body.len() < 511 {
            body.push(b'x');
        }
        body.extend_from_slice("é".as_bytes());
        assert!(looks_like_html(&body, None));
    }
}
