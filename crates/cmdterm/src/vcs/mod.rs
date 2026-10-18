//! Version control queries for project roots: discovery, worktrees, status,
//! remotes, branches, history and diffs, for git and Mercurial, and for
//! Jujutsu where the user turned it on.
//!
//! Every process goes through [`cmdterm_env::Environment`], so a project on an
//! SSH host behaves like a local one. Queries use one process each where the
//! VCS allows it and parse machine-readable output from borrowed byte slices.
//!
//! Mercurial "worktrees" are shares (`hg share`). Mercurial does not record
//! shares at the source, so this crate keeps a list in
//! `<root>/.hg/cmdterm-shares`, one absolute path per line. Jujutsu
//! "worktrees" are workspaces.

use std::path::{Component, Path, PathBuf};

use cmdterm_env::{CommandError, Environment};
use serde::{Deserialize, Serialize};

mod git;
pub(crate) use git::local_common_dir;
mod hg;
mod jj;
mod untracked;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsKind {
    Git,
    Mercurial,
    Jujutsu,
}

/// A checkout of a repository: a git worktree, a Mercurial share or a
/// Jujutsu workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Checked-out branch. Mercurial reports the active bookmark, or the named
    /// branch when no bookmark is active. Jujutsu reports a bookmark at the
    /// working-copy commit, else one at its parent.
    pub branch: Option<String>,
    /// Commit id of the checked-out revision. `None` before the first commit.
    /// Jujutsu reports the change id of the working-copy commit.
    pub head: Option<String>,
    pub is_main: bool,
    pub locked: bool,
    /// The worktree directory is gone (git: `prunable`; hg: listed share
    /// missing).
    pub prunable: bool,
    pub bare: bool,
}

/// Working-copy summary. Mercurial and Jujutsu have no staging area: their
/// changes all count as `modified`, `staged` stays 0, and `ahead`/`behind`
/// stay 0. Jujutsu tracks new files itself, so `untracked` stays 0 too.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub modified: u32,
    pub untracked: u32,
    pub conflicted: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

/// One entry of a history log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// Full commit id.
    pub id: String,
    /// Abbreviated id, as the VCS abbreviates it.
    pub short: String,
    pub author: String,
    /// Author date in seconds since the Unix epoch.
    pub time: i64,
    /// Branches, bookmarks and tags at the commit, comma-separated. Empty
    /// for most commits.
    pub refs: String,
    /// First line of the message.
    pub subject: String,
}

/// What a change request from a worktree into a base branch would contain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    /// Full id of the commit where the worktree's history left the base.
    pub merge_base: String,
    /// Commits after the merge base, newest first, at most [`LOG_LIMIT`].
    pub commits: Vec<Commit>,
    /// Unified diff from the merge base in git's extended format, with
    /// `a/` and `b/` path prefixes.
    pub patch: Vec<u8>,
}

/// Most commits a log query returns.
pub const LOG_LIMIT: usize = 5000;

/// A repository in an environment. For git, `root` is the main worktree (or
/// the bare repository directory), so every linked worktree of one repository
/// maps to the same value. For Mercurial, `root` is the share source; for
/// Jujutsu, the workspace that holds the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    pub env: Environment,
    pub kind: VcsKind,
    pub root: PathBuf,
    /// For git, where the app's commands read the metadata. They name it
    /// to git instead of letting git follow a worktree's `.git`, which a
    /// sandbox may have replaced. `None` for Mercurial and Jujutsu.
    pub git: Option<GitDirs>,
}

/// A git repository as the app found it when the project was added: the
/// main worktree (or bare repository) and the common git directory, with
/// symlinks resolved. Queries for a project with one read the metadata
/// there and never look it up through a worktree's `.git`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownRepo {
    pub root: PathBuf,
    pub git_dir: PathBuf,
}

/// Where git keeps a repository's metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDirs {
    /// The common directory: configuration, hooks, refs and the list of
    /// linked worktrees. Also the main worktree's git directory.
    pub common: PathBuf,
    /// Each linked worktree's path and its administrative directory, as
    /// the common directory lists them.
    pub linked: Vec<(PathBuf, PathBuf)>,
}

