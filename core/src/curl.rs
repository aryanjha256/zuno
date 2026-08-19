//! Importing a `curl` command line into a `RequestSpec`.
//!
//! Every browser's devtools has "Copy as cURL", which makes this the fastest path from
//! *a request that exists* to *a request you can edit and replay*. Hand-written curl from
//! docs and READMEs works too.
//!
//! Two principles shape the parsing:
//!
//! 1. **Unknown flags are reported, not fatal.** curl has hundreds of options and most
//!    are about output (`-s`, `-o`, `-w`) and mean nothing on import. Refusing to import
//!    because of one unrecognised flag would make the feature brittle exactly where it's
//!    most useful, so anything unhandled comes back in `ignored` for the UI to mention.
//! 2. **Don't silently change what the request does.** The most important case is
//!    `-d` with no `Content-Type`: curl sends `application/x-www-form-urlencoded`, so the
//!    import adds that header explicitly. Without it, the imported request would send
//!    `text/plain` and behave differently from the command it came from.

use std::path::PathBuf;

use thiserror::Error;

use crate::request::{
    Body, Header, Method, MultipartField, MultipartValue, RawKind, RequestSpec,
};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CurlError {
    #[error("that doesn't look like a curl command")]
    NotCurl,

    #[error("no URL found in the command")]
    NoUrl,

    #[error("unbalanced {quote} quote")]
    UnbalancedQuote { quote: char },

    #[error("{flag} needs a value")]
    MissingValue { flag: String },
}

/// A successful import, plus anything that was recognised but deliberately dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurlImport {
    pub spec: RequestSpec,
    /// Flags that were skipped, in the order encountered. Shown to the user so an
    /// import never silently loses part of the command.
    pub ignored: Vec<String>,
}

