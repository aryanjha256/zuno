//! Recovering what a Postman test script was checking.
//!
//! Scripts are JavaScript and no importer will ever run them. But the three shapes that make up
//! most real ones map exactly onto things Zuno already has — `pm.environment.set` onto a
//! `Capture`, a status check onto `expect_status`, `pm.expect(…).to.eql` onto an `Assertion` — so
//! this is a pattern matcher over a closed set of forms, not an interpreter.
//!
//! **The governing rule: a wrong recovery is far worse than no recovery.** A rule nobody wrote
//! makes a run fail — or worse, pass — for a reason that is nowhere in the collection, and the
//! person has no way to know Zuno invented it. So every shape below is matched *whole* or not at
//! all, and anything unmatched is returned verbatim in `unread` to be reported. There is no
//! partial credit and nothing is inferred from a name.
//!
//! **A guarded statement is refused, not recovered without its guard.** `if (code === 200) {
//! pm.environment.set("token", d.token) }` is extremely common, and Zuno has no condition on a
//! capture — so recovering it means dropping the `if`, which is a rule the person did not write.
//! Often the guard is *redundant* here (a capture that matches nothing is reported and writes
//! nothing), but "often" is not a basis for inventing rules, and the conservative call is also
//! the reversible one: the line is reported, so re-adding it is a click. One rule, no exceptions
//! — a matcher with a list of guards it feels safe about is a matcher nobody can predict.
//!
//! **Only `listen: "test"` scripts come here.** A `prerequest` script is setup that runs *before*
//! a response exists — `pm.environment.set("ts", Date.now())` is dynamic state, not a capture —
//! and Zuno has no pre-request hook to put it in, so those stay reported rather than mined.

use crate::assertion::{Assertion, Op};
use crate::capture::Capture;

/// What one script yielded.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Recovered {
    /// `None` when the script checked no status. The **first** check wins: a second one with a
    /// different value is a contradiction, and it goes to `unread` so the conflict is visible
    /// rather than resolved by whichever line happened to be last.
    pub expect_status: Option<u16>,
    pub captures: Vec<Capture>,
    pub assertions: Vec<Assertion>,
    /// Lines that matched no known shape, trimmed. Scaffolding is *not* in here — see
    /// `is_scaffolding`.
    pub unread: Vec<String>,
}

impl Recovered {
    pub fn is_empty(&self) -> bool {
        self.expect_status.is_none() && self.captures.is_empty() && self.assertions.is_empty()
    }

    pub fn count(&self) -> usize {
        usize::from(self.expect_status.is_some()) + self.captures.len() + self.assertions.len()
    }
}

/// Read a script's `exec` lines.
pub fn recover(exec: &[String]) -> Recovered {
    // The most common real capture is two lines — `var jsonData = pm.response.json();` then
    // `pm.environment.set("t", jsonData.token)` — so the path lives on a *local variable*. One
    // pass collecting those names is the difference between recovering a fifth of real captures
    // and recovering most of them, and it is bounded: a name is either bound to the parsed body
    // or it is not a path this can follow.
    let bound = body_bindings(exec);

    let mut out = Recovered::default();

    // One entry per open brace, saying whether *that* brace was opened by a line this could not
    // read. A depth counter is not enough: a `pm.test(…, function () {` wrapper opens a block
    // too, and it has to be transparent — counting it as a guard refuses the check inside every
    // well-written script there is.
    let mut blocks: Vec<bool> = Vec::new();

    for raw in exec {
        let line = strip_comment(raw).trim();
        let inside_guard = blocks.iter().any(|&guarded| guarded);

        let consumed = line.is_empty() || binds_body(line).is_some() || is_scaffolding(line);
        // `is_guarded` catches the one-line form, whose braces balance so no block is ever left
        // open: `if (x) { pm.environment.set(...) }`.
        let opens_guard = !consumed
            && (inside_guard || is_guarded(line) || !read_line(line, &bound, &mut out));
        if opens_guard {
            out.unread.push(line.to_string());
        }

        for brace in braces(line) {
            if brace == '{' {
                blocks.push(opens_guard);
            } else {
                blocks.pop();
            }
        }
    }
    out
}

