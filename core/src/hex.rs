//! A classic hex dump of a response body.
//!
//! **This exists because `BodyKind::Binary` was a dead end.** A binary response showed one line
//! saying how many bytes it was and offered nothing else — no preview, no way to check a magic
//! number, no way to see whether the thing you got back was a JPEG or an HTML error page with
//! the wrong content type. A hex dump is the lowest-common-denominator answer to "what did I
//! actually receive", and it needs no decoder for any particular format.
//!
//! **The output is text, and that is the design.** The viewer already has a virtualized text
//! path with search, selection, copy and horizontal scrolling; rendering hex rows by hand would
//! have meant reimplementing all four. So this produces a `String` shaped like `hexdump -C` and
//! the body view indexes it exactly as it indexes any other text.

use bytes::Bytes;

/// Bytes per row. Sixteen is the convention every hex viewer uses, and the width the ASCII
/// gutter is sized around.
pub const ROW_BYTES: usize = 16;

/// How much of a body is dumped.
///
/// The dump runs about 4.3× the input — three characters per byte, plus an offset and an ASCII
/// gutter per row — so this is the memory limit, not a speed one.
///
/// **Truncating from the front is the right half to keep**, which is not true of most caps. A
/// hex dump is read for magic numbers, headers and framing, and those are at the start; the
/// middle of a JPEG is the part nobody inspects. So a large body shows its beginning and says
/// so, rather than refusing like the JSON and HTML caps do.
pub const MAX_DUMP_BYTES: usize = 1024 * 1024;

/// How many rows `len` bytes will produce, capped.
pub fn row_count(len: usize) -> usize {
    len.min(MAX_DUMP_BYTES).div_ceil(ROW_BYTES)
}

/// Whether a body of `len` bytes is shown in full.
pub fn is_truncated(len: usize) -> bool {
    len > MAX_DUMP_BYTES
}

/// Render up to `MAX_DUMP_BYTES` as `hexdump -C` style text.
///
/// ```text
/// 00000000  ff d8 ff e0 00 10 4a 46  49 46 00 01 01 00 00 48  |......JFIF.....H|
/// ```
///
/// **Background executor only** — it allocates several times the body's size.
pub fn dump(body: &Bytes) -> String {
    let shown = &body[..body.len().min(MAX_DUMP_BYTES)];
    // 8 offset + 2 gap + 16*3 hex + 1 mid-gap + 2 gap + 18 gutter + newline, near enough.
    let mut out = String::with_capacity(shown.len().div_ceil(ROW_BYTES) * 80);

    for (row, chunk) in shown.chunks(ROW_BYTES).enumerate() {
        if row > 0 {
            out.push('\n');
        }
        // Eight digits addresses 4GB, which is far past any cap this will ever carry, and a
        // fixed width is what keeps the columns under each other.
        let offset = row * ROW_BYTES;
        out.push_str(&format!("{offset:08x}  "));

        for ix in 0..ROW_BYTES {
            match chunk.get(ix) {
                Some(byte) => out.push_str(&format!("{byte:02x} ")),
                // A short final row still has to pad, or its ASCII gutter slides left and stops
                // lining up with every row above it.
                None => out.push_str("   "),
            }
            if ix == ROW_BYTES / 2 - 1 {
                out.push(' ');
            }
        }

        out.push_str(" |");
        for byte in chunk {
            out.push(printable(*byte));
        }
        out.push('|');
    }

    out
}

/// `.` for anything outside printable ASCII.
///
/// Deliberately *not* widened to Unicode: a hex dump's gutter is a per-byte column, and a
/// multi-byte character would occupy one cell while consuming several bytes, so the two halves
/// of the row would stop corresponding. The point of the gutter is that column *n* is byte *n*.
fn printable(byte: u8) -> char {
    match byte {
        0x20..=0x7e => byte as char,
        _ => '.',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(body: &[u8]) -> Vec<String> {
        dump(&Bytes::copy_from_slice(body))
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn a_full_row_matches_the_conventional_layout() {
        let rows = lines(b"Hello, world!\x00\xff\xfe");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            "00000000  48 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 00 ff fe  |Hello, world!...|"
        );
    }

    /// The gutter is only readable while column *n* is byte *n*, and a short last row is where
    /// that alignment is lost if the hex columns are not padded.
    #[test]
    fn a_short_final_row_still_lines_its_gutter_up() {
        let rows = lines(b"0123456789abcdef!!");
        assert_eq!(rows.len(), 2);
        let gutter = |row: &str| row.find('|').expect("a gutter");
        assert_eq!(
            gutter(&rows[0]),
            gutter(&rows[1]),
            "the gutter must start at the same column:\n{}\n{}",
            rows[0],
            rows[1]
        );
        assert!(rows[1].ends_with("|!!|"), "{:?}", rows[1]);
    }

    #[test]
    fn offsets_advance_by_the_row_width() {
        let rows = lines(&[0u8; ROW_BYTES * 3]);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with("00000000  "));
        assert!(rows[1].starts_with("00000010  "));
        assert!(rows[2].starts_with("00000020  "));
    }

    /// Anything but printable ASCII is a dot, so the gutter never re-wraps or mis-counts.
    #[test]
    fn the_gutter_is_one_cell_per_byte() {
        // 'é' is two bytes in UTF-8 and must occupy two dots, not one character.
        let rows = lines("aé".as_bytes());
        assert!(rows[0].ends_with("|a..|"), "{:?}", rows[0]);

        let rows = lines(b"\t\n\r\x7f\x00");
        assert!(rows[0].ends_with("|.....|"), "{:?}", rows[0]);
    }

    #[test]
    fn an_empty_body_dumps_to_nothing() {
        assert_eq!(dump(&Bytes::new()), "");
        assert_eq!(row_count(0), 0);
    }

    /// A hex dump is read for magic numbers and framing, which are at the front — so a large
    /// body keeps its beginning rather than being refused outright.
    #[test]
    fn an_oversized_body_keeps_its_start_and_reports_the_cut() {
        let body = Bytes::from(vec![0xabu8; MAX_DUMP_BYTES + ROW_BYTES * 4]);
        assert!(is_truncated(body.len()));

        let text = dump(&body);
        let rows: Vec<_> = text.lines().collect();
        assert_eq!(rows.len(), MAX_DUMP_BYTES / ROW_BYTES);
        assert_eq!(rows.len(), row_count(body.len()));
        assert!(rows[0].starts_with("00000000  "));
    }

    #[test]
    fn row_count_agrees_with_the_rows_produced() {
        for len in [0, 1, 15, 16, 17, 31, 32, 33, 4096] {
            let body = Bytes::from(vec![0u8; len]);
            assert_eq!(
                row_count(len),
                dump(&body).lines().count(),
                "disagreement at {len} bytes"
            );
        }
    }

    /// The magic numbers people actually come here to check.
    #[test]
    fn a_jpeg_header_is_recognisable_at_a_glance() {
        let rows = lines(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00H");
        assert!(rows[0].contains("ff d8 ff e0"), "{:?}", rows[0]);
        assert!(rows[0].contains("|......JFIF"), "{:?}", rows[0]);
    }
}
