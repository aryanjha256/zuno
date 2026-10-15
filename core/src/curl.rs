//! Converting between a `curl` command line and a `RequestSpec`, in both directions.
//!
//! `parse` imports, `to_command` exports. They live in one module so the round trip is one
//! file's problem: a flag the exporter emits and the importer drops is a bug visible from
//! here, and `a_command_round_trips` asserts it.
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
    Auth, Body, Header, Method, MultipartField, MultipartValue, RawKind, RequestSettings,
    RequestSpec,
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

/// curl's short options that take a value, read from `curl --help all` (8.18) rather than
/// written from memory. Everything else after a single `-` is a switch.
///
/// **Knowing which flags take a value is what keeps the URL the URL.** Without it,
/// `curl -c cookies.txt https://…` read `cookies.txt` as the URL and the real one as a stray
/// argument, and `-XDELETE` — a cluster, the way curl reads every single-dash token — matched
/// nothing and imported as a GET.
const VALUE_SHORT: &str = "ACDEFHKPQTUXYbcdehmortuwxyz";

/// The long options that take a value, from the same source.
const VALUE_LONG: &[&str] = &[
    "--abstract-unix-socket", "--alt-svc", "--aws-sigv4", "--cacert", "--capath", "--cert",
    "--cert-type", "--ciphers", "--config", "--connect-timeout", "--connect-to", "--continue-at",
    "--cookie", "--cookie-jar", "--create-file-mode", "--crlfile", "--curves", "--data",
    "--data-ascii", "--data-binary", "--data-raw", "--data-urlencode", "--delegation",
    "--dns-interface", "--dns-ipv4-addr", "--dns-ipv6-addr", "--dns-servers", "--doh-url",
    "--dump-header", "--ech", "--egd-file", "--engine", "--etag-compare", "--etag-save",
    "--expect100-timeout", "--form", "--form-string", "--ftp-account",
    "--ftp-alternative-to-user", "--ftp-method", "--ftp-port", "--ftp-ssl-ccc-mode",
    "--happy-eyeballs-timeout-ms", "--haproxy-clientip", "--header", "--help", "--hostpubmd5",
    "--hostpubsha256", "--hsts", "--interface", "--ip-tos", "--ipfs-gateway", "--json",
    "--keepalive-cnt", "--keepalive-time", "--key", "--key-type", "--knownhosts", "--krb",
    "--libcurl", "--limit-rate", "--local-port", "--login-options", "--mail-auth", "--mail-from",
    "--mail-rcpt", "--max-filesize", "--max-redirs", "--max-time", "--netrc-file", "--noproxy",
    "--oauth2-bearer", "--output", "--output-dir", "--parallel-max", "--parallel-max-host",
    "--pass", "--pinnedpubkey", "--preproxy", "--proto", "--proto-default", "--proto-redir",
    "--proxy", "--proxy-cacert", "--proxy-capath", "--proxy-cert", "--proxy-cert-type",
    "--proxy-ciphers", "--proxy-crlfile", "--proxy-header", "--proxy-key", "--proxy-key-type",
    "--proxy-pass", "--proxy-pinnedpubkey", "--proxy-service-name", "--proxy-tls13-ciphers",
    "--proxy-tlsauthtype", "--proxy-tlspassword", "--proxy-tlsuser", "--proxy-user",
    "--proxy1.0", "--pubkey", "--quote", "--random-file", "--range", "--rate", "--referer",
    "--request", "--request-target", "--resolve", "--retry", "--retry-delay", "--retry-max-time",
    "--sasl-authzid", "--service-name", "--sigalgs", "--socks4", "--socks4a", "--socks5",
    "--socks5-gssapi-service", "--socks5-hostname", "--speed-limit", "--speed-time",
    "--ssl-sessions", "--stderr", "--telnet-option", "--tftp-blksize", "--time-cond", "--tls-max",
    "--tls13-ciphers", "--tlsauthtype", "--tlspassword", "--tlsuser", "--trace", "--trace-ascii",
    "--trace-config", "--unix-socket", "--upload-file", "--upload-flags", "--url", "--url-query",
    "--user", "--user-agent", "--variable", "--vlan-priority", "--write-out",
];

enum Arg {
    Flag(String, Option<String>),
    Positional(String),
}

/// Tokens as curl reads them: `-sSL` is three switches, `-XPOST` is `-X POST`, `-HX: y` is a
/// header, and a flag that takes a value takes the next token whatever it looks like.
fn expand(tokens: Vec<String>) -> Result<Vec<Arg>, CurlError> {
    let mut args = Vec::new();
    let mut tokens = tokens.into_iter();
    while let Some(token) = tokens.next() {
        if let Some(long) = token.strip_prefix("--").filter(|long| !long.is_empty()) {
            // `--flag=value` is not curl syntax, but it is accepted here as a convenience.
            let (flag, inline) = match long.split_once('=') {
                Some((flag, value)) => (format!("--{flag}"), Some(value.to_string())),
                None => (token.clone(), None),
            };
            let value = match inline {
                Some(value) => Some(value),
                None if VALUE_LONG.contains(&flag.as_str()) => Some(
                    tokens
                        .next()
                        .ok_or_else(|| CurlError::MissingValue { flag: flag.clone() })?,
                ),
                None => None,
            };
            args.push(Arg::Flag(flag, value));
        } else if token.len() > 1 && token.starts_with('-') {
            let mut chars = token[1..].char_indices();
            while let Some((at, short)) = chars.next() {
                let flag = format!("-{short}");
                if VALUE_SHORT.contains(short) {
                    let rest = &token[1 + at + short.len_utf8()..];
                    let value = if rest.is_empty() {
                        tokens
                            .next()
                            .ok_or_else(|| CurlError::MissingValue { flag: flag.clone() })?
                    } else {
                        rest.to_string()
                    };
                    args.push(Arg::Flag(flag, Some(value)));
                    break;
                }
                args.push(Arg::Flag(flag, None));
            }
        } else {
            args.push(Arg::Positional(token));
        }
    }
    Ok(args)
}

