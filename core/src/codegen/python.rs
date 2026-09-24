//! Python, with `requests`.
//!
//! **`requests.request(method, …)` rather than `requests.post(…)`**, because a custom verb has no
//! function of its own and one shape for every method is one thing to read.

use super::{PartValue, Wire, WireBody, string_literal};

pub(super) fn render(wire: &Wire) -> String {
    let mut args: Vec<String> = vec![string_literal(&wire.method), string_literal(&wire.url)];

    // A dict, so a repeated name is joined rather than dropped — see `Wire::merged_headers`.
    let headers = wire.merged_headers();
    if !headers.is_empty() {
        let rows: Vec<String> = headers
            .iter()
            .map(|(name, value)| format!("        {}: {},", string_literal(name), string_literal(value)))
            .collect();
        args.push(format!("headers={{\n{}\n    }}", rows.join("\n")));
    }

    match &wire.body {
        WireBody::None => {}
        // **Encoded here, not handed over as a `str`.** `http.client` encodes a text body as
        // ISO-8859-1, so a JSON body with one non-Latin character would raise before sending.
        WireBody::Text(text) => args.push(format!("data={}.encode(\"utf-8\")", string_literal(text))),
        WireBody::File(path) => args.push(format!(
            "data=open({}, \"rb\")",
            string_literal(&path.display().to_string())
        )),
        // A list of pairs rather than a dict, so the parts keep their order and a name may
        // repeat. `(None, value)` is how `requests` says "a field, not a file".
        WireBody::Multipart(parts) => {
            let rows: Vec<String> = parts
                .iter()
                .map(|part| {
                    let name = string_literal(&part.name);
                    match &part.value {
                        PartValue::Text(text) => {
                            format!("        ({name}, (None, {})),", string_literal(text))
                        }
                        PartValue::File(path) => format!(
                            "        ({name}, ({}, open({}, \"rb\"))),",
                            string_literal(&PartValue::filename(path)),
                            string_literal(&path.display().to_string())
                        ),
                    }
                })
                .collect();
            args.push(format!("files=[\n{}\n    ]", rows.join("\n")));
        }
    }

    // `requests` follows redirects and verifies by default; only the opposites need saying.
    if !wire.follow_redirects {
        args.push("allow_redirects=False".to_string());
    }
    if !wire.verify_tls {
        args.push("verify=False".to_string());
    }
    if let Some(timeout) = wire.timeout {
        args.push(format!("timeout={}", timeout.as_secs_f64()));
    }

    let mut out = String::from("import requests\n\nresponse = requests.request(\n");
    for arg in args {
        out.push_str(&format!("    {arg},\n"));
    }
    out.push_str(")\n\nprint(response.status_code)\nprint(response.text)\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_multipart_upload_keeps_its_order_and_filename() {
        let wire = Wire {
            method: "POST".into(),
            url: "https://api.test/upload".into(),
            headers: vec![("X-Trace".into(), "abc".into())],
            body: WireBody::Multipart(vec![
                super::super::Part {
                    name: "caption".into(),
                    value: PartValue::Text("a photo".into()),
                },
                super::super::Part {
                    name: "file".into(),
                    value: PartValue::File(PathBuf::from("/tmp/pic.png")),
                },
            ]),
            follow_redirects: false,
            verify_tls: true,
            compressed: true,
            timeout: Some(std::time::Duration::from_secs(5)),
        };
        assert_eq!(
            render(&wire),
            r#"import requests

response = requests.request(
    "POST",
    "https://api.test/upload",
    headers={
        "X-Trace": "abc",
    },
    files=[
        ("caption", (None, "a photo")),
        ("file", ("pic.png", open("/tmp/pic.png", "rb"))),
    ],
    allow_redirects=False,
    timeout=5,
)

print(response.status_code)
print(response.text)
"#
        );
    }
}
