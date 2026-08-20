//! Collections on disk: **one request per file, in a plain directory tree.**
//!
//! This is the persistence decision architecture.md §12 deferred through M1, and the
//! reason for the shape is git. A collection is a directory you can put under version
//! control, review in a pull request, and merge — which is only true if one request is one
//! file and the serialization is stable. A single-file bundle or a SQLite database would
//! each turn "added a header" into an unreadable diff, and that diffability is a genuine
//! differentiator rather than an implementation detail. Ephemeral state stays out: history,
//! the response cache, and the window session live elsewhere (`app/src/session.rs`).
//!
//! Two consequences of "the file is for humans and git":
//!
//! - **Filenames are derived from the request, not from an id.** `posts.json`, not
//!   `7f3a.json`. That means they collide, and `allocate` handles it — see below.
//! - **`RequestId` is written as 0, always.** It's a session-local handle assigned by
//!   `Workspace::next_id` when a buffer opens, so persisting the live value would put
//!   churn in every diff and manufacture merge conflicts over a number nothing reads
//!   across runs. Normalizing keeps the field (the format stays a plain `RequestSpec`,
//!   with no second type to drift) while keeping it out of the diff.

use std::io;
use std::path::{Path, PathBuf};

use crate::{RequestId, RequestSpec};

/// Requests are `.json` so editors, `jq`, and GitHub all treat them as what they are.
pub const EXTENSION: &str = "json";

/// How long a derived filename may get, in bytes, before the suffix.
///
/// Filesystems generally cap a component at 255 bytes; this is far below that because the
/// limit that matters is a human scanning a directory listing.
const MAX_STEM_BYTES: usize = 64;

/// How many `-2`, `-3`… variants to try before giving up.
const MAX_VARIANTS: u32 = 999;

#[derive(Debug, thiserror::Error)]
pub enum CollectionError {
    #[error("could not create {}: {source}", path.display())]
    Directory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not serialize the request: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("no free filename like {stem:?} in {}", root.display())]
    NoFreeName { root: PathBuf, stem: String },
}