/// Parse a curl command line.
pub fn parse(input: &str) -> Result<CurlImport, CurlError> {
    let tokens = tokenize(input)?;
    let mut tokens = tokens.into_iter().peekable();

    // Tolerate a copied shell prompt and the `curl` word itself. Some sources paste
    // `$ curl …`, and some paste only the arguments.
    let mut saw_curl = false;
    while let Some(first) = tokens.peek() {
        let first = first.trim();
        if first == "$" || first == "#" {
            tokens.next();
            continue;
        }
        if first.eq_ignore_ascii_case("curl") || first.ends_with("/curl") {
            tokens.next();
            saw_curl = true;
        }
        break;
    }

    let mut url: Option<String> = None;
    let mut method: Option<Method> = None;
    let mut headers: Vec<Header> = Vec::new();
    let mut data: Vec<String> = Vec::new();
    let mut form: Vec<MultipartField> = Vec::new();
    let mut binary_body: Option<PathBuf> = None;
    let mut ignored: Vec<String> = Vec::new();
    let mut explicit_content_type = false;
    let mut as_get = false;
    let mut spec = RequestSpec::default();

    while let Some(token) = tokens.next() {
        // A bare `-` is curl's stdin marker; nothing to import from it.
        if !token.starts_with('-') || token == "-" {
            if url.is_none() {
                url = Some(token);
            } else {
                ignored.push(token);
            }
            continue;
        }

        // Split `--flag=value` so both spellings work.
        let (flag, inline_value) = match token.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_string(), Some(value.to_string())),
            _ => (token.clone(), None),
        };

        let mut take_value = |flag: &str| -> Result<String, CurlError> {
            if let Some(value) = inline_value.clone() {
                return Ok(value);
            }
            tokens.next().ok_or_else(|| CurlError::MissingValue {
                flag: flag.to_string(),
            })
        };

        match flag.as_str() {
            "--url" => url = Some(take_value(&flag)?),

            "-X" | "--request" => {
                method = Some(parse_method(&take_value(&flag)?));
            }

            "-H" | "--header" => {
                let raw = take_value(&flag)?;
                if let Some(header) = parse_header(&raw) {
                    if header.name.eq_ignore_ascii_case("content-type") {
                        explicit_content_type = true;
                    }
                    headers.push(header);
                }
            }

            "-d" | "--data" | "--data-raw" | "--data-ascii" | "--data-urlencode" => {
                data.push(take_value(&flag)?);
            }

            "--data-binary" => {
                let value = take_value(&flag)?;
                // Only `--data-binary` gives `@` file semantics that we can represent.
                match value.strip_prefix('@') {
                    Some(path) => binary_body = Some(PathBuf::from(path)),
                    None => data.push(value),
                }
            }

            "--json" => {
                data.push(take_value(&flag)?);
                if !explicit_content_type {
                    headers.push(Header::new("Content-Type", "application/json"));
                    explicit_content_type = true;
                }
                headers.push(Header::new("Accept", "application/json"));
            }

            "-F" | "--form" | "--form-string" => {
                if let Some(field) = parse_form_field(&take_value(&flag)?) {
                    form.push(field);
                }
            }

            "-u" | "--user" => {
                // curl turns this into a Basic auth header; do the same so the imported
                // request is self-contained and the credential is visible/editable.
                let credentials = take_value(&flag)?;
                headers.push(Header::new(
                    "Authorization",
                    format!("Basic {}", base64(credentials.as_bytes())),
                ));
            }

            "-A" | "--user-agent" => headers.push(Header::new("User-Agent", take_value(&flag)?)),
            "-e" | "--referer" => headers.push(Header::new("Referer", take_value(&flag)?)),
            "-b" | "--cookie" => {
                let value = take_value(&flag)?;
                // `-b` with no `=` means "read cookies from this file", which is not
                // something an imported request can carry.
                if value.contains('=') {
                    headers.push(Header::new("Cookie", value));
                } else {
                    ignored.push(format!("{flag} {value}"));
                }
            }

            "-G" | "--get" => as_get = true,
            "-I" | "--head" => method = method.or(Some(Method::Head)),

            "-k" | "--insecure" => spec.settings.verify_tls = false,
            "-L" | "--location" => spec.settings.follow_redirects = true,
            "--compressed" => spec.settings.accept_encodings = true,

            "-m" | "--max-time" => {
                let value = take_value(&flag)?;
                if let Ok(seconds) = value.parse::<f64>()
                    && seconds > 0.0
                {
                    spec.settings.timeout = Some(std::time::Duration::from_secs_f64(seconds));
                }
            }

            // Output and diagnostic options: recognised, and meaningless on import.
            "-s" | "--silent" | "-v" | "--verbose" | "-i" | "--include" | "-f" | "--fail"
            | "-S" | "--show-error" | "-O" | "--remote-name" | "--progress-bar" | "-#"
            | "--no-progress-meter" | "-N" | "--no-buffer" => {}

            // Recognised as taking a value, but nothing in the model holds them yet.
            "-o" | "--output" | "-w" | "--write-out" | "--proxy" | "-x" | "--cacert" | "--cert"
            | "--key" | "--connect-timeout" | "--retry" | "--resolve" | "--interface" => {
                let value = take_value(&flag).unwrap_or_default();
                ignored.push(format!("{flag} {value}").trim_end().to_string());
            }

            _ => ignored.push(flag.clone()),
        }
    }

    let mut url = url.ok_or(CurlError::NoUrl)?;

    // curl itself would treat any bare word as a URL, so `curl this is garbage` parses
    // with url = "this". That's faithful to curl and useless as an import: pasting
    // arbitrary text would silently produce a nonsense request. Require either the
    // `curl` word or something that actually looks like a URL.
    if !saw_curl && !looks_like_url(&url) {
        return Err(CurlError::NotCurl);
    }

    // `-G` means "send the data as a query string instead of a body".
    if as_get && !data.is_empty() {
        let joined = data.join("&");
        let separator = if url.contains('?') { '&' } else { '?' };
        url = format!("{url}{separator}{joined}");
        data.clear();
        method = method.or(Some(Method::Get));
    }

    let body = build_body(&data, form, binary_body, &headers, &mut explicit_content_type);

    // curl's own default: data with no explicit type is form-urlencoded. Adding it here
    // is what stops an imported request behaving differently from its source command.
    if matches!(body, Body::Raw { .. }) && !explicit_content_type {
        headers.push(Header::new(
            "Content-Type",
            "application/x-www-form-urlencoded",
        ));
    }

    spec.method = method.unwrap_or(if matches!(body, Body::Empty) {
        Method::Get
    } else {
        Method::Post
    });
    spec.name = derive_name(&url);
    // The query string stays in the URL rather than being split into rows. Splitting
    // would mean decoding and re-encoding every value, which can invalidate a signed
    // URL — and presigned URLs are exactly the kind of thing people paste in.
    spec.url = url;
    spec.headers = headers;
    spec.body = body;

    Ok(CurlImport { spec, ignored })
}

