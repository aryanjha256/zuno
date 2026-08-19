//! Comparing a response against the previous run.
//!
//! This exists to answer one question in the edit → resend → compare loop: *did my
//! change do anything?* That's a summary question, so this is a summary diff — status,
//! timing, size, which headers moved, whether the body is byte-identical. A full inline
//! body diff is a different feature with a different UI, and building it here would
//! bury the signal.

use std::collections::BTreeMap;

use crate::response::ResponseData;

/// Headers that differ on every single request and carry no signal about whether *your*
/// change mattered. Excluding them is what keeps "3 headers changed" meaningful rather
/// than permanently true.
const VOLATILE_HEADERS: &[&str] = &[
    "date",
    "age",
    "x-request-id",
    "request-id",
    "cf-ray",
    "x-amzn-requestid",
    "x-amz-request-id",
    "x-served-by",
    "x-timer",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseDiff {
    /// `Some((previous, current))` only when the status actually moved.
    pub status: Option<(u16, u16)>,
    /// Current minus previous, in milliseconds. Negative means faster.
    pub duration_delta_ms: i64,
    /// Current minus previous, in decoded bytes.
    pub size_delta: i64,
    pub headers_added: Vec<String>,
    pub headers_removed: Vec<String>,
    pub headers_changed: Vec<String>,
    pub body_changed: bool,
    /// Current minus previous line count. Only meaningful when the body changed.
    pub line_delta: i64,
}

impl ResponseDiff {
    pub fn between(previous: &ResponseData, current: &ResponseData) -> Self {
        let status = (previous.status != current.status).then_some((previous.status, current.status));

        let before = header_map(previous);
        let after = header_map(current);

        let mut headers_added = Vec::new();
        let mut headers_changed = Vec::new();
        for (name, values) in &after {
            match before.get(name) {
                None => headers_added.push(name.clone()),
                Some(old) if old != values => headers_changed.push(name.clone()),
                Some(_) => {}
            }
        }
        let headers_removed = before
            .keys()
            .filter(|name| !after.contains_key(*name))
            .cloned()
            .collect();

        let body_changed = previous.body != current.body;

        Self {
            status,
            duration_delta_ms: millis(current.timing.total) - millis(previous.timing.total),
            size_delta: current.size.decoded as i64 - previous.size.decoded as i64,
            headers_added,
            headers_removed,
            headers_changed,
            body_changed,
            line_delta: count_lines(&current.body) - count_lines(&previous.body),
        }
    }

    /// True when nothing worth reporting moved. Timing and size always wobble, so
    /// neither counts on its own.
    pub fn is_quiet(&self) -> bool {
        self.status.is_none()
            && !self.body_changed
            && self.headers_added.is_empty()
            && self.headers_removed.is_empty()
            && self.headers_changed.is_empty()
    }

    pub fn header_change_count(&self) -> usize {
        self.headers_added.len() + self.headers_removed.len() + self.headers_changed.len()
    }
}

/// Lowercased name -> its values in received order, excluding volatile headers.
///
/// A `BTreeMap` rather than a hash map so `headers_added` and friends come out in a
/// stable order — a diff summary that reshuffles between runs is its own kind of noise.
fn header_map(response: &ResponseData) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for header in &response.headers {
        let name = header.name.to_ascii_lowercase();
        if VOLATILE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        map.entry(name).or_default().push(header.value.clone());
    }

    map
}

fn millis(duration: std::time::Duration) -> i64 {
    duration.as_millis() as i64
}