/// A filesystem-safe file stem derived from a request's label.
///
/// **This is a security boundary, not just tidying.** The label it's given comes from a
/// URL, so without sanitizing, a request pointing at `https://x.test/../../.ssh/config`
/// would write outside the collection. Every character that isn't alphanumeric, `.`, `-`,
/// or `_` becomes `-`, which removes both separators and traversal; the result is then
/// checked so it can never be empty, `.`, or `..`.
///
/// Unicode alphanumerics are kept rather than stripped — a request named in Japanese
/// should keep its name, and every filesystem Zuno targets stores UTF-8 bytes.
pub fn slug(label: &str) -> String {
    let mut out = String::new();
    let mut bytes = 0;

    for ch in label.chars() {
        let mapped = if ch.is_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            ch
        } else {
            '-'
        };

        // Collapse runs, so `a//b` and `a - b` don't become `a--b`.
        if mapped == '-' && out.ends_with('-') {
            continue;
        }

        // A dot is only meaningful *between* characters, as in `api.example.com`. Allowing
        // one anywhere lets `a/../b` through as `a-..-b`: safe, since it has no separator
        // and so is a single filename, but not a name anyone wants to read.
        if mapped == '.' && (out.is_empty() || out.ends_with(['-', '.'])) {
            continue;
        }

        // Truncate on a character boundary, never mid-codepoint.
        let width = mapped.len_utf8();
        if bytes + width > MAX_STEM_BYTES {
            break;
        }
        out.push(mapped);
        bytes += width;
    }

    let trimmed = out.trim_matches(['-', '.']);

    // `.`, `..`, and the empty string are all real filenames' worth of trouble: two of
    // them are directory entries that already exist, and the third isn't a name.
    if trimmed.is_empty() {
        "request".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Pick a free path under `root` for a request labelled `label`, creating `root` if needed.
///
/// Derived names collide — two requests can both be `posts` — and a derived name is not an
/// identity, so a collision must never overwrite. `posts.json` is followed by
/// `posts-2.json`. A buffer that already knows its path re-saves through `write` instead and
/// never comes here, which is what keeps repeated saves from breeding files.
pub fn allocate(root: &Path, label: &str) -> Result<PathBuf, CollectionError> {
    std::fs::create_dir_all(root).map_err(|source| CollectionError::Directory {
        path: root.to_path_buf(),
        source,
    })?;

    let stem = slug(label);

    let first = root.join(format!("{stem}.{EXTENSION}"));
    if !first.exists() {
        return Ok(first);
    }

    for n in 2..=MAX_VARIANTS {
        let candidate = root.join(format!("{stem}-{n}.{EXTENSION}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(CollectionError::NoFreeName {
        root: root.to_path_buf(),
        stem,
    })
}

/// Write a request to an exact path.
///
/// Writes to a sibling temp file and renames, which is atomic within a directory on every
/// platform Zuno targets. `app/src/session.rs` writes in place instead, and the difference
/// is deliberate: a truncated session costs you the tab layout, while a truncated
/// collection file costs a request you may have committed and intended to keep.
pub fn write(path: &Path, spec: &RequestSpec) -> Result<(), CollectionError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CollectionError::Directory {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let stored = RequestSpec {
        id: RequestId(0),
        ..spec.clone()
    };
    let mut bytes = serde_json::to_vec_pretty(&stored).map_err(CollectionError::Serialize)?;
    // Every line-oriented tool, and git itself, expects a trailing newline.
    bytes.push(b'\n');

    let temp = path.with_extension(format!("{EXTENSION}.tmp"));
    std::fs::write(&temp, &bytes).map_err(|source| CollectionError::Write {
        path: temp.clone(),
        source,
    })?;
    std::fs::rename(&temp, path).map_err(|source| CollectionError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory under the system temp dir, unique per test and process.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zuno-collection-{}-{name}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn a_slug_keeps_a_readable_name() {
        assert_eq!(slug("posts"), "posts");
        assert_eq!(slug("anchorsForUser"), "anchorsForUser");
        assert_eq!(slug("api.example.com"), "api.example.com");
        assert_eq!(slug("list repositories"), "list-repositories");
    }

    #[test]
    fn a_slug_cannot_escape_the_collection_directory() {
        // The label is derived from a URL, so this is reachable from typed input.
        assert_eq!(slug("../../.ssh/config"), "ssh-config");
        assert_eq!(slug("/etc/passwd"), "etc-passwd");
        assert_eq!(slug("a/../b"), "a-b");
        assert_eq!(slug("C:\\Windows\\system32"), "C-Windows-system32");

        for label in ["..", ".", "/", "//", "...", "-", "", "???"] {
            let stem = slug(label);
            assert!(!stem.is_empty(), "{label:?} produced an empty stem");
            assert_ne!(stem, ".", "{label:?}");
            assert_ne!(stem, "..", "{label:?}");
            assert!(!stem.contains('/'), "{label:?} kept a separator: {stem}");
            assert!(!stem.contains('\\'), "{label:?} kept a separator: {stem}");
        }
    }

    #[test]
    fn a_slug_is_bounded_and_never_splits_a_character() {
        // Multi-byte characters must not be cut mid-codepoint — the result has to stay
        // valid UTF-8, and `String` would panic on a bad boundary.
        let long = "é".repeat(200);
        let stem = slug(&long);
        assert!(stem.len() <= MAX_STEM_BYTES, "{} bytes", stem.len());
        assert!(stem.chars().all(|ch| ch == 'é'), "{stem}");

        let ascii = "a".repeat(200);
        assert_eq!(slug(&ascii).len(), MAX_STEM_BYTES);
    }

    #[test]
    fn a_slug_keeps_non_latin_names() {
        assert_eq!(slug("請求書"), "請求書");
    }

    #[test]
    fn a_collision_gets_a_suffix_rather_than_overwriting() {
        let root = scratch("collision");

        let first = allocate(&root, "posts").expect("allocate");
        assert_eq!(first.file_name().unwrap(), "posts.json");
        write(&first, &RequestSpec::sample()).expect("write");

        // A derived name is not an identity, so a second request labelled the same must
        // not land on top of the first.
        let second = allocate(&root, "posts").expect("allocate");
        assert_eq!(second.file_name().unwrap(), "posts-2.json");
        write(&second, &RequestSpec::sample()).expect("write");

        let third = allocate(&root, "posts").expect("allocate");
        assert_eq!(third.file_name().unwrap(), "posts-3.json");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_written_request_reads_back_identically_apart_from_its_id() {
        let root = scratch("roundtrip");
        let path = allocate(&root, "sample").expect("allocate");

        let spec = RequestSpec {
            id: RequestId(42),
            ..RequestSpec::sample()
        };
        write(&path, &spec).expect("write");

        let bytes = std::fs::read(&path).expect("read");
        let back: RequestSpec = serde_json::from_slice(&bytes).expect("parse");

        assert_eq!(back.id, RequestId(0), "the session-local id must not be persisted");
        assert_eq!(back.url, spec.url);
        assert_eq!(back.headers, spec.headers);
        assert_eq!(back.query, spec.query);
        assert_eq!(back.body, spec.body);
        assert_eq!(back.settings, spec.settings);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_written_file_is_diffable() {
        let root = scratch("diffable");
        let path = allocate(&root, "sample").expect("allocate");
        write(&path, &RequestSpec::sample()).expect("write");

        let text = std::fs::read_to_string(&path).expect("read");
        // Pretty-printed and newline-terminated, or every edit is a one-line diff of the
        // whole request and git reports "no newline at end of file" forever.
        assert!(text.contains('\n'), "must not be minified");
        assert!(text.ends_with('\n'), "must end with a newline");

        // Rewriting identical content must produce identical bytes, or every save dirties
        // the working tree whether or not anything changed.
        write(&path, &RequestSpec::sample()).expect("rewrite");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), text);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn writing_leaves_no_temp_file_behind() {
        let root = scratch("temp");
        let path = allocate(&root, "sample").expect("allocate");
        write(&path, &RequestSpec::sample()).expect("write");

        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn saving_into_a_missing_directory_creates_it() {
        // The collection root won't exist before the first save.
        let root = scratch("nested").join("deeper");
        let path = allocate(&root, "first").expect("allocate");
        write(&path, &RequestSpec::sample()).expect("write");
        assert!(path.exists());

        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }
}
