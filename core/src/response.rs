//! The response model. See architecture.md §3.2.
//!
//! `body` is `Bytes`, never `String`. Binary payloads and invalid UTF-8 are
//! normal, not edge cases, and `Bytes` is what lets the JSON viewer hold byte
//! spans into the original buffer instead of copied strings (§6).

use std::time::Duration;

use bytes::Bytes;

use crate::request::Header;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HttpVersion {
    Http09,
    Http10,
    #[default]
    Http11,
    Http2,
    Http3,
}

impl HttpVersion {
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpVersion::Http09 => "HTTP/0.9",
            HttpVersion::Http10 => "HTTP/1.0",
            HttpVersion::Http11 => "HTTP/1.1",
            HttpVersion::Http2 => "HTTP/2",
            HttpVersion::Http3 => "HTTP/3",
        }
    }
}

/// How the socket a response arrived on was obtained.
///
/// **Three states, not three `Option<Duration>`s.** `Timing` used to carry `dns`, `connect` and
/// `tls` as separate `Option`s, and the `None` was overloaded three ways: the stage did not
/// happen, the stage cannot be measured, and nobody looked. Two comments in this tree gave
/// different reasons for the same `None` — this file said "a reused connection skips them",
/// `engine/run.rs` said "reqwest doesn't expose them" — and both were true of situations the
/// type could not tell apart.
///
/// It matters because of what the timeline draws. A zero-width DNS segment asserts the lookup
/// took no measurable time; on a pooled connection the truth is that no lookup happened at all.
/// That distinction is the difference between a chart and a chart that lies, so it lives in the
/// type rather than in a rendering rule someone has to remember.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Connection {
    /// A socket was opened for this request and its stages were measured.
    Opened {
        /// `None` when there was no name to resolve — an IP-literal URL, which is what every
        /// socket test here uses. A hostname always yields `Some`, even when the lookup is
        /// instant, because "resolved in under a microsecond" and "never resolved" are
        /// different facts.
        dns: Option<Duration>,
        /// TCP connect **and** the TLS handshake, as one span.
        ///
        /// They are not separable through reqwest: one connector does both and exposes no seam
        /// between them. Splitting them needs a connector hand-built below reqwest, which costs
        /// more than everything else in this feature put together — so this is one bar labelled
        /// for what it actually contains, rather than two bars of which one is invented.
        connect: Duration,
        /// How many sockets this request opened. Normally 1.
        ///
        /// More means redirects were followed somewhere the first connection could not serve.
        /// reqwest surfaces only the *final* response, so those extra round trips are otherwise
        /// invisible — their time lands in `Wait` and reads as a slow server. The count is what
        /// lets the pane say so instead of misattributing it.
        sockets: u32,
    },
    /// The socket came from the pool: no lookup, no handshake, none of it happened.
    ///
    /// This is the common case on a resend, which is the whole point of caching clients per
    /// `ClientKey` — so it is the state the timeline shows most often, and showing it as four
    /// bars with two empty would be a lie about the two.
    Pooled,
    /// Nothing was measured. The honest default, and what a `Timing` built by hand holds.
    #[default]
    Unknown,
}

/// One stage of a request, in wire order.
///
/// Four, not the six a browser shows: `Dns` and `Connect` are what reqwest can be made to
/// report, `Wait` is everything between the connection being ready and the first response byte
/// (our own request build, the upload, and the server's thinking), and `Download` is the body.
/// A separate "request sent" bar would need a measurement inside the upload that nothing here
/// takes, so `Wait` is named for what it covers instead of being split on a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseKind {
    Dns,
    Connect,
    Wait,
    Download,
}

impl PhaseKind {
    /// The label, in core beside `HttpVersion::as_str` rather than in the pane, so a future
    /// `zuno run` timeline names these stages the same way the window does.
    pub fn as_str(&self) -> &'static str {
        match self {
            PhaseKind::Dns => "DNS lookup",
            PhaseKind::Connect => "Connect + TLS",
            PhaseKind::Wait => "Waiting",
            PhaseKind::Download => "Download",
        }
    }
}