/// The braces of a line that are structure, skipping any inside a string.
///
/// `pm.expect(d.tpl).to.eql("{}")` would otherwise open a block that never closes, and every
/// line after it in the script would be refused.
fn braces(line: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut quote: Option<char> = None;
    for ch in line.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if matches!(ch, '"' | '\'' | '`') => quote = Some(ch),
            None if matches!(ch, '{' | '}') => out.push(ch),
            None => {}
        }
    }
    out
}

/// Does control flow govern this line?
///
/// A keyword test rather than a parse, and deliberately blunt: anything it is unsure about is
/// refused, which is the safe direction here.
fn is_guarded(line: &str) -> bool {
    const KEYWORDS: [&str; 7] = ["if", "else", "for", "while", "switch", "catch", "do"];
    let line = line.trim_start();
    KEYWORDS.iter().any(|keyword| {
        line.strip_prefix(keyword)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['(', ' ', '{']))
    })
}

/// Try every known shape against one line. `false` means nothing matched.
fn read_line(line: &str, bound: &[String], out: &mut Recovered) -> bool {
    if let Some(status) = status_in(line) {
        return match out.expect_status {
            None => {
                out.expect_status = Some(status);
                true
            }
            // Saying it twice is harmless; saying it differently is a contradiction Zuno's one
            // slot cannot hold, so the loser is reported rather than dropped.
            Some(existing) => existing == status,
        };
    }

    if let Some((name, subject)) = variable_set_in(line) {
        let Some(path) = json_path(&subject, bound) else {
            return false;
        };
        out.captures.push(Capture {
            path,
            name,
            ..Capture::default()
        });
        return true;
    }

    if let Some(assertion) = expectation_in(line, bound) {
        out.assertions.push(assertion);
        return true;
    }

    false
}

/// Names bound to the parsed response body: `var jsonData = pm.response.json();`.
fn body_bindings(exec: &[String]) -> Vec<String> {
    exec.iter()
        .filter_map(|line| binds_body(strip_comment(line)))
        .collect()
}

/// The name this line binds to the response body, if it does.
fn binds_body(line: &str) -> Option<String> {
    let line = line.trim();
    let rest = ["var ", "let ", "const "]
        .iter()
        .find_map(|keyword| line.strip_prefix(keyword))?;
    let (name, value) = rest.split_once('=')?;
    let name = name.trim();

    // `JSON.parse(responseBody)` is the pre-`pm` spelling and still extremely common in older
    // collections, so both roots are recognised here and in `json_path`.
    let value = value.trim().trim_end_matches(';').trim();
    let is_body = value == "pm.response.json()"
        || value == "JSON.parse(responseBody)"
        || value == "JSON.parse(pm.response.text())";

    (is_body && !name.is_empty() && name.chars().all(is_ident_char)).then(|| name.to_string())
}

/// Is this line the block a check sits in rather than a check?
///
/// `pm.test("name", function () {` and its closing `});` are scaffolding, and reporting them as
/// unrecovered would bury the lines that actually matter under boilerplate — which is the same
/// failure as reporting nothing, since nobody reads a list that is mostly noise.
fn is_scaffolding(line: &str) -> bool {
    let dense: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if dense.is_empty() {
        return true;
    }
    // A `pm.test(…)` header whose body is on the following lines. One with the check on the
    // same line is *not* scaffolding, and `read_line` sees it whole — the shapes it looks for
    // are substrings, so the wrapper does not need removing.
    if let Some(open) = dense.find("function(){").or_else(|| dense.find("=>{")) {
        let after = &dense[open..];
        if let Some(body) = after.split_once('{') {
            return body.1.trim_matches(|c| matches!(c, '}' | ')' | ';')).is_empty();
        }
    }
    dense
        .chars()
        .all(|c| matches!(c, '{' | '}' | '(' | ')' | ';' | ',' | '[' | ']'))
}

/// The status a line checks for.
fn status_in(line: &str) -> Option<u16> {
    // `pm.response.to.have.status(201)`. A *name* — `status("Created")` is legal — is left
    // unread: mapping names to codes is a table of guesses, and `expect_status` holds a number.
    if let Some(args) = call_args(line, ".to.have.status(") {
        return args.first()?.parse().ok();
    }

    // `pm.expect(pm.response.code).to.eql(200)`, and the pre-`pm` `responseCode.code === 200`.
    let subject_is_code = ["pm.expect(pm.response.code)", "pm.expect(pm.response.status)"]
        .iter()
        .any(|form| line.contains(form));
    if subject_is_code {
        for opener in [".to.eql(", ".to.equal(", ".to.be.eql("] {
            if let Some(args) = call_args(line, opener) {
                return args.first()?.parse().ok();
            }
        }
        return None;
    }

    if line.contains("responseCode.code") || line.contains("pm.response.code") {
        let (_, rest) = line.split_once("==")?;
        return rest
            .trim_start_matches('=')
            .trim()
            .trim_end_matches([';', ')', '}'])
            .trim()
            .parse()
            .ok();
    }

    None
}

