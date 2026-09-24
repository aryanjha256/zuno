//! Java, with `java.net.http.HttpClient` (Java 11+) and nothing outside the JDK.
//!
//! One file with a `main`, so it runs as `java Main.java` with no build.
//!
//! **Two things the JDK does not do, written out instead of dropped.** `HttpClient` has no
//! multipart builder, so a multipart body is assembled by hand around a generated boundary; and it
//! has no switch for skipping certificate checks, so an unverified request carries a trust-all
//! `SSLContext`. Dropping either would be a snippet that sends something other than what Zuno
//! sends.

use super::{PartValue, Wire, WireBody, string_literal};

/// Headers `HttpClient` refuses to set, throwing `IllegalArgumentException` — checked against the
/// JDK itself rather than its documentation. It derives each of them from the request.
const RESTRICTED: [&str; 5] = ["connection", "content-length", "expect", "host", "upgrade"];

pub(super) fn render(wire: &Wire) -> String {
    let mut imports: Vec<&str> = vec![
        "java.net.URI",
        "java.net.http.HttpClient",
        "java.net.http.HttpRequest",
        "java.net.http.HttpResponse",
    ];
    let mut setup = String::new();

    let publisher = match &wire.body {
        WireBody::None => "HttpRequest.BodyPublishers.noBody()".to_string(),
        // `ofString` encodes as UTF-8, which is what Zuno sends.
        WireBody::Text(text) => format!("HttpRequest.BodyPublishers.ofString({})", string_literal(text)),
        WireBody::File(path) => {
            imports.push("java.nio.file.Path");
            format!(
                "HttpRequest.BodyPublishers.ofFile(Path.of({}))",
                string_literal(&path.display().to_string())
            )
        }
        WireBody::Multipart(parts) => {
            imports.extend([
                "java.io.ByteArrayOutputStream",
                "java.nio.charset.StandardCharsets",
                "java.util.UUID",
            ]);
            setup.push_str(
                "        // HttpClient has no multipart builder, so the body is written by hand.\n\
                 \x20       String boundary = \"zuno-\" + UUID.randomUUID();\n\
                 \x20       ByteArrayOutputStream body = new ByteArrayOutputStream();\n",
            );
            for part in parts {
                let disposition = match &part.value {
                    PartValue::Text(_) => {
                        format!("\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n", part.name)
                    }
                    PartValue::File(path) => format!(
                        "\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n\
                         Content-Type: application/octet-stream\r\n\r\n",
                        part.name,
                        PartValue::filename(path)
                    ),
                };
                setup.push_str(&format!(
                    "        body.write((\"--\" + boundary + {}).getBytes(StandardCharsets.UTF_8));\n",
                    string_literal(&disposition)
                ));
                match &part.value {
                    PartValue::Text(text) => setup.push_str(&format!(
                        "        body.write({}.getBytes(StandardCharsets.UTF_8));\n",
                        string_literal(text)
                    )),
                    PartValue::File(path) => {
                        for import in ["java.nio.file.Files", "java.nio.file.Path"] {
                            if !imports.contains(&import) {
                                imports.push(import);
                            }
                        }
                        setup.push_str(&format!(
                            "        body.write(Files.readAllBytes(Path.of({})));\n",
                            string_literal(&path.display().to_string())
                        ));
                    }
                }
                setup.push_str(
                    "        body.write(\"\\r\\n\".getBytes(StandardCharsets.UTF_8));\n",
                );
            }
            setup.push_str(
                "        body.write((\"--\" + boundary + \"--\\r\\n\").getBytes(StandardCharsets.UTF_8));\n\n",
            );
            "HttpRequest.BodyPublishers.ofByteArray(body.toByteArray())".to_string()
        }
    };

    let mut client = String::from("        HttpClient client = HttpClient.newBuilder()\n");
    // HttpClient's default is `NEVER`; Zuno's is to follow.
    if wire.follow_redirects {
        client.push_str("            .followRedirects(HttpClient.Redirect.NORMAL)\n");
    }
    if !wire.verify_tls {
        imports.extend([
            "java.security.SecureRandom",
            "java.security.cert.X509Certificate",
            "javax.net.ssl.SSLContext",
            "javax.net.ssl.TrustManager",
            "javax.net.ssl.X509TrustManager",
        ]);
        setup.push_str(
            "        // TLS verification is off for this request in Zuno. This skips the certificate\n\
             \x20       // check; the hostname is still checked unless the JVM is started with\n\
             \x20       // -Djdk.internal.httpclient.disableHostnameVerification=true.\n\
             \x20       SSLContext insecure = SSLContext.getInstance(\"TLS\");\n\
             \x20       insecure.init(null, new TrustManager[] { new X509TrustManager() {\n\
             \x20           public void checkClientTrusted(X509Certificate[] chain, String type) {}\n\
             \x20           public void checkServerTrusted(X509Certificate[] chain, String type) {}\n\
             \x20           public X509Certificate[] getAcceptedIssuers() { return new X509Certificate[0]; }\n\
             \x20       } }, new SecureRandom());\n\n",
        );
        client.push_str("            .sslContext(insecure)\n");
    }
    client.push_str("            .build();\n");

    let mut request = format!(
        "        HttpRequest request = HttpRequest.newBuilder()\n            .uri(URI.create({}))\n",
        string_literal(&wire.url)
    );
    let mut skipped: Vec<&str> = Vec::new();
    for (name, value) in &wire.headers {
        if RESTRICTED.contains(&name.to_ascii_lowercase().as_str()) {
            skipped.push(name);
            continue;
        }
        request.push_str(&format!(
            "            .header({}, {})\n",
            string_literal(name),
            string_literal(value)
        ));
    }
    if matches!(wire.body, WireBody::Multipart(_)) {
        request.push_str(
            "            .header(\"Content-Type\", \"multipart/form-data; boundary=\" + boundary)\n",
        );
    }
    request.push_str(&format!(
        "            .method({}, {publisher})\n",
        string_literal(&wire.method)
    ));
    if let Some(timeout) = wire.timeout {
        imports.push("java.time.Duration");
        request.push_str(&format!(
            "            .timeout(Duration.ofMillis({}))\n",
            timeout.as_millis()
        ));
    }
    request.push_str("            .build();\n");

    imports.sort_unstable();
    imports.dedup();
    let mut out = String::new();
    for import in &imports {
        out.push_str(&format!("import {import};\n"));
    }
    out.push_str("\npublic class Main {\n    public static void main(String[] args) throws Exception {\n");
    out.push_str(&setup);
    out.push_str(&client);
    out.push('\n');
    if !skipped.is_empty() {
        out.push_str(&format!(
            "        // Not set: HttpClient refuses {} and derives {} from the request.\n",
            skipped.join(", "),
            if skipped.len() == 1 { "it" } else { "them" }
        ));
    }
    out.push_str(&request);
    out.push_str(
        "\n        HttpResponse<String> response =\n\
         \x20           client.send(request, HttpResponse.BodyHandlers.ofString());\n\
         \x20       System.out.println(response.statusCode());\n\
         \x20       System.out.println(response.body());\n\
         \x20   }\n}\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_the_jdk_refuses_is_named_rather_than_set() {
        let wire = Wire {
            method: "GET".into(),
            url: "https://api.test/items".into(),
            headers: vec![("Host".into(), "other.test".into()), ("X-Kept".into(), "1".into())],
            body: WireBody::None,
            follow_redirects: true,
            verify_tls: true,
            compressed: true,
            timeout: None,
        };
        let out = render(&wire);
        assert!(!out.contains(".header(\"Host\""), "setting it throws at runtime: {out}");
        assert!(out.contains("HttpClient refuses Host"), "{out}");
        assert!(out.contains(".header(\"X-Kept\", \"1\")"), "{out}");
        assert!(out.contains(".followRedirects(HttpClient.Redirect.NORMAL)"), "{out}");
    }
}