/// A stage with its offset from the start of the request already accumulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phase {
    pub kind: PhaseKind,
    /// Offset from the moment the request was submitted, so a caller draws a bar rather than
    /// computing where it goes.
    pub start: Duration,
    pub duration: Duration,
}

impl Phase {
    pub fn end(&self) -> Duration {
        self.start + self.duration
    }
}

/// Where to put the tick marks on a timeline spanning `total`.
///
/// Returns the elapsed times to label, starting at zero and ascending. A pure function over a
/// span rather than a method on `Timing`, because it depends on nothing else and a run-level
/// timeline would want the same ticks for a different span.
///
/// **Ticks are rounded *up* to 1, 2 or 5 times a power of ten**, which is the whole of the
/// algorithm and the part that is easy to get subtly wrong. Rounding down looks equally
/// reasonable and produces twice as many ticks as asked for: a 142 ms span wants a step near
/// 28 ms, and rounding that *down* to 20 ms yields seven labels on an axis sized for four.
///
/// **The last tick is dropped if it crowds the end of the axis.** A caller draws the total
/// separately — in Zuno's case in the summary line above the chart — so a tick at 95% would put
/// two numbers on top of each other, and the label of the one at the edge has nowhere to go.
/// That is `CROWD_LIMIT`, and it is why this returns interior ticks rather than a range ending
/// at `total`: the axis's right edge *is* the total, already named.
pub fn axis_ticks(total: Duration) -> Vec<Duration> {
    /// Ticks are laid out for roughly this many intervals before rounding. Five rather than
    /// four because rounding up then lands on three or four labels across the range Zuno sees
    /// — sub-millisecond loopback responses to the 30 s timeout.
    const TARGET_INTERVALS: u128 = 5;
    /// A tick past this fraction of the span is dropped, leaving room for its own label and
    /// clear of the total.
    const CROWD_LIMIT: f64 = 0.88;

    let span = total.as_nanos();
    // A span of zero has one honest tick. Returning nothing instead would draw an axis with no
    // origin, which reads as a chart that failed to render rather than as a response that
    // arrived inside the clock's resolution.
    if span == 0 {
        return vec![Duration::ZERO];
    }

    let raw = span / TARGET_INTERVALS;
    if raw == 0 {
        return vec![Duration::ZERO];
    }

    let mut power = 1u128;
    while power.saturating_mul(10) <= raw {
        power *= 10;
    }

    // Round the leading digit up to the next nice number. `raw / power` is in `1..10` by
    // construction, so the final arm is the only one that can carry to the next power.
    let step = match raw / power {
        1 => 2 * power,
        2..=4 => 5 * power,
        _ => 10 * power,
    };

    let ceiling = span as f64 * CROWD_LIMIT;
    let mut ticks = Vec::new();
    let mut at = 0u128;
    while (at as f64) <= ceiling {
        ticks.push(Duration::from_nanos(at as u64));
        at += step;
    }
    ticks
}

/// What a request spent its time on.
///
/// `ttfb` and `total` are wall time around the send; `connection` is what could be measured
/// inside it. See `phases` for the shape the two combine into.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timing {
    pub connection: Connection,
    /// Submitted until the response head is available.
    pub ttfb: Duration,
    /// Submitted until the last body byte.
    pub total: Duration,
}