/// The variable a line writes, and the expression it writes from.
fn variable_set_in(line: &str) -> Option<(String, String)> {
    // Every scope Postman offers writes into the one place Zuno has. `environment`, `globals` and
    // `collectionVariables` differ in *where* Postman keeps them, and Zuno's answer to all three
    // is the active environment — a distinction it does not model, so flattening it is the
    // faithful reading rather than a loss.
    let openers = [
        "pm.environment.set(",
        "pm.globals.set(",
        "pm.collectionVariables.set(",
        "pm.variables.set(",
        "postman.setEnvironmentVariable(",
        "postman.setGlobalVariable(",
    ];

    let args = openers
        .iter()
        .find_map(|opener| call_args(line, opener))?;
    let [name, subject, ..] = args.as_slice() else {
        return None;
    };
    let name = unquote(name);
    (!name.is_empty()).then(|| (name, subject.clone()))
}

/// The assertion a `pm.expect(…)` line makes.
fn expectation_in(line: &str, bound: &[String]) -> Option<Assertion> {
    let subject = call_args(line, "pm.expect(")?.first()?.clone();
    let chain = line.split_once("pm.expect(").map(|(_, rest)| rest)?;

    // `to.have.property("id")` addresses a *child* of the subject, so the path grows rather
    // than the operator changing.
    if let Some(args) = call_args(chain, ".to.have.property(") {
        let key = unquote(args.first()?);
        let path = json_path(&format!("{subject}.{key}"), bound)?;
        return Some(Assertion {
            path,
            op: Op::Exists,
            ..Assertion::default()
        });
    }

    let path = json_path(&subject, bound)?;

    for opener in [".to.eql(", ".to.equal(", ".to.be.eql(", ".to.deep.equal("] {
        if let Some(args) = call_args(chain, opener) {
            return Some(Assertion {
                path,
                op: Op::Equals,
                value: unquote(args.first()?),
                enabled: true,
            });
        }
    }
    for opener in [".to.include(", ".to.contain(", ".to.have.string("] {
        if let Some(args) = call_args(chain, opener) {
            return Some(Assertion {
                path,
                op: Op::Contains,
                value: unquote(args.first()?),
                enabled: true,
            });
        }
    }
    // Every spelling of "there is something here". `to.be.ok` is deliberately absent: it is
    // truthiness, so it fails on `0`, `""` and `false`, and `Exists` passes on all three.
    for form in [".to.exist", ".to.not.be.undefined", ".to.not.be.null"] {
        if chain.contains(form) {
            return Some(Assertion {
                path,
                op: Op::Exists,
                ..Assertion::default()
            });
        }
    }

    None
}

