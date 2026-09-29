//! An HTTPie command line: `http POST url Header:value`.
//!
//! **HTTPie defaults to JSON, and that shapes three choices here.** A bare `name=value` item
//! becomes a JSON field, so a text body goes through `--raw` instead and arrives byte for byte; a
//! body read from stdin is labelled `application/json` unless told otherwise, so a binary upload
//! with no type carries an empty `Content-Type:` — HTTPie's syntax for "send none", which is what
//! Zuno sends; and multipart needs `--multipart` to turn items into form fields.

use super::{PartValue, Shell, Wire, WireBody};

pub(super) fn render(wire: &Wire, shell: Shell) -> String {
    let quote = |text: &str| shell.quote(text);
    let mut parts: Vec<String> = vec![format!("http {} {}", wire.method, quote(&wire.url))];

    // HTTPie follows nothing and verifies by default — the opposite of Zuno on the first.
    if wire.follow_redirects {
        parts.push("--follow".to_string());
    }
    if !wire.verify_tls {
        parts.push("--verify=no".to_string());
    }
    if let Some(timeout) = wire.timeout {
        parts.push(format!("--timeout={}", timeout.as_secs()));
    }
    match &wire.body {
        WireBody::Text(text) => parts.push(format!("--raw {}", quote(text))),
        WireBody::Multipart(_) => parts.push("--multipart".to_string()),
        WireBody::None | WireBody::File(_) => {}
    }

    for (name, value) in &wire.headers {
        // `Name:` with nothing after it *removes* a header in HTTPie; `Name;` is how an empty
        // value is sent.
        let item = if value.is_empty() {
            format!("{name};")
        } else {
            format!("{name}:{value}")
        };
        parts.push(quote(&item));
    }

    match &wire.body {
        WireBody::File(path) => {
            if !wire
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            {
                parts.push(quote("Content-Type:"));
            }
            // **PowerShell has no `<`** — it is a reserved operator there — so the body file is
            // HTTPie's own `@path` item instead, which reads it as the raw body. The empty
            // `Content-Type:` above still wins over the type `@` would guess from the extension.
            let path = path.display().to_string();
            parts.push(match shell {
                Shell::Posix => format!("< {}", quote(&path)),
                Shell::PowerShell => quote(&format!("@{path}")),
            });
        }
        WireBody::Multipart(fields) => {
            for part in fields {
                let name = escape_item_name(&part.name);
                let item = match &part.value {
                    PartValue::Text(text) => format!("{name}={text}"),
                    PartValue::File(path) => format!("{name}@{}", path.display()),
                };
                parts.push(quote(&item));
            }
        }
        WireBody::None | WireBody::Text(_) => {}
    }

    shell.join(&parts)
}

/// Backslash the characters HTTPie reads as item separators, so a part name containing one is
/// not split at it. The earliest separator in an item wins, so an unescaped `:` in a name would
/// turn a form field into a header.
fn escape_item_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if matches!(ch, ':' | '=' | '@' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wire(body: WireBody, headers: Vec<(String, String)>) -> Wire {
        Wire {
            method: "POST".into(),
            url: "https://api.test/items".into(),
            headers,
            body,
            follow_redirects: true,
            verify_tls: true,
            compressed: true,
            timeout: None,
        }
    }

    #[test]
    fn a_text_body_goes_raw_and_headers_are_items() {
        let out = render(
            &wire(
                WireBody::Text(r#"{"name":"ada"}"#.into()),
                vec![
                    ("X-Empty".into(), String::new()),
                    ("Content-Type".into(), "application/json".into()),
                ],
            ),
            Shell::Posix,
        );
        assert_eq!(
            out,
            "http POST 'https://api.test/items' \\\n  --follow \\\n  --raw '{\"name\":\"ada\"}' \\\n  \
             'X-Empty;' \\\n  'Content-Type:application/json'"
        );
    }

    #[test]
    fn a_binary_body_is_stdin_with_no_invented_type() {
        let body = || wire(WireBody::File(PathBuf::from("/tmp/blob.bin")), vec![]);
        let out = render(&body(), Shell::Posix);
        assert!(out.ends_with("'Content-Type:' \\\n  < '/tmp/blob.bin'"), "{out}");
        // PowerShell reserves `<`, so the file is HTTPie's own `@` item there.
        let out = render(&body(), Shell::PowerShell);
        assert!(out.ends_with("'Content-Type:' `\n  '@/tmp/blob.bin'"), "{out}");
    }

    #[test]
    fn a_part_name_cannot_become_a_header() {
        let out = render(
            &wire(
                WireBody::Multipart(vec![super::super::Part {
                    name: "a:b".into(),
                    value: PartValue::Text("1".into()),
                }]),
                vec![],
            ),
            Shell::Posix,
        );
        assert!(out.contains(r"'a\:b=1'"), "{out}");
    }
}
