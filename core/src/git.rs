//! Git, through the user's own `git`.
//!
//! **A subprocess, not a library.** Running the installed `git` means the user's SSH keys,
//! credential helper, config and hooks all apply exactly as in their terminal — which is the part a
//! library gets wrong, and pushing to a private remote is where it shows. It also adds nothing to
//! the dependency tree or the packages. No `git` on the machine means no git features, said so
//! plainly; installing it is the user's business.
//!
//! **The workspace is the repository** in the normal case. When it is instead a folder inside a
//! larger repository, everything here is scoped to that folder: status lists only its files and
//! counts the rest, and a commit takes only its changes — someone's uncommitted code elsewhere in
//! the repo is never swept into a commit made from Zuno.
//!
//! Blocking calls, all of them. The app runs them on the background executor (invariant 3).

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// How to run `git`: the program, plus environment overrides.
///
/// The overrides exist for the tests, which must not read or write the developer's own git config
/// (invariant 6) — `isolated` points `GIT_CONFIG_GLOBAL` at a scratch file. The app uses `system`.
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
    env: Vec<(String, String)>,
}

impl Git {
    pub fn system() -> Self {
        Self { program: PathBuf::from("git"), env: Vec::new() }
    }

    /// `git` reading `global_config` as its global config and no system config at all.
    pub fn isolated(global_config: &Path) -> Self {
        Self {
            program: PathBuf::from("git"),
            env: vec![
                ("GIT_CONFIG_GLOBAL".into(), global_config.display().to_string()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ],
        }
    }

    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new(&self.program);
        command
            .current_dir(dir)
            .stdin(Stdio::null())
            // **Never wait on a prompt nobody can see.** A credential or host-key question would
            // otherwise hang the call forever with no terminal to answer it in.
            .env("GIT_TERMINAL_PROMPT", "0")
            // English, so the few failures recognised below are recognised in every locale.
            .env("LC_ALL", "C");
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    fn run<I, S>(&self, dir: &Path, args: I) -> Result<Output, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.command(dir).args(args).output().map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                GitError::NotInstalled
            } else {
                GitError::Io(error.to_string())
            }
        })?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(GitError::Failed(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// No `git` on `PATH`.
    NotInstalled,
    /// Git has no name or email to put on a commit.
    NoIdentity,
    /// A commit with nothing in it.
    NothingToCommit,
    /// Git said no; its own message, trimmed.
    Failed(String),
    Io(String),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::NotInstalled => write!(f, "git isn't installed"),
            GitError::NoIdentity => write!(f, "git doesn't know who you are yet"),
            GitError::NothingToCommit => write!(f, "nothing to commit"),
            GitError::Failed(message) => write!(f, "{message}"),
            GitError::Io(message) => write!(f, "could not run git: {message}"),
        }
    }
}

/// A workspace folder inside a git repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// The workspace folder.
    pub workdir: PathBuf,
    /// The repository's root. Equal to `workdir` when the workspace *is* the repository.
    pub toplevel: PathBuf,
}

impl Repo {
    /// Whether the workspace is a folder inside a larger repository, which is when commits have to
    /// say they take only this folder.
    pub fn is_nested(&self) -> bool {
        self.toplevel != self.workdir
    }

    /// The workspace's path inside the repository, `/`-separated, empty when it is the root.
    fn prefix(&self) -> String {
        self.workdir
            .strip_prefix(&self.toplevel)
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default()
    }
}

/// What a workspace folder is, as far as git goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    NotInstalled,
    NotARepo,
    Repo(Repo),
}

pub fn probe(git: &Git, workdir: &Path) -> Probe {
    match git.run(workdir, ["rev-parse", "--show-toplevel"]) {
        Ok(output) => {
            let toplevel = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            // Canonical on both sides, or a symlinked home directory makes the workspace look
            // nested inside itself.
            let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            Probe::Repo(Repo {
                workdir: canonical(workdir),
                toplevel: canonical(&toplevel),
            })
        }
        Err(GitError::NotInstalled) => Probe::NotInstalled,
        Err(_) => Probe::NotARepo,
    }
}

/// How one file differs from the last commit. Staged and unstaged are not told apart: Zuno commits
/// everything in the workspace at once, so the difference never reaches the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Conflicted,
}

