//! Flows: a named, ordered sequence of requests to run.
//!
//! **Why a stored sequence rather than folder order.** A collection is organised by *resource* —
//! `Auth/`, `Users/`, `Orders/` — and a workflow runs *across* that: log in, create a user, read
//! it, delete it. Those are two different structures and one cannot encode the other. Filename
//! prefixes would mean a global sequence scattered through resource folders (`01-` in `Auth/`,
//! `02-` in `Users/`), which is unreadable and unmaintainable the moment anything is inserted.
//!
//! Folder order still exists, for the case it *is* right: smoke-testing everything under one
//! feature's folder, where the order genuinely does not matter. `runner::run` takes a step list
//! and neither producer knows about the other.
//!
//! **A reserved directory, exactly like `environments/`** — and for the same reason, which
//! environments hit first: `collection::scan` skips it by name, or every flow is reported as an
//! unparseable request. Steps are the collection-relative paths `scan` already produces and the
//! panel already draws, so a step is both readable in a diff and resolvable to a file.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The reserved directory name inside a collection. Skipped by `collection::scan`.
pub const DIRECTORY: &str = "flows";

#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    #[error("could not read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not a valid flow: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not write {}: {source}", path.display())]
    Write {
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
    #[error("could not serialize the flow: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("{0:?} already exists")]
    NameTaken(String),
    #[error("{0:?} is not a usable flow name")]
    InvalidName(String),
}

/// One ordered sequence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flow {
    /// Filename stem, and what the picker shows.
    #[serde(skip)]
    pub name: String,
    /// Collection-relative paths, in run order. A step may repeat — logging in twice around a
    /// teardown is an ordinary thing to want, and a list gives it for free where a set would not.
    pub steps: Vec<String>,
}

pub fn directory(collection_root: &Path) -> PathBuf {
    collection_root.join(DIRECTORY)
}

/// Every flow in the collection, sorted by name.
///
/// Like `collection::scan`, one unreadable file is skipped rather than failing the listing: a
/// half-edited flow must not hide the others.
pub fn scan(collection_root: &Path) -> Vec<Flow> {
    let Ok(listing) = std::fs::read_dir(directory(collection_root)) else {
        return Vec::new();
    };

    let mut names: Vec<String> = listing
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            Some(name.strip_suffix(".json")?.to_string())
        })
        .collect();
    names.sort();

    names
        .into_iter()
        .filter_map(|name| match read(collection_root, &name) {
            Ok(flow) => Some(flow),
            Err(error) => {
                eprintln!("[zuno] skipping flow: {error}");
                None
            }
        })
        .collect()
}

pub fn read(collection_root: &Path, name: &str) -> Result<Flow, FlowError> {
    let path = directory(collection_root).join(format!("{name}.json"));
    let bytes = std::fs::read(&path).map_err(|source| FlowError::Read {
        path: path.clone(),
        source,
    })?;

    let mut flow: Flow =
        serde_json::from_slice(&bytes).map_err(|source| FlowError::Parse { path, source })?;
    // The name is the filename, not a field: a `mv` cannot then leave a stale one behind — the
    // same reason `WorkspaceEntry` has no `name` and tab labels derive from the URL.
    flow.name = name.to_string();
    Ok(flow)
}

pub fn save(collection_root: &Path, flow: &Flow) -> Result<(), FlowError> {
    let dir = directory(collection_root);
    std::fs::create_dir_all(&dir).map_err(|source| FlowError::Write {
        path: dir.clone(),
        source,
    })?;

    let path = dir.join(format!("{}.json", flow.name));
    let mut bytes = serde_json::to_vec_pretty(flow).map_err(FlowError::Serialize)?;
    bytes.push(b'\n');

    // Through a temp file for `collection::write`'s reason: a half-written flow is one `scan`
    // skips, and the picker would silently lose it.
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, &bytes).map_err(|source| FlowError::Write {
        path: temp.clone(),
        source,
    })?;
    std::fs::rename(&temp, &path).map_err(|source| FlowError::Write { path, source })
}

/// Create an empty flow, returning the name it was given.
pub fn create(collection_root: &Path, label: &str) -> Result<String, FlowError> {
    let name = valid_name(label)?;
    if directory(collection_root).join(format!("{name}.json")).exists() {
        return Err(FlowError::NameTaken(name));
    }

    save(
        collection_root,
        &Flow {
            name: name.clone(),
            steps: Vec::new(),
        },
    )?;
    Ok(name)
}

pub fn rename(collection_root: &Path, from: &str, label: &str) -> Result<String, FlowError> {
    let name = valid_name(label)?;
    if name == from {
        return Ok(name);
    }

    let dir = directory(collection_root);
    if dir.join(format!("{name}.json")).exists() {
        return Err(FlowError::NameTaken(name));
    }

    let old = dir.join(format!("{from}.json"));
    std::fs::rename(&old, dir.join(format!("{name}.json")))
        .map_err(|source| FlowError::Write { path: old, source })?;
    Ok(name)
}

