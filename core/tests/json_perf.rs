//! Performance floor for the response viewer.
//!
//! M1.3's criterion is "a 10MB JSON response scrolls at 60fps and the UI never blocks"
//! (architecture.md §10). Scroll smoothness is a rendering property `uniform_list`
//! provides, but it only holds if two things here are true:
//!
//! 1. Parsing 10MB is fast enough to hide behind one background task.
//! 2. `visible_rows` — which runs on every fold — is O(rows) and not O(bytes).
//!
//! The bounds are deliberately loose (these run in a debug build, roughly 10-20x slower
//! than release). They exist to catch an accidental O(n²) or a per-row allocation
//! creeping in, not to certify a specific speed.
//!
//! Loose is not the same as unfailable, though. The fold assertion below used to compare the two
//! `visible_rows` calls within a factor of two — and since both are O(rows), nothing could ever
//! breach it. A bound that cannot fail measures nothing; it just reads like it does.

use std::time::{Duration, Instant};

use bytes::Bytes;
use zuno_core::{JsonOutline, LineIndex};

/// Roughly 10MB of plausible API output: an array of records with mixed value types.
fn big_json(target_bytes: usize) -> Bytes {
    let mut json = String::with_capacity(target_bytes + 1024);
    json.push_str("{\"items\":[");

    let mut ix = 0usize;
    while json.len() < target_bytes {
        if ix > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"id\":{ix},\"name\":\"record-{ix}\",\"active\":{},\"score\":{}.{},\
             \"tags\":[\"alpha\",\"beta\"],\"meta\":{{\"nested\":{{\"deep\":null}}}}}}",
            ix % 2 == 0,
            ix % 100,
            ix % 1000
        ));
        ix += 1;
    }

    json.push_str("]}");
    Bytes::from(json)
}

#[test]
fn ten_megabytes_of_json_flattens_quickly() {
    let source = big_json(10 * 1024 * 1024);
    let bytes = source.len();

    let started = Instant::now();
    let outline = JsonOutline::parse(source).expect("valid json");
    let parse = started.elapsed();

    eprintln!(
        "parse:        {bytes} bytes -> {} rows in {parse:?} ({:.0} MB/s)",
        outline.len(),
        (bytes as f64 / (1024.0 * 1024.0)) / parse.as_secs_f64()
    );

    assert!(
        parse < Duration::from_secs(3),
        "flattening 10MB took {parse:?}; something has gone quadratic"
    );
    assert!(outline.len() > 100_000, "expected a lot of rows");
}

#[test]
fn folding_a_huge_document_is_cheap() {
    let outline = JsonOutline::parse(big_json(10 * 1024 * 1024)).expect("valid json");
    let folded = vec![false; outline.len()];

    let started = Instant::now();
    let visible = outline.visible_rows(&folded);
    let unfolded = started.elapsed();
    assert_eq!(visible.len(), outline.len());

    // Fold the root: one row visible, and it must not have to walk the bytes to work
    // that out.
    let mut root_folded = folded.clone();
    root_folded[0] = true;
    let started = Instant::now();
    let visible = outline.visible_rows(&root_folded);
    let collapsed = started.elapsed();

    eprintln!(
        "visible_rows: {} rows in {unfolded:?} unfolded, {collapsed:?} with the root folded",
        outline.len()
    );

    assert_eq!(visible.len(), 1, "folding the root should leave one row");
    assert!(
        unfolded < Duration::from_millis(500),
        "rebuilding the visible index took {unfolded:?}"
    );
    // **Was `collapsed < unfolded * 2`, which could not fail for the reason the test exists.**
    // Both calls are O(rows), so a factor of two separated nothing: a `visible_rows` rewritten to
    // walk every row and filter would sit comfortably inside it. The property actually worth
    // holding is that folding *short-circuits* — an open folded row jumps `subtree_len` forward
    // instead of stepping — so collapsing the root should cost a couple of iterations against
    // 1.3M. Measured at ~900x in release (6.7ms vs 7.3us); 20x leaves room for a debug build and
    // a loaded machine while still failing outright if the skip is ever lost.
    assert!(
        collapsed * 20 < unfolded,
        "folding the root should skip the walk, not repeat it: {collapsed:?} vs {unfolded:?}"
    );
}

#[test]
fn indexing_lines_of_a_huge_body_is_cheap() {
    // The fallback path has to survive the same input.
    let source = big_json(10 * 1024 * 1024);
    let bytes = source.len();

    let started = Instant::now();
    let lines = LineIndex::build(source);
    let elapsed = started.elapsed();

    eprintln!(
        "line index:   {bytes} bytes -> {} lines in {elapsed:?}",
        lines.len()
    );

    assert!(
        elapsed < Duration::from_secs(1),
        "indexing lines took {elapsed:?}"
    );
    // Minified JSON is one enormous line — the case that would stall a naive renderer.
    assert_eq!(lines.len(), 1);
    let (text, truncated) = lines.line(0);
    assert!(truncated, "a 10MB line must be truncated for display");
    assert!(text.len() <= zuno_core::lines::MAX_DISPLAY_LINE);
}