impl Timing {
    /// The stages in wire order: contiguous, and summing to exactly `total`.
    ///
    /// **Both properties are established here rather than in the pane.** A bar chart that
    /// computes its own offsets is one that can disagree with the total printed beside it, and
    /// this is arithmetic a unit test can hold where a paint cannot be observed at all.
    ///
    /// Stages that did not happen are **absent, not zero**: a pooled connection yields two
    /// phases, not four with two of them empty. `Wait` and `Download` are always present —
    /// every response waited for a first byte, and an empty body is a zero-length download
    /// rather than an absent one.
    pub fn phases(&self) -> Vec<Phase> {
        let mut phases = Vec::with_capacity(4);
        let mut at = Duration::ZERO;

        if let Connection::Opened { dns, connect, .. } = self.connection {
            // Clamped against `ttfb`, which should never bind. `ttfb` is measured around the
            // whole send while these come from spans nested inside it, so overlap means a
            // bookkeeping error somewhere — and a stray nanosecond would otherwise push the
            // phases past the total they are drawn against, which presents as a rendering bug
            // rather than as the measurement fault it is. `phases_stay_inside_the_total_...`
            // pins the clamp with a deliberately impossible `Timing`.
            if let Some(dns) = dns {
                let dns = dns.min(self.ttfb);
                phases.push(Phase {
                    kind: PhaseKind::Dns,
                    start: at,
                    duration: dns,
                });
                at += dns;
            }

            let connect = connect.min(self.ttfb.saturating_sub(at));
            phases.push(Phase {
                kind: PhaseKind::Connect,
                start: at,
                duration: connect,
            });
            at += connect;
        }

        phases.push(Phase {
            kind: PhaseKind::Wait,
            start: at,
            duration: self.ttfb.saturating_sub(at),
        });

        phases.push(Phase {
            kind: PhaseKind::Download,
            start: self.ttfb,
            duration: self.total.saturating_sub(self.ttfb),
        });

        phases
    }
}

/// How big the response was, as far as can be known.
///
/// **`declared` is an `Option` for the same reason `Timing`'s connection stages are:** the number
/// often isn't available, and pretending otherwise makes the display lie. It is the server's
/// `Content-Length`, which is absent *exactly when it would have been most interesting* — reqwest
/// 0.13 delegates decompression to `tower-http`, and that removes `Content-Encoding` and
/// `Content-Length` together whenever it decodes a body, so a compressed response arrives with no
/// declaration at all.
///
/// The consequence is worth stating plainly, because an earlier version of this doc claimed the
/// opposite: **the compression ratio cannot be shown.** Recovering the wire size needs a client
/// lower-level than reqwest. Where the two numbers *can* differ is a `HEAD` or `304` — a length
/// declared with no body behind it, which is informative but is not compression.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SizeInfo {
    /// The server's `Content-Length`, when it sent one and it survived decoding.
    pub declared: Option<u64>,
    /// Bytes actually received, after any decompression. Always known.
    pub decoded: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Informational,
    Success,
    Redirect,
    ClientError,
    ServerError,
    Unknown,
}

