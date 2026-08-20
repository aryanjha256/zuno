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
    #[error("could not read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not a valid request: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
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

/// One request found on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Absolute path to the file, and the identity a buffer remembers.
    pub path: PathBuf,
    /// Path relative to the collection root, for display: `billing/invoices.json`.
    pub relative: String,
    pub spec: RequestSpec,
}

/// How deep to walk. Guards against a symlink cycle turning a scan into an infinite loop,
/// and against a pathological tree; nobody nests request folders eight deep on purpose.
const MAX_DEPTH: usize = 8;

/// Read every request in the collection, depth-first, sorted by relative path.
///
/// **Never fails as a whole.** A single unparseable file must not hide an entire
/// collection from the picker — a half-finished hand edit, or a merge conflict marker in
/// one request, is exactly when you most need to reach the others. Unreadable entries are
/// reported to stderr and skipped.
///
/// Does file IO and JSON parsing, so callers on the UI thread must push this to a
/// background executor (CLAUDE.md invariant 3).
pub fn scan(root: &Path) -> Vec<Entry> {
    let mut entries = Vec::new();
    walk(root, root, 0, &mut entries);
    // Sorted by the displayed string rather than by `path`, so the picker's order matches
    // what a person reads.
    entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    entries
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Entry>) {
    if depth > MAX_DEPTH {
        eprintln!("[zuno] skipping {}: nested deeper than {MAX_DEPTH}", dir.display());
        return;
    }

    // A missing root is normal — nothing has been saved yet — so it is not worth a
    // message. Anything else is.
    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!("[zuno] could not read {}: {error}", dir.display());
            return;
        }
    };

    for entry in listing.filter_map(Result::ok) {
        let path = entry.path();

        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // Skips `.git` above all: a git-diffable collection is expected to *be* a repo,
        // and walking its objects would be thousands of files that are never requests.
        if name.starts_with('.') {
            continue;
        }

        let Ok(kind) = entry.file_type() else { continue };

        // Recursing on `is_dir()` rather than filtering on `is_file()` is deliberate:
        // `file_type` does not follow symlinks, so a symlinked directory reports as a
        // symlink and is skipped (no cycles), while a symlink *to a request file* still
        // gets read — which is a reasonable thing for someone to set up.
        if kind.is_dir() {
            walk(root, &path, depth + 1, out);
            continue;
        }

        // `write` leaves a `.json.tmp` only if it dies between write and rename; ignore
        // any that exist rather than reporting a parse failure for a half-written file.
        if !name.ends_with(&format!(".{EXTENSION}")) {
            continue;
        }

        match read(&path) {
            Ok(spec) => out.push(Entry {
                relative: relative_label(root, &path),
                path,
                spec,
            }),
            Err(error) => eprintln!("[zuno] skipping {}: {error}", path.display()),
        }
    }
}

/// The path as shown in the picker, always with `/` separators.
fn relative_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Read one request file.
///
/// The stored `id` is always 0 (see the module docs); assigning a live one is the caller's
/// job, since only it knows which ids are already open.
pub fn read(path: &Path) -> Result<RequestSpec, CollectionError> {
    let bytes = std::fs::read(path).map_err(|source| CollectionError::Read {
        path: path.to_path_buf(),
        source,
    })?;

    serde_json::from_slice(&bytes).map_err(|source| CollectionError::Parse {
        path: path.to_path_buf(),
        source,
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

#[cfg(test)]
mod scan_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zuno-scan-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn save(root: &Path, label: &str, url: &str) -> PathBuf {
        let path = allocate(root, label).expect("allocate");
        write(
            &path,
            &RequestSpec {
                url: url.to_string(),
                ..RequestSpec::sample()
            },
        )
        .expect("write");
        path
    }

    #[test]
    fn a_missing_root_scans_to_nothing() {
        // Normal on a first run: nothing has been saved yet. Must not be an error.
        let root = std::env::temp_dir().join("zuno-scan-does-not-exist");
        assert!(scan(&root).is_empty());
    }

    #[test]
    fn requests_come_back_sorted_by_their_displayed_path() {
        let root = scratch("sorted");
        save(&root, "zebra", "https://a.test/zebra");
        save(&root, "alpha", "https://a.test/alpha");

        std::fs::create_dir_all(root.join("billing")).expect("mkdir");
        save(&root.join("billing"), "invoices", "https://a.test/invoices");

        let found = scan(&root);
        let labels: Vec<&str> = found.iter().map(|e| e.relative.as_str()).collect();
        assert_eq!(labels, ["alpha.json", "billing/invoices.json", "zebra.json"]);

        // The spec really is read back, not just the filename.
        assert_eq!(found[0].spec.url, "https://a.test/alpha");
        // And nested entries carry a usable absolute path.
        assert_eq!(found[1].path, root.join("billing").join("invoices.json"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn one_corrupt_file_does_not_hide_the_rest() {
        // The property that matters most: a half-finished hand edit or a merge conflict
        // marker in one request is exactly when you need to reach the others.
        let root = scratch("corrupt");
        save(&root, "good", "https://a.test/good");
        std::fs::write(root.join("broken.json"), b"{ not json").expect("write");
        std::fs::write(root.join("wrong-shape.json"), br#"{"hello":1}"#).expect("write");

        let found = scan(&root);
        assert_eq!(found.len(), 1, "the good request must survive: {found:?}");
        assert_eq!(found[0].relative, "good.json");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dotfiles_and_non_json_are_ignored() {
        let root = scratch("noise");
        save(&root, "real", "https://a.test/real");

        // A collection is expected to *be* a git repo; walking .git would be thousands of
        // files that are never requests.
        std::fs::create_dir_all(root.join(".git").join("objects")).expect("mkdir");
        std::fs::write(root.join(".git").join("objects").join("x.json"), b"{}").expect("write");
        std::fs::write(root.join("notes.md"), b"# notes").expect("write");
        std::fs::write(root.join(".hidden.json"), b"{}").expect("write");
        // A temp file left behind if `write` died between write and rename.
        std::fs::write(root.join("leftover.json.tmp"), b"{}").expect("write");

        let found = scan(&root);
        let labels: Vec<&str> = found.iter().map(|e| e.relative.as_str()).collect();
        assert_eq!(labels, ["real.json"]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_written_request_is_found_again_with_its_url_intact() {
        // The round trip the picker depends on, and the hole this closes: before `scan`,
        // Ctrl+S wrote files nothing could read back.
        let root = scratch("roundtrip");
        let written = save(&root, "invoices", "https://api.test/v1/invoices?page=2");

        let found = scan(&root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, written);
        assert_eq!(found[0].spec.url, "https://api.test/v1/invoices?page=2");
        assert_eq!(found[0].spec.headers, RequestSpec::sample().headers);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_symlinked_directory_cannot_cause_an_infinite_walk() {
        let root = scratch("cycle");
        save(&root, "real", "https://a.test/real");

        // A loop back to the root. `file_type` doesn't follow symlinks, so the link is not
        // a dir and never recursed into; MAX_DEPTH is the backstop if that ever changes.
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, root.join("loop")).expect("symlink");

        let found = scan(&root);
        assert_eq!(found.len(), 1, "{found:?}");

        std::fs::remove_dir_all(&root).ok();
    }
}
