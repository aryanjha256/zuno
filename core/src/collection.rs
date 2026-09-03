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

use crate::{Method, RequestId, RequestSpec};

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
    #[error("could not delete {}: {source}", path.display())]
    Delete {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not move {} to the trash: {source}", path.display())]
    Trash {
        path: PathBuf,
        #[source]
        source: trash::Error,
    },
    #[error("{0:?} already exists")]
    NameTaken(String),
    #[error("{} is not a request file", path.display())]
    NotAFile { path: PathBuf },
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
            // `environments/` is reserved by the collection format — it holds variables,
            // not requests. The skip is what makes that a *reservation* rather than a
            // convention: without it the directory is walked, every file in it is parsed as
            // a request, and each one is reported as unreadable on every single scan. The
            // listing happens to come out the same, because an environment doesn't
            // deserialize as a `RequestSpec` — but a file in there that did would be offered
            // as a request, and the log would be noise either way.
            if name == crate::environment::DIRECTORY {
                continue;
            }
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

/// Delete a request file.
///
/// **Deliberately narrow: one file, never a directory.** `remove_dir_all` on a path derived
/// from a UI selection is the shape of a mistake that has no undo, and a collection folder can
/// hold work that was never visible in the panel — an unreadable request is skipped by `scan`
/// (so it has no row) and would be destroyed anyway. Removing a folder is its own decision.
///
/// A file that is already gone is **not** an error. The panel's tree is a snapshot of the last
/// scan, so a request deleted in a terminal a moment ago is still on screen, and reporting a
/// failure for reaching the state the caller asked for would be noise.
pub fn remove(path: &Path) -> Result<(), CollectionError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CollectionError::Delete {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Move a request file to the desktop trash.
///
/// The recoverable sibling of `remove`, and the one a person reaches for by default — the XDG
/// trash keeps the original path in a `.trashinfo` file, so the desktop's "restore" puts it
/// back where it was. Hand-rolling that was rejected: the same-filesystem case is easy and the
/// cases that decide whether a restore works — a collection on another mount, a name already in
/// the trash — are the ones a hand-rolled version gets wrong quietly.
///
/// **Refuses anything that is not a regular file**, for `remove`'s reason: the path comes from
/// a UI selection, and `trash::delete` would happily take a whole directory with it.
pub fn trash(path: &Path) -> Result<(), CollectionError> {
    if !path.is_file() {
        return Err(CollectionError::NotAFile {
            path: path.to_path_buf(),
        });
    }

    trash::delete(path).map_err(|source| CollectionError::Trash {
        path: path.to_path_buf(),
        source,
    })
}

/// Rename a request file in place, returning its new path.
///
/// `label` is a display name, not a filename: it goes through `slug`, so a typed `../../evil`
/// cannot walk out of the directory — the same boundary `allocate` relies on, and the reason
/// this takes a label rather than a caller-built `PathBuf`. The extension is added here for the
/// same reason the panel strips it: the user is naming a request, not a file.
///
/// **Never overwrites.** A name already taken is an error rather than a silent replacement,
/// because the request being clobbered may be one the renamer has never seen.
pub fn rename(path: &Path, label: &str) -> Result<PathBuf, CollectionError> {
    let stem = slug(label);
    let parent = path.parent().unwrap_or(Path::new("."));
    let target = parent.join(format!("{stem}.{EXTENSION}"));

    // Renaming to the name it already has, or to one differing only in what `slug` strips.
    if target == path {
        return Ok(target);
    }
    if target.exists() {
        return Err(CollectionError::NameTaken(stem));
    }

    std::fs::rename(path, &target).map_err(|source| CollectionError::Write {
        path: target.clone(),
        source,
    })?;
    Ok(target)
}

/// Create a folder inside `parent`, returning its path.
///
/// `label` is typed by a person, so it goes through `slug` for the reason `rename` does — the
/// panel's text box is a path-traversal vector otherwise. Refuses a name already in use rather
/// than silently adopting an existing folder: "created" and "there was already one there" are
/// different answers and the caller says different things about them.
pub fn create_folder(parent: &Path, label: &str) -> Result<PathBuf, CollectionError> {
    let name = slug(label);
    let target = parent.join(&name);

    if target.exists() {
        return Err(CollectionError::NameTaken(name));
    }

    std::fs::create_dir_all(&target).map_err(|source| CollectionError::Directory {
        path: target.clone(),
        source,
    })?;
    Ok(target)
}

/// Move a request into `directory`, returning its new path.
///
/// **Keeps the filename.** A move is about *where* a request lives; renaming it at the same time
/// would make one gesture do two things and leave no way to do either alone. `rename` is the
/// other verb.
///
/// Refuses rather than overwriting, like `rename` — the request being clobbered may be one the
/// mover has never seen — and refuses a destination that is not a directory, so a path arriving
/// from the wrong picker row cannot turn a request into a directory entry.
pub fn move_to(path: &Path, directory: &Path) -> Result<PathBuf, CollectionError> {
    if !directory.is_dir() {
        return Err(CollectionError::NotAFile {
            path: directory.to_path_buf(),
        });
    }

    let name = path.file_name().ok_or_else(|| CollectionError::NotAFile {
        path: path.to_path_buf(),
    })?;
    let target = directory.join(name);

    // Already where it was asked to go. Not an error: the picker lists every directory, and
    // choosing the one a request is already in is a reasonable thing to do by accident.
    if target == path {
        return Ok(target.clone());
    }
    if target.exists() {
        return Err(CollectionError::NameTaken(
            name.to_string_lossy().to_string(),
        ));
    }

    std::fs::rename(path, &target).map_err(|source| CollectionError::Write {
        path: target.clone(),
        source,
    })?;
    Ok(target)
}

/// Every directory in the collection, relative to `root`, with `/` separators and sorted.
///
/// Walks the tree rather than deriving from `scan`'s entries, and that distinction is the whole
/// point: a directory exists whether or not it holds a request. Same skip rules as `walk` —
/// dotfiles, the reserved `environments/`, and `MAX_DEPTH` — so the two agree about what is part
/// of a collection.
pub fn folders(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect_folders(root, root, 0, &mut out);
    out.sort();
    out
}

fn collect_folders(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(listing) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in listing.filter_map(Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name == crate::environment::DIRECTORY {
            continue;
        }
        // `file_type` does not follow symlinks, so a symlinked directory reports as a symlink
        // and is skipped — the cycle guard `walk` relies on, for the same reason.
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }

        out.push(relative_label(root, &path));
        collect_folders(root, &path, depth + 1, out);
    }
}

/// Every place a request can be moved to, as `(absolute path, displayed label)`.
///
/// The root comes first, labelled `/`, because "move it back to the top" has to be reachable and
/// it is the one directory with no relative path to show.
pub fn destinations(root: &Path, folders: &[String]) -> Vec<(PathBuf, String)> {
    let mut out = vec![(root.to_path_buf(), "/".to_string())];
    out.extend(folders.iter().map(|relative| {
        let mut path = root.to_path_buf();
        for part in relative.split('/') {
            path.push(part);
        }
        (path, relative.clone())
    }));
    out
}

/// Copy a request to a fresh name beside it, returning the new path.
///
/// Copies the **bytes**, not a re-serialized `RequestSpec`. Reading and writing would normalize
/// the file — reordering nothing today, but silently rewriting anything a future field or a
/// hand edit put there — and a duplicate that differs from its original is a bad duplicate.
/// `allocate` picks the name, so this inherits the collision rule the rest of the format uses:
/// `posts.json` becomes `posts-2.json`, never an overwrite.
pub fn duplicate(path: &Path) -> Result<PathBuf, CollectionError> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| CollectionError::NotAFile {
            path: path.to_path_buf(),
        })?;
    let parent = path.parent().unwrap_or(Path::new("."));

    let target = allocate(parent, stem)?;
    std::fs::copy(path, &target).map_err(|source| CollectionError::Write {
        path: target.clone(),
        source,
    })?;
    Ok(target)
}

