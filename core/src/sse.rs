//! Server-sent events, parsed incrementally.
//!
//! **A pure state machine over bytes**, with no I/O and no knowledge of the engine, because the
//! interesting half of SSE is the framing and the framing is where the mistakes are. It is fed
//! whatever a chunk of the response body happened to contain — which is regularly half a line —
//! and answers with whichever events that completed.
//!
//! The rules are WHATWG's, and three of them are easy to get wrong by reading the format rather
//! than the spec: a line may end with `\n`, `\r\n` **or a bare `\r`**; an event with no `data`
//! is *not* dispatched, which is what makes a comment-only keep-alive invisible; and `id`
//! persists across events while `event` resets after each one.

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The `event:` field. `None` means the stream did not name one, which the spec calls
    /// `message` — kept as `None` rather than filled in, so a transcript can show the
    /// difference between a stream that names its events and one that does not.
    pub name: Option<String>,
    /// The last `id:` seen, which persists across events until the stream sets another.
    pub id: Option<String>,
    /// Every `data:` line of this event, joined with newlines and with the trailing one removed.
    pub data: String,
}

/// A ceiling on what one event, or one unterminated line, may accumulate.
///
/// **A stream declares no length**, so the engine's `max_body_bytes` guard — which reads
/// `Content-Length` and then watches a growing buffer — never applies to SSE. Without this a
/// server that sends `data:` forever with no blank line, or simply no newline at all, grows a
/// `String` until the process dies. 8 MiB matches the WebSocket's `MAX_FRAME_BYTES`, because a
/// frame and an event are the same thing to everything downstream.
pub const DEFAULT_LIMIT: usize = 8 * 1024 * 1024;

pub struct Parser {
    /// Bytes that did not end in a line terminator yet.
    pending: Vec<u8>,
    data: String,
    name: Option<String>,
    /// **Not reset between events**, unlike `name` and `data`: the spec's last-event-id is
    /// stream state, and it is what a reconnect would replay from.
    last_id: Option<String>,
    /// The server's requested reconnection delay in milliseconds, if it sent one. Nothing
    /// reconnects yet; it is read so that the field is not silently swallowed as an unknown.
    pub retry: Option<u64>,
    limit: usize,
    /// Set once something exceeded `limit`, carrying how far it got. Terminal: parsing stops
    /// and the caller tears the stream down.
    too_large: Option<usize>,
}

/// `Default` is the capped parser, not an unbounded one — a caller that forgets is the case
/// this guard exists for.
impl Default for Parser {
    fn default() -> Self {
        Self::with_limit(DEFAULT_LIMIT)
    }
}