impl GitDirs {
    /// The git directory of `worktree`, a worktree of the repository whose
    /// main worktree is `root`: the common directory for the main one, else
    /// the administrative directory that lists `worktree`. `None` for a
    /// path the repository does not list, or lists twice.
    pub fn git_dir(&self, root: &Path, worktree: &Path, local: bool) -> Option<&Path> {
        let same = |a: &Path, b: &Path| {
            normalize(a) == normalize(b)
                || (local
                    && std::fs::canonicalize(a)
                        .is_ok_and(|a| std::fs::canonicalize(b).is_ok_and(|b| a == b)))
        };
        if same(root, worktree) {
            return Some(&self.common);
        }
        let mut found = self.linked.iter().filter(|(path, _)| same(path, worktree));
        match (found.next(), found.next()) {
            (Some((_, admin)), None) => Some(admin),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    #[error("not a git, mercurial or jj repository: {}", .0.display())]
    NotARepository(PathBuf),
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("unexpected command output: {0}")]
    Parse(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// HEAD and the named revision share no commit, such as after one of
    /// them was rewritten from its root.
    #[error("HEAD and {0} share no commits")]
    NoCommonHistory(String),
}

/// Finds the repository containing the directory `path`. Tries git first,
/// then Mercurial, with one process each. Never asks Jujutsu.
pub fn discover(env: &Environment, path: &Path) -> Result<Repository, VcsError> {
    discover_with(env, path, false)
}

/// [`discover`], and with `jj` first asks Jujutsu, so a jj workspace wins
/// over the git repository it may share its directory with (a colocated
/// repository). A machine without `jj` falls through to git and Mercurial.
pub fn discover_with(env: &Environment, path: &Path, jj: bool) -> Result<Repository, VcsError> {
    open(env, path, jj, None)
}

/// [`discover_with`] for a project the app knows: with `known`, a git
/// repository is read where the project was found, and the worktree's
/// `.git` at `path` is not looked at. The repository's list of linked
/// worktrees is read with one process.
pub fn open(
    env: &Environment,
    path: &Path,
    jj: bool,
    known: Option<&KnownRepo>,
) -> Result<Repository, VcsError> {
    if jj && let Some(root) = jj::discover(env, path) {
        return Ok(Repository {
            env: env.clone(),
            kind: VcsKind::Jujutsu,
            root,
            git: None,
        });
    }
    let known = match known {
        Some(known) => Some(known.clone()),
        None => git::discover(env, path)?,
    };
    if let Some(known) = known {
        let git = git::dirs(env, &known.git_dir)?;
        return Ok(Repository {
            env: env.clone(),
            kind: VcsKind::Git,
            root: known.root,
            git: Some(git),
        });
    }
    match hg::discover(env, path) {
        Some(root) => Ok(Repository {
            env: env.clone(),
            kind: VcsKind::Mercurial,
            root,
            git: None,
        }),
        None => Err(VcsError::NotARepository(path.to_path_buf())),
    }
}

/// Clones `url` into a directory under `parent` named after the URL's last
/// path segment, then discovers it. Tries git first, then Mercurial when
/// `hg` is installed. A leading `~` in `parent` is the home directory on the
/// target machine; `parent` is created when missing. An existing directory
/// of that name is opened instead of cloned into.
pub fn clone(env: &Environment, url: &str, parent: &Path) -> Result<Repository, VcsError> {
    clone_with(env, url, parent, false)
}

/// [`clone`], and with `jj` a new git clone also becomes a colocated jj
/// repository when the machine has `jj`; the checkout is then discovered
/// with [`discover_with`].
pub fn clone_with(
    env: &Environment,
    url: &str,
    parent: &Path,
    jj: bool,
) -> Result<Repository, VcsError> {
    let name = clone_name(url)
        .ok_or_else(|| VcsError::InvalidArgument(format!("no repository name in {url}")))?;
    let out = env
        .command("sh")
        .args(["-c", CLONE, "sh", url])
        .arg(parent)
        .arg(name)
        .arg(if jj { "1" } else { "0" })
        .stdout()?;
    discover_with(env, &path_from_bytes(trim_newline(&out)), jj)
}

/// `sh -c CLONE sh URL PARENT NAME JJ`: prints the absolute checkout path.
const CLONE: &str = r#"p=$2
case $p in "~") p=$HOME ;; "~/"*) p=$HOME/${p#"~/"} ;; esac
mkdir -p -- "$p" && cd -- "$p" || exit 1
if [ ! -e "$3" ]; then
  if err=$(GIT_TERMINAL_PROMPT=0 git clone -q -- "$1" "$3" 2>&1 </dev/null); then
    if [ "$4" = 1 ] && command -v jj >/dev/null 2>&1; then
      (cd -- "$3" && jj git init --colocate --quiet </dev/null >/dev/null 2>&1) || :
    fi
  elif command -v hg >/dev/null 2>&1 &&
    HGPLAIN=1 hg clone -q -- "$1" "$3" </dev/null >/dev/null 2>&1; then :
  else printf '%s\n' "$err" >&2; exit 1
  fi
fi
printf '%s/%s\n' "$(pwd -P)" "$3"
"#;

/// The absolute path of the folder `path` on `env`, for a global session,
/// which needs no repository there. A leading `~` is the home directory
/// on that machine.
pub fn resolve_folder(env: &Environment, path: &Path) -> Result<PathBuf, VcsError> {
    // Git for Windows' `sh` would answer with an MSYS path such as `/c/x`.
    if cfg!(windows) && env.is_local() {
        let home = || std::env::home_dir().unwrap_or_default();
        let path = match path.strip_prefix("~") {
            Ok(rest) => home().join(rest),
            Err(_) => path.to_path_buf(),
        };
        return cmdterm_env::canonicalize(&path)
            .map_err(|error| VcsError::InvalidArgument(format!("{}: {error}", path.display())));
    }
    let out = env
        .command("sh")
        .args(["-c", RESOLVE_FOLDER, "sh"])
        .arg(path)
        .stdout()?;
    Ok(path_from_bytes(trim_newline(&out)))
}

/// `sh -c RESOLVE_FOLDER sh PATH`: prints the absolute path of `PATH`.
const RESOLVE_FOLDER: &str = r#"p=$1
case $p in "~") p=$HOME ;; "~/"*) p=$HOME/${p#"~/"} ;; esac
cd -- "$p" && pwd -P
"#;

/// The directory name a clone of `url` gets: its last path segment without
/// `.git`, as `git clone` picks it.
pub fn clone_name(url: &str) -> Option<&str> {
    let url = url.trim().trim_end_matches('/');
    let name = url.rsplit(['/', ':']).next()?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    let bad = name.is_empty() || name == "." || name == ".." || name.starts_with('-');
    (!bad && !name.contains(['\0', '\n', '\\'])).then_some(name)
}

impl Repository {
    /// The git repository as the app records it for the project, see
    /// [`KnownRepo`].
    pub fn known(&self) -> Option<KnownRepo> {
        let git = self.git.as_ref()?;
        Some(KnownRepo {
            root: self.root.clone(),
            git_dir: git.common.clone(),
        })
    }

    /// `git` in the worktree `worktree` of this git repository, or in the
    /// repository at `root`, with the options of the app's own queries,
    /// which run no command the metadata names and read it where
    /// [`Self::git`] says. Fails for a path the repository does not list as
    /// a worktree.
    pub(crate) fn git_command(&self, worktree: &Path) -> Result<cmdterm_env::EnvCommand, VcsError> {
        git::command(self, worktree)
    }

    /// The worktrees on this machine whose `.git` no longer leads to the
    /// metadata the app reads for them, such as after a sandbox replaced a
    /// `.git` symlink. Empty for other machines and other VCSs.
    pub fn replaced_dot_git(&self) -> Vec<PathBuf> {
        git::replaced_dot_git(self)
    }

    /// All worktrees; the main one comes first.
    pub fn worktrees(&self) -> Result<Vec<Worktree>, VcsError> {
        match self.kind {
            VcsKind::Git => git::worktrees(self),
            VcsKind::Mercurial => hg::worktrees(self),
            VcsKind::Jujutsu => jj::worktrees(self),
        }
    }

    /// Creates a worktree at `path` (relative paths resolve against `root`)
    /// with `branch` checked out. `create_branch` creates the branch (hg: a
    /// bookmark) at the main worktree's current revision. In Jujutsu the new
    /// workspace's working-copy commit is a child of that revision (`@-`)
    /// and a created bookmark points at it.
    pub fn add_worktree(
        &self,
        path: &Path,
        branch: &str,
        create_branch: bool,
    ) -> Result<Worktree, VcsError> {
        check_name(branch)?;
        let path = self.absolute(path);
        match self.kind {
            VcsKind::Git => git::add_worktree(self, &path, branch, create_branch),
            VcsKind::Mercurial => hg::add_worktree(self, &path, branch, create_branch),
            VcsKind::Jujutsu => jj::add_worktree(self, &path, branch, create_branch),
        }
    }

    /// Creates a worktree at `path` (relative paths resolve against `root`)
    /// at the revision checked out in the worktree `source`. With `branch`,
    /// that branch (hg: bookmark) is created there and checked out; without
    /// it, the new worktree is detached (hg: no active bookmark).
    pub fn add_worktree_from(
        &self,
        source: &Path,
        path: &Path,
        branch: Option<&str>,
    ) -> Result<Worktree, VcsError> {
        if let Some(branch) = branch {
            check_name(branch)?;
        }
        let source = self.absolute(source);
        let path = self.absolute(path);
        match self.kind {
            VcsKind::Git => git::add_worktree_at(self, &source, &path, "HEAD", branch),
            VcsKind::Mercurial => hg::add_worktree_at(self, &source, &path, ".", branch),
            VcsKind::Jujutsu => jj::add_worktree_at(self, &source, &path, "@-", branch),
        }
    }

    /// Creates a worktree at `path` (relative paths resolve against `root`)
    /// at `revision`: for git any commit-ish, such as a branch, tag, commit
    /// id or `origin/main`; for Mercurial and Jujutsu a revision or revset,
    /// of which the first match (jj: the newest) counts. With `branch`, that branch (hg: bookmark) is
    /// created there and checked out; without it, the new worktree is
    /// detached (hg: no active bookmark).
    pub fn add_worktree_at(
        &self,
        path: &Path,
        revision: &str,
        branch: Option<&str>,
    ) -> Result<Worktree, VcsError> {
        check_revision(revision)?;
        if let Some(branch) = branch {
            check_name(branch)?;
        }
        let path = self.absolute(path);
        match self.kind {
            VcsKind::Git => git::add_worktree_at(self, &self.root, &path, revision, branch),
            VcsKind::Mercurial => hg::add_worktree_at(self, &self.root, &path, revision, branch),
            VcsKind::Jujutsu => jj::add_worktree_at(self, &self.root, &path, revision, branch),
        }
    }

    /// Removes a linked worktree. Without `force`, refuses when it has
    /// uncommitted changes.
    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<(), VcsError> {
        let path = self.absolute(path);
        if path == self.root {
            return Err(VcsError::InvalidArgument(
                "cannot remove the main worktree".to_owned(),
            ));
        }
        match self.kind {
            VcsKind::Git => git::remove_worktree(self, &path, force),
            VcsKind::Mercurial => hg::remove_worktree(self, &path, force),
            VcsKind::Jujutsu => jj::remove_worktree(self, &path, force),
        }
    }

    /// Status of the worktree whose top-level directory is `worktree`.
    pub fn status(&self, worktree: &Path) -> Result<Status, VcsError> {
        match self.kind {
            VcsKind::Git => git::status(self, worktree),
            VcsKind::Mercurial => hg::status(&self.env, worktree),
            VcsKind::Jujutsu => jj::status(&self.env, worktree),
        }
    }

    pub fn remotes(&self) -> Result<Vec<Remote>, VcsError> {
        match self.kind {
            VcsKind::Git => git::remotes(self),
            VcsKind::Mercurial => hg::remotes(self),
            VcsKind::Jujutsu => jj::remotes(self),
        }
    }

    /// Paths of the files in the worktree `worktree`, relative to it and
    /// separated by NUL bytes: tracked files plus untracked ones the ignore
    /// rules let through. The order is the VCS's own and may repeat a path.
    pub fn files(&self, worktree: &Path) -> Result<Vec<u8>, VcsError> {
        match self.kind {
            VcsKind::Git => git::files(self, worktree),
            VcsKind::Mercurial => hg::files(&self.env, worktree),
            VcsKind::Jujutsu => jj::files(&self.env, worktree),
        }
    }

    /// The changed files of the worktree `worktree` as `X path\0` records:
    /// one status letter (`M` modified, `A` added, `D` deleted, `R`
    /// renamed, `U` conflicted, `?` untracked), a space and the path
    /// relative to the worktree.
    pub fn changes(&self, worktree: &Path) -> Result<Vec<u8>, VcsError> {
        match self.kind {
            VcsKind::Git => git::changes(self, worktree),
            VcsKind::Mercurial => hg::changes(&self.env, worktree),
            VcsKind::Jujutsu => jj::changes(&self.env, worktree),
        }
    }

    /// Local branches. Mercurial lists open named branches, then bookmarks.
    pub fn branches(&self) -> Result<Vec<String>, VcsError> {
        match self.kind {
            VcsKind::Git => git::branches(self),
            VcsKind::Mercurial => hg::branches(self),
            VcsKind::Jujutsu => jj::branches(self),
        }
    }

    /// Commits reachable from the revision checked out in `worktree`,
    /// newest first, at most [`LOG_LIMIT`]. Empty before the first commit.
    pub fn log(&self, worktree: &Path) -> Result<Vec<Commit>, VcsError> {
        match self.kind {
            VcsKind::Git => git::log(self, worktree, "HEAD"),
            VcsKind::Mercurial => hg::log(&self.env, worktree, "reverse(::.)"),
            VcsKind::Jujutsu => jj::log(&self.env, worktree, jj::LOG_REVSET),
        }
    }

    /// The commit `id` as text: header, full message, changed files with
    /// their line counts, then the diff. A merge commit diffs against its
    /// first parent.
    pub fn show(&self, worktree: &Path, id: &str) -> Result<Vec<u8>, VcsError> {
        check_revision(id)?;
        match self.kind {
            VcsKind::Git => git::show(self, worktree, id),
            VcsKind::Mercurial => hg::show(&self.env, worktree, id),
            VcsKind::Jujutsu => jj::show(&self.env, worktree, id),
        }
    }

    /// The branch a change request from `worktree` most likely targets:
    /// git's `origin/HEAD`, else the first of `origin/main`,
    /// `origin/master`, `main`, `master`, `trunk` that exists. Mercurial
    /// uses `default`, Jujutsu `trunk()` unless that is the root commit.
    pub fn default_base(&self, worktree: &Path) -> Result<Option<String>, VcsError> {
        match self.kind {
            VcsKind::Git => git::default_base(self, worktree),
            VcsKind::Mercurial => Ok(Some("default".to_owned())),
            VcsKind::Jujutsu => jj::default_base(&self.env, worktree),
        }
    }

    /// The changes of `worktree` since its history left `base`, a branch or
    /// any revision. With `uncommitted`, the diff runs to the files on disk
    /// instead of the checked-out commit, so staged and unstaged edits count,
    /// and untracked files that are not ignored show as new files. In
    /// Jujutsu the files on disk are the working-copy commit `@`; without
    /// `uncommitted` the review ends at its parent.
    pub fn review(
        &self,
        worktree: &Path,
        base: &str,
        uncommitted: bool,
    ) -> Result<Review, VcsError> {
        check_revision(base)?;
        match self.kind {
            VcsKind::Git => git::review(self, worktree, base, uncommitted),
            VcsKind::Mercurial => hg::review(&self.env, worktree, base, uncommitted),
            VcsKind::Jujutsu => jj::review(&self.env, worktree, base, uncommitted),
        }
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            normalize(path)
        } else {
            normalize(&self.root.join(path))
        }
    }
}

