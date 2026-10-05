//! What a request looked like as it went out — for the response's Sent view.
//!
//! **Reconstructed, not captured off the wire, and the reason is reqwest's shape.** reqwest 0.13
//! adds its own headers inside a fixed tower stack — cookies, redirects, decompression, then
//! hyper — with no hook at that level; the only hook is around the raw connection, where an
//! HTTP/2 request is HPACK-compressed frames. So this reads the `reqwest::Request` Zuno *built*
//! (exactly, not guessed), then adds what the client will add by **the same fixed rules it
//! applies**, each read from its source:
//!
//! - `accept: */*` and `user-agent` — the client's default headers, inserted only where none is
//!   set (`execute_request`'s `Entry::Vacant`). `accept` comes from `ClientBuilder::new` itself,
//!   and was missed until the wire test below compared against what a server received.
//! - `accept-encoding` — tower-http's `Decompression`, only where none is set, spelled exactly
//!   as `AcceptEncoding::to_header_value` spells all four codecs on.
//! - `cookie` — reqwest's `CookieService`, only where none is set, from **our** jar: the same
//!   `CookieStore::cookies(url)` lookup reqwest makes, on the same store.
//! - `host` and `content-length` — hyper's framing.
//!
//! Every row says which of these put it there, so the view explains as well as shows.

use bytes::Bytes;
use http::header::{ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_LENGTH, COOKIE, HOST, USER_AGENT};
use reqwest::cookie::CookieStore as _;

use super::cookies::Jar;
use crate::RequestSpec;
use crate::response::{SentHeader, SentRequest, SentSource};

/// Every codec on, which is how `build_client` configures it when encodings are accepted.
const ACCEPTED_ENCODINGS: &str = "zstd,gzip,deflate,br";

/// Describe `request` as it will go out. `spec` is the *resolved* request it was built from, so
/// a header can be attributed to a typed row or to the Auth tab.
pub fn describe(request: &reqwest::Request, spec: &RequestSpec, jar: Option<&Jar>) -> SentRequest {
    let url = request.url();
    let auth = spec.auth_header().is_some();
    let typed = |name: &str| {
        spec.enabled_headers()
            .any(|header| header.name.trim().eq_ignore_ascii_case(name))
    };

    let mut headers: Vec<SentHeader> = request
        .headers()
        .iter()
        .map(|(name, value)| SentHeader {
            name: name.as_str().to_string(),
            value: String::from_utf8_lossy(value.as_bytes()).into_owned(),
            source: if auth && *name == AUTHORIZATION {
                SentSource::Auth
            } else if typed(name.as_str()) {
                SentSource::Typed
            } else {
                SentSource::Zuno
            },
        })
        .collect();

    let has = |name: &http::HeaderName| request.headers().contains_key(name);
    let mut add = |name: &str, value: String, source: SentSource| {
        headers.push(SentHeader {
            name: name.to_string(),
            value,
            source,
        })
    };

    // The client's two default headers: `accept` from `ClientBuilder::new`, `user-agent` from
    // `build_client`. Both only where the request set none.
    if !has(&ACCEPT) {
        add("accept", "*/*".into(), SentSource::Client);
    }
    if !has(&USER_AGENT) {
        add("user-agent", concat!("zuno/", env!("CARGO_PKG_VERSION")).into(), SentSource::Client);
    }
    if spec.settings.accept_encodings && !has(&ACCEPT_ENCODING) {
        add("accept-encoding", ACCEPTED_ENCODINGS.into(), SentSource::Client);
    }
    if let Some(jar) = jar.filter(|_| spec.settings.cookie_store && !has(&COOKIE))
        && let Some(cookies) = jar.cookies(url)
    {
        add(
            "cookie",
            String::from_utf8_lossy(cookies.as_bytes()).into_owned(),
            SentSource::CookieJar,
        );
    }
    if !has(&HOST)
        && let Some(host) = url.host_str()
    {
        let host = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        add("host", host, SentSource::Client);
    }

    let bytes = request.body().and_then(reqwest::Body::as_bytes);
    if let Some(bytes) = bytes
        && !bytes.is_empty()
        && !has(&CONTENT_LENGTH)
    {
        add("content-length", bytes.len().to_string(), SentSource::Client);
    }

    SentRequest {
        method: request.method().as_str().to_string(),
        url: url.to_string(),
        headers,
        body: bytes.map(|bytes| {
            Bytes::copy_from_slice(&bytes[..bytes.len().min(SentRequest::BODY_KEPT)])
        }),
        body_len: bytes.map(<[u8]>::len),
        body_streamed: request.body().is_some() && bytes.is_none(),
        redirected: false,
    }
}
