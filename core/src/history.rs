//! Every request a workspace has sent, kept on disk — the global history.
//!
//! **Beside the per-tab history, not instead of it.** A tab keeps its last ten responses in memory
//! for the diff and `Ctrl+H`; this is the record that outlives the tab and the session, answering
//! "what did I send to `/orders` yesterday, and what came back".
//!
//! **An append-only JSON-lines log plus one file per kept body**, not SQLite. A send costs one
//! appended line, a few hundred entries load and filter in memory in milliseconds, and the format
//! is readable with `tail`. architecture.md records SQLite as where history would go once it paid
//! for itself — searching *inside* bodies would be that point; listing and filtering by URL is not.
//!
//! **The request is kept as typed**, `{{placeholders}}` and all, so a value from `dev.local.json`
//! never reaches this file. A secret typed literally into a header is kept as typed, exactly as
//! the session and the collection file already keep it.
//!
//! **A line this build cannot read is skipped, never rewritten away.** Pruning works on the raw
//! lines and parses only their `id`, so a newer Zuno's entry survives an older one trimming the
//! log — the forward direction invariant 11 is about, which a re-serializing prune would lose.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::request::{Header, RequestSpec};
use crate::response::{Connection, HttpVersion, ResponseData, SizeInfo, Timing};

/// Entries kept. Trimmed in batches of `SLACK`, so a send is an append and not a rewrite.
pub const MAX_ENTRIES: usize = 500;
const SLACK: usize = 50;
/// A body larger than this is summarised ("not kept, 4.2 MB") rather than stored.
pub const MAX_BODY: usize = 1024 * 1024;
/// All kept bodies together. Past it the oldest bodies go; their entries stay.
pub const MAX_BODIES_TOTAL: u64 = 50 * 1024 * 1024;

const LOG: &str = "history.jsonl";
const BODIES: &str = "bodies";

/// One send.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Unique and increasing, so it orders entries and names the body file.
    pub id: u64,
    /// Unix milliseconds.
    pub at: u64,
    /// The request as typed — see the module comment.
    pub spec: RequestSpec,
    /// The collection file the tab was open from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub outcome: Outcome,
}

/// How a send ended, as far as history keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outcome {
    Response(Recorded),
    /// A socket, a stream or a gRPC call: the handshake's answer, not the conversation.
    /// `status` is `None` for a client-streaming call that opened before the server replied.
    Opened {
        status: Option<u16>,
        #[serde(default)]
        status_text: String,
    },
    Failed { error: String },
}

/// A response, minus what only made sense live: the connection breakdown, the request as sent
/// and the certificate are all `None`/unknown when one is shown again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recorded {
    pub status: u16,
    pub status_text: String,
    pub version: String,
    pub headers: Vec<Header>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trailers: Vec<Header>,
    pub ttfb_us: u64,
    pub total_us: u64,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared: Option<u64>,
    pub body: Kept,
}

/// Whether the body is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kept {
    Stored,
    /// Over `MAX_BODY` when it arrived.
    TooLarge,
    Empty,
}

impl Recorded {
    /// What to record for `response`, and the body to store beside it when it is kept.
    pub fn of(response: &ResponseData) -> (Recorded, Option<Bytes>) {
        let body = if response.body.is_empty() {
            Kept::Empty
        } else if response.body.len() > MAX_BODY {
            Kept::TooLarge
        } else {
            Kept::Stored
        };
        let recorded = Recorded {
            status: response.status,
            status_text: response.status_text.clone(),
            version: response.version.as_str().to_string(),
            headers: response.headers.clone(),
            trailers: response.trailers.clone(),
            ttfb_us: micros(response.timing.ttfb),
            total_us: micros(response.timing.total),
            size: response.size.decoded,
            declared: response.size.declared,
            body,
        };
        let stored = (body == Kept::Stored).then(|| response.body.clone());
        (recorded, stored)
    }

    /// The response again, for the pane. `body` is empty when it was not kept.
    pub fn to_response(&self, body: Bytes) -> ResponseData {
        ResponseData {
            status: self.status,
            status_text: self.status_text.clone(),
            version: version_of(&self.version),
            headers: self.headers.clone(),
            trailers: self.trailers.clone(),
            body,
            timing: Timing {
                connection: Connection::Unknown,
                ttfb: Duration::from_micros(self.ttfb_us),
                total: Duration::from_micros(self.total_us),
            },
            size: SizeInfo {
                declared: self.declared,
                decoded: self.size,
            },
            sent: None,
            network: None,
        }
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn version_of(text: &str) -> HttpVersion {
    match text {
        "HTTP/0.9" => HttpVersion::Http09,
        "HTTP/1.0" => HttpVersion::Http10,
        "HTTP/2" => HttpVersion::Http2,
        "HTTP/3" => HttpVersion::Http3,
        _ => HttpVersion::Http11,
    }
}

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// A fresh entry id: microseconds since the epoch, bumped past the last one handed out so two
/// sends in the same microsecond still get distinct body files.
pub fn next_id() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_micros()).unwrap_or(u64::MAX));
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let id = now.max(last + 1);
        match LAST.compare_exchange_weak(last, id, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return id,
            Err(actual) => last = actual,
        }
    }
}

