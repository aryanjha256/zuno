//! Go, with `net/http` and nothing outside the standard library.
//!
//! **The import list is computed, not fixed.** Go refuses to compile a file with an unused import,
//! so a snippet that always imported `os` and `mime/multipart` would fail on every plain GET.

use super::{PartValue, Wire, WireBody, string_literal};

pub(super) fn render(wire: &Wire) -> String {
    let mut imports: Vec<&str> = vec!["fmt", "io", "net/http"];
    let mut setup = String::new();
    let body_arg: String;

    match &wire.body {
        WireBody::None => body_arg = "nil".to_string(),
        WireBody::Text(text) => {
            imports.push("strings");
            body_arg = format!("strings.NewReader({})", string_literal(text));
        }
        // **Read whole rather than handed over as an `*os.File`.** `NewRequest` sets a length
        // only for in-memory readers, so a file would go out chunked where Zuno sends a
        // `Content-Length` — a difference some servers refuse.
        WireBody::File(path) => {
            imports.extend(["bytes", "os"]);
            setup.push_str(&format!(
                "\tdata, err := os.ReadFile({})\n\tif err != nil {{\n\t\tpanic(err)\n\t}}\n\n",
                string_literal(&path.display().to_string())
            ));
            body_arg = "bytes.NewReader(data)".to_string();
        }
        WireBody::Multipart(parts) => {
            imports.extend(["bytes", "mime/multipart"]);
            setup.push_str("\tvar body bytes.Buffer\n\tform := multipart.NewWriter(&body)\n");
            for part in parts {
                let name = string_literal(&part.name);
                match &part.value {
                    PartValue::Text(text) => setup.push_str(&format!(
                        "\tform.WriteField({name}, {})\n",
                        string_literal(text)
                    )),
                    // A block per file, so `data` and `part` can be declared again for the next
                    // one — `:=` twice in one scope is a compile error.
                    PartValue::File(path) => {
                        if !imports.contains(&"os") {
                            imports.push("os");
                        }
                        setup.push_str(&format!(
                            "\t{{\n\
                             \t\tdata, err := os.ReadFile({})\n\
                             \t\tif err != nil {{\n\t\t\tpanic(err)\n\t\t}}\n\
                             \t\tpart, err := form.CreateFormFile({name}, {})\n\
                             \t\tif err != nil {{\n\t\t\tpanic(err)\n\t\t}}\n\
                             \t\tpart.Write(data)\n\
                             \t}}\n",
                            string_literal(&path.display().to_string()),
                            string_literal(&PartValue::filename(path)),
                        ));
                    }
                }
            }
            setup.push_str("\tform.Close()\n\n");
            body_arg = "&body".to_string();
        }
    }

    let mut client_fields: Vec<String> = Vec::new();
    if let Some(timeout) = wire.timeout {
        imports.push("time");
        client_fields.push(format!("\t\tTimeout: {} * time.Millisecond,", timeout.as_millis()));
    }
    // Go follows redirects by default; not following needs a policy that stops at the first.
    if !wire.follow_redirects {
        client_fields.push(
            "\t\tCheckRedirect: func(req *http.Request, via []*http.Request) error {\n\
             \t\t\treturn http.ErrUseLastResponse\n\
             \t\t},"
                .to_string(),
        );
    }
    if !wire.verify_tls {
        imports.push("crypto/tls");
        client_fields.push(
            "\t\tTransport: &http.Transport{\n\
             \t\t\tTLSClientConfig: &tls.Config{InsecureSkipVerify: true},\n\
             \t\t},"
                .to_string(),
        );
    }

    imports.sort_unstable();
    let mut out = String::from("package main\n\nimport (\n");
    for import in &imports {
        out.push_str(&format!("\t\"{import}\"\n"));
    }
    out.push_str(")\n\nfunc main() {\n");
    out.push_str(&setup);
    out.push_str(&format!(
        "\treq, err := http.NewRequest({}, {}, {body_arg})\n\tif err != nil {{\n\t\tpanic(err)\n\t}}\n",
        string_literal(&wire.method),
        string_literal(&wire.url),
    ));
    // `Add`, not `Set`, so a repeated name is sent twice as Zuno sends it.
    for (name, value) in &wire.headers {
        out.push_str(&format!(
            "\treq.Header.Add({}, {})\n",
            string_literal(name),
            string_literal(value)
        ));
    }
    if matches!(wire.body, WireBody::Multipart(_)) {
        out.push_str("\treq.Header.Set(\"Content-Type\", form.FormDataContentType())\n");
    }

    if client_fields.is_empty() {
        out.push_str("\n\tclient := &http.Client{}\n");
    } else {
        out.push_str(&format!("\n\tclient := &http.Client{{\n{}\n\t}}\n", client_fields.join("\n")));
    }
    out.push_str(
        "\tres, err := client.Do(req)\n\
         \tif err != nil {\n\t\tpanic(err)\n\t}\n\
         \tdefer res.Body.Close()\n\n\
         \tout, err := io.ReadAll(res.Body)\n\
         \tif err != nil {\n\t\tpanic(err)\n\t}\n\
         \tfmt.Println(res.Status)\n\
         \tfmt.Println(string(out))\n\
         }\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_get_imports_only_what_it_uses() {
        let wire = Wire {
            method: "GET".into(),
            url: "https://api.test/items".into(),
            headers: vec![],
            body: WireBody::None,
            follow_redirects: true,
            verify_tls: true,
            compressed: true,
            timeout: None,
        };
        let out = render(&wire);
        assert!(out.starts_with(
            "package main\n\nimport (\n\t\"fmt\"\n\t\"io\"\n\t\"net/http\"\n)\n"
        ), "{out}");
        assert!(out.contains("http.NewRequest(\"GET\", \"https://api.test/items\", nil)"));
    }
}