/// Loose enough to accept `example.com/api` and `localhost:3000`, strict enough to
/// reject prose.
fn looks_like_url(candidate: &str) -> bool {
    candidate.contains("://")
        || candidate.contains('.')
        || candidate.starts_with("localhost")
        || candidate.starts_with('[') // bare IPv6 literal
}

fn build_body(
    data: &[String],
    form: Vec<MultipartField>,
    binary: Option<PathBuf>,
    headers: &[Header],
    explicit_content_type: &mut bool,
) -> Body {
    if let Some(path) = binary {
        return Body::Binary(path);
    }
    if !form.is_empty() {
        *explicit_content_type = true; // multipart sets its own boundary
        return Body::Multipart(form);
    }
    if data.is_empty() {
        return Body::Empty;
    }

    // Multiple -d flags are concatenated with `&`, matching curl.
    let text = data.join("&");
    Body::Raw {
        kind: raw_kind_for(headers),
        text,
    }
}

fn raw_kind_for(headers: &[Header]) -> RawKind {
    let content_type = headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| header.value.to_ascii_lowercase())
        .unwrap_or_default();

    if content_type.contains("json") {
        RawKind::Json
    } else if content_type.contains("xml") {
        RawKind::Xml
    } else if content_type.contains("html") {
        RawKind::Html
    } else {
        RawKind::Text
    }
}

fn parse_method(raw: &str) -> Method {
    match raw.to_ascii_uppercase().as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "PATCH" => Method::Patch,
        "DELETE" => Method::Delete,
        "HEAD" => Method::Head,
        "OPTIONS" => Method::Options,
        other => Method::Other(other.to_string()),
    }
}

/// `Name: value`. A header with no colon is not a header; `Name:` is a legitimate
/// empty value.
fn parse_header(raw: &str) -> Option<Header> {
    let (name, value) = raw.split_once(':')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(Header::new(name, value.trim()))
}

/// `name=value`, or `name=@path` / `name=<path` for a file part.
fn parse_form_field(raw: &str) -> Option<MultipartField> {
    let (name, value) = raw.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let value = match value.strip_prefix('@').or_else(|| value.strip_prefix('<')) {
        // Strip curl's `;type=…` part parameters; the model doesn't carry them yet.
        Some(path) => MultipartValue::File(PathBuf::from(
            path.split(';').next().unwrap_or(path).to_string(),
        )),
        None => MultipartValue::Text(value.to_string()),
    };

    Some(MultipartField {
        enabled: true,
        name: name.to_string(),
        value,
    })
}

/// A readable name from the URL's last path segment, so an imported request isn't
/// called "Untitled".
///
/// Shares `label_from_url` with `RequestSpec::label` — the tab strip wants the same
/// derivation, and two copies would drift.
fn derive_name(url: &str) -> String {
    // "Imported" rides in as the fallback name, which is precisely what `label_for`'s
    // second argument is for. Sharing the derivation with the tab strip means the two
    // can't drift.
    crate::request::label_for(url, "Imported").to_string()
}

