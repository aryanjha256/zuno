//! A whole collection as one file, for handing to another Zuno.
//!
//! **An envelope, not a second schema.** Each request is its existing `RequestSpec`
//! serialization embedded verbatim — the same bytes `collection::write` puts on disk. That is
//! the entire design decision: a hand-written "Zuno spec" would be a *second description of a
//! request*, and every new kind and every new field would have to be kept in step with the model
//! forever. Here a new kind travels for free, exactly as it does in a collection directory.
//!
//! **Why it exists when Postman export already does.** Postman has no home for per-request
//! settings, captures, assertions or `expect_status`, so exporting to it to reach another *Zuno*
//! user loses precisely the things Zuno adds. Interop should not be the only way out.
//!
//! **This one carries a version, and collection files deliberately do not.** A collection file
//! is yours, on your disk, and a version line would be diff churn on every one of them
//! (invariant 9). A bundle is the opposite: written once and opened *somewhere else, possibly by
//! an older build*. That is exactly the case where "written by a newer Zuno" beats a cryptic
//! parse error — which is the lesson 0.2.9 taught expensively.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::collection::Entry;
use crate::environment::EnvironmentFile;
use crate::import::{Import, ImportError, Imported, NamedVariables, Variable};
use crate::request::RequestSpec;

/// Bumped when the envelope's shape changes. The *specs* inside version themselves, through
/// `RequestSpec`'s own compatibility shim.
pub const VERSION: u32 = 1;

/// Obviously ours, and obviously JSON.
pub const EXTENSION: &str = "zuno.json";

#[derive(Serialize, Deserialize)]
struct Stored {
    zuno: u32,
    name: String,
    /// Every directory under the export root, so an **empty** folder survives the trip. `scan`
    /// only returns files, so without this a folder you made and have not filled yet vanishes.
    #[serde(default)]
    folders: Vec<String>,
    requests: Vec<StoredRequest>,
    #[serde(default)]
    environments: Vec<StoredEnvironment>,
}

#[derive(Serialize, Deserialize)]
struct StoredRequest {
    /// Relative to the export root: `billing/invoices.json`.
    path: String,
    /// The request, in exactly the shape a collection file holds.
    spec: RequestSpec,
}

#[derive(Serialize, Deserialize)]
struct StoredEnvironment {
    name: String,
    /// **The committed half only.** `dev.local.json` is gitignored because it holds secrets, and
    /// a bundle is a thing you send to someone — putting the local half in it would ship tokens
    /// to whoever receives it, which is the leak invariant 10's file split exists to prevent.
    values: BTreeMap<String, String>,
}

/// Build a bundle from everything under one collection root.
pub fn to_bundle(name: &str, entries: &[Entry], folders: &[String], environments: &[EnvironmentFile]) -> String {
    let stored = Stored {
        zuno: VERSION,
        name: name.to_string(),
        folders: folders.to_vec(),
        requests: entries
            .iter()
            .map(|entry| StoredRequest {
                path: entry.relative.clone(),
                spec: entry.spec.clone(),
            })
            .collect(),
        environments: environments
            .iter()
            .filter(|file| !file.committed.is_empty())
            .map(|file| StoredEnvironment {
                name: file.name.clone(),
                values: file.committed.clone(),
            })
            .collect(),
    };

    serde_json::to_string_pretty(&stored).unwrap_or_default()
}

/// Whether a document looks like one of ours, for `import::parse`'s sniff.
pub fn claims(root: &Value) -> bool {
    root.get("zuno").and_then(Value::as_u64).is_some() && root.get("requests").is_some()
}