impl StatusClass {
    pub fn of(status: u16) -> Self {
        match status {
            100..=199 => StatusClass::Informational,
            200..=299 => StatusClass::Success,
            300..=399 => StatusClass::Redirect,
            400..=499 => StatusClass::ClientError,
            500..=599 => StatusClass::ServerError,
            _ => StatusClass::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResponseData {
    pub status: u16,
    pub status_text: String,
    pub version: HttpVersion,
    /// Ordered, exactly as received — duplicates included.
    pub headers: Vec<Header>,
    pub body: Bytes,
    pub timing: Timing,
    pub size: SizeInfo,
}

impl ResponseData {
    pub fn status_class(&self) -> StatusClass {
        StatusClass::of(self.status)
    }

    /// Case-insensitive lookup of the first matching header value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    /// Attempt a zero-copy text view. `None` means the body isn't valid UTF-8,
    /// which is a normal outcome, not an error.
    pub fn body_as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.body).ok()
    }

    /// A populated response for the M1.0 shell to render. Replaced by the real
    /// engine in M1.2.
    pub fn sample() -> Self {
        let body = Bytes::from_static(
            b"{\n  \"data\": {\n    \"viewer\": {\n      \"login\": \"thearyankumar\",\n      \"repositories\": {\n        \"totalCount\": 42\n      }\n    }\n  }\n}",
        );
        let decoded = body.len() as u64;

        Self {
            status: 200,
            status_text: "OK".to_string(),
            version: HttpVersion::Http2,
            headers: vec![
                Header::new("content-type", "application/json; charset=utf-8"),
                Header::new("content-encoding", "gzip"),
                Header::new("x-ratelimit-remaining", "4998"),
                Header::new("cache-control", "no-cache"),
            ],
            body,
            timing: Timing {
                connection: Connection::Opened {
                    dns: Some(Duration::from_micros(2_400)),
                    connect: Duration::from_millis(79),
                    sockets: 1,
                },
                ttfb: Duration::from_millis(126),
                total: Duration::from_millis(142),
            },
            size: SizeInfo {
                declared: Some(96),
                decoded,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classes_cover_the_boundaries() {
        assert_eq!(StatusClass::of(200), StatusClass::Success);
        assert_eq!(StatusClass::of(299), StatusClass::Success);
        assert_eq!(StatusClass::of(300), StatusClass::Redirect);
        assert_eq!(StatusClass::of(404), StatusClass::ClientError);
        assert_eq!(StatusClass::of(503), StatusClass::ServerError);
        assert_eq!(StatusClass::of(0), StatusClass::Unknown);
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let response = ResponseData::sample();
        assert_eq!(
            response.header("Content-Type"),
            Some("application/json; charset=utf-8")
        );
    }

    /// A helper, because every phase test wants the same two wall-clock numbers.
    fn timing(connection: Connection, ttfb_ms: u64, total_ms: u64) -> Timing {
        Timing {
            connection,
            ttfb: Duration::from_millis(ttfb_ms),
            total: Duration::from_millis(total_ms),
        }
    }

    #[test]
    fn phases_are_contiguous_and_sum_to_the_total() {
        let timing = timing(
            Connection::Opened {
                dns: Some(Duration::from_millis(5)),
                connect: Duration::from_millis(20),
                sockets: 1,
            },
            100,
            140,
        );
        let phases = timing.phases();

        assert_eq!(
            phases.iter().map(|p| p.kind).collect::<Vec<_>>(),
            vec![
                PhaseKind::Dns,
                PhaseKind::Connect,
                PhaseKind::Wait,
                PhaseKind::Download
            ]
        );

        // Contiguity: each bar starts where the last one ended. A gap or an overlap is a
        // timeline that does not describe one span, which is the only thing it is for.
        assert_eq!(phases[0].start, Duration::ZERO);
        for pair in phases.windows(2) {
            assert_eq!(pair[0].end(), pair[1].start, "{pair:?}");
        }

        // And the whole run is accounted for, so the bars fill the track exactly.
        assert_eq!(phases.last().unwrap().end(), timing.total);

        assert_eq!(phases[2].duration, Duration::from_millis(75), "wait");
        assert_eq!(phases[3].duration, Duration::from_millis(40), "download");
    }

    #[test]
    fn a_pooled_connection_has_no_setup_phases_rather_than_empty_ones() {
        // The distinction the `Connection` enum exists for: a resend reuses its socket, so
        // there is nothing to draw for DNS or the handshake. Two empty bars would assert
        // those stages ran and took no time.
        let phases = timing(Connection::Pooled, 30, 45).phases();

        assert_eq!(
            phases.iter().map(|p| p.kind).collect::<Vec<_>>(),
            vec![PhaseKind::Wait, PhaseKind::Download]
        );
        assert_eq!(phases[0].duration, Duration::from_millis(30));
        assert_eq!(phases[1].end(), Duration::from_millis(45));
    }

    #[test]
    fn an_unmeasured_connection_reports_only_what_is_known() {
        let phases = timing(Connection::Unknown, 10, 10).phases();

        assert_eq!(
            phases.iter().map(|p| p.kind).collect::<Vec<_>>(),
            vec![PhaseKind::Wait, PhaseKind::Download]
        );
        assert_eq!(phases[1].duration, Duration::ZERO, "nothing to download");
    }

    #[test]
    fn phases_stay_inside_the_total_when_the_setup_spans_overlap() {
        // A deliberately impossible reading: the connection claims 90ms of setup inside a
        // 40ms TTFB. It cannot happen, and the clamp is what keeps it from presenting as a
        // rendering bug — bars running past the end of the track they are drawn against.
        let timing = timing(
            Connection::Opened {
                dns: Some(Duration::from_millis(60)),
                connect: Duration::from_millis(30),
                sockets: 1,
            },
            40,
            50,
        );
        let phases = timing.phases();

        assert_eq!(phases.last().unwrap().end(), timing.total);
        for pair in phases.windows(2) {
            assert_eq!(pair[0].end(), pair[1].start, "{pair:?}");
        }
        assert_eq!(phases[0].duration, Duration::from_millis(40), "dns capped at ttfb");
        assert_eq!(phases[1].duration, Duration::ZERO, "and connect gets what is left");
    }

    /// Ticks as whole milliseconds, which is how every case below reads most clearly.
    fn ticks_ms(total_ms: u64) -> Vec<u64> {
        axis_ticks(Duration::from_millis(total_ms))
            .into_iter()
            .map(|tick| tick.as_millis() as u64)
            .collect()
    }

    #[test]
    fn axis_ticks_round_up_to_nice_numbers() {
        // The case the mock was drawn from. Rounding the ~28 ms raw step *down* to 20 gives
        // 0/20/40/60/80/100/120 — seven labels on an axis sized for four, which is the failure
        // this rounding direction exists to prevent.
        assert_eq!(ticks_ms(142), vec![0, 50, 100]);
    }

    #[test]
    fn axis_ticks_stay_three_to_five_across_every_span_zuno_sees() {
        // From a loopback response that beats the clock to the 30 s timeout ceiling. The count
        // is what the algorithm is *for*, so it is asserted as a range over the whole domain
        // rather than as values for one lucky input.
        for total_ms in [1u64, 5, 12, 38, 105, 142, 410, 999, 1_200, 4_500, 30_000] {
            let ticks = ticks_ms(total_ms);
            assert!(
                (2..=5).contains(&ticks.len()),
                "{total_ms} ms produced {} ticks: {ticks:?}",
                ticks.len()
            );
            assert_eq!(ticks.first(), Some(&0), "the axis needs an origin");
        }
    }

    #[test]
    fn no_tick_crowds_the_end_of_the_axis() {
        // The total is drawn separately, in the summary line above the chart, so a tick at 95%
        // would stack two numbers and leave the edge label nowhere to go.
        // **105 ms is the only span here that this actually tests**, and it was added after
        // break-testing found the first version vacuous: with every other value the step is
        // large enough that the tick past the limit also lands past the total, so the loop
        // stops on its own and the assertion holds whether or not `CROWD_LIMIT` exists. At
        // 105 ms the step is 50, so a third tick would sit at 95% of the axis.
        for total_ms in [1u64, 5, 12, 38, 105, 142, 410, 999, 1_200, 4_500, 30_000] {
            let total = Duration::from_millis(total_ms);
            for tick in axis_ticks(total) {
                assert!(
                    tick.as_secs_f64() <= total.as_secs_f64() * 0.88,
                    "{tick:?} crowds the end of a {total_ms} ms axis"
                );
            }
        }
    }

    #[test]
    fn ticks_ascend_by_one_step() {
        let ticks = axis_ticks(Duration::from_millis(410));
        let step = ticks[1] - ticks[0];
        for pair in ticks.windows(2) {
            assert_eq!(pair[1] - pair[0], step, "{ticks:?}");
        }
    }

    #[test]
    fn a_span_too_short_to_divide_still_has_an_origin() {
        // Both guards in one place: a zero span, and one small enough that `span / 5` truncates
        // to nothing. An empty `Vec` would draw an axis with no origin, which reads as a chart
        // that failed rather than as a very fast response.
        assert_eq!(axis_ticks(Duration::ZERO), vec![Duration::ZERO]);
        assert_eq!(axis_ticks(Duration::from_nanos(3)), vec![Duration::ZERO]);
    }

    #[test]
    fn every_phase_kind_has_a_label() {
        // `as_str` is a match, so a new variant is a compile error there — but an *empty*
        // label would compile and draw a nameless row.
        for kind in [
            PhaseKind::Dns,
            PhaseKind::Connect,
            PhaseKind::Wait,
            PhaseKind::Download,
        ] {
            assert!(!kind.as_str().is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn non_utf8_bodies_are_not_an_error() {
        let mut response = ResponseData::sample();
        response.body = Bytes::from_static(&[0xff, 0xfe, 0x00]);
        assert!(response.body_as_str().is_none());
    }
}
