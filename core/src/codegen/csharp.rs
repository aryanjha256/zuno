//! C#, with `HttpClient` — top-level statements, so it is a whole `Program.cs`.
//!
//! **Content headers go on the content, and that split is the trap here.** `HttpClient` keeps
//! `Content-Type` and its siblings on `HttpContent.Headers`, and `TryAddWithoutValidation` on the
//! *request's* headers returns `false` for one — the header silently never leaves. So each header
//! is routed by name.

use super::{PartValue, Wire, WireBody, string_literal};

/// The headers .NET files under `HttpContent.Headers`.
const CONTENT_HEADERS: [&str; 10] = [
    "allow",
    "content-disposition",
    "content-encoding",
    "content-language",
    "content-length",
    "content-location",
    "content-md5",
    "content-range",
    "content-type",
    "expires",
];

fn is_content_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    CONTENT_HEADERS.contains(&lower.as_str()) || lower == "last-modified"
}

pub(super) fn render(wire: &Wire) -> String {
    let mut out = String::from(
        "using System;\nusing System.IO;\nusing System.Net;\nusing System.Net.Http;\nusing System.Text;\n\n",
    );

    let mut handler: Vec<&str> = Vec::new();
    // HttpClientHandler follows redirects by default; only not following needs saying.
    if !wire.follow_redirects {
        handler.push("    AllowAutoRedirect = false,");
    }
    if wire.compressed {
        handler.push("    AutomaticDecompression = DecompressionMethods.All,");
    }
    if !wire.verify_tls {
        handler.push(
            "    ServerCertificateCustomValidationCallback =\n        \
             HttpClientHandler.DangerousAcceptAnyServerCertificateValidator,",
        );
    }
    if handler.is_empty() {
        out.push_str("using var client = new HttpClient();\n");
    } else {
        out.push_str(&format!(
            "var handler = new HttpClientHandler\n{{\n{}\n}};\nusing var client = new HttpClient(handler);\n",
            handler.join("\n")
        ));
    }
    if let Some(timeout) = wire.timeout {
        out.push_str(&format!(
            "client.Timeout = TimeSpan.FromMilliseconds({});\n",
            timeout.as_millis()
        ));
    }

    out.push_str(&format!(
        "\nusing var request = new HttpRequestMessage(new HttpMethod({}), {});\n",
        string_literal(&wire.method),
        string_literal(&wire.url)
    ));

    let (content_headers, request_headers): (Vec<_>, Vec<_>) =
        wire.headers.iter().partition(|(name, _)| is_content_header(name));
    for (name, value) in &request_headers {
        out.push_str(&format!(
            "request.Headers.TryAddWithoutValidation({}, {});\n",
            string_literal(name),
            string_literal(value)
        ));
    }

    // `ByteArrayContent` rather than `StringContent`, which labels itself
    // `text/plain; charset=utf-8` — a type Zuno would not have sent.
    let content = match &wire.body {
        WireBody::None => {
            // A content header on a request with no body still needs content to hang from.
            (!content_headers.is_empty()).then(|| "new ByteArrayContent(Array.Empty<byte>())".to_string())
        }
        WireBody::Text(text) => Some(format!(
            "new ByteArrayContent(Encoding.UTF8.GetBytes({}))",
            string_literal(text)
        )),
        WireBody::File(path) => Some(format!(
            "new ByteArrayContent(File.ReadAllBytes({}))",
            string_literal(&path.display().to_string())
        )),
        WireBody::Multipart(parts) => {
            out.push_str("\nvar form = new MultipartFormDataContent();\n");
            for part in parts {
                let name = string_literal(&part.name);
                match &part.value {
                    PartValue::Text(text) => out.push_str(&format!(
                        "form.Add(new StringContent({}), {name});\n",
                        string_literal(text)
                    )),
                    PartValue::File(path) => out.push_str(&format!(
                        "form.Add(new ByteArrayContent(File.ReadAllBytes({})), {name}, {});\n",
                        string_literal(&path.display().to_string()),
                        string_literal(&PartValue::filename(path))
                    )),
                }
            }
            Some("form".to_string())
        }
    };
    if let Some(content) = content {
        out.push_str(&format!("request.Content = {content};\n"));
        for (name, value) in &content_headers {
            out.push_str(&format!(
                "request.Content.Headers.TryAddWithoutValidation({}, {});\n",
                string_literal(name),
                string_literal(value)
            ));
        }
    }

    out.push_str(
        "\nusing var response = await client.SendAsync(request);\n\
         Console.WriteLine((int)response.StatusCode);\n\
         Console.WriteLine(await response.Content.ReadAsStringAsync());\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_type_goes_on_the_content_not_the_request() {
        let wire = Wire {
            method: "POST".into(),
            url: "https://api.test/items".into(),
            headers: vec![
                ("X-Trace".into(), "abc".into()),
                ("Content-Type".into(), "application/json".into()),
            ],
            body: WireBody::Text("{}".into()),
            follow_redirects: true,
            verify_tls: true,
            compressed: false,
            timeout: None,
        };
        let out = render(&wire);
        assert!(
            out.contains("request.Headers.TryAddWithoutValidation(\"X-Trace\", \"abc\");"),
            "{out}"
        );
        assert!(
            out.contains(
                "request.Content.Headers.TryAddWithoutValidation(\"Content-Type\", \"application/json\");"
            ),
            "on the request's own headers it is silently dropped: {out}"
        );
        assert!(!out.contains("request.Headers.TryAddWithoutValidation(\"Content-Type\""), "{out}");
    }
}