const DAY_MS: i64 = 86_400_000;

/// The local day `at_ms` falls on, as days since 1970-01-01, for a zone `offset_secs` east of UTC.
pub fn local_day(at_ms: u64, offset_secs: i32) -> i64 {
    (at_ms as i64 + i64::from(offset_secs) * 1000).div_euclid(DAY_MS)
}

/// A day's heading: `Today`, `Yesterday`, else `Mon 6 Oct`.
///
/// The zone is an argument rather than read here, so this stays a pure function a test can pin —
/// the app reads it once at startup (see `app/src/history.rs`).
pub fn day_heading(day: i64, today: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    match today - day {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        _ => {
            let (year, month, date) = civil_from_days(day);
            let weekday = WEEKDAYS[day.rem_euclid(7) as usize];
            let month = MONTHS[(month - 1) as usize];
            let (current, ..) = civil_from_days(today);
            if year == current {
                format!("{weekday} {date} {month}")
            } else {
                format!("{weekday} {date} {month} {year}")
            }
        }
    }
}

/// A row's time: how long ago within today (`2m`, `3h`), the clock time on any earlier day.
pub fn row_time(at_ms: u64, now_ms: u64, offset_secs: i32) -> String {
    if local_day(at_ms, offset_secs) != local_day(now_ms, offset_secs) {
        let local = (at_ms as i64 + i64::from(offset_secs) * 1000).rem_euclid(DAY_MS) / 60_000;
        return format!("{:02}:{:02}", local / 60, local % 60);
    }
    let minutes = now_ms.saturating_sub(at_ms) / 60_000;
    match minutes {
        0 => "now".to_string(),
        1..60 => format!("{minutes}m"),
        _ => format!("{}h", minutes / 60),
    }
}

/// Days since 1970-01-01 to `(year, month, day)` in the proleptic Gregorian calendar — Howard
/// Hinnant's `civil_from_days`, which is exact for every date this will ever see.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// One workspace's history directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

/// Just enough of a line to prune it, whatever version wrote the rest.
#[derive(Deserialize)]
struct IdOnly {
    id: u64,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn log(&self) -> PathBuf {
        self.dir.join(LOG)
    }

    fn body_path(&self, id: u64) -> PathBuf {
        self.dir.join(BODIES).join(id.to_string())
    }

