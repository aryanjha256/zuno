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
        let output = self.command(dir).args(args).output().map_err(spawn_error)?;
        checked(output)
    }

    /// `run`, for a call that talks to a remote — and may therefore never come back.
    ///
    /// **`GIT_TERMINAL_PROMPT=0` covers git's own prompts and not ssh's**: a key with a passphrase
    /// and no agent makes ssh read `/dev/tty` directly, so a Zuno launched from a terminal would
    /// wait on a question nobody can see, with the Sync button spinning forever. Bounded instead,
    /// and the timeout says what probably happened.
    fn run_remote<I, S>(&self, dir: &Path, args: I) -> Result<Output, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut child = self
            .command(dir)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(spawn_error)?;
        let deadline = std::time::Instant::now() + REMOTE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GitError::Failed(
                        "git gave up waiting for the remote — it may be asking for a password or \
                         passphrase. Run one fetch in a terminal, or add your key to an ssh agent."
                            .to_string(),
                    ));
                }
                Err(error) => return Err(GitError::Io(error.to_string())),
            }
        }
        checked(child.wait_with_output().map_err(|error| GitError::Io(error.to_string()))?)
    }
}

/// How long a fetch, pull or push may take before it is presumed stuck on a prompt.
const REMOTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

fn spawn_error(error: io::Error) -> GitError {
    if error.kind() == io::ErrorKind::NotFound {
        GitError::NotInstalled
    } else {
        GitError::Io(error.to_string())
    }
}