/// Rejects names that a command would read as an option.
fn check_name(name: &str) -> Result<(), VcsError> {
    if name.is_empty() || name.starts_with('-') || name.contains(['\0', '\n']) {
        Err(VcsError::InvalidArgument(format!(
            "bad branch name {name:?}"
        )))
    } else {
        Ok(())
    }
}

/// Rejects revisions that a command would read as an option.
fn check_revision(revision: &str) -> Result<(), VcsError> {
    if revision.is_empty() || revision.starts_with('-') || revision.contains(['\0', '\n']) {
        Err(VcsError::InvalidArgument(format!(
            "bad revision {revision:?}"
        )))
    } else {
        Ok(())
    }
}

/// Parses log records: fields separated by 0x1f, records ended by 0x1e.
/// Fields: id, short id, author, time, refs, subject. `refs` goes through
/// `refs` to normalize it.
fn parse_log(out: &[u8], refs: impl Fn(&[u8]) -> String) -> Vec<Commit> {
    out.split(|&b| b == 0x1e)
        .filter_map(|record| {
            let record = record.trim_ascii_start();
            let mut fields = record.split(|&b| b == 0x1f);
            let id = fields.next().filter(|id| !id.is_empty())?;
            if id.iter().all(|&b| b == b'0') {
                return None;
            }
            let short = fields.next()?;
            let author = fields.next()?;
            let time = std::str::from_utf8(fields.next()?).ok()?;
            let time = time.split(' ').next()?.parse().ok()?;
            let names = fields.next()?;
            let subject = fields.next()?;
            Some(Commit {
                id: string_from_bytes(id),
                short: string_from_bytes(short),
                author: string_from_bytes(author),
                time,
                refs: refs(names),
                subject: string_from_bytes(trim_newline(subject)),
            })
        })
        .collect()
}