impl ChangeKind {
    /// The one-letter mark a row carries.
    pub fn letter(&self) -> &'static str {
        match self {
            ChangeKind::Modified => "M",
            ChangeKind::Added => "A",
            ChangeKind::Deleted => "D",
            ChangeKind::Renamed => "R",
            ChangeKind::Conflicted => "!",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Relative to the workspace folder, `/`-separated.
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// `None` on a detached HEAD.
    pub branch: Option<String>,
    /// Whether the branch tracks a remote branch at all.
    pub upstream: bool,
    pub ahead: u32,
    pub behind: u32,
    /// Changes inside the workspace folder, sorted by path.
    pub files: Vec<FileChange>,
    /// Changes elsewhere in the repository — left alone, and counted so the UI can say so.
    pub outside: usize,
}

pub fn status(git: &Git, repo: &Repo) -> Result<Status, GitError> {
    let output = git.run(
        &repo.toplevel,
        ["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"],
    )?;
    Ok(parse_status(&output.stdout, &repo.prefix()))
}

/// Read `git status --porcelain=v2 --branch -z`, keeping the entries under `prefix`.
///
/// Pure, so the format is pinned without a repository. The `-z` form is the one that survives
/// spaces and non-ASCII in a path: nothing is quoted, every record ends in NUL, and a rename's
/// original path is the record after it.
pub fn parse_status(bytes: &[u8], prefix: &str) -> Status {
    let mut status = Status::default();
    let mut records = bytes.split(|&b| b == 0).map(|r| String::from_utf8_lossy(r).to_string());
    let mut changes = Vec::new();

    while let Some(record) = records.next() {
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.head" if value != "(detached)" => status.branch = Some(value.to_string()),
                "branch.upstream" => status.upstream = true,
                "branch.ab" => {
                    for part in value.split(' ') {
                        if let Some(n) = part.strip_prefix('+') {
                            status.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            status.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let mut fields = record.splitn(2, ' ');
        let (Some(tag), Some(rest)) = (fields.next(), fields.next()) else { continue };
        let change = match tag {
            "1" => rest.splitn(8, ' ').nth(7).map(|path| (path.to_string(), kind_of(rest))),
            "2" => {
                // The original path is the next record; consumed so it is not read as an entry.
                let _origin = records.next();
                rest.splitn(9, ' ').nth(8).map(|path| (path.to_string(), ChangeKind::Renamed))
            }
            "u" => rest.splitn(10, ' ').nth(9).map(|path| (path.to_string(), ChangeKind::Conflicted)),
            "?" => Some((rest.to_string(), ChangeKind::Added)),
            _ => None,
        };
        if let Some(change) = change {
            changes.push(change);
        }
    }

    for (path, kind) in changes {
        match within(&path, prefix) {
            Some(relative) => status.files.push(FileChange { path: relative, kind }),
            None => status.outside += 1,
        }
    }
    status.files.sort_by(|a, b| a.path.cmp(&b.path));
    status
}

/// An ordinary entry's kind, from its two-letter `XY` (index, worktree).
fn kind_of(rest: &str) -> ChangeKind {
    let xy = rest.split(' ').next().unwrap_or("..");
    if xy.contains('A') {
        ChangeKind::Added
    } else if xy.contains('D') {
        ChangeKind::Deleted
    } else {
        ChangeKind::Modified
    }
}

/// `path` relative to the workspace, when it is inside it.
fn within(path: &str, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some(path.to_string());
    }
    path.strip_prefix(prefix)?.strip_prefix('/').map(str::to_string)
}

/// Make the workspace a repository.
pub fn init(git: &Git, workdir: &Path) -> Result<(), GitError> {
    git.run(workdir, ["init"])?;
    Ok(())
}

/// Commit every change inside the workspace folder, and nothing outside it.
///
/// `add -A -- .` stages the folder, deletions included; `commit --only -- .` then commits exactly
/// that pathspec, whatever else happens to be staged in the repository — which is the half that
/// matters when the workspace is a folder in someone's code repo.
pub fn commit(git: &Git, repo: &Repo, message: &str) -> Result<(), GitError> {
    let status = status(git, repo)?;
    if status.files.is_empty() {
        return Err(GitError::NothingToCommit);
    }
    git.run(&repo.workdir, ["add", "-A", "--", "."])?;
    match git.run(&repo.workdir, ["commit", "--only", "-m", message, "--", "."]) {
        Ok(_) => Ok(()),
        Err(GitError::Failed(stderr)) if is_identity_error(&stderr) => Err(GitError::NoIdentity),
        Err(error) => Err(error),
    }
}

fn is_identity_error(stderr: &str) -> bool {
    stderr.contains("Please tell me who you are")
        || stderr.contains("empty ident name")
        || stderr.contains("unable to auto-detect email address")
}

/// Set the name and email commits carry — for every repository on this machine, or this one only.
pub fn set_identity(
    git: &Git,
    workdir: &Path,
    name: &str,
    email: &str,
    everywhere: bool,
) -> Result<(), GitError> {
    let scope = if everywhere { "--global" } else { "--local" };
    git.run(workdir, ["config", scope, "user.name", name])?;
    git.run(workdir, ["config", scope, "user.email", email])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch repository with its own global config, so nothing here reads or writes the
    /// developer's (invariant 6).
    struct Scratch {
        root: PathBuf,
        git: Git,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("zuno-git-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("ws")).unwrap();
            let config = root.join("gitconfig");
            std::fs::write(&config, "[init]\n\tdefaultBranch = main\n").unwrap();
            Self { git: Git::isolated(&config), root }
        }

        fn ws(&self) -> PathBuf {
            self.root.join("ws")
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.ws().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn identify(&self) {
            set_identity(&self.git, &self.ws(), "Test", "test@example.com", true).unwrap();
        }

        fn repo(&self, dir: &Path) -> Repo {
            match probe(&self.git, dir) {
                Probe::Repo(repo) => repo,
                other => panic!("expected a repository, got {other:?}"),
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn status_reads_every_kind_of_record() {
        let bytes = b"# branch.oid abc\0# branch.head feat/refunds\0# branch.upstream origin/feat/refunds\0\
# branch.ab +1 -2\0\
1 .M N... 100644 100644 100644 h1 h2 payments-api/create-order.json\0\
1 A. N... 000000 100644 100644 h1 h2 payments-api/refund order.json\0\
1 .D N... 100644 100644 000000 h1 h2 old.json\0\
2 R. N... 100644 100644 100644 h1 h2 R100 orders/new.json\0orders/old.json\0\
u UU N... 100644 100644 100644 100644 h1 h2 h3 orders/conflict.json\0\
? environments/dev.json\0";
        let status = parse_status(bytes, "");
        assert_eq!(status.branch.as_deref(), Some("feat/refunds"));
        assert!(status.upstream);
        assert_eq!((status.ahead, status.behind), (1, 2));
        let files: Vec<(&str, ChangeKind)> =
            status.files.iter().map(|f| (f.path.as_str(), f.kind)).collect();
        assert_eq!(
            files,
            vec![
                ("environments/dev.json", ChangeKind::Added),
                ("old.json", ChangeKind::Deleted),
                ("orders/conflict.json", ChangeKind::Conflicted),
                ("orders/new.json", ChangeKind::Renamed),
                ("payments-api/create-order.json", ChangeKind::Modified),
                ("payments-api/refund order.json", ChangeKind::Added),
            ]
        );
    }

    #[test]
    fn a_nested_workspace_sees_only_its_own_files() {
        let bytes = b"# branch.head main\0\
1 .M N... 100644 100644 100644 h1 h2 api/zuno/orders/list.json\0\
1 .M N... 100644 100644 100644 h1 h2 api/src/main.rs\0\
? api/zunoish/x.json\0";
        let status = parse_status(bytes, "api/zuno");
        assert_eq!(status.files.len(), 1);
        assert_eq!(status.files[0].path, "orders/list.json");
        // `api/zunoish` shares the prefix's characters but is not inside it.
        assert_eq!(status.outside, 2);
    }

    #[test]
    fn a_folder_that_is_not_a_repository_says_so() {
        let scratch = Scratch::new("probe");
        assert_eq!(probe(&scratch.git, &scratch.ws()), Probe::NotARepo);
        init(&scratch.git, &scratch.ws()).unwrap();
        let repo = scratch.repo(&scratch.ws());
        assert!(!repo.is_nested());
    }

    #[test]
    fn a_commit_takes_the_workspace_and_nothing_else() {
        let scratch = Scratch::new("nested");
        scratch.identify();
        // The repository is the scratch root; the workspace is a folder inside it.
        init(&scratch.git, &scratch.root).unwrap();
        std::fs::write(scratch.root.join("code.rs"), "fn main() {}").unwrap();
        scratch.write("orders/list.json", "{}");
        scratch.write("gone.json", "{}");

        let repo = scratch.repo(&scratch.ws());
        assert!(repo.is_nested());
        let before = status(&scratch.git, &repo).unwrap();
        assert_eq!(before.files.len(), 2);
        // `gitconfig` and `code.rs`, outside the workspace.
        assert_eq!(before.outside, 2);

        commit(&scratch.git, &repo, "first").unwrap();
        let after = status(&scratch.git, &repo).unwrap();
        assert!(after.files.is_empty(), "everything in the workspace was committed");
        assert_eq!(after.outside, 2, "and nothing outside it was");

        // A deletion is a change like any other, and commits too.
        std::fs::remove_file(scratch.ws().join("gone.json")).unwrap();
        let deleted = status(&scratch.git, &repo).unwrap();
        assert_eq!(deleted.files, vec![FileChange { path: "gone.json".into(), kind: ChangeKind::Deleted }]);
        commit(&scratch.git, &repo, "remove").unwrap();
        assert!(status(&scratch.git, &repo).unwrap().files.is_empty());
    }

    #[test]
    fn a_commit_without_a_name_asks_for_one() {
        let scratch = Scratch::new("identity");
        init(&scratch.git, &scratch.ws()).unwrap();
        scratch.write("a.json", "{}");
        let repo = scratch.repo(&scratch.ws());
        // No user.name anywhere git looks — and no fallback to the machine's hostname, which git
        // would otherwise use as an email.
        set_identity(&scratch.git, &scratch.ws(), "", "", true).unwrap();

        assert_eq!(commit(&scratch.git, &repo, "first"), Err(GitError::NoIdentity));

        set_identity(&scratch.git, &scratch.ws(), "Test", "test@example.com", false).unwrap();
        commit(&scratch.git, &repo, "first").unwrap();
    }

    #[test]
    fn nothing_to_commit_is_its_own_answer() {
        let scratch = Scratch::new("empty");
        scratch.identify();
        init(&scratch.git, &scratch.ws()).unwrap();
        let repo = scratch.repo(&scratch.ws());
        assert_eq!(commit(&scratch.git, &repo, "x"), Err(GitError::NothingToCommit));
    }
}