    /// Record a send. The body is written before the line, so a line never names a body that a
    /// crash left unwritten.
    pub fn append(&self, entry: &Entry, body: Option<&[u8]>) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        if let Some(body) = body {
            fs::create_dir_all(self.dir.join(BODIES))?;
            fs::write(self.body_path(entry.id), body)?;
        }
        let line = serde_json::to_string(entry).map_err(io::Error::other)?;
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log())?;
        log.write_all(format!("{line}\n").as_bytes())?;
        drop(log);
        self.prune()
    }

    /// Every entry this build can read, newest first. A missing log is an empty history.
    pub fn load(&self) -> Vec<Entry> {
        let Ok(text) = fs::read_to_string(self.log()) else {
            return Vec::new();
        };
        let mut entries: Vec<Entry> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        entries.reverse();
        entries
    }

    /// Forget everything: the log and every kept body. The directory goes with them.
    pub fn clear(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.dir) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// The stored body of `id`, or `None` when it was never kept or has since been pruned.
    pub fn read_body(&self, id: u64) -> Option<Bytes> {
        fs::read(self.body_path(id)).ok().map(Bytes::from)
    }

    fn prune(&self) -> io::Result<()> {
        let text = fs::read_to_string(self.log())?;
        let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
        if lines.len() > MAX_ENTRIES + SLACK {
            let (dropped, kept) = lines.split_at(lines.len() - MAX_ENTRIES);
            for line in dropped {
                if let Ok(IdOnly { id }) = serde_json::from_str(line) {
                    let _ = fs::remove_file(self.body_path(id));
                }
            }
            // Temp and rename: a crash mid-write leaves the old log, not half of the new one.
            let temp = self.dir.join(format!("{LOG}.tmp"));
            fs::write(&temp, kept.iter().map(|line| format!("{line}\n")).collect::<String>())?;
            fs::rename(&temp, self.log())?;
        }
        self.prune_bodies()
    }

    /// Drop the oldest bodies until the rest fit `MAX_BODIES_TOTAL`. Ids increase, so the
    /// smallest is the oldest.
    fn prune_bodies(&self) -> io::Result<()> {
        let Ok(read) = fs::read_dir(self.dir.join(BODIES)) else {
            return Ok(());
        };
        let mut bodies: Vec<(u64, u64, PathBuf)> = read
            .filter_map(Result::ok)
            .filter_map(|file| {
                let id = file.file_name().to_str()?.parse().ok()?;
                let size = file.metadata().ok()?.len();
                Some((id, size, file.path()))
            })
            .collect();
        let mut total: u64 = bodies.iter().map(|(_, size, _)| size).sum();
        bodies.sort_by_key(|(id, _, _)| *id);
        for (_, size, path) in bodies {
            if total <= MAX_BODIES_TOTAL {
                break;
            }
            fs::remove_file(path)?;
            total -= size;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("zuno-history-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Store::new(dir)
    }

    fn entry(url: &str, outcome: Outcome) -> Entry {
        let mut spec = RequestSpec::default();
        spec.url = url.to_string();
        Entry {
            id: next_id(),
            at: now_ms(),
            spec,
            path: None,
            outcome,
        }
    }

    fn response(body: &[u8]) -> ResponseData {
        ResponseData {
            status: 200,
            status_text: "OK".into(),
            version: HttpVersion::Http2,
            headers: vec![Header::new("content-type", "application/json")],
            trailers: Vec::new(),
            body: Bytes::copy_from_slice(body),
            timing: Timing {
                connection: Connection::Pooled,
                ttfb: Duration::from_millis(12),
                total: Duration::from_millis(30),
            },
            size: SizeInfo { declared: Some(body.len() as u64), decoded: body.len() as u64 },
            sent: None,
            network: None,
        }
    }

    #[test]
    fn a_send_comes_back_newest_first_with_its_body() {
        let store = scratch("roundtrip");
        let mut spec_vars = entry("{{base}}/users", Outcome::Failed { error: "refused".into() });
        spec_vars.spec.headers = vec![Header::new("Authorization", "Bearer {{token}}")];
        store.append(&spec_vars, None).unwrap();

        let (recorded, body) = Recorded::of(&response(br#"{"ok":true}"#));
        let ok = entry("https://api.test/users", Outcome::Response(recorded.clone()));
        store.append(&ok, body.as_deref()).unwrap();

        let loaded = store.load();
        assert_eq!(loaded, vec![ok.clone(), spec_vars.clone()]);
        // Kept as typed — the placeholder, never what it resolved to.
        assert_eq!(loaded[1].spec.headers[0].value, "Bearer {{token}}");

        let shown = recorded.to_response(store.read_body(ok.id).expect("kept"));
        assert_eq!(&shown.body[..], br#"{"ok":true}"#);
        assert_eq!(shown.status, 200);
        assert_eq!(shown.version, HttpVersion::Http2);
        assert_eq!(shown.timing.total, Duration::from_millis(30));

        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn a_body_over_the_cap_is_summarised_not_stored() {
        let (recorded, body) = Recorded::of(&response(&vec![b'x'; MAX_BODY + 1]));
        assert_eq!(recorded.body, Kept::TooLarge);
        assert!(body.is_none());
        assert_eq!(recorded.size, (MAX_BODY + 1) as u64);

        let (recorded, body) = Recorded::of(&response(b""));
        assert_eq!(recorded.body, Kept::Empty);
        assert!(body.is_none());
    }

    #[test]
    fn the_log_is_trimmed_to_the_newest_entries_and_their_bodies() {
        let store = scratch("trim");
        let mut ids = Vec::new();
        for n in 0..=(MAX_ENTRIES + SLACK) {
            let (recorded, body) = Recorded::of(&response(b"{}"));
            let entry = entry(&format!("https://api.test/{n}"), Outcome::Response(recorded));
            ids.push(entry.id);
            store.append(&entry, body.as_deref()).unwrap();
        }

        let loaded = store.load();
        assert_eq!(loaded.len(), MAX_ENTRIES);
        assert_eq!(loaded[0].spec.url, format!("https://api.test/{}", MAX_ENTRIES + SLACK));
        assert!(store.read_body(ids[0]).is_none(), "a trimmed entry's body goes with it");
        assert!(store.read_body(*ids.last().unwrap()).is_some());

        let _ = fs::remove_dir_all(store.dir());
    }

    /// The forward direction: an entry from a newer Zuno is skipped on load but survives a trim.
    #[test]
    fn a_line_this_build_cannot_read_survives_a_trim() {
        let store = scratch("unknown");
        fs::create_dir_all(store.dir()).unwrap();
        let future = format!(r#"{{"id":{},"at":0,"outcome":{{"type":"teleported"}}}}"#, u64::MAX);
        fs::write(store.dir().join(LOG), format!("{future}\n")).unwrap();

        for n in 0..(MAX_ENTRIES + SLACK) {
            store
                .append(&entry(&format!("https://api.test/{n}"), Outcome::Failed { error: String::new() }), None)
                .unwrap();
        }
        let text = fs::read_to_string(store.dir().join(LOG)).unwrap();
        assert!(!text.contains("teleported"), "it was the oldest, so the trim takes it");
        assert_eq!(store.load().len(), MAX_ENTRIES);

        // Newest instead, and it must stay through a trim.
        let store = scratch("unknown-new");
        for n in 0..(MAX_ENTRIES + SLACK) {
            store
                .append(&entry(&format!("https://api.test/{n}"), Outcome::Failed { error: String::new() }), None)
                .unwrap();
        }
        let mut log = fs::OpenOptions::new().append(true).open(store.dir().join(LOG)).unwrap();
        writeln!(log, "{future}").unwrap();
        drop(log);
        store.append(&entry("https://api.test/last", Outcome::Failed { error: String::new() }), None).unwrap();

        let text = fs::read_to_string(store.dir().join(LOG)).unwrap();
        assert!(text.contains("teleported"), "a newer build's entry is kept, not re-serialized away");
        assert_eq!(store.load().len(), MAX_ENTRIES - 1, "and skipped on load");

        let _ = fs::remove_dir_all(store.dir());
        let _ = fs::remove_dir_all(scratch("unknown").dir());
    }

    #[test]
    fn the_oldest_bodies_go_once_all_of_them_pass_the_cap() {
        let store = scratch("bodies");
        let big = vec![b'x'; MAX_BODY];
        let count = (MAX_BODIES_TOTAL / MAX_BODY as u64) as usize + 2;
        let mut ids = Vec::new();
        for n in 0..count {
            let (recorded, body) = Recorded::of(&response(&big));
            let entry = entry(&format!("https://api.test/{n}"), Outcome::Response(recorded));
            ids.push(entry.id);
            store.append(&entry, body.as_deref()).unwrap();
        }
        assert!(store.read_body(ids[0]).is_none());
        assert!(store.read_body(ids[1]).is_none());
        assert!(store.read_body(ids[2]).is_some());
        assert_eq!(store.load().len(), count, "the entries stay; only their bodies went");

        let _ = fs::remove_dir_all(store.dir());
    }

    /// Expected values from `date -u -d @1791374400` (Wed 7 Oct 2026, 12:00 UTC) and its year-ago
    /// twin, not worked out by hand — the first version of this test had the weekday wrong.
    #[test]
    fn days_are_headed_in_the_local_zone() {
        let wednesday_utc_noon: u64 = 1_791_331_200_000 + 12 * 3_600_000;
        let ist = 5 * 3600 + 1800;
        let today = local_day(wednesday_utc_noon, ist);
        assert_eq!(day_heading(today, today), "Today");
        assert_eq!(day_heading(today - 1, today), "Yesterday");
        assert_eq!(day_heading(today, today + 2), "Wed 7 Oct");
        assert_eq!(day_heading(today - 365, today), "Tue 7 Oct 2025");

        // 20:00 UTC is 01:30 the next day in India: the zone decides the day.
        let late: u64 = 1_791_331_200_000 + 20 * 3_600_000;
        assert_eq!(local_day(late, 0), local_day(wednesday_utc_noon, 0));
        assert_eq!(local_day(late, ist), local_day(wednesday_utc_noon, ist) + 1);
    }

    #[test]
    fn a_row_says_minutes_today_and_a_clock_time_before() {
        let ist = 5 * 3600 + 1800;
        let now: u64 = 1_791_331_200_000 + 12 * 3_600_000; // 17:30 IST
        assert_eq!(row_time(now - 30_000, now, ist), "now");
        assert_eq!(row_time(now - 14 * 60_000, now, ist), "14m");
        assert_eq!(row_time(now - 3 * 3_600_000, now, ist), "3h");
        assert_eq!(row_time(now - 24 * 3_600_000, now, ist), "17:30");
    }

}