/// Lexically removes `.` and `..` components. Does not touch the file system,
/// which may be on another machine.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn string_from_bytes(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn trim_newline(mut bytes: &[u8]) -> &[u8] {
    while let [rest @ .., b'\n' | b'\r'] = bytes {
        bytes = rest;
    }
    bytes
}

fn split_once(bytes: &[u8], sep: u8) -> (&[u8], &[u8]) {
    match bytes.iter().position(|&b| b == sep) {
        Some(i) => (&bytes[..i], &bytes[i + 1..]),
        None => (bytes, &[]),
    }
}

fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|&b| b == b'\n')
        .map(trim_newline)
        .filter(|line| !line.is_empty())
}

fn non_empty_string(bytes: &[u8]) -> Option<String> {
    (!bytes.is_empty()).then(|| string_from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_dots() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(
            normalize(Path::new("/srv/s/.hg/../../r/.hg")),
            PathBuf::from("/srv/r/.hg")
        );
    }

    #[test]
    fn parses_log_records() {
        let out = b"aaaa\x1faa\x1fAda\x1f1700000000\x1fHEAD -> main\x1fFirst line\x1e\n\
bbbb\x1fbb\x1fBo\x1f1600000000 -3600\x1f\x1fSecond\x1e\n";
        let commits = parse_log(out, string_from_bytes);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].refs, "HEAD -> main");
        assert_eq!(commits[0].subject, "First line");
        assert_eq!(commits[1].time, 1_600_000_000);
        assert_eq!(commits[1].short, "bb");
        let null = b"0000\x1f0\x1fx\x1f0 0\x1f\x1f\x1e";
        assert!(parse_log(null, string_from_bytes).is_empty());
    }

    #[test]
    fn rejects_option_like_branch() {
        assert!(check_name("-delete").is_err());
        assert!(check_name("").is_err());
        assert!(check_name("feature/x").is_ok());
    }
}
