//! JavaScript `fetch`, as an ES module — top-level `await`, runnable with `node file.mjs` and
//! pasteable into a browser console.
//!
//! **Node-flavoured where a file is involved.** A browser cannot read a path off disk, so a
//! binary or multipart body imports `readFile` from `node:fs/promises`; everything else runs in
//! both.

use super::{PartValue, Wire, WireBody, string_literal};

pub(super) fn render(wire: &Wire) -> String {
    let mut out = String::new();
    let reads_files = match &wire.body {
        WireBody::File(_) => true,
        WireBody::Multipart(parts) => parts
            .iter()
            .any(|part| matches!(part.value, PartValue::File(_))),
        _ => false,
    };
    if reads_files {
        out.push_str("import { readFile } from \"node:fs/promises\";\n\n");
    }

    if let WireBody::Multipart(parts) = &wire.body {
        out.push_str("const form = new FormData();\n");
        for part in parts {
            let name = string_literal(&part.name);
            match &part.value {
                PartValue::Text(text) => {
                    out.push_str(&format!("form.append({name}, {});\n", string_literal(text)));
                }
                PartValue::File(path) => out.push_str(&format!(
                    "form.append({name}, new Blob([await readFile({})]), {});\n",
                    string_literal(&path.display().to_string()),
                    string_literal(&PartValue::filename(path)),
                )),
            }
        }
        out.push('\n');
    }

    if !wire.verify_tls {
        out.push_str(
            "// TLS verification is off for this request in Zuno. fetch has no per-request switch:\n\
             // Node needs NODE_TLS_REJECT_UNAUTHORIZED=0 in its environment, and a browser cannot\n\
             // do it at all.\n",
        );
    }

    let mut options: Vec<String> = Vec::new();
    if wire.method != "GET" {
        options.push(format!("method: {}", string_literal(&wire.method)));
    }
    if !wire.headers.is_empty() {
        options.push(format!("headers: {}", headers(wire)));
    }
    match &wire.body {
        WireBody::None => {}
        WireBody::Text(text) => options.push(format!("body: {}", string_literal(text))),
        WireBody::File(path) => options.push(format!(
            "body: await readFile({})",
            string_literal(&path.display().to_string())
        )),
        // fetch writes the boundary into the Content-Type itself — which is why `Wire` never
        // carries a typed one for a multipart body.
        WireBody::Multipart(_) => options.push("body: form".to_string()),
    }
    // fetch follows redirects by default, so only the opposite needs saying.
    if !wire.follow_redirects {
        options.push("redirect: \"manual\"".to_string());
    }
    if let Some(timeout) = wire.timeout {
        options.push(format!("signal: AbortSignal.timeout({})", timeout.as_millis()));
    }

    let url = string_literal(&wire.url);
    if options.is_empty() {
        out.push_str(&format!("const response = await fetch({url});\n"));
    } else {
        out.push_str(&format!("const response = await fetch({url}, {{\n"));
        for option in options {
            out.push_str(&format!("  {option},\n"));
        }
        out.push_str("});\n");
    }
    out.push_str("\nconsole.log(response.status);\nconsole.log(await response.text());\n");
    out
}

/// An object literal when every name is distinct, and a list of pairs when one repeats.
///
/// **Both are what `Headers` accepts**, and the object is what people write — but an object
/// cannot hold a name twice, and silently keeping only the last would drop a header Zuno sends.
fn headers(wire: &Wire) -> String {
    let repeats = wire.headers.iter().enumerate().any(|(ix, (name, _))| {
        wire.headers[..ix]
            .iter()
            .any(|(seen, _)| seen.eq_ignore_ascii_case(name))
    });

    let rows: Vec<String> = wire
        .headers
        .iter()
        .map(|(name, value)| {
            if repeats {
                format!("    [{}, {}],", string_literal(name), string_literal(value))
            } else {
                format!("    {}: {},", string_literal(name), string_literal(value))
            }
        })
        .collect();

    let (open, close) = if repeats { ("[", "]") } else { ("{", "}") };
    format!("{open}\n{}\n  {close}", rows.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_post_reads_the_way_it_is_written_by_hand() {
        let wire = Wire {
            method: "POST".into(),
            url: "https://api.test/items?page=2".into(),
            headers: vec![
                ("X-Trace".into(), "abc".into()),
                ("Content-Type".into(), "application/json".into()),
            ],
            body: WireBody::Text(r#"{"name":"ada"}"#.into()),
            follow_redirects: true,
            verify_tls: true,
            compressed: true,
            timeout: None,
        };
        assert_eq!(
            render(&wire),
            r#"const response = await fetch("https://api.test/items?page=2", {
  method: "POST",
  headers: {
    "X-Trace": "abc",
    "Content-Type": "application/json",
  },
  body: "{\"name\":\"ada\"}",
});

console.log(response.status);
console.log(await response.text());
"#
        );
    }

    #[test]
    fn a_repeated_header_is_kept_rather_than_overwritten() {
        let wire = Wire {
            method: "GET".into(),
            url: "https://x.test".into(),
            headers: vec![("Accept".into(), "a".into()), ("accept".into(), "b".into())],
            body: WireBody::None,
            follow_redirects: true,
            verify_tls: true,
            compressed: true,
            timeout: None,
        };
        let out = render(&wire);
        assert!(out.contains(r#"["Accept", "a"],"#) && out.contains(r#"["accept", "b"],"#), "{out}");
    }
}
