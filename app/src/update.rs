//! Noticing that a newer Zuno has been released.
//!
//! Deliberately **a notice and not an updater**. Zuno installs system-wide through `apt`, so
//! installing an update means root — and a *window* asking for a password is a different and
//! much larger thing than a terminal asking for one. The answer is `scripts/install.sh`, which
//! is one command for both installing and updating, so all this has to do is say that a new
//! version exists and hand over the command.
//!
//! **The check reads a redirect, not the GitHub API.** `releases/latest` answers `302` with the
//! tag in `location`, which needs no JSON parsing and — unlike `api.github.com` — has no
//! 60-request hourly limit shared across everyone behind one NAT. An office all launching at
//! nine o'clock would exhaust that limit, and the failure is silent by design, so the feature
//! would simply stop working for the people most likely to have it.
//!
//! Everything here fails to *nothing*. Offline, blocked by a firewall, rate-limited, answered
//! with something unparseable — all of them mean "say nothing". A convenience that can put an
//! error on screen is a liability, not a feature.

use gpui::SharedString;
use zuno_core::request::{RequestId, RequestSpec};
use zuno_core::response::ResponseData;

/// Where the check looks. The redirect this performs is the whole mechanism.
pub const LATEST_URL: &str = "https://github.com/aryanjha256/zuno/releases/latest";

/// Where `What's new` goes.
pub const RELEASES_URL: &str = "https://github.com/aryanjha256/zuno/releases";

/// What the menu copies.
///
/// The same string as `README.md` and the release notes, and
/// `the_install_command_matches_the_readme` reads the README to keep it that way — three
/// places carrying a command that must be identical is three places for one of them to rot,
/// and the one that rots silently is the one nobody runs.
pub const INSTALL_COMMAND: &str =
    "curl -fsSL https://raw.githubusercontent.com/aryanjha256/zuno/main/scripts/install.sh | sh";

/// The version this binary was built as.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// What the last check found.
///
/// `Unknown` is its own state rather than being folded into `Current`, for the reason
/// `Connection` and `SizeInfo::declared` are in core: "nobody looked" and "looked and found
/// nothing newer" are different claims, and only one of them should ever be reported as
/// "you are up to date".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Update {
    #[default]
    Unknown,
    Current,
    Available(String),
}

impl Update {
    /// The version to offer, or `None` when there is nothing to say.
    ///
    /// A dismissed version answers `None` until a *later* one is released — the dismissal is
    /// stored as the version it applied to rather than as a flag, so it expires on its own and
    /// there is no reset to forget.
    pub fn offered(&self, dismissed: Option<&str>) -> Option<&str> {
        let Update::Available(version) = self else {
            return None;
        };
        match dismissed {
            Some(dismissed) if !zuno_core::version::is_newer(version, dismissed) => None,
            _ => Some(version),
        }
    }
}

/// The request the check makes.
///
/// **Redirects off**, which is the point: following them lands on an HTML page that would have
/// to be scraped, while the 302 itself carries the answer in one header. It goes through
/// `Engine::send` like every other request rather than through a second HTTP client, so it
/// inherits the proxy and the trusted CAs — the corporate network that needs both is exactly
/// where a second client would silently fail.
pub fn latest_request(settings: zuno_core::request::RequestSettings) -> RequestSpec {
    RequestSpec {
        id: RequestId(0),
        url: LATEST_URL.to_string(),
        method: zuno_core::Method::Get,
        settings: zuno_core::request::RequestSettings {
            follow_redirects: false,
            ..settings
        },
        ..RequestSpec::default()
    }
}

/// The released version named by a `302`'s `location`, if there is one.
///
/// Pure, so the parsing is unit-testable without a socket — the network half is covered
/// separately, over a real one.
pub fn tag_from_response(response: &ResponseData) -> Option<String> {
    // Any 3xx: GitHub answers 302 today, and a move to 301 or 308 is not a reason to stop
    // noticing releases.
    if !(300..400).contains(&response.status) {
        return None;
    }
    let location = response
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("location"))?;
    let (_, tag) = location.value.rsplit_once("/tag/")?;
    let tag = tag.trim();
    // Parsed before being believed, so a redirect to something unexpected reports nothing
    // rather than putting an arbitrary string in the titlebar.
    zuno_core::version::parse(tag)?;
    Some(tag.trim_start_matches(['v', 'V']).to_string())
}

