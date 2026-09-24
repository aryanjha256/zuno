//! A runnable `curl` command — the answer to "here's the repro".
//!
//! `curl::parse` is the other direction and the pair is tested as one: every flag emitted here
//! is one the importer reads, or a round trip reports it as ignored (`a_command_round_trips`).
//!
//! Multi-line with `\` continuations, one flag per line, which is what devtools emits and what
//! reads as a repro in an issue.

use super::{PartValue, Wire, WireBody, shell_quote};

pub(super) fn render(wire: &Wire) -> String {
    let mut parts: Vec<String> = vec![format!("curl {}", shell_quote(&wire.url))];

    // curl infers POST from a body, so `-X` is redundant for a plain POST — but emitting it
    // always is what makes the round trip exact, and it is how devtools writes it. The one case
    // it is load-bearing rather than decorative is a GET *with* a body, where omitting it would
    // silently turn the request into a POST.
    if wire.method != "GET" || wire.has_body() {
        parts.push(format!("-X {}", wire.method));
    }

    for (name, value) in &wire.headers {
        parts.push(format!("-H {}", shell_quote(&header(name, value))));
    }
    // **curl labels a body with no type as a form**, and Zuno sends a file body with none. An
    // empty `Content-Type:` is curl's way of saying "not that one", so the command sends what Zuno
    // sends. Found by running the output, not by reading it.
    if matches!(wire.body, WireBody::File(_)) && !has_content_type(wire) {
        parts.push(format!("-H {}", shell_quote("Content-Type:")));
    }

    // Only flags that are **wire-observable and differ from curl's own default**, which is the
    // same line `parse` draws in the other direction (architecture.md §10, M1.5).
    //
    // Deliberately absent, each for a recorded reason:
    // - `--max-redirs`: `parse` already decided this isn't worth faithfulness, and emitting a flag
    //   the importer doesn't read would make every exported command report an ignored flag on the
    //   way back in.
    // - the cookie jar: `cookie_store` is an in-process jar shared per client config. curl's `-b`
    //   and `-c` are *files*. There is no flag that means "the jar this app happens to hold", and
    //   inventing one would export a request that behaves differently.
    if wire.follow_redirects {
        parts.push("-L".to_string());
    }
    if wire.compressed {
        parts.push("--compressed".to_string());
    }
    if !wire.verify_tls {
        parts.push("-k".to_string());
    }
    if let Some(timeout) = wire.timeout {
        parts.push(format!("--max-time {}", timeout.as_secs()));
    }

    match &wire.body {
        WireBody::None => {}
        // `--data-raw`, never `-d`: `-d` strips newlines and treats a leading `@` as a filename,
        // so a JSON body starting with `@` or spanning lines would be mangled. A form arrives
        // here already encoded by `encode_form`, which keeps it byte-exact with what Zuno sends —
        // one `--data-urlencode` per field would let curl do the encoding, and curl only encodes
        // after the `=`.
        WireBody::Text(text) => parts.push(format!("--data-raw {}", shell_quote(text))),
        // `--data-binary`, because `-d`/`--data` would strip newlines out of a binary file.
        WireBody::File(path) => parts.push(format!(
            "--data-binary {}",
            shell_quote(&format!("@{}", path.display()))
        )),
        WireBody::Multipart(fields) => {
            for part in fields {
                let value = match &part.value {
                    PartValue::Text(text) => format!("{}={text}", part.name),
                    // `@` is curl's own file syntax, and it derives the filename from the path
                    // exactly as the engine does — so the part arrives with the same name.
                    PartValue::File(path) => format!("{}=@{}", part.name, path.display()),
                };
                parts.push(format!("-F {}", shell_quote(&value)));
            }
        }
    }

    parts.join(" \\\n  ")
}

/// A header as curl spells it — shared with PHP's curl extension, which is the same library.
///
/// **An empty value is `Name;`**, because `Name:` with nothing after it tells curl to *remove*
/// the header. Exported as `Name: `, a header typed with an empty value silently never left.
pub(super) fn header(name: &str, value: &str) -> String {
    if value.trim().is_empty() {
        format!("{name};")
    } else {
        format!("{name}: {value}")
    }
}

pub(super) fn has_content_type(wire: &Wire) -> bool {
    wire.headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
}