/// One row of the collection tree, as the panel draws it.
///
/// **Flat with a depth, not nested**, for the same reason `json::Row` is: the panel renders
/// through `uniform_list`, which demands an O(1)-indexable list of fixed-height rows. A nested
/// structure would have to be walked to answer "what is row 40", which is the question the
/// renderer asks on every frame.
///
/// Folding is deliberately *not* represented here. Which directories are collapsed is view
/// state that belongs to the window, so this is always the whole tree and the app computes
/// which rows are visible — the same split `JsonOutline` makes between `rows` and `visible`.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub depth: u16,
    /// What to draw: a directory's name, or a request's filename with `.json` removed.
    ///
    /// The extension is stripped because it is the same on every row and therefore carries no
    /// information — the panel is a list of requests, not a file manager.
    pub name: String,
    /// The directory, or the request file.
    ///
    /// Doubles as the fold key and as the identity a later delete or rename acts on, which is
    /// why it is a full path rather than the display name: two directories at different depths
    /// can share a name, and a `HashSet<String>` of collapsed names would fold both.
    pub path: PathBuf,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    Directory,
    /// Carries the method and URL rather than the whole `RequestSpec`: the panel draws both,
    /// and a spec holds the body, which for a large request would be cloned per scan and held
    /// for as long as the panel is open.
    Request { method: Method, url: String },
}