fn checked(output: Output) -> Result<Output, GitError> {
    if output.status.success() {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // Some failures — a conflicting rebase among them — explain themselves on stdout.
        Err(GitError::Failed(if stderr.is_empty() { stdout } else { stderr }))
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
    /// A sync stopped on a file changed on both sides; the repository is mid-rebase or mid-merge.
    Conflict,
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
            GitError::Conflict => write!(f, "a request changed on both sides"),
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

/// A sync that stopped partway, waiting on the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Rebase,
    Merge,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// `None` on a detached HEAD — which is also what a rebase in progress looks like.
    pub branch: Option<String>,
    /// No commit yet: there is nothing to publish until there is one.
    pub initial: bool,
    /// Whether any remote is configured; Publish is offered until one is.
    pub remote: bool,
    /// A rebase or merge stopped on a conflict.
    pub operation: Option<Operation>,
    /// What the remote branch changes that this one has not taken yet, inside the workspace —
    /// as of the last fetch, so it is current right after a Sync and may lag before one.
    pub incoming: Vec<FileChange>,
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
    let mut status = parse_status(&output.stdout, &repo.prefix());
    status.remote = !git.run(&repo.toplevel, ["remote"])?.stdout.trim_ascii().is_empty();
    let dir = git_dir(git, repo)?;
    status.operation = operation_in(&dir);
    // Which commit is "theirs" and which "ours" for the incoming list. **Mid-rebase `HEAD` is
    // detached** — no branch, no `@{upstream}` — so both come from the files the rebase keeps:
    // `onto` is what it is rebasing onto, `ORIG_HEAD` the branch as it was, `head-name` its name.
    let sides = match status.operation {
        Some(Operation::Rebase) => {
            let state = ["rebase-merge", "rebase-apply"]
                .iter()
                .map(|name| dir.join(name))
                .find(|path| path.exists());
            let read = |file: &str| {
                state
                    .as_ref()
                    .and_then(|state| std::fs::read_to_string(state.join(file)).ok())
                    .map(|text| text.trim().to_string())
            };
            if let Some(branch) = read("head-name") {
                status.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
            }
            read("onto").map(|onto| (onto, "ORIG_HEAD".to_string()))
        }
        _ if status.upstream && (status.behind > 0 || status.operation.is_some()) => {
            Some(("@{upstream}".to_string(), "HEAD".to_string()))
        }
        _ => None,
    };
    if let Some((theirs, ours)) = sides {
        status.incoming = incoming(git, repo, &theirs, &ours).unwrap_or_default();
    }
    Ok(status)
}

fn git_dir(git: &Git, repo: &Repo) -> Result<PathBuf, GitError> {
    let output = git.run(&repo.toplevel, ["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

/// Whether a rebase or merge is waiting, read off the git directory the way git itself does.
fn operation(git: &Git, repo: &Repo) -> Result<Option<Operation>, GitError> {
    Ok(operation_in(&git_dir(git, repo)?))
}

fn operation_in(dir: &Path) -> Option<Operation> {
    if dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists() {
        Some(Operation::Rebase)
    } else if dir.join("MERGE_HEAD").exists() {
        Some(Operation::Merge)
    } else {
        None
    }
}

/// The workspace files `theirs` changes since it and `ours` diverged.
fn incoming(git: &Git, repo: &Repo, theirs: &str, ours: &str) -> Result<Vec<FileChange>, GitError> {
    let base = git.run(&repo.toplevel, ["merge-base", ours, theirs])?;
    let base = String::from_utf8_lossy(&base.stdout).trim().to_string();
    let output = git.run(
        &repo.toplevel,
        ["diff", "--name-status", "-z", "--no-renames", &base, theirs],
    )?;
    let prefix = repo.prefix();
    let mut fields = output.stdout.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).to_string());
    let mut files = Vec::new();
    while let (Some(letter), Some(path)) = (fields.next(), fields.next()) {
        let kind = match letter.as_str() {
            "A" => ChangeKind::Added,
            "D" => ChangeKind::Deleted,
            _ => ChangeKind::Modified,
        };
        if let Some(path) = within(&path, &prefix) {
            files.push(FileChange { path, kind });
        }
    }
    Ok(files)
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
                "branch.oid" => status.initial = value == "(initial)",
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

/// Put the repository on a remote for the first time: `origin` set to `url`, the branch pushed
/// and tracked, so every later Sync is plain.
pub fn publish(git: &Git, repo: &Repo, url: &str) -> Result<(), GitError> {
    if status(git, repo)?.initial {
        return Err(GitError::Failed("Commit first — there is nothing to publish yet".into()));
    }
    let remotes = git.run(&repo.toplevel, ["remote"])?;
    let has_origin = String::from_utf8_lossy(&remotes.stdout).lines().any(|r| r == "origin");
    let verb = if has_origin { "set-url" } else { "add" };
    git.run(&repo.toplevel, ["remote", verb, "origin", url])?;
    git.run_remote(&repo.toplevel, ["push", "-u", "origin", "HEAD"])?;
    Ok(())
}

/// Bring the branch level with its remote: fetch, take their commits under ours, push ours.
///
/// **Rebase unless the repository says otherwise** — `pull.rebase=false` is the one way a team
/// tells git it wants merges, and it is honoured; anything else (unset included) rebases, so the
/// history stays a line rather than gaining a merge commit per sync. `--autostash` lets a sync run
/// over uncommitted edits, putting them back afterwards. Done as a local rebase or merge onto the
/// fetched `@{upstream}` rather than `git pull`, which would fetch a second time.
///
/// A conflict is not a failure: it returns `Conflict` and leaves the repository mid-operation for
/// `continue_sync` or `abort_sync`.
pub fn sync(git: &Git, repo: &Repo) -> Result<(), GitError> {
    let before = status(git, repo)?;
    if before.operation.is_some() {
        return Err(GitError::Conflict);
    }
    if !before.remote {
        return Err(GitError::Failed("Publish to a remote first".into()));
    }
    git.run_remote(&repo.toplevel, ["fetch", "--prune"])?;
    let fetched = status(git, repo)?;
    if fetched.upstream && fetched.behind > 0 {
        let merges = git
            .run(&repo.toplevel, ["config", "--get", "pull.rebase"])
            .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "false")
            .unwrap_or(false);
        let taken = if merges {
            git.run(&repo.toplevel, ["merge", "--no-edit", "--autostash", "@{upstream}"])
        } else {
            git.run(&repo.toplevel, ["rebase", "--autostash", "@{upstream}"])
        };
        if let Err(error) = taken {
            return Err(match operation(git, repo)? {
                Some(_) => GitError::Conflict,
                None => error,
            });
        }
    }
    push_if_ahead(git, repo)
}

fn push_if_ahead(git: &Git, repo: &Repo) -> Result<(), GitError> {
    let now = status(git, repo)?;
    if !now.upstream {
        git.run_remote(&repo.toplevel, ["push", "-u", "origin", "HEAD"])?;
    } else if now.ahead > 0 {
        git.run_remote(&repo.toplevel, ["push"])?;
    }
    Ok(())
}

/// Finish a sync that stopped on a conflict, once the files are fixed — then push.
///
/// A conflicted file stays "unmerged" to git until it is added, so whether it is *fixed* is read
/// from its contents: any line still opening with git's `<<<<<<<` marker means not yet, and the
/// file is named. Without that check, Continue would commit the markers into a request file and
/// the next scan would report it unreadable.
pub fn continue_sync(git: &Git, repo: &Repo) -> Result<(), GitError> {
    let now = status(git, repo)?;
    let Some(operation) = now.operation else {
        return push_if_ahead(git, repo);
    };
    for file in now.files.iter().filter(|f| f.kind == ChangeKind::Conflicted) {
        let text = std::fs::read_to_string(repo.workdir.join(&file.path)).unwrap_or_default();
        if text.lines().any(|line| line.starts_with("<<<<<<<")) {
            return Err(GitError::Failed(format!("{} still has conflict markers", file.path)));
        }
    }
    git.run(&repo.workdir, ["add", "-A", "--", "."])?;
    let finished = match operation {
        // `core.editor=true` accepts the commit message git would otherwise open an editor for.
        Operation::Rebase => git.run(&repo.toplevel, ["-c", "core.editor=true", "rebase", "--continue"]),
        Operation::Merge => git.run(&repo.toplevel, ["commit", "--no-edit"]),
    };
    if let Err(error) = finished {
        // The next commit of a rebase can conflict too; that is the same state again, not an error.
        return Err(match self::operation(git, repo)? {
            Some(_) => GitError::Conflict,
            None => error,
        });
    }
    push_if_ahead(git, repo)
}

/// Put everything back as it was before the sync began, uncommitted edits included.
pub fn abort_sync(git: &Git, repo: &Repo) -> Result<(), GitError> {
    match operation(git, repo)? {
        Some(Operation::Rebase) => git.run(&repo.toplevel, ["rebase", "--abort"])?,
        Some(Operation::Merge) => git.run(&repo.toplevel, ["merge", "--abort"])?,
        None => return Ok(()),
    };
    Ok(())
}

/// A file as the last commit has it, by its path inside the workspace — `None` when that commit
/// does not have it (a new file, or no commit yet), which is an answer rather than an error.
pub fn show_head(git: &Git, repo: &Repo, path: &str) -> Result<Option<Vec<u8>>, GitError> {
    let prefix = repo.prefix();
    let full = if prefix.is_empty() { path.to_string() } else { format!("{prefix}/{path}") };
    match git.run(&repo.toplevel, ["show", &format!("HEAD:{full}")]) {
        Ok(output) => Ok(Some(output.stdout)),
        Err(GitError::Failed(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Git's own diff of one file against the last commit, as text — the Changes tab's raw view. Empty
/// when the saved file matches it; edits still only on screen are not in a file git can see.
pub fn diff_file(git: &Git, repo: &Repo, path: &str) -> Result<String, GitError> {
    let output = git.run(&repo.workdir, ["diff", "--no-color", "HEAD", "--", path])?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A workspace's name from its repository's URL: `git@github.com:acme/acme-platform.git` and
/// `https://github.com/acme/acme-platform` are both `acme-platform`. `None` when nothing usable is
/// left, so the caller asks rather than inventing one.
pub fn repo_name(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':', '\\']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

/// Clone `url` into `dest`, which must not exist yet. Git removes a half-made `dest` itself when
/// the clone fails.
pub fn clone(git: &Git, url: &str, dest: &Path) -> Result<(), GitError> {
    let parent = dest.parent().ok_or_else(|| GitError::Failed("Nowhere to put it".into()))?;
    std::fs::create_dir_all(parent).map_err(|error| GitError::Io(error.to_string()))?;
    let dest = dest.display().to_string();
    git.run_remote(parent, ["clone", "--", url, &dest])?;
    Ok(())
}

/// Throw away one file's changes since the last commit: a new file is deleted, a changed or deleted
/// one is put back as committed.
///
/// Renames and conflicts are refused rather than guessed at — a rename needs its original path,
/// which the row does not carry, and a conflict has Continue and Undo for exactly this.
pub fn discard(git: &Git, repo: &Repo, change: &FileChange) -> Result<(), GitError> {
    let path = repo.workdir.join(&change.path);
    match change.kind {
        ChangeKind::Added => {
            // Staged-but-new files are unstaged first; for an untracked one this fails harmlessly.
            let _ = git.run(&repo.workdir, ["rm", "--cached", "-q", "--", &change.path]);
            std::fs::remove_file(&path).map_err(|error| GitError::Io(error.to_string()))
        }
        ChangeKind::Modified | ChangeKind::Deleted => {
            git.run(
                &repo.workdir,
                ["restore", "--source=HEAD", "--staged", "--worktree", "--", &change.path],
            )?;
            Ok(())
        }
        ChangeKind::Renamed | ChangeKind::Conflicted => Err(GitError::Failed(
            "Only an added, changed or deleted file can be discarded here".into(),
        )),
    }
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

    /// A bare repository for the team's remote, the workspace published to it, and a teammate's
    /// clone of it beside them.
    fn team(name: &str) -> (Scratch, Repo, Repo) {
        let scratch = Scratch::new(name);
        scratch.identify();
        let remote = scratch.root.join("remote.git");
        scratch.git.run(&scratch.root, ["init", "--bare", "remote.git"]).unwrap();

        init(&scratch.git, &scratch.ws()).unwrap();
        scratch.write("orders/create.json", "{\n  \"amount\": 100\n}\n");
        let mine = scratch.repo(&scratch.ws());
        commit(&scratch.git, &mine, "first").unwrap();
        publish(&scratch.git, &mine, &remote.display().to_string()).unwrap();

        scratch
            .git
            .run(&scratch.root, ["clone", &remote.display().to_string(), "theirs"])
            .unwrap();
        let theirs = scratch.repo(&scratch.root.join("theirs"));
        (scratch, mine, theirs)
    }

    fn merges(scratch: &Scratch, repo: &Repo) -> usize {
        let out = scratch.git.run(&repo.toplevel, ["rev-list", "--merges", "HEAD"]).unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }

    #[test]
    fn sync_takes_their_commits_under_ours_and_pushes() {
        let (scratch, mine, theirs) = team("sync");
        assert!(status(&scratch.git, &mine).unwrap().upstream, "publish tracks the branch");

        std::fs::write(theirs.workdir.join("users.json"), "{}").unwrap();
        commit(&scratch.git, &theirs, "theirs").unwrap();
        sync(&scratch.git, &theirs).unwrap();

        scratch.write("health.json", "{}");
        commit(&scratch.git, &mine, "mine").unwrap();
        sync(&scratch.git, &mine).unwrap();

        assert!(mine.workdir.join("users.json").exists(), "their request arrived");
        let after = status(&scratch.git, &mine).unwrap();
        assert_eq!((after.ahead, after.behind), (0, 0), "and ours went up");
        assert_eq!(merges(&scratch, &mine), 0, "rebased: no merge commit");
    }

    #[test]
    fn a_repo_that_asks_for_merges_gets_one() {
        let (scratch, mine, theirs) = team("merge");
        scratch.git.run(&mine.toplevel, ["config", "pull.rebase", "false"]).unwrap();

        std::fs::write(theirs.workdir.join("users.json"), "{}").unwrap();
        commit(&scratch.git, &theirs, "theirs").unwrap();
        sync(&scratch.git, &theirs).unwrap();
        scratch.write("health.json", "{}");
        commit(&scratch.git, &mine, "mine").unwrap();

        sync(&scratch.git, &mine).unwrap();
        assert_eq!(merges(&scratch, &mine), 1);
    }

    /// Both sides change the same request: Sync stops on it, Continue refuses while the markers are
    /// still in the file, then finishes — and pushes — once they are gone.
    #[test]
    fn a_conflict_stops_the_sync_until_it_is_fixed() {
        let (scratch, mine, theirs) = team("conflict");
        std::fs::write(theirs.workdir.join("orders/create.json"), "{\n  \"amount\": 300\n}\n").unwrap();
        commit(&scratch.git, &theirs, "theirs").unwrap();
        sync(&scratch.git, &theirs).unwrap();
        scratch.write("orders/create.json", "{\n  \"amount\": 250\n}\n");
        commit(&scratch.git, &mine, "mine").unwrap();

        assert_eq!(sync(&scratch.git, &mine), Err(GitError::Conflict));
        let stopped = status(&scratch.git, &mine).unwrap();
        assert_eq!(stopped.operation, Some(Operation::Rebase));
        assert!(stopped.files.iter().any(|f| f.path == "orders/create.json"
            && f.kind == ChangeKind::Conflicted));
        assert!(
            stopped.incoming.iter().any(|f| f.path == "orders/create.json"),
            "what the remote brings is listed even mid-rebase: {:?}",
            stopped.incoming
        );

        assert!(matches!(continue_sync(&scratch.git, &mine), Err(GitError::Failed(m)) if m.contains("conflict markers")));

        scratch.write("orders/create.json", "{\n  \"amount\": 250\n}\n");
        continue_sync(&scratch.git, &mine).unwrap();
        let done = status(&scratch.git, &mine).unwrap();
        assert_eq!(done.operation, None);
        assert_eq!((done.ahead, done.behind), (0, 0), "finished and pushed");
    }

    #[test]
    fn undo_puts_the_branch_back_as_it_was() {
        let (scratch, mine, theirs) = team("abort");
        std::fs::write(theirs.workdir.join("orders/create.json"), "{\n  \"amount\": 300\n}\n").unwrap();
        commit(&scratch.git, &theirs, "theirs").unwrap();
        sync(&scratch.git, &theirs).unwrap();
        scratch.write("orders/create.json", "{\n  \"amount\": 250\n}\n");
        commit(&scratch.git, &mine, "mine").unwrap();
        assert_eq!(sync(&scratch.git, &mine), Err(GitError::Conflict));

        abort_sync(&scratch.git, &mine).unwrap();
        let back = status(&scratch.git, &mine).unwrap();
        assert_eq!(back.operation, None);
        assert_eq!((back.ahead, back.behind), (1, 1));
        let text = std::fs::read_to_string(mine.workdir.join("orders/create.json")).unwrap();
        assert!(text.contains("250"), "our version, untouched: {text}");
    }

    #[test]
    fn a_repository_names_its_workspace() {
        assert_eq!(repo_name("git@github.com:acme/acme-platform.git").as_deref(), Some("acme-platform"));
        assert_eq!(repo_name("https://github.com/acme/acme-platform/").as_deref(), Some("acme-platform"));
        assert_eq!(repo_name("/srv/git/team.git").as_deref(), Some("team"));
        assert_eq!(repo_name("https://example.com/.git"), None);
        assert_eq!(repo_name(""), None);
    }

    #[test]
    fn a_clone_is_a_workspace_on_its_remote() {
        let (scratch, _mine, _theirs) = team("clone");
        let dest = scratch.root.join("workspaces").join("acme");
        let remote = scratch.root.join("remote.git").display().to_string();
        clone(&scratch.git, &remote, &dest).unwrap();
        assert!(dest.join("orders/create.json").exists());
        let repo = scratch.repo(&dest);
        assert!(status(&scratch.git, &repo).unwrap().upstream, "tracking from the start");
        assert!(clone(&scratch.git, "/nowhere/at/all.git", &scratch.root.join("x")).is_err());
    }

    #[test]
    fn discard_puts_each_kind_of_change_back() {
        let scratch = Scratch::new("discard");
        scratch.identify();
        init(&scratch.git, &scratch.ws()).unwrap();
        scratch.write("kept.json", "{\"v\": 1}");
        scratch.write("gone.json", "{}");
        let repo = scratch.repo(&scratch.ws());
        commit(&scratch.git, &repo, "first").unwrap();

        scratch.write("kept.json", "{\"v\": 2}");
        std::fs::remove_file(scratch.ws().join("gone.json")).unwrap();
        scratch.write("new.json", "{}");
        for change in status(&scratch.git, &repo).unwrap().files {
            discard(&scratch.git, &repo, &change).unwrap();
        }

        assert!(status(&scratch.git, &repo).unwrap().files.is_empty());
        assert_eq!(show_head(&scratch.git, &repo, "kept.json").unwrap().as_deref(), Some(&b"{\"v\": 1}"[..]));
        assert_eq!(show_head(&scratch.git, &repo, "never.json").unwrap(), None);
        assert_eq!(std::fs::read_to_string(scratch.ws().join("kept.json")).unwrap(), "{\"v\": 1}");
        assert!(scratch.ws().join("gone.json").exists());
        assert!(!scratch.ws().join("new.json").exists());
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