fn count_lines(body: &[u8]) -> i64 {
    if body.is_empty() {
        return 0;
    }
    let newlines = body.iter().filter(|byte| **byte == b'\n').count() as i64;
    // A body not ending in a newline still has a final line.
    if body.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;

    use super::*;
    use crate::request::Header;
    use crate::response::{HttpVersion, SizeInfo, Timing};

    fn response(status: u16, headers: Vec<Header>, body: &'static str, total_ms: u64) -> ResponseData {
        ResponseData {
            status,
            status_text: "OK".into(),
            version: HttpVersion::Http11,
            headers,
            body: Bytes::from_static(body.as_bytes()),
            timing: Timing {
                dns: None,
                connect: None,
                tls: None,
                ttfb: Duration::from_millis(total_ms / 2),
                total: Duration::from_millis(total_ms),
            },
            size: SizeInfo {
                wire: body.len() as u64,
                decoded: body.len() as u64,
            },
        }
    }

    #[test]
    fn an_identical_response_is_quiet() {
        let headers = vec![Header::new("content-type", "application/json")];
        let a = response(200, headers.clone(), "{\"a\":1}", 100);
        let b = response(200, headers, "{\"a\":1}", 140);

        let diff = ResponseDiff::between(&a, &b);
        assert!(diff.is_quiet(), "only timing moved: {diff:?}");
        assert_eq!(diff.duration_delta_ms, 40);
        assert_eq!(diff.size_delta, 0);
        assert!(!diff.body_changed);
    }

    #[test]
    fn a_status_change_is_reported_with_both_values() {
        let a = response(200, vec![], "ok", 10);
        let b = response(404, vec![], "ok", 10);

        let diff = ResponseDiff::between(&a, &b);
        assert_eq!(diff.status, Some((200, 404)));
        assert!(!diff.is_quiet());
    }

    #[test]
    fn a_changed_body_reports_size_and_line_deltas() {
        let a = response(200, vec![], "one\ntwo", 10);
        let b = response(200, vec![], "one\ntwo\nthree", 10);

        let diff = ResponseDiff::between(&a, &b);
        assert!(diff.body_changed);
        assert_eq!(diff.size_delta, 6);
        assert_eq!(diff.line_delta, 1);
    }

    #[test]
    fn volatile_headers_never_count_as_a_change() {
        // Otherwise every single resend would claim headers changed, and the signal
        // would be worthless.
        let a = response(
            200,
            vec![
                Header::new("date", "Mon, 01 Jan 2024 00:00:00 GMT"),
                Header::new("x-request-id", "aaa"),
                Header::new("content-type", "application/json"),
            ],
            "{}",
            10,
        );
        let b = response(
            200,
            vec![
                Header::new("date", "Tue, 02 Jan 2024 00:00:00 GMT"),
                Header::new("x-request-id", "bbb"),
                Header::new("content-type", "application/json"),
            ],
            "{}",
            10,
        );

        let diff = ResponseDiff::between(&a, &b);
        assert!(diff.is_quiet(), "{diff:?}");
        assert_eq!(diff.header_change_count(), 0);
    }

    #[test]
    fn header_names_are_compared_case_insensitively() {
        let a = response(200, vec![Header::new("Content-Type", "text/plain")], "x", 10);
        let b = response(200, vec![Header::new("content-type", "text/plain")], "x", 10);
        assert!(ResponseDiff::between(&a, &b).is_quiet());
    }

    #[test]
    fn added_removed_and_changed_headers_are_separated() {
        let a = response(
            200,
            vec![
                Header::new("content-type", "text/plain"),
                Header::new("x-gone", "1"),
            ],
            "x",
            10,
        );
        let b = response(
            200,
            vec![
                Header::new("content-type", "application/json"),
                Header::new("x-new", "1"),
            ],
            "x",
            10,
        );

        let diff = ResponseDiff::between(&a, &b);
        assert_eq!(diff.headers_changed, vec!["content-type"]);
        assert_eq!(diff.headers_added, vec!["x-new"]);
        assert_eq!(diff.headers_removed, vec!["x-gone"]);
        assert_eq!(diff.header_change_count(), 3);
    }

    #[test]
    fn duplicate_header_values_are_compared_as_a_sequence() {
        let a = response(
            200,
            vec![Header::new("set-cookie", "a=1"), Header::new("set-cookie", "b=2")],
            "x",
            10,
        );
        let b = response(200, vec![Header::new("set-cookie", "a=1")], "x", 10);

        let diff = ResponseDiff::between(&a, &b);
        assert_eq!(diff.headers_changed, vec!["set-cookie"]);
    }

    #[test]
    fn a_faster_response_reports_a_negative_delta() {
        let a = response(200, vec![], "x", 200);
        let b = response(200, vec![], "x", 80);
        assert_eq!(ResponseDiff::between(&a, &b).duration_delta_ms, -120);
    }

    #[test]
    fn line_counting_handles_trailing_newlines_and_empties() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a"), 1);
        assert_eq!(count_lines(b"a\n"), 1);
        assert_eq!(count_lines(b"a\nb"), 2);
        assert_eq!(count_lines(b"a\nb\n"), 2);
    }
}
