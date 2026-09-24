//! PHP, with the curl extension — what nearly every PHP HTTP client is built on, and present on
//! almost every install.
//!
//! **Every literal is single-quoted**: in a double-quoted PHP string `$name` expands a variable,
//! so a body carrying one would change when pasted — see `single_quoted`.

use super::{PartValue, Wire, WireBody, single_quoted};

pub(super) fn render(wire: &Wire) -> String {
    let mut options: Vec<String> = Vec::new();

    // HEAD needs `NOBODY`, or curl waits for a body that never comes; GET is the default.
    match wire.method.as_str() {
        "GET" => {}
        "HEAD" => options.push("CURLOPT_NOBODY => true".to_string()),
        method => options.push(format!("CURLOPT_CUSTOMREQUEST => {}", single_quoted(method))),
    }

    // libcurl's header rules, which are curl's: see `curl::header`, and the file-body case where
    // an empty `Content-Type:` stops libcurl labelling the body a form.
    let mut rows: Vec<String> = wire
        .merged_headers()
        .iter()
        .map(|(name, value)| format!("        {},", single_quoted(&super::curl::header(name, value))))
        .collect();
    if matches!(wire.body, WireBody::File(_)) && !super::curl::has_content_type(wire) {
        rows.push(format!("        {},", single_quoted("Content-Type:")));
    }
    if !rows.is_empty() {
        options.push(format!("CURLOPT_HTTPHEADER => [\n{}\n    ]", rows.join("\n")));
    }

    match &wire.body {
        WireBody::None => {}
        WireBody::Text(text) => options.push(format!("CURLOPT_POSTFIELDS => {}", single_quoted(text))),
        WireBody::File(path) => options.push(format!(
            "CURLOPT_POSTFIELDS => file_get_contents({})",
            single_quoted(&path.display().to_string())
        )),
        // An array makes curl send multipart, writing its own boundary.
        WireBody::Multipart(parts) => {
            let rows: Vec<String> = parts
                .iter()
                .map(|part| {
                    let name = single_quoted(&part.name);
                    match &part.value {
                        PartValue::Text(text) => format!("        {name} => {},", single_quoted(text)),
                        PartValue::File(path) => format!(
                            "        {name} => new CURLFile({}, 'application/octet-stream', {}),",
                            single_quoted(&path.display().to_string()),
                            single_quoted(&PartValue::filename(path))
                        ),
                    }
                })
                .collect();
            options.push(format!("CURLOPT_POSTFIELDS => [\n{}\n    ]", rows.join("\n")));
        }
    }

    options.push("CURLOPT_RETURNTRANSFER => true".to_string());
    if wire.follow_redirects {
        options.push("CURLOPT_FOLLOWLOCATION => true".to_string());
    }
    // An empty string means "every encoding curl supports", and decode it.
    if wire.compressed {
        options.push("CURLOPT_ENCODING => ''".to_string());
    }
    if !wire.verify_tls {
        options.push("CURLOPT_SSL_VERIFYPEER => false".to_string());
        options.push("CURLOPT_SSL_VERIFYHOST => 0".to_string());
    }
    if let Some(timeout) = wire.timeout {
        options.push(format!("CURLOPT_TIMEOUT_MS => {}", timeout.as_millis()));
    }

    let mut out = format!("<?php\n\n$ch = curl_init({});\n", single_quoted(&wire.url));
    out.push_str("curl_setopt_array($ch, [\n");
    for option in options {
        out.push_str(&format!("    {option},\n"));
    }
    out.push_str(
        "]);\n\n$response = curl_exec($ch);\n\
         echo curl_getinfo($ch, CURLINFO_HTTP_CODE), \"\\n\";\n\
         echo $response, \"\\n\";\n\
         curl_close($ch);\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_post_with_a_variable_like_body_stays_literal() {
        let wire = Wire {
            method: "POST".into(),
            url: "https://api.test/items".into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: WireBody::Text(r#"{"price":"$amount"}"#.into()),
            follow_redirects: false,
            verify_tls: true,
            compressed: false,
            timeout: None,
        };
        assert_eq!(
            render(&wire),
            r#"<?php

$ch = curl_init('https://api.test/items');
curl_setopt_array($ch, [
    CURLOPT_CUSTOMREQUEST => 'POST',
    CURLOPT_HTTPHEADER => [
        'Content-Type: application/json',
    ],
    CURLOPT_POSTFIELDS => '{"price":"$amount"}',
    CURLOPT_RETURNTRANSFER => true,
]);

$response = curl_exec($ch);
echo curl_getinfo($ch, CURLINFO_HTTP_CODE), "\n";
echo $response, "\n";
curl_close($ch);
"#
        );
    }
}