/// Arrange scanned entries into a flat, depth-tagged tree.
///
/// **Directories sort before files at each level**, then alphabetically within each group —
/// the file-tree convention every editor shares. `scan` returns entries sorted by their
/// relative path, which interleaves the two (`a.json` sorts before `a/b.json` because `.`
/// precedes `/`), so the ordering has to be rebuilt here rather than inherited.
///
/// Recursive, like `walk` above and unlike `json::flatten`: the input comes from `scan`, which
/// enforces `MAX_DEPTH`, so the nesting is bounded before it reaches this function. The
/// flattener has no such guarantee about a server's JSON, which is why it is iterative.
pub fn tree(root: &Path, entries: &[Entry], folders: &[String]) -> Vec<Node> {
    let mut branch = Branch::default();

    // **Folders first, and they come from the filesystem rather than from the entries.** A
    // directory earns a row by existing, not by holding a request — derive them from `entries`
    // and a folder you just created is invisible until you put something in it, which is also
    // the one thing you cannot do while it has no row to move onto. New folder and Move stopped
    // composing entirely, and the status message admitting it ("appears once a request is in
    // it") was the design flaw wearing a notice.
    for relative in folders {
        let mut here = &mut branch;
        for component in relative.split('/') {
            here = here.dirs.entry(component).or_default();
        }
    }

    for entry in entries {
        let mut components: Vec<&str> = entry.relative.split('/').collect();
        // A relative label always ends in a filename; anything else is not an entry `scan`
        // produced, and there is no row to draw for it.
        let Some(file) = components.pop() else { continue };

        let mut here = &mut branch;
        for component in components {
            here = here.dirs.entry(component).or_default();
        }
        here.files.insert(file, entry);
    }

    let mut out = Vec::new();
    flatten_branch(&branch, root, 0, &mut out);
    out
}

/// The nested form, built only to be flattened. `BTreeMap` rather than sorting afterwards
/// because it keeps each level ordered as it is filled, and the two maps are what puts every
/// directory ahead of every file without a comparator that has to know which is which.
#[derive(Default)]
struct Branch<'a> {
    dirs: std::collections::BTreeMap<&'a str, Branch<'a>>,
    files: std::collections::BTreeMap<&'a str, &'a Entry>,
}