/// curl's `--data-urlencode` and `--url-query` encoding: unreserved characters kept, a space as
/// `+`, everything else percent-encoded — measured against curl 8.18, which is what a server
/// receiving the original command saw. Sending the text raw turned Twilio's `From=+1501…` into a
/// number starting with a space, and split any value holding a `&` in two.
fn url_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// One `--data-urlencode` / `--url-query` argument, in its four spellings: `content`,
/// `=content`, `name=content`, and the file forms `@file` / `name@file`, which name a file this
/// importer does not read and so come back `None`.
fn encode_pair(raw: &str) -> Option<String> {
    let at = raw.find('@');
    match raw.find('=') {
        Some(eq) if at.is_none_or(|at| eq < at) => {
            let (name, content) = (&raw[..eq], &raw[eq + 1..]);
            Some(if name.is_empty() {
                url_encode(content)
            } else {
                format!("{name}={}", url_encode(content))
            })
        }
        _ if at.is_some() => None,
        _ => Some(url_encode(raw)),
    }
}

/// Chrome's "Copy as cURL (cmd)" escapes for cmd.exe: every quote is `^"`, specials are `^&`,
/// and a line ends in ` ^`. Undone first, which leaves an ordinary double-quoted command.
/// Recognised by how that output always starts, so a bash command with a `^` in its data is
/// never touched.
fn uncmd(input: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = input.trim_start();
    let after = trimmed
        .strip_prefix("curl.exe")
        .or_else(|| trimmed.strip_prefix("curl"))
        .unwrap_or_default();
    if !after.trim_start().starts_with("^\"") {
        return std::borrow::Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '^' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\r') => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            Some('\n') | None => {}
            Some(escaped) => out.push(escaped),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Parse a curl command line.
pub fn parse(input: &str) -> Result<CurlImport, CurlError> {
    let tokens = tokenize(&uncmd(input))?;
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
        // `curl.exe` is how Microsoft's docs and PowerShell spell it, since `curl` alone is an
        // alias for `Invoke-WebRequest` there.
        let lower = first.to_ascii_lowercase();
        if lower == "curl" || lower == "curl.exe" || lower.ends_with("/curl") || lower.ends_with("\\curl.exe") {
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
    // **Start from curl's defaults, not Zuno's**, for the two settings that change what goes on
    // the wire. curl does not follow redirects without `-L` and sends no `Accept-Encoding`
    // without `--compressed`; Zuno has both on. Starting from Zuno's defaults made both flags
    // no-ops *and* made their absence unrepresentable, so `curl https://x/redirects` imported as
    // a request that follows the redirect and reports the destination's 200 instead of the 302 you
    // were looking at — principle 2 at the top of this file, broken.
    //
    // The line is drawn at *wire-observable* behaviour. `timeout` stays at Zuno's 30s even though
    // curl waits forever, because that one is a local guard rather than something the server can
    // tell apart, and "no timeout by default" is a worse default than a wrong one. `max_redirects`
    // likewise stays at Zuno's 10 rather than curl's 50: it only applies once `-L` is present, and
    // the settings panel can change it.
    let mut spec = RequestSpec {
        settings: RequestSettings {
            follow_redirects: false,
            accept_encodings: false,
            ..RequestSettings::default()
        },
        ..RequestSpec::default()
    };

    // `-d @file`: curl reads the file, where `--data-raw @file` sends the text. Sent as the file's
    // bytes, which differs from curl only in curl stripping the file's newlines.
    let mut data_file: Option<PathBuf> = None;
    let mut upload: Option<PathBuf> = None;
    let mut url_query: Vec<String> = Vec::new();

    for arg in expand(tokens.collect())? {
        let (flag, value) = match arg {
            // A bare `-` is curl's stdin marker; nothing to import from it.
            Arg::Positional(token) => {
                if url.is_none() && token != "-" {
                    url = Some(token);
                } else {
                    ignored.push(token);
                }
                continue;
            }
            Arg::Flag(flag, value) => (flag, value),
        };
        // `expand` has already taken a value for every flag that needs one.
        let value_of = || value.clone().unwrap_or_default();

        match flag.as_str() {
            "--url" => url = Some(value_of()),

            "-X" | "--request" => method = Some(parse_method(&value_of())),

            "-H" | "--header" => {
                if let Some(header) = parse_header(&value_of()) {
                    if header.name.eq_ignore_ascii_case("content-type") {
                        explicit_content_type = true;
                    }
                    headers.push(header);
                }
            }

            "-d" | "--data" | "--data-ascii" => {
                let value = value_of();
                match value.strip_prefix('@').filter(|path| !path.is_empty() && *path != "-") {
                    Some(path) => data_file = Some(PathBuf::from(path)),
                    None => data.push(value),
                }
            }
            "--data-raw" => data.push(value_of()),
            "--data-urlencode" => match encode_pair(&value_of()) {
                Some(encoded) => data.push(encoded),
                None => ignored.push(format!("{flag} {} (a file to encode)", value_of())),
            },
            "--url-query" => {
                // `+` opts out of encoding.
                let value = value_of();
                match value.strip_prefix('+') {
                    Some(raw) => url_query.push(raw.to_string()),
                    None => match encode_pair(&value) {
                        Some(encoded) => url_query.push(encoded),
                        None => ignored.push(format!("{flag} {value} (a file to encode)")),
                    },
                }
            }

            "--data-binary" => {
                let value = value_of();
                // Only `--data-binary` gives `@` file semantics that we can represent.
                match value.strip_prefix('@') {
                    Some(path) => binary_body = Some(PathBuf::from(path)),
                    None => data.push(value),
                }
            }

            "--json" => {
                data.push(value_of());
                if !explicit_content_type {
                    headers.push(Header::new("Content-Type", "application/json"));
                    explicit_content_type = true;
                }
                headers.push(Header::new("Accept", "application/json"));
            }

            "-F" | "--form" => {
                if let Some(field) = parse_form_field(&value_of()) {
                    form.push(field);
                }
            }
            // The literal twin of `-F`: a leading `@` or `<` is text, not a file.
            "--form-string" => {
                if let Some((name, text)) = value_of().split_once('=') {
                    form.push(MultipartField {
                        enabled: true,
                        name: name.trim().to_string(),
                        value: MultipartValue::Text(text.to_string()),
                    });
                }
            }

            "-T" | "--upload-file" => upload = Some(PathBuf::from(value_of())),

            "-u" | "--user" => {
                // curl turns this into a Basic auth header; do the same so the imported
                // request is self-contained and the credential is visible/editable.
                headers.push(Header::new(
                    "Authorization",
                    format!("Basic {}", base64(value_of().as_bytes())),
                ));
            }
            "--oauth2-bearer" => spec.auth = Auth::Bearer { token: value_of() },

            "-A" | "--user-agent" => headers.push(Header::new("User-Agent", value_of())),
            "-e" | "--referer" => headers.push(Header::new("Referer", value_of())),
            "-b" | "--cookie" => {
                let value = value_of();
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
                if let Ok(seconds) = value_of().parse::<f64>()
                    && seconds > 0.0
                {
                    spec.settings.timeout = Some(std::time::Duration::from_secs_f64(seconds));
                }
            }

            // Output and diagnostic options: recognised, and meaningless on import.
            "-s" | "--silent" | "-v" | "--verbose" | "-i" | "--include" | "-f" | "--fail"
            | "-S" | "--show-error" | "-O" | "--remote-name" | "--progress-bar" | "-#"
            | "--no-progress-meter" | "-N" | "--no-buffer" => {}

            // Anything else is named, with its value when it took one.
            _ => ignored.push(match &value {
                Some(value) => format!("{flag} {value}"),
                None => flag.clone(),
            }),
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

    if !url_query.is_empty() {
        let separator = if url.contains('?') { '&' } else { '?' };
        url = format!("{url}{separator}{}", url_query.join("&"));
    }

    // `-T` is a PUT of the file, and a URL ending in `/` gets the file's name appended.
    if let Some(path) = upload {
        if url.ends_with('/')
            && let Some(name) = path.file_name()
        {
            url.push_str(&name.to_string_lossy());
        }
        method = method.or(Some(Method::Put));
        binary_body = binary_body.or(Some(path));
    }

    // A `-d @file` among other `-d` values would need the file read to join them; the text
    // ones are kept and the file is named rather than silently dropped.
    let form_encoded_file = match data_file {
        Some(path) if data.is_empty() && binary_body.is_none() => {
            binary_body = Some(path);
            true
        }
        Some(path) => {
            ignored.push(format!("-d @{} (alongside other data)", path.display()));
            false
        }
        None => false,
    };

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
    if (matches!(body, Body::Raw { .. }) || form_encoded_file) && !explicit_content_type {
        headers.push(Header::new(
            "Content-Type",
            "application/x-www-form-urlencoded",
        ));
    }

    // A curl command is an HTTP request by construction — there is no flag that makes it
    // anything else, which is why the importer names the kind outright.
    let http = spec.http_mut().expect("a new spec is HTTP");
    http.method = method.unwrap_or(if matches!(body, Body::Empty) {
        Method::Get
    } else {
        Method::Post
    });
    http.body = body;
    spec.name = derive_name(&url);
    // The query string stays in the URL rather than being split into rows. Splitting
    // would mean decoding and re-encoding every value, which can invalidate a signed
    // URL — and presigned URLs are exactly the kind of thing people paste in.
    spec.url = url;
    spec.headers = headers;

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
/// curl's three header forms, as curl reads them.
///
/// `Name: value` sends it. **`Name:` with nothing after the colon *removes* a header curl would
/// add itself** — it was read here as "send it empty", so re-importing one of the exporter's own
/// commands would have sent an empty `Content-Type` where curl sends none. And `Name;` is curl's
/// spelling for an empty value, which used to be dropped for having no colon.
fn parse_header(raw: &str) -> Option<Header> {
    if let Some((name, value)) = raw.split_once(':') {
        let name = name.trim();
        if name.is_empty() || value.trim().is_empty() {
            return None;
        }
        return Some(Header::new(name, value.trim()));
    }
    let name = raw.trim().strip_suffix(';')?.trim();
    (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| Header::new(name, ""))
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
            // Windows-style continuations: `^` from cmd.exe instructions, a backtick from
            // PowerShell's. The `\r` of a CRLF is whitespace below, so consuming one char is enough.
            '^' | '`' if matches!(chars.peek(), Some('\n') | Some('\r')) => {
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

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Render a request as a runnable `curl` command.
///
/// The inverse of `parse`. The rendering lives in `codegen` beside every other target, all of
/// them drawn from one wire description; it is reachable under this name because this module's
/// round-trip tests are what hold the two directions together. The spec is expected to be
/// pre-resolved through `Resolver::without_secrets` — see `codegen`.
pub fn to_command(spec: &RequestSpec) -> String {
    crate::codegen::Target::Curl
        .render(spec, None)
        .unwrap_or_default()
}

/// Standard base64. Written out rather than pulling a dependency for ~15 lines, and shared with
/// `postman.rs`, which lowers Postman's basic auth into the same header this parses.

pub(crate) fn base64(input: &[u8]) -> String {
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
    use crate::request::{FormField, GraphQlRequest, QueryParam, RequestKind};

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
        assert_eq!(spec.http().unwrap().method, Method::Get);
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

        assert_eq!(spec.http().unwrap().method, Method::Post, "data implies POST");
        assert_eq!(spec.url, "https://api.example.com/v2/items?page=2");
        assert_eq!(spec.headers.len(), 3);
        assert_eq!(header_of(spec, "Authorization"), Some("Bearer abc.def"));
        assert_eq!(
            spec.http().unwrap().body,
            Body::Raw {
                text: r#"{"name":"zuno","tags":["a","b"]}"#.to_string(),
                kind: RawKind::Json,
            }
        );
        assert!(import.ignored.is_empty(), "{:?}", import.ignored);
    }

    /// **Flags read the way curl reads them**, each case found by sending the original command
    /// with real curl and the import with Zuno to one server and comparing what arrived.
    #[test]
    fn flags_are_read_the_way_curl_reads_them() {
        // A single-dash token is a cluster, and a value flag takes the rest of it.
        let spec = import("curl -XDELETE -H'X-Token: abc' https://x.test/a").spec;
        assert_eq!(spec.http().unwrap().method, Method::Delete);
        assert!(spec.headers.iter().any(|h| h.name == "X-Token" && h.value == "abc"));
        let spec = import("curl -sX PATCH https://x.test/a -d 'a=1'").spec;
        assert_eq!((spec.http().unwrap().method.clone(), spec.url.as_str()), (Method::Patch, "https://x.test/a"));
        assert!(import("curl -sSL https://x.test/a").spec.settings.follow_redirects);

        // A flag Zuno has no use for still takes its value, so the URL stays the URL.
        let imported = import("curl -c jar.txt --max-redirs 5 https://x.test/a");
        assert_eq!(imported.spec.url, "https://x.test/a");
        assert!(imported.ignored.contains(&"-c jar.txt".to_string()), "{:?}", imported.ignored);

        // Encoded as curl 8.18 sends it: Twilio's own example lost the `+` of a phone number.
        let spec = import("curl https://x.test/m --data-urlencode 'From=+1501' --data-urlencode 'q=a b&c'").spec;
        let Body::Raw { text, .. } = &spec.http().unwrap().body else { panic!("a body") };
        assert_eq!(text, "From=%2B1501&q=a+b%26c");

        // `-d @file` is the file, sent form-encoded unless told otherwise; `--data-raw` is not.
        let spec = import("curl https://x.test/i -d @body.json").spec;
        assert_eq!(spec.http().unwrap().body, Body::Binary("body.json".into()));
        assert!(spec.headers.iter().any(|h| h.value == "application/x-www-form-urlencoded"));
        let spec = import("curl https://x.test/i --data-raw @body.json").spec;
        assert!(matches!(&spec.http().unwrap().body, Body::Raw { text, .. } if text == "@body.json"));

        // `-T` is a PUT of the file; `--oauth2-bearer` is the Auth tab.
        let spec = import("curl -T report.csv https://x.test/up/").spec;
        assert_eq!((spec.http().unwrap().method.clone(), spec.url.as_str()), (Method::Put, "https://x.test/up/report.csv"));
        let spec = import("curl --oauth2-bearer t0k https://x.test/me").spec;
        assert_eq!(spec.auth, Auth::Bearer { token: "t0k".into() });

        // Chrome's "Copy as cURL (cmd)" and PowerShell's `curl.exe`.
        let spec = import("curl ^\"https://x.test/w^\" ^\n  -H ^\"accept: a/b^\" ^\n  --data-raw ^\"^{^\\^\"n^\\^\":1^}^\"").spec;
        assert_eq!(spec.url, "https://x.test/w");
        assert!(matches!(&spec.http().unwrap().body, Body::Raw { text, .. } if text == r#"{"n":1}"#));
        let spec = import("curl.exe https://x.test/ps `\n  -H \"Accept: a/b\"").spec;
        assert_eq!(spec.url, "https://x.test/ps");
        assert_eq!(spec.headers.len(), 1);
    }

    #[test]
    fn the_query_string_is_left_in_the_url() {
        // Splitting it into rows would mean decode/re-encode, which can break a signed
        // URL — and presigned URLs are exactly what people paste.
        let spec = import("curl 'https://x.test/o?X-Sig=a%2Bb%3D&t=1'").spec;
        assert_eq!(spec.url, "https://x.test/o?X-Sig=a%2Bb%3D&t=1");
        assert!(spec.http().unwrap().query.is_empty());
    }

    #[test]
    fn an_explicit_method_wins_over_inference() {
        let spec = import("curl -X PUT https://x.test/a -d 'body'").spec;
        assert_eq!(spec.http().unwrap().method, Method::Put);
    }

    #[test]
    fn custom_methods_survive() {
        let spec = import("curl -X PROPFIND https://x.test/a").spec;
        assert_eq!(spec.http().unwrap().method, Method::Other("PROPFIND".to_string()));
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
        assert_eq!(spec.http().unwrap().method, Method::Post);
    }

    #[test]
    fn multiple_data_flags_are_joined_with_ampersands() {
        let spec = import("curl https://x.test/a -d one=1 -d two=2").spec;
        let Body::Raw { text, .. } = &spec.http().unwrap().body else {
            panic!("expected a raw body: {:?}", spec.http().unwrap().body);
        };
        assert_eq!(text, "one=1&two=2");
    }

    #[test]
    fn the_json_flag_sets_both_content_type_and_accept() {
        let spec = import(r#"curl https://x.test/a --json '{"a":1}'"#).spec;
        assert_eq!(header_of(&spec, "content-type"), Some("application/json"));
        assert_eq!(header_of(&spec, "accept"), Some("application/json"));
        assert!(matches!(
            spec.http().unwrap().body,
            Body::Raw { kind: RawKind::Json, .. }
        ));
    }

    #[test]
    fn get_moves_data_into_the_query_string() {
        let spec = import("curl -G https://x.test/search -d q=rust -d page=2").spec;
        assert_eq!(spec.http().unwrap().method, Method::Get);
        assert_eq!(spec.url, "https://x.test/search?q=rust&page=2");
        assert_eq!(spec.http().unwrap().body, Body::Empty);
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
    fn absent_flags_keep_curls_behaviour_rather_than_zunos() {
        // The bug, and the weak assertion that hid it. `RequestSettings::default()` has redirect
        // following and Accept-Encoding **on**; curl has both off. So `-L` and `--compressed` were
        // no-ops, and — the part that actually changed requests — their *absence* did nothing.
        // `settings_flags_are_applied` asserted `follow_redirects` after `-L`, which was true by
        // default, so it passed with the `-L` arm deleted from `parse` entirely.
        let spec = import("curl https://x.test/a").spec;
        assert!(
            !spec.settings.follow_redirects,
            "curl does not follow redirects without -L"
        );
        assert!(
            !spec.settings.accept_encodings,
            "curl sends no Accept-Encoding without --compressed"
        );

        // `-k` was always faithful, because there the polarity happened to line up: both verify by
        // default, so the flag only ever had to turn something off.
        assert!(spec.settings.verify_tls);

        // Deliberately *not* faithful, and the reason is in `parse`: a local guard rather than
        // anything the server can distinguish.
        assert_eq!(spec.settings.timeout, RequestSettings::default().timeout);
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
        let Body::Multipart(fields) = &spec.http().unwrap().body else {
            panic!("expected multipart: {:?}", spec.http().unwrap().body);
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].value, MultipartValue::Text("zuno".to_string()));
        assert_eq!(
            fields[1].value,
            MultipartValue::File(PathBuf::from("/tmp/a.png")),
            "the ;type= parameter should be stripped"
        );
        assert_eq!(spec.http().unwrap().method, Method::Post);
    }

    #[test]
    fn data_binary_with_an_at_sign_becomes_a_file_body() {
        let spec = import("curl https://x.test/a --data-binary @payload.bin").spec;
        assert_eq!(spec.http().unwrap().body, Body::Binary(PathBuf::from("payload.bin")));
    }

    #[test]
    fn equals_form_flags_work_too() {
        let spec = import("curl --url=https://x.test/a --request=DELETE").spec;
        assert_eq!(spec.url, "https://x.test/a");
        assert_eq!(spec.http().unwrap().method, Method::Delete);
    }

    #[test]
    fn output_flags_are_silently_dropped_because_they_mean_nothing_here() {
        let import = import("curl -s -v -i --compressed https://x.test/a");
        assert!(import.ignored.is_empty(), "{:?}", import.ignored);
        // `--compressed` is in that list because it parses, not because it does nothing —
        // asserting only `ignored.is_empty()` would pass whether or not it took effect.
        assert!(import.spec.settings.accept_encodings, "--compressed should turn encodings on");
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
        let Body::Raw { text, .. } = &spec.http().unwrap().body else {
            panic!("expected raw");
        };
        assert_eq!(text, r#"{"a":"b"}"#);
    }

    #[test]
    fn ansi_c_quoting_is_decoded() {
        // Chrome emits $'...' when the body contains a single quote.
        let spec = import(r#"curl https://x.test/a --data-raw $'{"name":"O\'Brien"}'"#).spec;
        let Body::Raw { text, .. } = &spec.http().unwrap().body else {
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
        // **Spelt `Name;` in curl, and `Name:` means something else.** Measured against curl
        // 8.18: `-H 'X-Semi;'` sends `X-Semi:` with an empty value, while `-H 'X-Colon:'` sends
        // nothing at all — it removes a header, which is how `-H 'User-Agent:'` drops curl's own.
        // This test used to assert the colon form imported as an empty header, which gave the
        // request a header the command it came from never sent.
        let spec = import("curl https://x.test/a -H 'X-Empty;' -H 'X-Gone:'").spec;
        assert_eq!(header_of(&spec, "x-empty"), Some(""));
        assert_eq!(header_of(&spec, "x-gone"), None, "a removal is not a header");
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

    // ---- export ---------------------------------------------------------

    /// A spec with curl's own defaults for the two settings whose polarity differs, so an
    /// export under test carries only the flags the case is actually about.
    fn plain(url: &str) -> RequestSpec {
        let mut spec = RequestSpec::default();
        spec.url = url.to_string();
        spec.settings.follow_redirects = false;
        spec.settings.accept_encodings = false;
        spec
    }

    #[test]
    fn the_simplest_export() {
        assert_eq!(
            to_command(&plain("https://api.example.com/users")),
            "curl 'https://api.example.com/users'"
        );
    }

    #[test]
    fn a_get_carries_no_method_flag_but_anything_else_does() {
        // curl's default is GET, so `-X GET` on a plain GET is noise.
        assert!(!to_command(&plain("https://x.test/a")).contains("-X"));

        let mut spec = plain("https://x.test/a");
        spec.http_mut().unwrap().method = Method::Delete;
        assert!(to_command(&spec).contains("-X DELETE"));
    }

    #[test]
    fn a_get_with_a_body_states_its_method_explicitly() {
        // **The one case where -X is load-bearing rather than decorative.** curl infers POST from
        // the presence of a body, so omitting it here would silently change the request's method.
        let mut spec = plain("https://x.test/search");
        spec.http_mut().unwrap().body = Body::Raw {
            text: "{\"q\":\"ada\"}".to_string(),
            kind: RawKind::Json,
        };
        assert_eq!(spec.http().unwrap().method, Method::Get);

        let command = to_command(&spec);
        assert!(
            command.contains("-X GET"),
            "a GET with a body must say so or curl makes it a POST: {command}"
        );
    }

    #[test]
    fn query_rows_reach_the_url_percent_encoded() {
        let mut spec = plain("https://x.test/search");
        spec.http_mut().unwrap().query = vec![
            QueryParam::new("q", "a b&c"),
            QueryParam::new("page", "2"),
        ];

        let command = to_command(&spec);
        assert!(
            command.contains("?q=a+b%26c&page=2"),
            "the exported URL must be the one the engine would request: {command}"
        );
    }

    #[test]
    fn a_disabled_row_is_not_exported() {
        // Muting a row is half of how people debug; a copied command has to agree with what the
        // app would send, not with what is merely typed on screen.
        let mut spec = plain("https://x.test/a");
        spec.headers = vec![
            Header::new("X-Kept", "1"),
            Header {
                enabled: false,
                name: "X-Muted".into(),
                value: "2".into(),
            },
        ];
        spec.http_mut().unwrap().query = vec![QueryParam {
            enabled: false,
            name: "hidden".into(),
            value: "yes".into(),
        }];

        let command = to_command(&spec);
        assert!(command.contains("X-Kept"));
        assert!(!command.contains("X-Muted"), "{command}");
        assert!(!command.contains("hidden"), "{command}");
    }

    #[test]
    fn exported_form_body_matches_the_wire() {
        // Pins the export to `build_body` rather than to a second encoder. Break `encode_form` and
        // both move together; re-implement it here and this fails.
        let mut spec = plain("https://x.test/token");
        spec.http_mut().unwrap().method = Method::Post;
        // Struct literals: `FormField` has no `new`, unlike `Header` and `QueryParam`. Adding one
        // for two test call sites would be API ahead of a caller (invariant 1).
        spec.http_mut().unwrap().body = Body::Form(vec![
            FormField {
                enabled: true,
                name: "grant_type".into(),
                value: "client_credentials".into(),
            },
            FormField {
                enabled: true,
                name: "scope".into(),
                value: "read write".into(),
            },
        ]);

        let wire = match crate::engine::build::build_body(&spec).expect("body") {
            crate::engine::build::PreparedBody::Bytes { bytes, .. } => {
                String::from_utf8(bytes).expect("utf8")
            }
            other => panic!("expected bytes, got {other:?}"),
        };

        assert_eq!(wire, "grant_type=client_credentials&scope=read+write");
        assert!(
            to_command(&spec).contains(&format!("--data-raw '{wire}'")),
            "the exported form body must be byte-identical to what is sent"
        );
    }

    /// **A JSON body with no `Content-Type` typed still exports one.** The engine derives it, so
    /// Zuno sends `application/json`; curl, given `--data-raw` and no header, sends
    /// `application/x-www-form-urlencoded`. The command looked right and was a different request
    /// — the exporter read the spec directly and never learnt about the derivation.
    #[test]
    fn a_body_exports_the_content_type_the_engine_derives() {
        let mut spec = plain("https://x.test/items");
        spec.http_mut().unwrap().method = Method::Post;
        spec.http_mut().unwrap().body = Body::Raw {
            text: r#"{"a":1}"#.to_string(),
            kind: RawKind::Json,
        };

        let command = to_command(&spec);
        assert!(
            command.contains("-H 'Content-Type: application/json'"),
            "curl would send this JSON as a form: {command}"
        );
        let back = parse(&command).expect("re-import");
        assert_eq!(header_of(&back.spec, "Content-Type"), Some("application/json"));
    }

    #[test]
    fn a_raw_body_uses_data_raw_rather_than_d() {
        // `-d` strips newlines and reads a leading `@` as a filename. Both would corrupt a real
        // JSON body, and neither failure is visible in the command text.
        let mut spec = plain("https://x.test/a");
        spec.http_mut().unwrap().method = Method::Post;
        spec.http_mut().unwrap().body = Body::Raw {
            text: "{\n  \"at\": \"@home\"\n}".to_string(),
            kind: RawKind::Json,
        };

        let command = to_command(&spec);
        assert!(command.contains("--data-raw"), "{command}");
        assert!(!command.contains(" -d "), "{command}");
    }

    #[test]
    fn a_whitespace_only_raw_body_exports_nothing() {
        // Matches `build_body`, which sends nothing for it — so the command must not claim a body
        // and must not gain a `-X` it doesn't need.
        let mut spec = plain("https://x.test/a");
        spec.http_mut().unwrap().body = Body::Raw {
            text: "   \n ".to_string(),
            kind: RawKind::Json,
        };

        let command = to_command(&spec);
        assert!(!command.contains("--data"), "{command}");
        assert!(!command.contains("-X"), "{command}");
    }

    #[test]
    fn multipart_parts_become_f_flags_with_file_syntax() {
        let mut spec = plain("https://x.test/upload");
        spec.http_mut().unwrap().method = Method::Post;
        spec.http_mut().unwrap().body = Body::Multipart(vec![
            MultipartField {
                enabled: true,
                name: "caption".into(),
                value: MultipartValue::Text("a photo".into()),
            },
            MultipartField {
                enabled: true,
                name: "file".into(),
                value: MultipartValue::File(PathBuf::from("/tmp/pic.png")),
            },
        ]);

        let command = to_command(&spec);
        assert!(command.contains("-F 'caption=a photo'"), "{command}");
        assert!(command.contains("-F 'file=@/tmp/pic.png'"), "{command}");
    }

    #[test]
    fn a_binary_body_exports_as_a_file_reference_not_its_contents() {
        // Only the path is ever held (see `RequestView::binary_path`), and a command that inlined
        // a 2GB upload would be useless anyway.
        let mut spec = plain("https://x.test/upload");
        spec.http_mut().unwrap().method = Method::Put;
        spec.http_mut().unwrap().body = Body::Binary(PathBuf::from("/tmp/blob.bin"));

        assert!(to_command(&spec).contains("--data-binary '@/tmp/blob.bin'"));
    }

    #[test]
    fn only_settings_that_differ_from_curls_defaults_are_flagged() {
        // A spec at *Zuno's* defaults differs from curl in two places, and both are
        // wire-observable, so both must appear.
        let mut spec = RequestSpec::default();
        spec.url = "https://x.test/a".to_string();

        let command = to_command(&spec);
        assert!(command.contains("-L"), "redirects are on by default here: {command}");
        assert!(command.contains("--compressed"), "{command}");
        assert!(!command.contains("-k"), "TLS verification is on: {command}");
        assert!(
            !command.contains("--max-time"),
            "the default timeout is a local guard, not worth a flag: {command}"
        );

        spec.settings.verify_tls = false;
        assert!(to_command(&spec).contains("-k"));
    }

    #[test]
    fn a_chosen_timeout_is_exported_and_the_default_is_not() {
        let mut spec = plain("https://x.test/a");
        assert!(!to_command(&spec).contains("--max-time"));

        spec.settings.timeout = Some(std::time::Duration::from_secs(5));
        assert!(to_command(&spec).contains("--max-time 5"));
    }

    #[test]
    fn single_quotes_in_a_value_cannot_break_out_of_the_quoting() {
        // A shell-injection bug in a string the user is about to paste into a terminal. The
        // closing quote has to be escaped as '\'' — anything less and `; rm -rf /` would run.
        let mut spec = plain("https://x.test/a");
        spec.http_mut().unwrap().method = Method::Post;
        spec.http_mut().unwrap().body = Body::Raw {
            text: "it's '; echo pwned; '".to_string(),
            kind: RawKind::Text,
        };

        let command = to_command(&spec);

        // **Checked by re-tokenizing, not by counting quotes.** A first version of this test
        // asserted the rendered command had an even number of `'`, which is simply false: POSIX
        // has no escape inside single quotes, so embedding one means closing, emitting `\'`, and
        // reopening — `'\''` contributes three quotes, and correct output is routinely odd.
        //
        // `tokenize` is the right oracle. It is this module's own shell-word splitter, already
        // tested against real curl command lines, so a round trip through it proves the shell
        // would hand back exactly what went in — and pins the exporter's quoting to the
        // importer's parsing, which is the pair that could actually drift.
        let tokens = tokenize(&command).expect("the exported command must tokenize");
        assert!(
            tokens.iter().any(|token| token == "it's '; echo pwned; '"),
            "the payload must survive quoting intact, as one token: {tokens:?}"
        );
        assert!(
            !tokens.iter().any(|token| token.contains("echo pwned") && token != "it's '; echo pwned; '"),
            "nothing may escape into a separate word: {tokens:?}"
        );
    }

    #[test]
    fn the_command_is_multi_line_with_one_flag_per_line() {
        let mut spec = plain("https://x.test/a");
        spec.headers = vec![Header::new("A", "1"), Header::new("B", "2")];

        let command = to_command(&spec);
        let lines: Vec<&str> = command.lines().collect();
        assert_eq!(lines.len(), 3, "url + two headers: {command}");
        assert!(
            lines[..lines.len() - 1].iter().all(|line| line.ends_with(" \\")),
            "every line but the last continues: {command}"
        );
        assert!(lines[1].starts_with("  -H "), "{command}");
    }

    #[test]
    fn a_command_round_trips() {
        // The reason import and export share a file. Every flag the exporter emits must be one the
        // importer reads — anything else comes back in `ignored`, which is how the two drift.
        let mut original = plain("https://api.example.com/v1/things");
        original.http_mut().unwrap().method = Method::Post;
        original.headers = vec![
            Header::new("Content-Type", "application/json"),
            Header::new("X-Trace-Id", "abc123"),
        ];
        original.http_mut().unwrap().query = vec![QueryParam::new("page", "2")];
        original.http_mut().unwrap().body = Body::Raw {
            text: "{\"name\":\"ada\"}".to_string(),
            kind: RawKind::Json,
        };
        original.settings.verify_tls = false;
        original.settings.timeout = Some(std::time::Duration::from_secs(7));

        let command = to_command(&original);
        let back = parse(&command).expect("an exported command must re-import");

        assert!(
            back.ignored.is_empty(),
            "the exporter emitted flags the importer drops: {:?}\n{command}",
            back.ignored
        );
        assert_eq!(back.spec.http().unwrap().method, Method::Post);
        // The query row moved into the URL, which is `parse`'s documented choice — so compare
        // against the resolved URL rather than the raw one.
        assert_eq!(back.spec.url, "https://api.example.com/v1/things?page=2");
        assert_eq!(back.spec.http().unwrap().body, original.http().unwrap().body);
        assert!(!back.spec.settings.verify_tls, "-k must survive");
        assert_eq!(
            back.spec.settings.timeout,
            Some(std::time::Duration::from_secs(7)),
            "--max-time must survive"
        );
        assert_eq!(
            header_of(&back.spec, "X-Trace-Id"),
            Some("abc123"),
            "headers must survive"
        );
    }

    #[test]
    fn a_round_trip_preserves_the_two_settings_whose_polarity_differs() {
        // The trap `parse` records in the other direction: Zuno defaults redirects and encodings
        // *on*, curl defaults both off. An export that omitted the flags would come back with them
        // off, quietly changing what the request does.
        let spec = {
            let mut spec = RequestSpec::default();
            spec.url = "https://x.test/a".to_string();
            spec
        };
        let back = parse(&to_command(&spec)).expect("re-import").spec;

        assert!(back.settings.follow_redirects, "-L must survive the trip");
        assert!(back.settings.accept_encodings, "--compressed must survive");

        // And the inverse: a request that does *not* follow redirects must not acquire the habit.
        let mut off = spec.clone();
        off.settings.follow_redirects = false;
        off.settings.accept_encodings = false;
        let back = parse(&to_command(&off)).expect("re-import").spec;
        assert!(!back.settings.follow_redirects);
        assert!(!back.settings.accept_encodings);
    }

    #[test]
    fn a_multipart_command_round_trips() {
        let mut original = plain("https://x.test/upload");
        original.http_mut().unwrap().method = Method::Post;
        original.http_mut().unwrap().body = Body::Multipart(vec![
            MultipartField {
                enabled: true,
                name: "caption".into(),
                value: MultipartValue::Text("hello".into()),
            },
            MultipartField {
                enabled: true,
                name: "file".into(),
                value: MultipartValue::File(PathBuf::from("/tmp/pic.png")),
            },
        ]);

        let back = parse(&to_command(&original)).expect("re-import");
        assert!(back.ignored.is_empty(), "{:?}", back.ignored);
        assert_eq!(back.spec.http().unwrap().body, original.http().unwrap().body);
    }

    fn graphql(method: Method, query: &str, variables: &str) -> RequestSpec {
        RequestSpec {
            url: "https://api.test/graphql".to_string(),
            kind: RequestKind::GraphQl(GraphQlRequest {
                method,
                query: query.to_string(),
                variables: variables.to_string(),
                operation: None,
                transport: Default::default(),
                schema: String::new(),
                connection_params: String::new(),
            }),
            ..RequestSpec::default()
        }
    }

    /// **Exported as the envelope that goes on the wire, not as the two editors it was typed
    /// in.** `query` and `variables` mean nothing to curl, so a command carrying them would
    /// paste into a terminal and fail.
    #[test]
    fn a_graphql_request_exports_the_envelope_curl_would_send() {
        let spec = graphql(Method::Post, "query Me { me { id } }", r#"{"x": 1}"#);
        let command = to_command(&spec);

        assert!(command.contains("-X POST"), "got {command}");
        let sent: serde_json::Value = {
            let start = command.find("--data-raw '").expect("a body") + "--data-raw '".len();
            let rest = &command[start..];
            let end = rest.find('\'').expect("a closing quote");
            serde_json::from_str(&rest[..end]).expect("the exported body must be valid JSON")
        };
        assert_eq!(sent["query"], "query Me { me { id } }");
        // As JSON, not as the quoted string it is held as — the same envelope `build` sends.
        assert_eq!(sent["variables"]["x"], 1);
    }

    /// A GET carries the envelope in the URL, and the exported command has to as well — a bare
    /// endpoint would return an error when pasted, which is the failure copy-as-curl exists to
    /// avoid.
    #[test]
    fn a_get_graphql_request_exports_its_envelope_in_the_url() {
        let spec = graphql(Method::Get, "{ me { id } }", "");
        let command = to_command(&spec);

        assert!(command.contains("query="), "the URL must carry the query: {command}");
        assert!(
            !command.contains("--data-raw"),
            "a GET must not also send a body: {command}"
        );
    }
}