impl Parser {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            data: String::new(),
            name: None,
            last_id: None,
            retry: None,
            limit,
            too_large: None,
        }
    }

    /// How far an oversized event got, once one has been seen.
    ///
    /// **Read after every `push`.** The parser has no error channel — it answers with the
    /// events a chunk completed — so this is the only way the overflow surfaces, and ignoring
    /// it means the stream simply stops producing frames with nothing saying why.
    pub fn too_large(&self) -> Option<usize> {
        self.too_large
    }

    /// Feed one chunk. Returns the events it completed, which is usually none.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Event> {
        if self.too_large.is_some() {
            return Vec::new();
        }

        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some((line, consumed)) = next_line(&self.pending) {
            let line = String::from_utf8_lossy(&line).into_owned();
            self.pending.drain(..consumed);
            if let Some(event) = self.line(&line) {
                events.push(event);
            }
            if self.too_large.is_some() {
                self.pending.clear();
                return events;
            }
        }

        // **Checked after the loop, because an unterminated line never reaches `line`.** A
        // stream that sends megabytes with no terminator in them leaves every byte sitting in
        // `pending`, which is the cheapest way to exhaust memory here and the one a per-event
        // check alone would miss entirely.
        if self.pending.len() > self.limit {
            self.too_large = Some(self.pending.len());
            self.pending.clear();
        }

        events
    }

    /// Forget what a dropped connection left half-said.
    ///
    /// **`last_id` and `retry` survive, everything else does not.** Those two are stream state —
    /// the id is what the next connection resumes from and the delay is what the server asked
    /// for — while a partial line and a half-built event belong to the connection that died.
    /// Carrying them over would splice the tail of one message onto the head of another.
    pub fn reset_between_connections(&mut self) {
        self.pending.clear();
        self.data.clear();
        self.name = None;
    }

    /// The last `id:` the stream sent, which a reconnect resumes from.
    pub fn last_id(&self) -> Option<&str> {
        self.last_id.as_deref()
    }

    /// Whatever is left when the stream ends.
    ///
    /// A server that closes without a trailing blank line has still said something, and
    /// dropping it would lose the last event of every such stream.
    ///
    /// **The unterminated tail is discarded, though.** It used to be fed through `line` as if it
    /// were complete, so a connection cut mid-line turned `data: {"a":1` into an event holding
    /// invalid JSON. Half a field is not a field; only the lines that actually arrived count.
    pub fn finish(&mut self) -> Option<Event> {
        self.pending.clear();
        self.dispatch()
    }

    fn line(&mut self, line: &str) -> Option<Event> {
        if line.is_empty() {
            return self.dispatch();
        }
        // A comment. Servers send these as keep-alives, which is precisely why an event with no
        // data must not be dispatched — otherwise every heartbeat becomes a blank row.
        if line.starts_with(':') {
            return None;
        }

        let (field, value) = match line.split_once(':') {
            // "A single leading space is removed", not all whitespace — `data:  x` means ` x`.
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };

        match field {
            "event" => self.name = Some(value.to_string()),
            "data" => {
                let grown = self.data.len() + value.len() + 1;
                if grown > self.limit {
                    self.too_large = Some(grown);
                    self.data.clear();
                    return None;
                }
                self.data.push_str(value);
                self.data.push('\n');
            }
            // A NUL makes the id unusable as a header value on reconnect, so the spec drops it.
            "id" if !value.contains('\0') => self.last_id = Some(value.to_string()),
            // **Assigned only on success.** `parse().ok()` behind a digits-only guard looked
            // equivalent and was not: a value that is all digits and too large for `u64` passes
            // the guard, parses to `None`, and *wipes* a delay the server had already set.
            "retry" => {
                if let Ok(delay) = value.parse::<u64>() {
                    self.retry = Some(delay);
                }
            }
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<Event> {
        // **Empty data is not an event.** This is the rule that keeps keep-alives out of the
        // transcript, and it also means `event: ping` on its own produces nothing.
        if self.data.is_empty() {
            self.name = None;
            return None;
        }

        let mut data = std::mem::take(&mut self.data);
        if data.ends_with('\n') {
            data.pop();
        }

        Some(Event {
            name: self.name.take(),
            id: self.last_id.clone(),
            data,
        })
    }
}

/// The next complete line, and how many bytes it used.
///
/// `None` while the terminator has not arrived — including for a trailing `\r`, which cannot be
/// resolved until the next byte is known: it is a line on its own unless a `\n` follows, and a
/// chunk boundary lands between the two often enough to matter.
fn next_line(bytes: &[u8]) -> Option<(Vec<u8>, usize)> {
    let at = bytes.iter().position(|b| *b == b'\n' || *b == b'\r')?;
    match bytes[at] {
        b'\n' => Some((bytes[..at].to_vec(), at + 1)),
        _ => match bytes.get(at + 1) {
            Some(b'\n') => Some((bytes[..at].to_vec(), at + 2)),
            Some(_) => Some((bytes[..at].to_vec(), at + 1)),
            None => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(chunks: &[&str]) -> Vec<Event> {
        let mut parser = Parser::default();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.push(chunk.as_bytes()));
        }
        events
    }

    #[test]
    fn a_plain_event_carries_its_data_and_no_name() {
        let events = all(&["data: hello\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[0].name, None, "an unnamed event stays unnamed");
    }

    /// **The rule that keeps a stream readable.** Servers send `:` comments every few seconds to
    /// hold the connection open; dispatching them would fill the transcript with blank rows.
    #[test]
    fn comments_and_empty_events_dispatch_nothing() {
        assert!(all(&[": keep-alive\n\n"]).is_empty());
        assert!(all(&["event: ping\n\n"]).is_empty(), "a name with no data is not an event");
        assert!(all(&["\n\n\n"]).is_empty());
    }

    #[test]
    fn data_lines_join_with_newlines_and_lose_only_the_last() {
        let events = all(&["data: one\ndata: two\ndata:\n\n"]);
        assert_eq!(events[0].data, "one\ntwo\n", "a blank data line is a blank line, not nothing");
    }

    /// Only one space is stripped, so indented JSON survives.
    #[test]
    fn a_single_leading_space_is_removed_and_no_more() {
        assert_eq!(all(&["data:  indented\n\n"])[0].data, " indented");
        assert_eq!(all(&["data:tight\n\n"])[0].data, "tight");
    }

    /// **The id persists, the name does not.** Getting this backwards makes every later event
    /// inherit a name it never had.
    #[test]
    fn the_id_carries_forward_and_the_name_resets() {
        let events = all(&["id: 7\nevent: add\ndata: a\n\n", "data: b\n\n"]);
        assert_eq!(events[0].name.as_deref(), Some("add"));
        assert_eq!(events[0].id.as_deref(), Some("7"));
        assert_eq!(events[1].name, None, "the name must not leak into the next event");
        assert_eq!(events[1].id.as_deref(), Some("7"), "but the id is stream state");
    }

    /// A chunk boundary can land anywhere, including between a `\r` and its `\n`.
    #[test]
    fn a_line_split_across_chunks_still_parses() {
        assert_eq!(all(&["data: hel", "lo\n\n"])[0].data, "hello");
        assert_eq!(all(&["data: x\r", "\n\r\n"])[0].data, "x", "CRLF split across chunks");
        assert_eq!(all(&["data: a\rdata: b\n\n"])[0].data, "a\nb", "a bare CR ends a line");
    }

    /// **A trailing `\r` must not be treated as a line yet**, and this is the case a reading of
    /// the format rather than the spec gets wrong. It is a terminator on its own *unless* the
    /// next byte is `\n` — so until that byte arrives there is no way to know whether the line
    /// has ended once or is about to end once. A chunk boundary lands there often enough that
    /// guessing produces a phantom blank line, which dispatches an event early.
    #[test]
    fn a_trailing_carriage_return_waits_for_the_byte_that_decides_it() {
        let mut parser = Parser::default();
        assert!(
            parser.push(b"data: x\r").is_empty(),
            "the \\r could still be half of a CRLF"
        );
        assert!(parser.push(b"\n").is_empty(), "it was — and one line is not an event");
        assert_eq!(
            parser.push(b"\n").len(),
            1,
            "the blank line after it is what dispatches"
        );
    }

    #[test]
    fn retry_is_read_and_a_bad_one_is_ignored() {
        let mut parser = Parser::default();
        parser.push(b"retry: 2500\ndata: x\n\n");
        assert_eq!(parser.retry, Some(2500));
        parser.push(b"retry: soon\ndata: y\n\n");
        assert_eq!(parser.retry, Some(2500), "a non-numeric retry leaves the old one alone");

        // **All digits and still unusable.** This is the case a digits-only guard waves through
        // and `parse().ok()` then turns into `None`, silently discarding a delay the server had
        // already asked for.
        parser.push(b"retry: 99999999999999999999999\ndata: z\n\n");
        assert_eq!(
            parser.retry,
            Some(2500),
            "a retry too large to parse must not wipe the one that worked"
        );
    }

    /// A stream that ends without its trailing blank line has still said something — but a
    /// stream cut **mid-line** has not.
    ///
    /// The difference is the whole of `finish`. A complete `data:` line with no blank line after
    /// it is an event the server finished writing; `data: {"a":1` with the connection gone is
    /// half a field, and emitting it produces a row of invalid JSON that looks like the server's
    /// fault.
    #[test]
    fn a_complete_final_line_survives_and_a_cut_one_does_not() {
        let mut whole = Parser::default();
        assert!(whole.push(b"data: last\n").is_empty());
        assert_eq!(whole.finish().expect("the final event").data, "last");

        let mut cut = Parser::default();
        assert!(cut.push(b"data: {\"a\":1").is_empty());
        assert!(
            cut.finish().is_none(),
            "half a line is not an event, however tempting the bytes look"
        );
    }

    /// **The two ways an event stream can exhaust memory**, and the second is the one a
    /// per-event check alone would miss.
    ///
    /// A stream declares no length, so nothing upstream bounds it: the engine's `max_body_bytes`
    /// watches a buffer that a stream never fills. So a server that sends `data:` forever with
    /// no dispatching blank line grows `data`, and one that sends bytes with no line terminator
    /// at all grows `pending` without a single line ever reaching `line`.
    #[test]
    fn an_event_that_never_ends_is_refused_rather_than_held() {
        let mut growing = Parser::with_limit(64);
        let events = growing.push(b"data: ");
        assert!(events.is_empty());
        // No blank line anywhere, so nothing ever dispatches and every chunk is retained.
        for _ in 0..8 {
            assert!(growing.push(b"x".repeat(16).as_slice()).is_empty());
            assert!(growing.push(b"\ndata: ").is_empty());
        }
        assert!(
            growing.too_large().is_some(),
            "an event accumulating past the limit must be refused, not held"
        );
        assert!(
            growing.push(b"data: more\n\n").is_empty(),
            "once refused it stays refused — the stream is being torn down"
        );

        let mut unterminated = Parser::with_limit(64);
        assert!(unterminated.push(b"data: ".as_slice()).is_empty());
        assert!(unterminated.push(b"y".repeat(200).as_slice()).is_empty());
        assert!(
            unterminated.too_large().is_some(),
            "a line with no terminator is held whole, so it needs the same ceiling"
        );

        // The limit is a ceiling, not a target: an ordinary stream must be untouched by it.
        let mut ordinary = Parser::with_limit(64);
        let events = ordinary.push(b"data: hi\n\n");
        assert_eq!(events.len(), 1);
        assert!(ordinary.too_large().is_none());
    }
}
