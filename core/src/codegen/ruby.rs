//! Ruby, with `Net::HTTP` from the standard library.
//!
//! **Every literal is single-quoted**: in a double-quoted Ruby string `#{...}` runs code, so a
//! JSON body carrying one would execute when pasted — see `single_quoted`.

use super::{PartValue, Wire, WireBody, single_quoted};

/// `Net::HTTP`'s class per verb; anything else goes through `HTTPGenericRequest`.
fn request_class(method: &str) -> Option<&'static str> {
    Some(match method {
        "GET" => "Get",
        "POST" => "Post",
        "PUT" => "Put",
        "PATCH" => "Patch",
        "DELETE" => "Delete",
        "HEAD" => "Head",
        "OPTIONS" => "Options",
        _ => return None,
    })
}

pub(super) fn render(wire: &Wire) -> String {
    let mut out = String::from("require 'net/http'\nrequire 'uri'\n\n");
    out.push_str(&format!("uri = URI({})\n", single_quoted(&wire.url)));
    match request_class(&wire.method) {
        Some(class) => out.push_str(&format!("request = Net::HTTP::{class}.new(uri)\n")),
        None => out.push_str(&format!(
            "request = Net::HTTPGenericRequest.new({}, {}, true, uri)\n",
            single_quoted(&wire.method),
            wire.has_body()
        )),
    }

    // `[]=` for the first of a name, which *replaces* Net::HTTP's own default for it; `add_field`
    // for a repeat, which appends — so a repeated header is sent twice, as Zuno sends it.
    let mut seen: Vec<String> = Vec::new();
    for (name, value) in &wire.headers {
        let lower = name.to_ascii_lowercase();
        if seen.contains(&lower) {
            out.push_str(&format!(
                "request.add_field({}, {})\n",
                single_quoted(name),
                single_quoted(value)
            ));
        } else {
            out.push_str(&format!("request[{}] = {}\n", single_quoted(name), single_quoted(value)));
            seen.push(lower);
        }
    }

    match &wire.body {
        WireBody::None => {}
        WireBody::Text(text) => out.push_str(&format!("request.body = {}\n", single_quoted(text))),
        WireBody::File(path) => {
            // Net::HTTP labels a body with no type as a form and warns about it. Zuno sends
            // none; there is no way to ask Net::HTTP for none, so the snippet says so.
            if !wire
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            {
                out.push_str(
                    "# Zuno sends this body with no Content-Type. Net::HTTP will label it\n\
                     # application/x-www-form-urlencoded unless you set one.\n",
                );
            }
            out.push_str(&format!(
                "request.body = File.binread({})\n",
                single_quoted(&path.display().to_string())
            ));
        }
        WireBody::Multipart(parts) => {
            let rows: Vec<String> = parts
                .iter()
                .map(|part| {
                    let name = single_quoted(&part.name);
                    match &part.value {
                        PartValue::Text(text) => format!("  [{name}, {}],", single_quoted(text)),
                        PartValue::File(path) => format!(
                            "  [{name}, File.open({}), {{ filename: {} }}],",
                            single_quoted(&path.display().to_string()),
                            single_quoted(&PartValue::filename(path))
                        ),
                    }
                })
                .collect();
            out.push_str(&format!(
                "request.set_form([\n{}\n], 'multipart/form-data')\n",
                rows.join("\n")
            ));
        }
    }

    out.push_str("\nhttp = Net::HTTP.new(uri.host, uri.port)\nhttp.use_ssl = uri.scheme == 'https'\n");
    if !wire.verify_tls {
        out.push_str("http.verify_mode = OpenSSL::SSL::VERIFY_NONE\n");
    }
    if let Some(timeout) = wire.timeout {
        let seconds = timeout.as_secs_f64();
        out.push_str(&format!("http.open_timeout = {seconds}\nhttp.read_timeout = {seconds}\n"));
    }
    if wire.follow_redirects {
        out.push_str(
            "# Zuno follows redirects for this request; Net::HTTP never does, so a 3xx comes back\n\
             # as the answer here.\n",
        );
    }
    out.push_str("\nresponse = http.request(request)\nputs response.code\nputs response.body\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_cannot_interpolate_and_a_repeated_header_appends() {
        let wire = Wire {
            method: "POST".into(),
            url: "https://api.test/items".into(),
            headers: vec![("Accept".into(), "a".into()), ("accept".into(), "b".into())],
            body: WireBody::Text("#{system('id')}".into()),
            follow_redirects: false,
            verify_tls: true,
            compressed: true,
            timeout: None,
        };
        let out = render(&wire);
        assert!(out.contains("request.body = '#{system(\\'id\\')}'"), "{out}");
        assert!(out.contains("request['Accept'] = 'a'"), "{out}");
        assert!(out.contains("request.add_field('accept', 'b')"), "{out}");
        assert!(out.contains("Net::HTTP::Post.new(uri)"), "{out}");
    }
}