// ---------------------------------------------------------------------------
// Shell-ish tokenizer
// ---------------------------------------------------------------------------

/// Split a command line the way a POSIX shell would, as far as import needs.
///
/// Handles single quotes, double quotes with escapes, `$'…'` ANSI-C quoting (Chrome
/// emits it for bodies containing quotes), backslash escapes, and backslash-newline line
/// continuations. Bare newlines count as whitespace, so a pasted multi-line command works
/// whether or not the continuations survived the copy.
fn tokenize(input: &str) -> Result<Vec<String>, CurlError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // Windows-style continuation, as pasted from cmd.exe instructions.
            '^' if matches!(chars.peek(), Some('\n') | Some('\r')) => {
                chars.next();
            }
            '\\' => match chars.peek() {
                Some('\n') => {
                    chars.next();
                }
                Some('\r') => {
                    chars.next();
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                }
                Some(_) => {
                    current.push(chars.next().unwrap());
                    has_token = true;
                }
                None => {}
            },

            '\'' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return Err(CurlError::UnbalancedQuote { quote: '\'' }),
                    }
                }
            }

            '"' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            // Inside double quotes a shell only treats these as escapes.
                            Some(c @ ('"' | '\\' | '$' | '`')) => current.push(c),
                            Some('\n') => {}
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => return Err(CurlError::UnbalancedQuote { quote: '"' }),
                        },
                        Some(c) => current.push(c),
                        None => return Err(CurlError::UnbalancedQuote { quote: '"' }),
                    }
                }
            }

            // ANSI-C quoting: $'...' with real escape sequences.
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                has_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => current.push('\n'),
                            Some('t') => current.push('\t'),
                            Some('r') => current.push('\r'),
                            Some('0') => current.push('\0'),
                            Some(c @ ('\\' | '\'' | '"')) => current.push(c),
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => return Err(CurlError::UnbalancedQuote { quote: '\'' }),
                        },
                        Some(c) => current.push(c),
                        None => return Err(CurlError::UnbalancedQuote { quote: '\'' }),
                    }
                }
            }

            c if c.is_whitespace() => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }

            c => {
                current.push(c);
                has_token = true;
            }
        }
    }

    if has_token {
        tokens.push(current);
    }

    if tokens.is_empty() {
        return Err(CurlError::NotCurl);
    }

    Ok(tokens)
}

