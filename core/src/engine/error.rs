//! Engine failures, as data the UI can render inline.
//!
//! Every variant owns its data — no borrowed `&str`, no `reqwest::Error` held by
//! reference — so `EngineError` is `Clone` and can travel through the event channel
//! and sit in view state. That costs a few allocations on the failure path, which is
//! exactly the path where allocations don't matter.

use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EngineError {
    #[error("enter a URL first")]
    EmptyUrl,

    #[error("{url:?} is not a valid URL: {reason}")]
    InvalidUrl { url: String, reason: String },

    /// A `{{template}}` placeholder that nothing has substituted.
    ///
    /// This needs its own variant because `Url::parse` happily accepts
    /// `https://{{baseUrl}}/users` — it reads the placeholder as a hostname. Without
    /// this check the user gets "could not connect to {{baseurl}}" after a pointless
    /// DNS lookup, instead of being told what's actually wrong.
    // Deliberately free of keystrokes. Naming `Ctrl+E` here would put a keybinding in `core`,
    // which cannot see the keymap and so cannot know if it has been rebound — the same class of
    // stale claim this message used to make about milestones.
    #[error(
        "{{{{{name}}}}} is not defined (in {location}) — add it to an environment, or select one that defines it"
    )]
    UnresolvedVariable { name: String, location: String },

    #[error("{scheme}:// is not an HTTP scheme — use http or https")]
    UnsupportedScheme { scheme: String },

    #[error("{method:?} is not a valid HTTP method")]
    InvalidMethod { method: String },

    #[error("{name:?} is not a valid header name")]
    InvalidHeaderName { name: String },

    #[error("the value for header {name:?} contains characters that cannot be sent")]
    InvalidHeaderValue { name: String, value: String },

    #[error("could not read the body file {path}: {reason}")]
    BodyFileUnreadable { path: PathBuf, reason: String },

    /// Could not turn a valid-looking spec into a request. Distinct from a network
    /// failure: nothing left the machine.
    #[error("could not build the request: {reason}")]
    Build { reason: String },

    #[error("could not connect to {host}: {reason}")]
    Connect { host: String, reason: String },

    #[error("timed out after {:.1}s", after.as_secs_f64())]
    Timeout { after: Duration },

    #[error("TLS handshake failed: {reason}")]
    Tls { reason: String },

    #[error("too many redirects")]
    TooManyRedirects,

    #[error("the response body ended early: {reason}")]
    IncompleteBody { reason: String },

    /// The response is larger than Zuno will hold in memory.
    ///
    /// Distinct from `body_view::MAX_AUTO_PARSE`, which caps the *index* built for display and
    /// falls back to a raw view. This caps the transfer itself, because until it existed nothing
    /// did: the body streamed into an unbounded `Vec<u8>`, and `HISTORY_LIMIT` then retained up to
    /// eleven of them per buffer.
    ///
    /// Not `is_local`: the request went out and the server answered, so a retry is not free.
    #[error(
        "the response body is at least {} MB, over the {} MB Zuno will hold in memory",
        size / (1024 * 1024),
        limit / (1024 * 1024)
    )]
    BodyTooLarge { limit: usize, size: usize },

    #[error("request failed: {reason}")]
    Other { reason: String },
}

impl EngineError {
    /// Classify a `reqwest::Error` into something worth showing a person.
    ///
    /// reqwest's own `Display` is often a nest of "error sending request for url
    /// (...): error trying to connect: ...", so the useful signal gets buried. This
    /// pulls out the category and the innermost cause.
    pub fn from_reqwest(error: &reqwest::Error, timeout: Option<Duration>) -> Self {
        if error.is_timeout() {
            return EngineError::Timeout {
                after: timeout.unwrap_or_default(),
            };
        }
        if error.is_redirect() {
            return EngineError::TooManyRedirects;
        }

        let reason = root_cause(error);
        let host = error
            .url()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_else(|| "the server".to_string());

        if error.is_connect() {
            // rustls surfaces certificate problems through the connect path, so
            // separate them out — "certificate verify failed" and "connection
            // refused" call for very different fixes.
            if reason.to_ascii_lowercase().contains("certificate")
                || reason.to_ascii_lowercase().contains("tls")
            {
                return EngineError::Tls { reason };
            }
            return EngineError::Connect { host, reason };
        }
        if error.is_body() || error.is_decode() {
            return EngineError::IncompleteBody { reason };
        }
        if error.is_builder() {
            return EngineError::Build { reason };
        }

        EngineError::Other { reason }
    }

    /// True when nothing was sent, so a retry is safe and free.
    pub fn is_local(&self) -> bool {
        matches!(
            self,
            EngineError::EmptyUrl
                | EngineError::InvalidUrl { .. }
                | EngineError::UnresolvedVariable { .. }
                | EngineError::UnsupportedScheme { .. }
                | EngineError::InvalidMethod { .. }
                | EngineError::InvalidHeaderName { .. }
                | EngineError::InvalidHeaderValue { .. }
                | EngineError::BodyFileUnreadable { .. }
                | EngineError::Build { .. }
        )
    }
}

/// Walk to the innermost cause, which is where the actual reason lives.
fn root_cause(error: &(dyn std::error::Error + 'static)) -> String {
    let mut current = error;
    while let Some(source) = current.source() {
        current = source;
    }
    current.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_failures_are_distinguishable_from_network_ones() {
        assert!(EngineError::EmptyUrl.is_local());
        assert!(
            EngineError::InvalidHeaderName {
                name: "bad header".into()
            }
            .is_local()
        );
        assert!(
            !EngineError::Timeout {
                after: Duration::from_secs(30)
            }
            .is_local()
        );
        assert!(
            !EngineError::Connect {
                host: "example.test".into(),
                reason: "refused".into()
            }
            .is_local()
        );
    }

    #[test]
    fn an_unresolved_variable_reads_back_as_a_placeholder() {
        // The braces are four levels of escaping in the format string, which is easy to get wrong
        // while editing the copy around them — and `{baseUrl}` instead of `{{baseUrl}}` would name
        // something the user never typed. This message used to end "environments arrive in M2",
        // which is the other way copy goes wrong: it aged.
        let error = EngineError::UnresolvedVariable {
            name: "baseUrl".into(),
            location: "the URL".into(),
        };
        let message = error.to_string();
        assert!(message.contains("{{baseUrl}}"), "{message}");
        assert!(message.contains("the URL"), "{message}");
    }

    #[test]
    fn messages_name_the_offending_input() {
        let error = EngineError::InvalidHeaderName {
            name: "has space".into(),
        };
        assert!(error.to_string().contains("has space"));

        let error = EngineError::UnsupportedScheme {
            scheme: "ftp".into(),
        };
        assert!(error.to_string().contains("ftp"));
    }
}