/// A JavaScript expression into the notation `capture::extract` reads, or `None`.
///
/// `None` is the important half: `pm.response.responseTime`, `pm.response.text()` and anything
/// touching `.length` are all real subjects Zuno has no path for, and a guess at any of them is a
/// rule the person never wrote.
///
/// **`.length` is the trap here, because it reads exactly like a key.** `pm.environment.set(
/// "user_count", response.length)` is an ordinary line in an ordinary collection, and translating
/// it to `$.length` gives a capture that matches nothing on an array — or, on an object that
/// happens to carry a `length` member, captures the wrong value. A body genuinely keyed `length`
/// loses its capture and is reported, which is the right side of that trade: this was found by
/// running a real collection through, not by reading the code.
fn json_path(expression: &str, bound: &[String]) -> Option<String> {
    let expression = expression.trim();

    let mut rest = None;
    for root in ["pm.response.json()", "JSON.parse(responseBody)"] {
        if let Some(after) = expression.strip_prefix(root) {
            rest = Some(after);
            break;
        }
    }
    if rest.is_none() {
        for name in bound {
            if let Some(after) = expression.strip_prefix(name.as_str())
                && (after.is_empty() || after.starts_with(['.', '[']))
            {
                rest = Some(after);
                break;
            }
        }
    }
    let mut rest = rest?;

    let mut path = String::from("$");
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            let end = after.find(['.', '[']).unwrap_or(after.len());
            let key = &after[..end];
            if key.is_empty() || !key.chars().all(is_ident_char) || is_js_property(key) {
                return None;
            }
            path.push('.');
            path.push_str(key);
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let end = after.find(']')?;
            let inner = after[..end].trim();
            if inner.starts_with('"') || inner.starts_with('\'') {
                let key = unquote(inner);
                // `d["length"]` and `d.length` are the same access in JavaScript, so the
                // bracketed form is refused too rather than read as a statement of intent.
                if is_js_property(&key) {
                    return None;
                }
                // Bracketed and requoted the way `JsonOutline::path_to` writes an awkward key,
                // because `capture::extract` is what has to read it back.
                path.push_str(&format!("[\"{key}\"]"));
            } else if inner.chars().all(|c| c.is_ascii_digit()) && !inner.is_empty() {
                path.push_str(&format!("[{inner}]"));
            } else {
                // A computed index — `items[i]` — has no single answer.
                return None;
            }
            rest = &after[end + 1..];
        } else {
            return None;
        }
    }

    Some(path)
}

/// The arguments of the call that `opener` starts, split on top-level commas.
///
/// Written out rather than reached for with a regex because the arguments nest and quote:
/// `pm.environment.set("t", jsonData.data[0].id)` has a comma inside no brackets and a dot path
/// that must survive whole, and `pm.expect(x).to.eql("a, b")` has a comma inside a string.
fn call_args(text: &str, opener: &str) -> Option<Vec<String>> {
    let start = text.find(opener)? + opener.len();
    let body = &text[start..];

    let mut args = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;

    for ch in body.chars() {
        if let Some(q) = quote {
            current.push(ch);
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                quote = Some(ch);
                current.push(ch);
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' | '}' if depth == 0 => {
                args.push(current.trim().to_string());
                return Some(args);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    // Unterminated: the call runs past the end of the line, so nothing here is trustworthy.
    None
}

/// Drop a trailing `//` comment.
///
/// Quote-aware, because a `//` inside a string is not a comment — and a URL in an assertion
/// (`pm.expect(d.url).to.eql("https://a.test")`) puts one there in the most ordinary way
/// possible. Getting this wrong truncates the line and turns a good match into an unread one.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == b'"' || ch == b'\'' || ch == b'`' => quote = Some(ch),
            None if ch == b'/' && bytes.get(i + 1) == Some(&b'/') => return &line[..i],
            None => {}
        }
        i += 1;
    }
    line
}

/// A JavaScript property on the parsed body that is not a member of it.
///
/// One name, because `length` is the only one that turns up in real test scripts *and* reads as a
/// plausible JSON key. `constructor` and friends are neither. Adding more later is additive;
/// guessing at a list now is a semantic to keep.
fn is_js_property(key: &str) -> bool {
    key == "length"
}

fn is_ident_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