/// Move a flow to the desktop trash.
///
/// Trash rather than delete for the collection panel's reason: a flow is authored work, and the
/// order in it is the part that took the thought.
pub fn trash(collection_root: &Path, name: &str) -> Result<(), FlowError> {
    let path = directory(collection_root).join(format!("{name}.json"));
    if !path.exists() {
        return Ok(());
    }

    trash::delete(&path).map_err(|source| FlowError::Trash { path, source })
}

/// Slug a typed label into a filename stem, refusing one with nothing usable in it.
fn valid_name(label: &str) -> Result<String, FlowError> {
    // Checked on the label for `environment::valid_name`'s reason: `slug` answers an unusable
    // name with `"request"`, which is the right fallback for a derived filename and the wrong
    // one here.
    if !label.chars().any(char::is_alphanumeric) {
        return Err(FlowError::InvalidName(label.to_string()));
    }
    Ok(crate::collection::slug(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zuno-flow-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(directory(&dir)).expect("scratch");
        dir
    }

    #[test]
    fn a_flow_round_trips_its_order() {
        // Order *is* the content. A flow that came back sorted, deduplicated or reordered would
        // be a different flow, silently.
        let root = scratch("round-trip");
        let flow = Flow {
            name: "user-lifecycle".into(),
            steps: vec![
                "Auth/Login.json".into(),
                "Users/Create.json".into(),
                "Users/Delete.json".into(),
                // Logging in again around a teardown is ordinary, and a list gives it for free.
                "Auth/Login.json".into(),
            ],
        };

        save(&root, &flow).expect("save");
        assert_eq!(read(&root, "user-lifecycle").expect("read"), flow);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_name_comes_from_the_filename_not_the_file() {
        // Stored, it could go stale against a `mv` — the same reason `WorkspaceEntry` has no
        // `name` and tab labels derive from the URL.
        let root = scratch("naming");
        std::fs::write(
            directory(&root).join("smoke.json"),
            r#"{"name":"something-else","steps":[]}"#,
        )
        .expect("write");

        assert_eq!(read(&root, "smoke").expect("read").name, "smoke");
    }

    #[test]
    fn scanning_lists_flows_and_skips_an_unreadable_one() {
        let root = scratch("scan");
        save(&root, &Flow { name: "beta".into(), steps: vec!["a.json".into()] }).expect("save");
        save(&root, &Flow { name: "alpha".into(), steps: Vec::new() }).expect("save");
        // A half-edited flow must not hide the others, the same courtesy `collection::scan`
        // extends to an unparseable request.
        std::fs::write(directory(&root).join("broken.json"), "{oh no").expect("write");

        let names: Vec<String> = scan(&root).into_iter().map(|flow| flow.name).collect();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn creating_refuses_a_taken_or_unusable_name() {
        let root = scratch("names");
        assert_eq!(create(&root, "User lifecycle").expect("create"), "User-lifecycle");
        assert!(create(&root, "User lifecycle").is_err(), "already taken");
        assert!(create(&root, "///").is_err(), "nothing usable in it");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn renaming_keeps_the_steps() {
        let root = scratch("rename");
        save(&root, &Flow { name: "old".into(), steps: vec!["a.json".into()] }).expect("save");

        assert_eq!(rename(&root, "old", "new").expect("rename"), "new");
        assert_eq!(read(&root, "new").expect("read").steps, vec!["a.json".to_string()]);
        assert!(!directory(&root).join("old.json").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_flows_directory_is_not_scanned_as_requests() {
        // The reason it is reserved by name at all: without it every flow is reported as an
        // unparseable request on every scan — exactly what `environments/` hit first.
        //
        // **The file inside `flows/` is a valid request, deliberately.** A *flow* in there is
        // skipped either way, because it does not deserialize as a `RequestSpec` — so asserting
        // with one proves nothing about the reservation, which is the same trap
        // `environment.rs` already documents. Only a file that would otherwise be offered as a
        // request can tell the two apart.
        let root = scratch("reserved");
        let spec = serde_json::to_vec(&crate::RequestSpec::default()).expect("json");
        std::fs::write(directory(&root).join("looks-like-a-request.json"), &spec).expect("write");
        std::fs::write(root.join("real.json"), &spec).expect("write");

        let found: Vec<String> = crate::collection::scan(&root)
            .into_iter()
            .map(|entry| entry.relative)
            .collect();
        assert_eq!(found, vec!["real.json".to_string()]);

        std::fs::remove_dir_all(&root).ok();
    }
}
