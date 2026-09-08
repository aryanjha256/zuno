//! Checking a response against what a request says it expects.
//!
//! **The comparison half of `capture`.** Both address a value with a JSONPath in the notation
//! `JsonOutline::path_to` emits, so a path you copied out of a response with `Alt+C` works in
//! either — and `capture::extract` is the one function that reads it, rather than a second
//! implementation that could drift.
//!
//! **The status is not an assertion.** Every request wants to check it, so a design where it
//! competes for a row in the table means every request carries a row saying the obvious.
//! `RequestSpec::expect_status` is its own slot; the table is for the body.

use serde::{Deserialize, Serialize};

use crate::json::JsonOutline;

/// How an asserted value is compared. Three, deliberately.
///
/// **No `<` or `>`.** `capture::extract` returns text, so a numeric comparison needs a parse and
/// a decision about whether `1.0` equals `1` — a real design question for a case nobody has asked
/// for. Adding one later is additive; guessing at it now is a semantic that has to be kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// The path resolves at all. `value` is ignored.
    Exists,
    Equals,
    Contains,
}

impl Op {
    pub fn label(self) -> &'static str {
        match self {
            Op::Exists => "exists",
            Op::Equals => "equals",
            Op::Contains => "contains",
        }
    }
}

/// One "the response must look like this" rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assertion {
    pub enabled: bool,
    /// JSONPath into the response body: `$.data.id`.
    pub path: String,
    pub op: Op,
    /// Compared as text, and unquoted the way `capture::extract` unquotes: assert `ok`, not
    /// `"ok"`. Ignored by `Op::Exists`.
    pub value: String,
}

impl Default for Assertion {
    fn default() -> Self {
        Self {
            enabled: true,
            path: String::new(),
            op: Op::Exists,
            value: String::new(),
        }
    }
}

/// Why a response did not match. Typed and renderable, like `EngineError`: a run that reports
/// "3 failed" and cannot say what failed is a run you have to repeat by hand.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    #[error("expected status {expected}, got {actual}")]
    Status { expected: u16, actual: u16 },
    #[error("{path} needs a JSON body, and this response has none")]
    NotJson { path: String },
    #[error("{path} matched nothing")]
    Missing { path: String },
    #[error("{path} is {actual:?}, expected it to {} {expected:?}", op.label())]
    Mismatch {
        path: String,
        op: Op,
        expected: String,
        actual: String,
    },
}

/// Check the status, when the request states one.
pub fn check_status(expect: Option<u16>, actual: u16) -> Option<Failure> {
    expect.filter(|expected| *expected != actual).map(|expected| Failure::Status {
        expected,
        actual,
    })
}

/// Check one assertion. `outline` is `None` when the body was not JSON.
pub fn check(outline: Option<&JsonOutline>, assertion: &Assertion) -> Option<Failure> {
    let path = assertion.path.clone();

    let Some(outline) = outline else {
        return Some(Failure::NotJson { path });
    };
    let Some(actual) = crate::capture::extract(outline, &path) else {
        return Some(Failure::Missing { path });
    };

    let matched = match assertion.op {
        Op::Exists => true,
        Op::Equals => actual == assertion.value,
        Op::Contains => actual.contains(&assertion.value),
    };

    (!matched).then(|| Failure::Mismatch {
        path,
        op: assertion.op,
        expected: assertion.value.clone(),
        actual,
    })
}