/// Strip one layer of JS quotes. A bare expression comes back unchanged, which is what lets the
/// callers hand anything to this without asking first.
fn unquote(text: &str) -> String {
    let text = text.trim();
    for quote in ['"', '\'', '`'] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return text[1..text.len() - 1].to_string();
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(lines: &[&str]) -> Recovered {
        recover(&lines.iter().map(|line| line.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn every_spelling_of_a_status_check_is_recognised() {
        // Four forms in the wild, and the last two predate `pm` entirely. Missing one means a
        // request that Postman checked arrives unchecked.
        for line in [
            "pm.response.to.have.status(201);",
            "pm.test(\"created\", () => { pm.response.to.have.status(201); });",
            "pm.expect(pm.response.code).to.eql(201);",
            "tests[\"created\"] = responseCode.code === 201;",
        ] {
            assert_eq!(
                read(&[line]).expect_status,
                Some(201),
                "not recognised: {line}"
            );
        }

        // A status *name* is legal in Postman and has no number to store, so it is left unread
        // rather than mapped through a table of guesses.
        let named = read(&["pm.response.to.have.status(\"Created\");"]);
        assert_eq!(named.expect_status, None);
        assert_eq!(named.unread.len(), 1);
    }

    #[test]
    fn a_second_status_check_that_disagrees_is_reported_rather_than_silently_losing() {
        // Zuno has one slot. Saying 200 twice is harmless; saying 200 and then 201 is a
        // contradiction, and resolving it by line order would be a rule nobody wrote.
        let agreeing = read(&[
            "pm.response.to.have.status(200);",
            "pm.expect(pm.response.code).to.eql(200);",
        ]);
        assert_eq!(agreeing.expect_status, Some(200));
        assert!(agreeing.unread.is_empty(), "{:?}", agreeing.unread);

        let conflicting = read(&[
            "pm.response.to.have.status(200);",
            "pm.response.to.have.status(201);",
        ]);
        assert_eq!(conflicting.expect_status, Some(200));
        assert_eq!(conflicting.unread.len(), 1, "{:?}", conflicting.unread);
    }

    #[test]
    fn a_capture_is_recovered_through_the_local_variable_real_scripts_use() {
        // The two-line form is the common one, and handling only the one-liner would recover a
        // small fraction of real captures.
        let direct = read(&["pm.environment.set(\"token\", pm.response.json().access_token);"]);
        assert_eq!(direct.captures.len(), 1);
        assert_eq!(direct.captures[0].path, "$.access_token");

        let via_variable = read(&[
            "var jsonData = pm.response.json();",
            "pm.environment.set(\"id\", jsonData.data[0].id);",
        ]);
        assert_eq!(via_variable.captures.len(), 1, "{:?}", via_variable);
        assert_eq!(via_variable.captures[0].path, "$.data[0].id");
        assert_eq!(via_variable.captures[0].name, "id");
        // The binding line is consumed, not reported.
        assert!(via_variable.unread.is_empty(), "{:?}", via_variable.unread);

        // `JSON.parse(responseBody)` is the pre-`pm` spelling and still very common.
        let legacy = read(&[
            "var d = JSON.parse(responseBody);",
            "postman.setEnvironmentVariable(\"t\", d.token);",
        ]);
        assert_eq!(legacy.captures.len(), 1, "{:?}", legacy);
        assert_eq!(legacy.captures[0].path, "$.token");

        // Every scope Postman offers writes into the one place Zuno has.
        for setter in [
            "pm.globals.set(\"t\", pm.response.json().t);",
            "pm.collectionVariables.set(\"t\", pm.response.json().t);",
        ] {
            assert_eq!(read(&[setter]).captures.len(), 1, "not recognised: {setter}");
        }
    }

    #[test]
    fn each_assertion_operator_maps_to_the_one_zuno_has() {
        let cases = [
            ("pm.expect(jsonData.status).to.eql(\"open\");", Op::Equals, "open"),
            ("pm.expect(jsonData.status).to.equal(\"open\");", Op::Equals, "open"),
            ("pm.expect(jsonData.name).to.include(\"Ali\");", Op::Contains, "Ali"),
            ("pm.expect(jsonData.name).to.contain(\"Ali\");", Op::Contains, "Ali"),
            ("pm.expect(jsonData.id).to.exist;", Op::Exists, ""),
        ];
        for (line, op, value) in cases {
            let out = read(&["var jsonData = pm.response.json();", line]);
            assert_eq!(out.assertions.len(), 1, "not recognised: {line}");
            assert_eq!(out.assertions[0].op, op, "{line}");
            assert_eq!(out.assertions[0].value, value, "{line}");
        }

        // `to.have.property("x")` addresses a *child*, so the path grows rather than the
        // operator changing — asserting `Exists` on the parent would pass on any response at all.
        let property = read(&[
            "var jsonData = pm.response.json();",
            "pm.expect(jsonData.data).to.have.property(\"id\");",
        ]);
        assert_eq!(property.assertions.len(), 1, "{property:?}");
        assert_eq!(property.assertions[0].path, "$.data.id");
        assert_eq!(property.assertions[0].op, Op::Exists);
    }

    #[test]
    fn a_shape_zuno_has_no_rule_for_is_left_unread_rather_than_approximated() {
        // The governing rule of the whole module. Each of these is a real assertion someone
        // wrote, and each has *no* faithful translation — a rule Zuno invented here would fail
        // a run for a reason that is nowhere in the collection.
        let lines = [
            // No numeric comparison exists: `Op` is Exists/Equals/Contains on purpose.
            "pm.expect(jsonData.items.length).to.be.above(0);",
            // `.length` is a JavaScript property, not a member of the body — and it reads
            // exactly like a key, which is what made this ship wrong. Found by running a real
            // collection through, not by reading the code.
            "pm.environment.set(\"user_count\", jsonData.length);",
            "pm.environment.set(\"n\", jsonData.items.length);",
            "pm.environment.set(\"n\", jsonData[\"length\"]);",
            // Not the response body at all.
            "pm.expect(pm.response.responseTime).to.be.below(500);",
            "pm.expect(pm.response.headers.get(\"Content-Type\")).to.include(\"json\");",
            // Truthiness, which fails on 0, \"\" and false where `Exists` passes.
            "pm.expect(jsonData.count).to.be.ok;",
            // A computed index has no single answer.
            "pm.expect(jsonData.items[i].id).to.eql(\"x\");",
            // A condition Zuno cannot hold. Dropping it would be a rule nobody wrote.
            "if (jsonData.next) { pm.environment.set(\"next\", jsonData.next); }",
        ];
        for line in lines {
            let out = read(&["var jsonData = pm.response.json();", line]);
            assert!(
                out.is_empty(),
                "{line} must not be approximated, got {out:?}"
            );
            assert_eq!(out.unread.len(), 1, "and must be reported: {line}");
        }
    }

    #[test]
    fn a_statement_inside_a_conditional_block_is_refused_too() {
        // The shape that actually appears in real collections. Recovering the capture and
        // dropping the `if` would give the request a rule that fires unconditionally, which is
        // not what the script said — so the block is reported and the run stays honest.
        let out = read(&[
            "if (pm.response.code === 200) {",
            "    var d = pm.response.json();",
            "    pm.environment.set(\"token\", d.token);",
            "}",
        ]);
        assert!(out.captures.is_empty(), "{out:?}");
        // The guard *and* the statement it governs, so the report shows what to re-add.
        assert_eq!(out.unread.len(), 2, "{:?}", out.unread);

        // And the block ending must restore recovery, or one conditional silences the rest of
        // the script.
        let after = read(&[
            "if (x) {",
            "    doSomething();",
            "}",
            "pm.response.to.have.status(200);",
        ]);
        assert_eq!(after.expect_status, Some(200), "{after:?}");
    }

    #[test]
    fn scaffolding_is_not_reported_as_an_unread_line() {
        // A list that is mostly `});` is a list nobody reads, which is the same outcome as
        // reporting nothing at all.
        let out = read(&[
            "pm.test(\"Status code is 200\", function () {",
            "    pm.response.to.have.status(200);",
            "});",
            "",
            "// a comment",
            "pm.test(\"body\", () => {",
            "    pm.expect(pm.response.json().ok).to.eql(true);",
            "});",
        ]);
        assert_eq!(out.expect_status, Some(200));
        assert_eq!(out.assertions.len(), 1);
        assert!(out.unread.is_empty(), "{:?}", out.unread);
    }

    #[test]
    fn a_url_in_an_asserted_value_is_not_read_as_a_comment() {
        // `//` inside a string is not a comment, and truncating there turns a good match into an
        // unread line — in the most ordinary case there is.
        let out = read(&[
            "pm.expect(pm.response.json().url).to.eql(\"https://a.test/x\");",
        ]);
        assert_eq!(out.assertions.len(), 1, "{out:?}");
        assert_eq!(out.assertions[0].value, "https://a.test/x");

        // And a real trailing comment still goes.
        let commented = read(&["pm.response.to.have.status(200); // the happy path"]);
        assert_eq!(commented.expect_status, Some(200));
        assert!(commented.unread.is_empty(), "{:?}", commented.unread);
    }

    #[test]
    fn a_key_that_is_not_an_identifier_is_bracketed_the_way_extract_reads_it() {
        // `capture::extract` is the one reader of these paths, and it wants an awkward key
        // bracketed and quoted the way `JsonOutline::path_to` writes one.
        let out = read(&[
            "pm.environment.set(\"t\", pm.response.json()[\"access-token\"]);",
        ]);
        assert_eq!(out.captures.len(), 1, "{out:?}");
        assert_eq!(out.captures[0].path, "$[\"access-token\"]");
    }
}