/// Standard base64. Written out rather than pulling a dependency for ~15 lines used on
/// exactly one code path.
fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let triple =
            ((bytes[0] as u32) << 16) | ((bytes[1] as u32) << 8) | (bytes[2] as u32);

        out.push(TABLE[(triple >> 18 & 0x3f) as usize] as char);
        out.push(TABLE[(triple >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import(input: &str) -> CurlImport {
        parse(input).expect("should parse")
    }

    fn header_of<'a>(spec: &'a RequestSpec, name: &str) -> Option<&'a str> {
        spec.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }

    #[test]
    fn the_simplest_command() {
        let spec = import("curl https://api.example.com/users").spec;
        assert_eq!(spec.method, Method::Get);
        assert_eq!(spec.url, "https://api.example.com/users");
        assert_eq!(spec.name, "users");
    }

    #[test]
    fn the_curl_word_is_optional_and_a_prompt_is_tolerated() {
        assert_eq!(import("https://x.test/a").spec.url, "https://x.test/a");
        assert_eq!(import("$ curl https://x.test/a").spec.url, "https://x.test/a");
        assert_eq!(
            import("/usr/bin/curl https://x.test/a").spec.url,
            "https://x.test/a"
        );
    }

    #[test]
    fn a_chrome_style_copy_as_curl_round_trips() {
        // The realistic shape: single-quoted URL, repeated -H, --data-raw, --compressed.
        let input = r#"curl 'https://api.example.com/v2/items?page=2' \
  -H 'accept: application/json' \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer abc.def' \
  --data-raw '{"name":"zuno","tags":["a","b"]}' \
  --compressed"#;

        let import = import(input);
        let spec = &import.spec;

        assert_eq!(spec.method, Method::Post, "data implies POST");
        assert_eq!(spec.url, "https://api.example.com/v2/items?page=2");
        assert_eq!(spec.headers.len(), 3);
        assert_eq!(header_of(spec, "Authorization"), Some("Bearer abc.def"));
        assert_eq!(
            spec.body,
            Body::Raw {
                text: r#"{"name":"zuno","tags":["a","b"]}"#.to_string(),
                kind: RawKind::Json,
            }
        );
        assert!(import.ignored.is_empty(), "{:?}", import.ignored);
    }

    #[test]
    fn the_query_string_is_left_in_the_url() {
        // Splitting it into rows would mean decode/re-encode, which can break a signed
        // URL — and presigned URLs are exactly what people paste.
        let spec = import("curl 'https://x.test/o?X-Sig=a%2Bb%3D&t=1'").spec;
        assert_eq!(spec.url, "https://x.test/o?X-Sig=a%2Bb%3D&t=1");
        assert!(spec.query.is_empty());
    }

    #[test]
    fn an_explicit_method_wins_over_inference() {
        let spec = import("curl -X PUT https://x.test/a -d 'body'").spec;
        assert_eq!(spec.method, Method::Put);
    }

    #[test]
    fn custom_methods_survive() {
        let spec = import("curl -X PROPFIND https://x.test/a").spec;
        assert_eq!(spec.method, Method::Other("PROPFIND".to_string()));
    }

    #[test]
    fn data_without_a_content_type_gets_curls_default() {
        // curl sends form-urlencoded here. Without this the import would send
        // text/plain and behave differently from the command it came from.
        let spec = import("curl https://x.test/a -d 'a=1&b=2'").spec;
        assert_eq!(
            header_of(&spec, "content-type"),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(spec.method, Method::Post);
    }

    #[test]
    fn multiple_data_flags_are_joined_with_ampersands() {
        let spec = import("curl https://x.test/a -d one=1 -d two=2").spec;
        let Body::Raw { text, .. } = &spec.body else {
            panic!("expected a raw body: {:?}", spec.body);
        };
        assert_eq!(text, "one=1&two=2");
    }

    #[test]
    fn the_json_flag_sets_both_content_type_and_accept() {
        let spec = import(r#"curl https://x.test/a --json '{"a":1}'"#).spec;
        assert_eq!(header_of(&spec, "content-type"), Some("application/json"));
        assert_eq!(header_of(&spec, "accept"), Some("application/json"));
        assert!(matches!(
            spec.body,
            Body::Raw { kind: RawKind::Json, .. }
        ));
    }

    #[test]
    fn get_moves_data_into_the_query_string() {
        let spec = import("curl -G https://x.test/search -d q=rust -d page=2").spec;
        assert_eq!(spec.method, Method::Get);
        assert_eq!(spec.url, "https://x.test/search?q=rust&page=2");
        assert_eq!(spec.body, Body::Empty);
    }

    #[test]
    fn get_appends_to_an_existing_query_string() {
        let spec = import("curl -G 'https://x.test/s?a=1' -d b=2").spec;
        assert_eq!(spec.url, "https://x.test/s?a=1&b=2");
    }

    #[test]
    fn basic_auth_becomes_a_visible_header() {
        let spec = import("curl -u alice:s3cret https://x.test/a").spec;
        // base64("alice:s3cret")
        assert_eq!(
            header_of(&spec, "authorization"),
            Some("Basic YWxpY2U6czNjcmV0")
        );
    }

    #[test]
    fn base64_padding_is_correct_for_every_length() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn shorthand_header_flags_become_headers() {
        let spec = import(
            "curl https://x.test/a -A 'zuno/1.0' -e https://ref.test/ -b 'session=abc'",
        )
        .spec;
        assert_eq!(header_of(&spec, "user-agent"), Some("zuno/1.0"));
        assert_eq!(header_of(&spec, "referer"), Some("https://ref.test/"));
        assert_eq!(header_of(&spec, "cookie"), Some("session=abc"));
    }

    #[test]
    fn a_cookie_file_is_reported_rather_than_guessed_at() {
        let import = import("curl https://x.test/a -b cookies.txt");
        assert!(header_of(&import.spec, "cookie").is_none());
        assert!(
            import.ignored.iter().any(|flag| flag.contains("cookies.txt")),
            "{:?}",
            import.ignored
        );
    }

    #[test]
    fn settings_flags_are_applied() {
        let spec = import("curl -k -L --max-time 5 https://x.test/a").spec;
        assert!(!spec.settings.verify_tls);
        assert!(spec.settings.follow_redirects);
        assert_eq!(spec.settings.timeout, Some(std::time::Duration::from_secs(5)));
    }

    #[test]
    fn multipart_forms_are_recognised_including_files() {
        let spec = import("curl https://x.test/up -F name=zuno -F 'file=@/tmp/a.png;type=image/png'").spec;
        let Body::Multipart(fields) = &spec.body else {
            panic!("expected multipart: {:?}", spec.body);
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].value, MultipartValue::Text("zuno".to_string()));
        assert_eq!(
            fields[1].value,
            MultipartValue::File(PathBuf::from("/tmp/a.png")),
            "the ;type= parameter should be stripped"
        );
        assert_eq!(spec.method, Method::Post);
    }

    #[test]
    fn data_binary_with_an_at_sign_becomes_a_file_body() {
        let spec = import("curl https://x.test/a --data-binary @payload.bin").spec;
        assert_eq!(spec.body, Body::Binary(PathBuf::from("payload.bin")));
    }

    #[test]
    fn equals_form_flags_work_too() {
        let spec = import("curl --url=https://x.test/a --request=DELETE").spec;
        assert_eq!(spec.url, "https://x.test/a");
        assert_eq!(spec.method, Method::Delete);
    }

    #[test]
    fn output_flags_are_silently_dropped_because_they_mean_nothing_here() {
        let import = import("curl -s -v -i --compressed https://x.test/a");
        assert!(import.ignored.is_empty(), "{:?}", import.ignored);
    }

    #[test]
    fn unknown_and_unsupported_flags_are_reported_not_fatal() {
        let import = import("curl --proxy http://p:8080 --frobnicate https://x.test/a");
        assert_eq!(import.spec.url, "https://x.test/a", "import still succeeds");
        assert!(
            import.ignored.iter().any(|f| f.contains("--proxy")),
            "{:?}",
            import.ignored
        );
        assert!(
            import.ignored.iter().any(|f| f == "--frobnicate"),
            "{:?}",
            import.ignored
        );
    }

    #[test]
    fn double_quoted_bodies_with_escapes_survive() {
        let spec = import(r#"curl https://x.test/a -H "content-type: application/json" -d "{\"a\":\"b\"}""#).spec;
        let Body::Raw { text, .. } = &spec.body else {
            panic!("expected raw");
        };
        assert_eq!(text, r#"{"a":"b"}"#);
    }

    #[test]
    fn ansi_c_quoting_is_decoded() {
        // Chrome emits $'...' when the body contains a single quote.
        let spec = import(r#"curl https://x.test/a --data-raw $'{"name":"O\'Brien"}'"#).spec;
        let Body::Raw { text, .. } = &spec.body else {
            panic!("expected raw");
        };
        assert_eq!(text, r#"{"name":"O'Brien"}"#);
    }

    #[test]
    fn line_continuations_and_bare_newlines_both_work() {
        let with_backslashes = "curl https://x.test/a \\\n  -H 'a: 1' \\\n  -H 'b: 2'";
        let bare = "curl https://x.test/a\n  -H 'a: 1'\n  -H 'b: 2'";

        assert_eq!(import(with_backslashes).spec.headers.len(), 2);
        assert_eq!(import(bare).spec.headers.len(), 2);
    }

    #[test]
    fn headers_without_a_colon_are_not_headers() {
        let spec = import("curl https://x.test/a -H 'not-a-header'").spec;
        assert!(spec.headers.is_empty());
    }

    #[test]
    fn an_empty_header_value_is_legitimate() {
        let spec = import("curl https://x.test/a -H 'X-Empty:'").spec;
        assert_eq!(header_of(&spec, "x-empty"), Some(""));
    }

    #[test]
    fn duplicate_headers_are_both_kept() {
        let spec = import("curl https://x.test/a -H 'x-tag: one' -H 'x-tag: two'").spec;
        let values: Vec<_> = spec
            .headers
            .iter()
            .filter(|h| h.name == "x-tag")
            .map(|h| h.value.as_str())
            .collect();
        assert_eq!(values, vec!["one", "two"]);
    }

    #[test]
    fn missing_url_is_an_error() {
        assert_eq!(parse("curl -X POST"), Err(CurlError::NoUrl));
    }

    #[test]
    fn unbalanced_quotes_are_an_error() {
        assert_eq!(
            parse("curl 'https://x.test/a"),
            Err(CurlError::UnbalancedQuote { quote: '\'' })
        );
        assert_eq!(
            parse(r#"curl "https://x.test/a"#),
            Err(CurlError::UnbalancedQuote { quote: '"' })
        );
    }

    #[test]
    fn a_flag_missing_its_value_is_an_error() {
        assert_eq!(
            parse("curl https://x.test/a -H"),
            Err(CurlError::MissingValue {
                flag: "-H".to_string()
            })
        );
    }

    #[test]
    fn empty_input_is_rejected() {
        assert_eq!(parse("   "), Err(CurlError::NotCurl));
    }

    #[test]
    fn prose_is_rejected_rather_than_read_as_a_url() {
        // curl would treat "this" as a hostname; an importer must not. Otherwise
        // pasting arbitrary clipboard text quietly builds a nonsense request.
        assert_eq!(
            parse("this is not a curl command at all"),
            Err(CurlError::NotCurl)
        );
        assert_eq!(parse("hello world"), Err(CurlError::NotCurl));
    }

    #[test]
    fn a_bare_url_without_the_curl_word_is_still_accepted() {
        assert_eq!(import("example.com/api").spec.url, "example.com/api");
        assert_eq!(import("localhost:3000/health").spec.url, "localhost:3000/health");
        // With the curl word, even an odd host is the user's business.
        assert_eq!(import("curl myhost/path").spec.url, "myhost/path");
    }

    #[test]
    fn names_are_derived_from_the_url() {
        assert_eq!(import("curl https://x.test/v1/users").spec.name, "users");
        assert_eq!(import("curl https://x.test/").spec.name, "x.test");
        assert_eq!(import("curl https://x.test").spec.name, "x.test");
        assert_eq!(import("curl http://localhost:3000").spec.name, "localhost:3000");
        assert_eq!(import("curl https://x.test/a?q=1").spec.name, "a");
    }

    #[test]
    fn the_imported_spec_is_sendable_by_the_engine() {
        // The real contract: import must produce something build.rs accepts.
        let spec = import(
            r#"curl 'https://api.example.com/v1/x?a=1' -H 'content-type: application/json' -d '{"k":1}'"#,
        )
        .spec;

        let url = crate::engine::build::resolve_url(&spec).expect("a resolvable URL");
        assert_eq!(url.as_str(), "https://api.example.com/v1/x?a=1");
        crate::engine::build::build_headers(&spec).expect("valid headers");
    }
}