/// Read a bundle into the same `Import` every other importer produces.
///
/// **Deliberately the same type**, so the whole existing pipeline — folder allocation, collision
/// suffixes, the skipped report, writing to disk — is reused rather than written a second time.
/// A bundle import is therefore non-destructive in the same way an OpenAPI or Postman one is:
/// it lands beside what is already there.
pub fn parse(root: &Value) -> Result<Import, ImportError> {
    let stored: Stored =
        serde_json::from_value(root.clone()).map_err(|error| ImportError::Malformed(error.to_string()))?;

    // A file from a newer build may hold kinds and fields this one cannot express. Refusing with
    // a message beats importing a subset and silently dropping the rest.
    if stored.zuno > VERSION {
        return Err(ImportError::NewerBundle {
            found: stored.zuno,
            supported: VERSION,
        });
    }

    let mut skipped = Vec::new();
    let requests = stored
        .requests
        .into_iter()
        .map(|request| {
            let mut parts: Vec<String> =
                request.path.split('/').map(str::to_string).collect();
            // The filename is not carried as a name: `collection::allocate` derives one and
            // handles collisions, which is what makes an import land *beside* what is there.
            parts.pop();
            Imported {
                folders: parts,
                spec: request.spec,
            }
        })
        .collect();

    // Folders with no requests in them. Named here so the import can create them; a folder you
    // made and have not filled is still something you arranged.
    let empty: Vec<String> = stored
        .folders
        .into_iter()
        .filter(|folder| !folder.is_empty())
        .collect();
    if !empty.is_empty() {
        skipped.push(format!(
            "{} empty folder(s) — recreate them if you need them: {}",
            empty.len(),
            empty.join(", ")
        ));
    }

    Ok(Import {
        requests,
        title: Some(stored.name),
        variables: Vec::new(),
        environments: stored
            .environments
            .into_iter()
            .map(|environment| NamedVariables {
                name: environment.name,
                variables: environment
                    .values
                    .into_iter()
                    .map(|(name, value)| Variable {
                        name,
                        value,
                        // Never secret: a bundle carries only the committed half, so nothing in
                        // it belongs in the gitignored file on the way back in either.
                        secret: false,
                    })
                    .collect(),
            })
            .collect(),
        recovered: 0,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::{folders, scan, write};
    use crate::request::{GraphQlRequest, Method, RequestKind};

    fn root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zuno-bundle-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");
        root
    }

    /// **The point of the format: nothing is lost.**
    ///
    /// Postman export drops settings, captures and assertions because Postman has no field for
    /// them — that is why this exists. Asserted on the whole `RequestSpec` rather than field by
    /// field, because the guarantee is "the spec is embedded verbatim" and a per-field check
    /// would pass while a newly added field quietly went missing.
    #[test]
    fn a_bundle_round_trips_a_request_exactly() {
        let root = root();

        let mut spec = RequestSpec::sample();
        spec.settings.verify_tls = false;
        spec.settings.timeout = Some(std::time::Duration::from_secs(90));
        spec.expect_status = Some(201);
        spec.captures = vec![crate::capture::Capture {
            path: "$.token".into(),
            name: "token".into(),
            ..Default::default()
        }];
        std::fs::create_dir_all(root.join("billing")).expect("subdir");
        write(&root.join("billing/create.json"), &spec).expect("write");

        let json = to_bundle("mine", &scan(&root), &folders(&root), &[]);
        let value: Value = serde_json::from_str(&json).expect("valid JSON");
        let import = parse(&value).expect("re-import");

        assert_eq!(import.requests.len(), 1);
        let back = &import.requests[0];
        assert_eq!(back.folders, vec!["billing".to_string()]);

        // `id` is normalized to 0 on write (invariant 9) and reassigned on open, so it is the
        // one field that legitimately differs.
        let mut expected = spec.clone();
        expected.id = crate::request::RequestId(0);
        assert_eq!(
            back.spec, expected,
            "everything Postman drops — settings, captures, expect_status — must survive"
        );
    }

    /// A kind the envelope knows nothing about travels anyway, because the spec is embedded
    /// rather than re-mapped. This is the property that makes gRPC free here.
    #[test]
    fn a_bundle_carries_a_kind_it_does_not_model() {
        let root = root();

        let mut spec = RequestSpec::default();
        spec.url = "https://api.test/graphql".into();
        spec.kind = RequestKind::GraphQl(GraphQlRequest {
            method: Method::Post,
            query: "query Me { me { id } }".into(),
            variables: r#"{"n":1}"#.into(),
            operation: Some("Me".into()),
        });
        write(&root.join("me.json"), &spec).expect("write");

        let json = to_bundle("mine", &scan(&root), &folders(&root), &[]);
        let value: Value = serde_json::from_str(&json).expect("valid JSON");
        let import = parse(&value).expect("re-import");

        let graphql = import.requests[0]
            .spec
            .graphql()
            .expect("a GraphQL request must come back a GraphQL request");
        assert_eq!(graphql.operation.as_deref(), Some("Me"));

        std::fs::remove_dir_all(&root).ok();
    }

    /// **A newer bundle is refused, not partially read.**
    ///
    /// This is the whole reason it carries a version while collection files do not: it is opened
    /// on someone else's machine, by a build we cannot see. Importing a subset and dropping the
    /// rest is the silent-loss shape 0.2.9 already demonstrated.
    #[test]
    fn a_bundle_from_a_newer_zuno_is_refused() {
        let value: Value = serde_json::json!({
            "zuno": VERSION + 1,
            "name": "future",
            "requests": [],
        });
        assert!(matches!(
            parse(&value),
            Err(ImportError::NewerBundle { .. })
        ));
    }

    /// Secrets never leave. The gitignored half is not in the file at all.
    #[test]
    fn a_bundle_carries_the_committed_half_and_never_the_local_one() {
        let environment = EnvironmentFile {
            name: "dev".into(),
            committed: BTreeMap::from([("baseUrl".into(), "https://api.test".into())]),
            local: BTreeMap::from([("token".into(), "super-secret".into())]),
        };

        let json = to_bundle("mine", &[], &[], std::slice::from_ref(&environment));

        assert!(json.contains("baseUrl"), "the committed half travels");
        assert!(
            !json.contains("super-secret") && !json.contains("token"),
            "the gitignored half must never reach a file you send to someone: {json}"
        );
    }
}