fn flatten_branch(branch: &Branch<'_>, parent: &Path, depth: u16, out: &mut Vec<Node>) {
    for (name, sub) in &branch.dirs {
        let path = parent.join(name);
        out.push(Node {
            depth,
            name: (*name).to_string(),
            path: path.clone(),
            kind: NodeKind::Directory,
        });
        flatten_branch(sub, &path, depth + 1, out);
    }

    for (name, entry) in &branch.files {
        out.push(Node {
            depth,
            name: name
                .strip_suffix(&format!(".{EXTENSION}"))
                .unwrap_or(name)
                .to_string(),
            path: entry.path.clone(),
            kind: NodeKind::Request {
                method: entry.spec.method.clone(),
                url: entry.spec.url.clone(),
            },
        });
    }
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

    /// A row as the panel would read it, for assertions that don't care about paths.
    fn shape(nodes: &[Node]) -> Vec<(u16, &str, bool)> {
        nodes
            .iter()
            .map(|node| {
                (
                    node.depth,
                    node.name.as_str(),
                    matches!(node.kind, NodeKind::Directory),
                )
            })
            .collect()
    }

    #[test]
    fn creating_a_folder_slugs_the_name_and_refuses_a_collision() {
        let root = scratch("folder-create");
        std::fs::create_dir_all(&root).expect("mkdir");

        let made = create_folder(&root, "Billing API").expect("create");
        assert_eq!(made, root.join("Billing-API"));
        assert!(made.is_dir());

        // The name comes from a text box, so it is the same traversal boundary as `rename`.
        let escaped = create_folder(&root, "../evil").expect("create");
        assert_eq!(escaped.parent(), Some(root.as_path()), "{escaped:?}");
        assert!(!root.parent().map(|p| p.join("evil").exists()).unwrap_or(false));

        // Refused rather than silently adopting the folder that is already there.
        assert!(create_folder(&root, "Billing API").is_err());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn moving_a_request_keeps_its_filename_and_its_contents() {
        let root = scratch("move-basic");
        let path = save(&root, "invoices", "https://a.test/invoices");
        let before = std::fs::read(&path).expect("read");
        let billing = create_folder(&root, "billing").expect("create");

        let moved = move_to(&path, &billing).expect("move");

        assert_eq!(moved, billing.join("invoices.json"), "the filename travels with it");
        assert!(!path.exists());
        assert_eq!(std::fs::read(&moved).expect("read"), before);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn moving_onto_an_existing_name_is_refused_rather_than_overwriting() {
        let root = scratch("move-collide");
        let billing = create_folder(&root, "billing").expect("create");
        let source = save(&root, "invoices", "https://a.test/root-one");
        let target = save(&billing, "invoices", "https://a.test/billing-one");
        let untouched = std::fs::read(&target).expect("read");

        assert!(move_to(&source, &billing).is_err());
        assert!(source.is_file(), "the source must survive a refused move");
        assert_eq!(
            std::fs::read(&target).expect("read"),
            untouched,
            "and the request it would have replaced must be untouched"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn moving_into_the_directory_it_is_already_in_is_a_no_op() {
        // The picker lists every directory including the one the request is in, and choosing it
        // is a reasonable accident. Reporting "already exists" about the file being moved is the
        // same mistake `rename` avoids.
        let root = scratch("move-noop");
        let path = save(&root, "invoices", "https://a.test/invoices");

        assert_eq!(move_to(&path, &root).expect("move"), path);
        assert!(path.is_file());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_move_destinations_are_the_root_and_every_directory_including_empty_ones() {
        let root = scratch("move-destinations");
        save(&root, "top", "https://a.test/top");
        let deep = root.join("billing").join("eu");
        std::fs::create_dir_all(&deep).expect("mkdir");
        save(&deep, "vat", "https://a.test/vat");
        // A directory with nothing in it — the case this test exists for.
        std::fs::create_dir_all(root.join("empty")).expect("mkdir");

        let found = destinations(&root, &folders(&root));
        let labels: Vec<&str> = found.iter().map(|(_, label)| label.as_str()).collect();

        // **`empty` is offered**, and that is the fix rather than an oversight: moving a request
        // into a folder is what fills it, so refusing to offer an empty one made New folder and
        // Move unable to compose at all.
        assert_eq!(labels, ["/", "billing", "billing/eu", "empty"]);
        assert_eq!(found[0].0, root, "the root has to be reachable to move back to");
        assert_eq!(found[2].0, deep, "and an intermediate directory carries a real path");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn renaming_moves_the_file_and_leaves_its_contents_alone() {
        let root = scratch("rename-basic");
        let path = save(&root, "posts", "https://a.test/posts");
        let before = std::fs::read(&path).expect("read");

        let renamed = rename(&path, "articles").expect("rename");

        assert_eq!(renamed, root.join("articles.json"));
        assert!(!path.exists(), "the old name must be gone");
        assert_eq!(
            std::fs::read(&renamed).expect("read"),
            before,
            "renaming must not rewrite the request"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn renaming_goes_through_slug_so_a_typed_name_cannot_escape_the_directory() {
        // The label comes from a text box the user types into, which makes this the same
        // security boundary `allocate` relies on — and the reason `rename` takes a label
        // rather than a caller-built path.
        let root = scratch("rename-escape");
        std::fs::create_dir_all(root.join("nested")).expect("mkdir");
        let path = save(&root.join("nested"), "posts", "https://a.test/posts");

        let renamed = rename(&path, "../../evil").expect("rename");

        assert_eq!(
            renamed.parent(),
            Some(root.join("nested").as_path()),
            "the file must stay in its own directory: {renamed:?}"
        );
        assert!(!root.join("evil.json").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn renaming_onto_an_existing_name_is_refused_rather_than_overwriting() {
        // The request being clobbered may be one the renamer has never seen.
        let root = scratch("rename-collide");
        let path = save(&root, "posts", "https://a.test/posts");
        let other = save(&root, "articles", "https://a.test/articles");
        let untouched = std::fs::read(&other).expect("read");

        assert!(rename(&path, "articles").is_err());
        assert!(path.is_file(), "the source must survive a refused rename");
        assert_eq!(std::fs::read(&other).expect("read"), untouched);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn renaming_to_the_same_name_is_a_no_op_rather_than_a_collision() {
        // Otherwise opening the rename box and pressing Enter reports "already exists" about
        // the file being renamed.
        let root = scratch("rename-noop");
        let path = save(&root, "posts", "https://a.test/posts");

        assert_eq!(rename(&path, "posts").expect("rename"), path);
        assert!(path.is_file());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn duplicating_copies_the_bytes_to_a_free_name_beside_it() {
        let root = scratch("duplicate");
        std::fs::create_dir_all(root.join("billing")).expect("mkdir");
        let path = save(&root.join("billing"), "invoices", "https://a.test/invoices");
        let original = std::fs::read(&path).expect("read");

        let copy = duplicate(&path).expect("duplicate");

        assert_eq!(copy, root.join("billing").join("invoices-2.json"));
        assert!(path.is_file(), "the original must survive");
        assert_eq!(
            std::fs::read(&copy).expect("read"),
            original,
            "a duplicate that differs from its original is a bad duplicate"
        );

        // And again, to prove the collision rule keeps stepping rather than overwriting.
        assert_eq!(
            duplicate(&path).expect("duplicate"),
            root.join("billing").join("invoices-3.json")
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn trashing_refuses_a_directory() {
        // `trash::delete` would take the whole folder. The path comes from a UI selection, so
        // this is `remove`'s guard for the same reason.
        let root = scratch("trash-dir");
        std::fs::create_dir_all(root.join("billing")).expect("mkdir");
        save(&root.join("billing"), "invoices", "https://a.test/invoices");

        assert!(trash(&root.join("billing")).is_err());
        assert!(root.join("billing").join("invoices.json").is_file());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn deleting_a_request_removes_only_that_file() {
        let root = scratch("remove-one");
        save(&root, "keep", "https://a.test/keep");
        let doomed = save(&root, "doomed", "https://a.test/doomed");
        std::fs::create_dir_all(root.join("billing")).expect("mkdir");
        save(&root.join("billing"), "invoices", "https://a.test/invoices");

        remove(&doomed).expect("delete");

        let entries = scan(&root);
        let left: Vec<&str> = entries.iter().map(|e| e.relative.as_str()).collect();
        // Asserted as the *whole* listing rather than "doomed is absent": a delete that took
        // the sibling or the directory with it would pass the weaker check.
        assert_eq!(left, ["billing/invoices.json", "keep.json"]);
        assert!(root.join("billing").is_dir(), "the directory must survive");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn deleting_a_request_that_is_already_gone_is_not_an_error() {
        // The panel's tree is a snapshot of the last scan, so a request removed in a terminal
        // a moment ago is still drawn. Reporting a failure for reaching the state the caller
        // asked for would be noise.
        let root = scratch("remove-missing");
        std::fs::create_dir_all(&root).expect("mkdir");
        assert!(remove(&root.join("never-existed.json")).is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn deleting_refuses_a_directory_rather_than_taking_its_contents() {
        // `remove_file` on a directory is an error on every platform Zuno targets, and that is
        // the property being pinned: nothing here can escalate into `remove_dir_all`.
        let root = scratch("remove-dir");
        std::fs::create_dir_all(root.join("billing")).expect("mkdir");
        save(&root.join("billing"), "invoices", "https://a.test/invoices");

        assert!(remove(&root.join("billing")).is_err());
        assert!(
            root.join("billing").join("invoices.json").is_file(),
            "the request inside it must be untouched"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_flat_collection_is_one_row_per_request() {
        let root = scratch("tree-flat");
        save(&root, "zebra", "https://a.test/zebra");
        save(&root, "alpha", "https://a.test/alpha");

        let nodes = tree(&root, &scan(&root), &folders(&root));
        assert_eq!(shape(&nodes), [(0, "alpha", false), (0, "zebra", false)]);

        // The extension is dropped: it is identical on every row, so it says nothing.
        assert!(nodes.iter().all(|node| !node.name.ends_with(".json")));
        // And the method comes through, since the panel draws it.
        match &nodes[0].kind {
            NodeKind::Request { method, url } => {
                assert_eq!(method, &RequestSpec::sample().method);
                assert_eq!(url, "https://a.test/alpha");
            }
            other => panic!("expected a request, got {other:?}"),
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_empty_directory_still_gets_a_row() {
        // A directory earns a row by existing. Deriving the tree from `scan`'s entries meant a
        // folder you had just created was invisible until you put a request in it — and the one
        // verb that could put a request in it, Move, only offered folders that already had rows.
        // New folder and Move could not compose at all.
        let root = scratch("tree-empty-dir");
        std::fs::create_dir_all(root.join("drafts")).expect("mkdir");
        save(&root, "posts", "https://a.test/posts");

        let nodes = tree(&root, &scan(&root), &folders(&root));
        assert_eq!(
            shape(&nodes),
            [(0, "drafts", true), (0, "posts", false)],
            "an empty directory must be drawn, above the requests as usual"
        );
        assert_eq!(nodes[0].path, root.join("drafts"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_folder_walk_skips_what_the_request_walk_skips() {
        // The two have to agree about what is part of a collection, or `environments/` becomes a
        // move destination and `.git` becomes a folder tree.
        let root = scratch("folders-skips");
        std::fs::create_dir_all(root.join("billing")).expect("mkdir");
        std::fs::create_dir_all(root.join(".git").join("objects")).expect("mkdir");
        std::fs::create_dir_all(crate::environment::directory(&root)).expect("mkdir");

        assert_eq!(folders(&root), ["billing"]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_directory_becomes_a_row_and_its_requests_are_indented() {
        let root = scratch("tree-nested");
        std::fs::create_dir_all(root.join("billing")).expect("mkdir");
        save(&root.join("billing"), "invoices", "https://a.test/invoices");

        let nodes = tree(&root, &scan(&root), &folders(&root));
        assert_eq!(shape(&nodes), [(0, "billing", true), (1, "invoices", false)]);

        // A directory row carries the directory's own path, which is what a fold key and a
        // later rename both act on. Deriving it from the child would give the file instead.
        assert_eq!(nodes[0].path, root.join("billing"));
        assert_eq!(nodes[1].path, root.join("billing").join("invoices.json"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn directories_sort_before_requests_at_the_same_level() {
        // The one ordering `scan` cannot supply. It sorts by relative path, where `alpha.json`
        // precedes `beta/x.json` because `.` sorts before `/` — so inheriting its order puts a
        // root-level request above a directory, which no file tree does.
        let root = scratch("tree-order");
        save(&root, "alpha", "https://a.test/alpha");
        std::fs::create_dir_all(root.join("beta")).expect("mkdir");
        save(&root.join("beta"), "nested", "https://a.test/nested");

        let entries = scan(&root);
        // The interleaving this test exists to correct, asserted so the premise is visible.
        let relatives: Vec<&str> = entries.iter().map(|e| e.relative.as_str()).collect();
        assert_eq!(relatives, ["alpha.json", "beta/nested.json"]);

        let nodes = tree(&root, &entries, &folders(&root));
        assert_eq!(
            shape(&nodes),
            [(0, "beta", true), (1, "nested", false), (0, "alpha", false)]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nesting_several_deep_keeps_one_row_per_level() {
        let root = scratch("tree-deep");
        let deep = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).expect("mkdir");
        save(&deep, "leaf", "https://a.test/leaf");

        let nodes = tree(&root, &scan(&root), &folders(&root));
        assert_eq!(
            shape(&nodes),
            [
                (0, "a", true),
                (1, "b", true),
                (2, "c", true),
                (3, "leaf", false)
            ]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn two_directories_sharing_a_name_get_distinct_paths() {
        // The reason `Node::path` is a path rather than the display name: the panel folds by
        // this value, so equal names at different depths would collapse together.
        let root = scratch("tree-samename");
        std::fs::create_dir_all(root.join("v1").join("users")).expect("mkdir");
        std::fs::create_dir_all(root.join("v2").join("users")).expect("mkdir");
        save(&root.join("v1").join("users"), "list", "https://a.test/v1");
        save(&root.join("v2").join("users"), "list", "https://a.test/v2");

        let nodes = tree(&root, &scan(&root), &folders(&root));
        let users: Vec<&Node> = nodes.iter().filter(|node| node.name == "users").collect();
        assert_eq!(users.len(), 2);
        assert_ne!(users[0].path, users[1].path);

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
    fn the_reserved_environments_directory_is_never_walked() {
        // Uses a *valid* request file, which is what makes this discriminate: an
        // environment wouldn't parse as a `RequestSpec` and so would be skipped anyway,
        // leaving the assertion true whether or not the reservation is honoured.
        let root = scratch("reserved");
        save(&root, "real", "https://a.test/real");

        let reserved = root.join(crate::environment::DIRECTORY);
        std::fs::create_dir_all(&reserved).expect("mkdir");
        let decoy = reserved.join("looks-like-a-request.json");
        write(&decoy, &RequestSpec::sample()).expect("write");

        let found = scan(&root);
        let names: Vec<&str> = found.iter().map(|e| e.relative.as_str()).collect();
        assert_eq!(names, ["real.json"], "the reserved directory must not be walked");

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