/// What the chip says.
///
/// Lowercase, matching `cookies on` and `proxy system` rather than shouting — a new release is
/// an offer, not a problem.
pub fn badge_label(version: &str) -> SharedString {
    format!("update {version}").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_core::request::Header;
    use zuno_core::response::{Connection, HttpVersion, SizeInfo, Timing};

    fn redirect(status: u16, location: Option<&str>) -> ResponseData {
        ResponseData {
            status,
            status_text: String::new(),
            version: HttpVersion::Http2,
            headers: location
                .map(|value| vec![Header::new("location", value)])
                .unwrap_or_default(),
            body: bytes::Bytes::new(),
            timing: Timing {
                connection: Connection::Unknown,
                ttfb: std::time::Duration::ZERO,
                total: std::time::Duration::ZERO,
            },
            size: SizeInfo::default(),
        }
    }

    #[test]
    fn the_tag_comes_out_of_the_location_header() {
        let response = redirect(
            302,
            Some("https://github.com/aryanjha256/zuno/releases/tag/v0.2.5"),
        );
        assert_eq!(tag_from_response(&response).as_deref(), Some("0.2.5"));
    }

    /// Header names arrive lowercased from `http::HeaderMap`, but nothing in the type says so,
    /// and a case-sensitive match would fail on a proxy that rewrote them.
    #[test]
    fn the_location_header_is_matched_case_insensitively() {
        let response = redirect(
            302,
            Some("https://github.com/aryanjha256/zuno/releases/tag/v0.3.0"),
        );
        let mut upper = response.clone();
        upper.headers[0].name = "Location".to_string();
        assert_eq!(tag_from_response(&upper).as_deref(), Some("0.3.0"));
    }

    #[test]
    fn a_non_redirect_says_nothing() {
        assert_eq!(tag_from_response(&redirect(200, None)), None);
        // A 404 carrying a stale location header must not be read as a release.
        assert_eq!(
            tag_from_response(&redirect(
                404,
                Some("https://github.com/aryanjha256/zuno/releases/tag/v9.9.9")
            )),
            None
        );
    }

    /// Every shape the network can answer with that is not a release. Each one must produce no
    /// notice at all rather than a chip reading something arbitrary.
    #[test]
    fn an_unexpected_redirect_says_nothing() {
        assert_eq!(tag_from_response(&redirect(302, None)), None);
        assert_eq!(
            tag_from_response(&redirect(302, Some("https://github.com/login"))),
            None
        );
        assert_eq!(
            tag_from_response(&redirect(302, Some(".../releases/tag/nightly"))),
            None
        );
        assert_eq!(tag_from_response(&redirect(302, Some(".../tag/"))), None);
    }

    #[test]
    fn nothing_is_offered_unless_a_version_is_available() {
        assert_eq!(Update::Unknown.offered(None), None);
        assert_eq!(Update::Current.offered(None), None);
        assert_eq!(
            Update::Available("0.3.0".into()).offered(None),
            Some("0.3.0")
        );
    }

    /// The dismissal stores the version it applied to, so it lapses by itself when a later one
    /// lands. A bool would need something to reset it, and the thing that forgets to reset it
    /// is a user who never sees another update.
    #[test]
    fn a_dismissal_covers_that_version_and_lapses_on_the_next() {
        let update = Update::Available("0.3.0".into());
        assert_eq!(update.offered(Some("0.3.0")), None);
        assert_eq!(update.offered(Some("0.4.0")), None);
        assert_eq!(update.offered(Some("0.2.9")), Some("0.3.0"));
    }

    /// The command in the README is the one people actually run. If this drifts, the app
    /// copies something that no longer exists and nothing else notices.
    #[test]
    fn the_install_command_matches_the_readme() {
        let readme = include_str!("../../README.md");
        assert!(
            readme.contains(INSTALL_COMMAND),
            "README.md does not carry the install command the update menu copies:\n  {INSTALL_COMMAND}"
        );
    }

    /// The same command again, in the file that every release page is built from.
    #[test]
    fn the_install_command_matches_the_release_notes() {
        let workflow = include_str!("../../.github/workflows/release.yml");
        assert!(
            workflow.contains(INSTALL_COMMAND),
            "release.yml does not carry the install command the update menu copies:\n  {INSTALL_COMMAND}"
        );
    }
}