/// Every failure in one response, status first.
///
/// **Disabled rows and half-typed ones are skipped here rather than by each caller**, which is
/// the mistake the equivalent filter in the capture runner invites — a check the caller forgets
/// to skip is a run that fails on a row you were still typing.
pub fn check_all(
    outline: Option<&JsonOutline>,
    expect: Option<u16>,
    status: u16,
    assertions: &[Assertion],
) -> Vec<Failure> {
    let mut failures = Vec::new();
    failures.extend(check_status(expect, status));

    for assertion in assertions {
        if !assertion.enabled || assertion.path.trim().is_empty() {
            continue;
        }
        failures.extend(check(outline, assertion));
    }

    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn outline(json: &str) -> JsonOutline {
        JsonOutline::parse(Bytes::from(json.to_string())).expect("valid json")
    }

    fn rule(path: &str, op: Op, value: &str) -> Assertion {
        Assertion {
            path: path.to_string(),
            op,
            value: value.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_status_is_checked_only_when_the_request_states_one() {
        assert_eq!(check_status(None, 500), None, "a request with no expectation cannot fail");
        assert_eq!(check_status(Some(200), 200), None);
        assert_eq!(
            check_status(Some(200), 401),
            Some(Failure::Status { expected: 200, actual: 401 })
        );
    }

    #[test]
    fn the_three_operators_do_what_they_say() {
        let doc = outline(r#"{"status":"ok","id":42,"tags":["a","b"]}"#);

        assert_eq!(check(Some(&doc), &rule("$.status", Op::Exists, "")), None);
        assert_eq!(check(Some(&doc), &rule("$.status", Op::Equals, "ok")), None);
        assert_eq!(check(Some(&doc), &rule("$.status", Op::Contains, "o")), None);
        // Numbers compare as the text they were written as, which is what `extract` returns.
        assert_eq!(check(Some(&doc), &rule("$.id", Op::Equals, "42")), None);
        assert_eq!(check(Some(&doc), &rule("$.tags[1]", Op::Equals, "b")), None);
    }

    #[test]
    fn a_value_is_compared_unquoted_the_way_capture_extracts_it() {
        // The rule that keeps the two halves interchangeable: a path copied with `Alt+C` and a
        // value read off the screen must assert without anyone quoting them by hand.
        let doc = outline(r#"{"status":"ok"}"#);
        assert_eq!(check(Some(&doc), &rule("$.status", Op::Equals, "ok")), None);
        assert!(
            check(Some(&doc), &rule("$.status", Op::Equals, "\"ok\"")).is_some(),
            "the JSON quotes are not part of the value"
        );
    }

    #[test]
    fn a_failure_says_what_it_found_not_just_that_it_failed() {
        let doc = outline(r#"{"status":"error"}"#);

        // "3 assertions failed" with no detail is a run you have to repeat by hand.
        let failure = check(Some(&doc), &rule("$.status", Op::Equals, "ok")).expect("a failure");
        let text = failure.to_string();
        assert!(text.contains("$.status"), "{text}");
        assert!(text.contains("error"), "names what it actually found: {text}");
        assert!(text.contains("ok"), "and what was asked for: {text}");

        let missing = check(Some(&doc), &rule("$.nope", Op::Exists, "")).expect("a failure");
        assert!(missing.to_string().contains("$.nope"));
    }

    #[test]
    fn a_body_assertion_on_a_non_json_response_is_a_failure_not_a_pass() {
        // The dangerous alternative is treating "no body to check" as nothing to check: a run
        // that goes green because the endpoint returned HTML is worse than one that goes red.
        let failure = check(None, &rule("$.id", Op::Exists, "")).expect("a failure");
        assert!(matches!(failure, Failure::NotJson { .. }), "{failure:?}");
    }

    #[test]
    fn check_all_skips_disabled_and_half_typed_rows() {
        let doc = outline(r#"{"status":"ok"}"#);
        let assertions = vec![
            Assertion { enabled: false, ..rule("$.nope", Op::Exists, "") },
            // Still being typed. Failing a run on it would make the table unusable to author in.
            rule("   ", Op::Exists, ""),
            rule("$.status", Op::Equals, "ok"),
        ];

        assert!(check_all(Some(&doc), Some(200), 200, &assertions).is_empty());
    }

    #[test]
    fn check_all_reports_the_status_first_and_then_every_body_failure() {
        // Every failure, not the first: a run exists to tell you everything that is wrong in one
        // pass, and stopping at the first means running it again to find the second.
        let doc = outline(r#"{"status":"error"}"#);
        let assertions = vec![
            rule("$.status", Op::Equals, "ok"),
            rule("$.missing", Op::Exists, ""),
        ];

        let failures = check_all(Some(&doc), Some(200), 401, &assertions);
        assert_eq!(failures.len(), 3);
        assert!(matches!(failures[0], Failure::Status { .. }), "status leads: {failures:?}");
        assert!(matches!(failures[1], Failure::Mismatch { .. }));
        assert!(matches!(failures[2], Failure::Missing { .. }));
    }
}
