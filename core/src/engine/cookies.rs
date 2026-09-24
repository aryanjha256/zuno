//! The cookie jar: one per engine, shared by every client, and readable.
//!
//! **One jar, not one per client, and that fixed a bug nobody had noticed.** reqwest's
//! `cookie_store(true)` gives each client a private jar, and clients are cached per settings —
//! timeout, redirects, TLS verification, HTTP version. So a login request at the defaults and a
//! follow-up with a five-second timeout did not share a session: the second was sent without the
//! cookie the first had just received, and a gRPC call never saw any. Every client that stores
//! cookies now gets this jar through `cookie_provider`, as a browser or Postman would behave.
//!
//! **Readable is the other half.** The private jar could not be listed, so nothing could show
//! what was stored, and clearing it meant dropping every cached client — and with them every
//! pooled connection. This one is listed by `snapshot` and emptied in place.

use std::sync::{Arc, MutexGuard, PoisonError};

use reqwest_cookie_store::{CookieStore, CookieStoreMutex};

/// The jar every cookie-storing client shares.
pub type Jar = Arc<CookieStoreMutex>;

pub fn new_jar() -> Jar {
    Arc::new(CookieStoreMutex::default())
}

/// One stored cookie, as the viewer shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCookie {
    /// The host it was set by, or the domain it covers — see `host_only`.
    pub domain: String,
    pub path: String,
    pub name: String,
    pub value: String,
    /// Unix seconds, or `None` for a session cookie, which ends when Zuno does.
    pub expires: Option<i64>,
    /// Set without a `Domain` attribute, so sent to exactly that host and not its subdomains.
    pub host_only: bool,
    pub secure: bool,
    pub http_only: bool,
}

/// **A poisoned lock is still a jar.** The mutex is only ever held to read or write a map, so a
/// panic elsewhere while it was held leaves the data intact; refusing it would lose every cookie
/// over something that did not touch them.
fn lock(jar: &Jar) -> MutexGuard<'_, CookieStore> {
    jar.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every live cookie, sorted by domain, then path, then name — the order someone scanning for one
/// site's session reads in. Expired cookies are left out: the store keeps them until they are
/// overwritten, but they are never sent, so showing one would describe a request that does not
/// happen.
pub fn snapshot(jar: &Jar) -> Vec<StoredCookie> {
    let store = lock(jar);
    let mut cookies: Vec<StoredCookie> = store
        .iter_unexpired()
        .map(|cookie| StoredCookie {
            domain: String::from(&cookie.domain),
            path: String::from(&cookie.path),
            name: cookie.name().to_string(),
            value: cookie.value().to_string(),
            expires: match &cookie.expires {
                cookie_store::CookieExpiration::AtUtc(at) => Some(at.unix_timestamp()),
                cookie_store::CookieExpiration::SessionEnd => None,
            },
            host_only: matches!(cookie.domain, cookie_store::CookieDomain::HostOnly(_)),
            secure: cookie.secure().unwrap_or(false),
            http_only: cookie.http_only().unwrap_or(false),
        })
        .collect();
    cookies.sort_by(|a, b| {
        (a.domain.as_str(), a.path.as_str(), a.name.as_str())
            .cmp(&(b.domain.as_str(), b.path.as_str(), b.name.as_str()))
    });
    cookies
}

/// Forget one cookie. `false` if it was already gone — a response may have replaced or expired
/// it between the list being drawn and the key being pressed.
pub fn remove(jar: &Jar, cookie: &StoredCookie) -> bool {
    lock(jar)
        .remove(&cookie.domain, &cookie.path, &cookie.name)
        .is_some()
}

pub fn clear(jar: &Jar) {
    lock(jar).clear();
}
