//! Git backend: porcelain, `-z` output parsed in place.

use std::path::{Path, PathBuf};

use cmdterm_env::{CommandError, EnvCommand, Environment};

use crate::vcs::{
    Commit, GitDirs, KnownRepo, LOG_LIMIT, Remote, Repository, Review, Status, VcsError, Worktree,
    lines, non_empty_string, parse_log, path_from_bytes, split_once, string_from_bytes,
    trim_newline,
};

/// `git` in the worktree `worktree` of `repo`, or in the repository at
/// `repo.root`, without optional locks so background polls never contend
/// with the user's own git commands.
///
/// Sandboxes may write a worktree and most of its metadata, though not its
/// configuration or hooks (see `runtime.rs`), and the app runs these
/// commands outside them. So they also run no fsmonitor command and no
/// hook, and do not look inside submodules for changes: a submodule's
/// configuration, and a submodule a sandbox staged, are the sandbox's, and
/// `git status` there would run the filters they name. `-c` reaches the
/// git processes started for submodules too. A submodule whose commit
/// changed still counts.
///
/// A sandbox may also replace the worktree's `.git`: a symlink cannot be
/// kept from it, and neither can the `.git` of a worktree inside another
/// one's. So git gets the git directory and the common directory from
/// [`Repository::git`] and never reads `.git`.
pub(crate) fn command(repo: &Repository, worktree: &Path) -> Result<EnvCommand, VcsError> {
    let command = repo.env.command("git").args(HOST_SAFE);
    Ok(with_dirs(command, repo, worktree)?
        .arg("--no-optional-locks")
        .cwd(worktree)
        .var("GIT_TERMINAL_PROMPT", "0"))
}

/// `command` with the variables that name `worktree`'s metadata to git,
/// see [`command`]. Unchanged without [`Repository::git`].
fn with_dirs(
    command: EnvCommand,
    repo: &Repository,
    worktree: &Path,
) -> Result<EnvCommand, VcsError> {
    let Some(dirs) = &repo.git else {
        return Ok(command);
    };
    let Some(git_dir) = dirs.git_dir(&repo.root, worktree, repo.env.is_local()) else {
        return Err(VcsError::InvalidArgument(format!(
            "{} is not a worktree of {}",
            worktree.display(),
            repo.root.display()
        )));
    };
    // `GIT_COMMON_DIR` wins over the git directory's `commondir`.
    let command = command
        .var("GIT_DIR", git_dir)
        .var("GIT_COMMON_DIR", &dirs.common);
    // A bare repository has no worktree.
    Ok(if repo.root == dirs.common {
        command
    } else {
        command.var("GIT_WORK_TREE", worktree)
    })
}

fn git(repo: &Repository, worktree: &Path) -> Result<EnvCommand, VcsError> {
    command(repo, worktree)
}

/// Options that keep git from running commands that the repository's
/// metadata names, see [`git`].
pub(crate) const HOST_SAFE: [&str; 6] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "diff.ignoreSubmodules=dirty",
];

/// The main worktree (or bare repository) containing `path` and its common
/// git directory, or `None` when `path` is not in a git repository. Fails
/// only when an SSH connection fails.
pub(crate) fn discover(env: &Environment, path: &Path) -> Result<Option<KnownRepo>, VcsError> {
    let output = match env
        .command("git")
        .args(HOST_SAFE)
        .arg("--no-optional-locks")
        .cwd(path)
        .var("GIT_TERMINAL_PROMPT", "0")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--absolute-git-dir",
            "--git-common-dir",
            "--is-bare-repository",
            "--show-toplevel",
        ])
        .output()
    {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    // ssh exits 255 when it cannot connect; git never does.
    if !env.is_local() && output.status.code() == Some(255) {
        return Err(VcsError::Command(CommandError::Failed {
            code: Some(255),
            stderr: string_from_bytes(output.stderr.trim_ascii()),
        }));
    }
    Ok(parse_discover(&output.stdout, output.status.success()))
}

/// Picks the main worktree and the common directory from `rev-parse`
/// output. In a bare repository `--show-toplevel` fails after the first
/// three lines are printed.
fn parse_discover(stdout: &[u8], success: bool) -> Option<KnownRepo> {
    let mut it = stdout.split(|&b| b == b'\n');
    let git_dir = it.next().filter(|l| !l.is_empty())?;
    let common_dir = it.next().filter(|l| !l.is_empty())?;
    let bare = it.next()? == b"true";
    let common = path_from_bytes(common_dir);
    let known = |root: PathBuf| {
        Some(KnownRepo {
            root,
            git_dir: common.clone(),
        })
    };
    if bare {
        return known(common.clone());
    }
    if !success {
        return None;
    }
    let toplevel = it.next().filter(|l| !l.is_empty())?;
    if git_dir == common_dir {
        return known(path_from_bytes(toplevel));
    }
    // Linked worktree: the main worktree owns the common `.git` directory.
    if common.file_name().is_some_and(|name| name == ".git") {
        known(common.parent()?.to_path_buf())
    } else {
        known(common.clone())
    }
}

/// The common git directory of the main worktree `root` on this machine,
/// found by following its `.git` as git does, without running git. `None`
/// without one.
pub(crate) fn local_common_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    let git_dir = if std::fs::metadata(&dot).ok()?.is_dir() {
        cmdterm_env::canonicalize(&dot).ok()?
    } else {
        let text = std::fs::read_to_string(&dot).ok()?;
        let dir = text.strip_prefix("gitdir:")?.trim();
        cmdterm_env::canonicalize(root.join(dir)).ok()?
    };
    if !git_dir.join("HEAD").is_file() {
        return None;
    }
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) if text.trim_end() != "." => {
            cmdterm_env::canonicalize(git_dir.join(text.trim_end())).ok()
        }
        _ => Some(git_dir),
    }
}

/// The worktrees of `repo` on this machine whose `.git` no longer leads
/// to the git directory the app's commands read for them, as a sandbox
/// may leave it. git run by hand there, or by a shell prompt or an editor,
/// reads the other one. Missing worktrees and `.git` entries do not count.
pub(crate) fn replaced_dot_git(repo: &Repository) -> Vec<PathBuf> {
    let Some(dirs) = repo.git.as_ref().filter(|_| repo.env.is_local()) else {
        return Vec::new();
    };
    if repo.root == dirs.common {
        return Vec::new();
    }
    let main = std::iter::once((&repo.root, &dirs.common));
    let linked = dirs
        .linked
        .iter()
        .map(|(worktree, admin)| (worktree, admin));
    (main.chain(linked))
        .filter(|(worktree, git_dir)| !leads_to(worktree, git_dir))
        .map(|(worktree, _)| worktree.clone())
        .collect()
}

/// Whether the `.git` of `worktree` leads to `git_dir`, or is missing.
fn leads_to(worktree: &Path, git_dir: &Path) -> bool {
    let dot = worktree.join(".git");
    let Ok(meta) = std::fs::metadata(&dot) else {
        return true;
    };
    let found = if meta.is_dir() {
        cmdterm_env::canonicalize(&dot).ok()
    } else {
        (std::fs::read_to_string(&dot).ok())
            .and_then(|text| Some(worktree.join(text.strip_prefix("gitdir:")?.trim())))
            .and_then(|dir| cmdterm_env::canonicalize(dir).ok())
    };
    found.is_some() && found == cmdterm_env::canonicalize(git_dir).ok()
}

/// The git directories of the repository whose common directory is
/// `common`: the linked worktrees come from its `worktrees` list, whose
/// `gitdir` files name each worktree's `.git`. Sandboxes cannot change that
/// list, see `runtime.rs`. One `sh` call on another machine.
pub(crate) fn dirs(env: &Environment, common: &Path) -> Result<GitDirs, VcsError> {
    let mut linked = Vec::new();
    if env.is_local() {
        if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
            for entry in entries.flatten() {
                let admin = entry.path();
                if let Ok(text) = std::fs::read(admin.join("gitdir")) {
                    linked.extend(linked_worktree(admin, &text));
                }
            }
        }
    } else {
        let out = env
            .command("sh")
            .args(["-c", LINKED, "sh"])
            .arg(common)
            .stdout()?;
        let mut fields = out.split(|&b| b == 0);
        while let (Some(admin), Some(text)) = (fields.next(), fields.next()) {
            linked.extend(linked_worktree(path_from_bytes(admin), text));
        }
    }
    linked.sort();
    Ok(GitDirs {
        common: common.to_path_buf(),
        linked,
    })
}

/// `sh -c LINKED sh COMMON`: `ADMIN NUL GITDIR NUL` for each linked
/// worktree, `GITDIR` being what its `gitdir` file holds.
const LINKED: &str = r#"for d in "$1"/worktrees/*; do
  [ -f "$d/gitdir" ] || continue
  printf '%s\0' "$d"; cat "$d/gitdir"; printf '\0'
done
"#;

/// The worktree whose administrative directory `admin` holds `gitdir`
/// with the text `text`: the path of its `.git`, absolute or relative to
/// `admin`.
fn linked_worktree(admin: PathBuf, text: &[u8]) -> Option<(PathBuf, PathBuf)> {
    let dot = path_from_bytes(trim_newline(text));
    if dot.as_os_str().is_empty() {
        return None;
    }
    let dot = crate::vcs::normalize(&admin.join(dot));
    let worktree = dot.parent()?.to_path_buf();
    (dot.file_name()? == ".git").then_some((worktree, admin))
}

pub(crate) fn worktrees(repo: &Repository) -> Result<Vec<Worktree>, VcsError> {
    let out = git(repo, &repo.root)?
        .args(["worktree", "list", "--porcelain", "-z"])
        .stdout()?;
    parse_worktrees(&out)
}

fn parse_worktrees(out: &[u8]) -> Result<Vec<Worktree>, VcsError> {
    let mut list = Vec::new();
    let mut current: Option<Worktree> = None;
    for field in out.split(|&b| b == 0) {
        if field.is_empty() {
            list.extend(current.take());
            continue;
        }
        let (key, value) = split_once(field, b' ');
        if key == b"worktree" {
            list.extend(current.take());
            current = Some(Worktree {
                path: path_from_bytes(value),
                branch: None,
                head: None,
                is_main: list.is_empty(),
                locked: false,
                prunable: false,
                bare: false,
            });
            continue;
        }
        let Some(worktree) = current.as_mut() else {
            return Err(VcsError::Parse(format!(
                "worktree list: field before worktree: {}",
                String::from_utf8_lossy(field)
            )));
        };
        match key {
            b"HEAD" => {
                worktree.head =
                    (!value.iter().all(|&b| b == b'0')).then(|| string_from_bytes(value));
            }
            b"branch" => {
                let name = value.strip_prefix(b"refs/heads/").unwrap_or(value);
                worktree.branch = non_empty_string(name);
            }
            b"bare" => worktree.bare = true,
            b"locked" => worktree.locked = true,
            b"prunable" => worktree.prunable = true,
            _ => {}
        }
    }
    list.extend(current);
    Ok(list)
}

/// `sh -c SCRIPT sh PATH ARGS...`: `git worktree add ARGS`, then the new
/// worktree's `HEAD`. The checkout runs no hook, see [`HOST_SAFE`]. The
/// variables that name the repository's metadata (see [`command`]) are
/// for the first command only.
fn add_script() -> String {
    let safe = HOST_SAFE.join(" ");
    format!(
        "p=$1; shift; git {safe} worktree add --quiet \"$@\" >&2 && \
         unset GIT_DIR GIT_COMMON_DIR GIT_WORK_TREE && \
         exec git {safe} -C \"$p\" rev-parse --verify --quiet HEAD"
    )
}

pub(crate) fn add_worktree(
    repo: &Repository,
    path: &Path,
    branch: &str,
    create_branch: bool,
) -> Result<Worktree, VcsError> {
    // One shell: add, then print the new HEAD so we need no second query.
    let command = repo
        .env
        .command("sh")
        .cwd(&repo.root)
        .var("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", &add_script(), "sh"])
        .arg(path);
    let mut command = with_dirs(command, repo, &repo.root)?;
    command = if create_branch {
        command.args(["-b", branch]).arg(path)
    } else {
        command.arg(path).arg(branch)
    };
    let out = command.stdout()?;
    Ok(Worktree {
        path: path.to_path_buf(),
        branch: Some(branch.to_owned()),
        head: non_empty_string(trim_newline(&out)),
        is_main: false,
        locked: false,
        prunable: false,
        bare: false,
    })
}

/// Adds a worktree at `revision` as `cwd` resolves it, so `HEAD` names the
/// commit checked out in `cwd`.
pub(crate) fn add_worktree_at(
    repo: &Repository,
    cwd: &Path,
    path: &Path,
    revision: &str,
    branch: Option<&str>,
) -> Result<Worktree, VcsError> {
    let command = repo
        .env
        .command("sh")
        .cwd(cwd)
        .var("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", &add_script(), "sh"])
        .arg(path);
    let mut command = with_dirs(command, repo, cwd)?;
    command = match branch {
        Some(branch) => command.args(["-b", branch]),
        None => command.arg("--detach"),
    };
    let out = command.arg(path).arg(revision).stdout()?;
    Ok(Worktree {
        path: path.to_path_buf(),
        branch: branch.map(str::to_owned),
        head: non_empty_string(trim_newline(&out)),
        is_main: false,
        locked: false,
        prunable: false,
        bare: false,
    })
}

/// Removes the linked worktree `path`. Without `force` it refuses one with
/// changes, or with a checked out submodule, as `git worktree remove` does.
/// git's own check would run `git status` with the worktree's `.git`,
/// which a sandbox may have replaced (see [`command`]), so the check runs
/// here with the repository's metadata, and git removes the worktree with
/// `--force`, which skips its check. git never looks inside a submodule:
/// a sandbox may have written its configuration.
pub(crate) fn remove_worktree(repo: &Repository, path: &Path, force: bool) -> Result<(), VcsError> {
    let refuse = |reason: &str| {
        Err(VcsError::Command(CommandError::Failed {
            code: Some(128),
            stderr: format!("fatal: '{}' {reason}", path.display()),
        }))
    };
    if !force {
        let staged = git(repo, path)?
            .args(["ls-files", "--stage", "-z"])
            .stdout()?;
        let submodules = (staged.split(|&b| b == 0))
            .filter(|entry| entry.starts_with(b"160000 "))
            .map(|entry| path.join(path_from_bytes(split_once(entry, b'\t').1)));
        let admin = (repo.git.as_ref())
            .and_then(|dirs| dirs.git_dir(&repo.root, path, repo.env.is_local()))
            .map(|admin| admin.join("modules"))
            .unwrap_or_default();
        let checked_out = repo
            .env
            .command("sh")
            .args(["-c", CHECKED_OUT, "sh"])
            .arg(admin)
            .args(submodules.map(|dir| dir.join(".git")))
            .output()
            .map_err(CommandError::Spawn)?;
        if !checked_out.status.success() {
            return refuse("contains submodules and cannot be removed");
        }
        if !git(repo, path)?
            .args(["status", "--porcelain"])
            .stdout()?
            .is_empty()
        {
            return refuse("contains modified or untracked files, use --force to delete it");
        }
    }
    git(repo, &repo.root)?
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .stdout()?;
    Ok(())
}

/// `sh -c CHECKED_OUT sh MODULES DOT...`: fails when the directory
/// `MODULES` (a worktree's submodule directories) or any `DOT` (a
/// submodule's `.git`) exists, as git judges a submodule checked out.
const CHECKED_OUT: &str = r#"[ -n "$1" ] && [ -d "$1" ] && exit 1
shift
for d do [ -e "$d" ] && exit 1; done
exit 0
"#;

pub(crate) fn status(repo: &Repository, worktree: &Path) -> Result<Status, VcsError> {
    let out = git(repo, worktree)?
        .args(["status", "--porcelain=v2", "--branch", "-z"])
        .stdout()?;
    Ok(parse_status(&out))
}

fn parse_status(out: &[u8]) -> Status {
    let mut status = Status::default();
    let mut records = out.split(|&b| b == 0);
    while let Some(record) = records.next() {
        match record.first() {
            Some(b'#') => parse_header(record, &mut status),
            Some(b'1') => count_xy(record, &mut status),
            Some(b'2') => {
                count_xy(record, &mut status);
                // Renames carry the original path as an extra record.
                records.next();
            }
            Some(b'u') => status.conflicted += 1,
            Some(b'?') => status.untracked += 1,
            _ => {}
        }
    }
    status
}

fn parse_header(record: &[u8], status: &mut Status) {
    if let Some(head) = record.strip_prefix(b"# branch.head ") {
        status.branch = (head != b"(detached)").then(|| string_from_bytes(head));
    } else if let Some(ab) = record.strip_prefix(b"# branch.ab ") {
        for part in ab.split(|&b| b == b' ') {
            let parse = |digits: &[u8]| {
                std::str::from_utf8(digits)
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0)
            };
            match part {
                [b'+', digits @ ..] => status.ahead = parse(digits),
                [b'-', digits @ ..] => status.behind = parse(digits),
                _ => {}
            }
        }
    }
}

fn count_xy(record: &[u8], status: &mut Status) {
    if let [_, b' ', x, y, ..] = record {
        if *x != b'.' {
            status.staged += 1;
        }
        if *y != b'.' {
            status.modified += 1;
        }
    }
}

pub(crate) fn remotes(repo: &Repository) -> Result<Vec<Remote>, VcsError> {
    let output = git(repo, &repo.root)?
        .args(["config", "-z", "--get-regexp", r"^remote\..*\.url$"])
        .output()
        .map_err(CommandError::Spawn)?;
    match output.status.code() {
        Some(0) => Ok(parse_remotes(&output.stdout)),
        // `--get-regexp` exits 1 when nothing matches.
        Some(1) if output.stdout.is_empty() => Ok(Vec::new()),
        code => Err(VcsError::Command(CommandError::Failed {
            code,
            stderr: string_from_bytes(output.stderr.trim_ascii()),
        })),
    }
}

fn parse_remotes(out: &[u8]) -> Vec<Remote> {
    out.split(|&b| b == 0)
        .filter_map(|entry| {
            let (key, url) = split_once(entry, b'\n');
            let name = key.strip_prefix(b"remote.")?.strip_suffix(b".url")?;
            Some(Remote {
                name: string_from_bytes(name),
                url: string_from_bytes(url),
            })
        })
        .collect()
}

pub(crate) fn files(repo: &Repository, worktree: &Path) -> Result<Vec<u8>, VcsError> {
    Ok(git(repo, worktree)?
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stdout()?)
}

pub(crate) fn changes(repo: &Repository, worktree: &Path) -> Result<Vec<u8>, VcsError> {
    let out = git(repo, worktree)?
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .stdout()?;
    Ok(parse_changes(&out))
}

/// `XY path` records of `git status --porcelain=v1 -z` as
/// [`crate::vcs::Repository::changes`] records.
fn parse_changes(out: &[u8]) -> Vec<u8> {
    let mut changes = Vec::with_capacity(out.len());
    let mut records = out.split(|&b| b == 0);
    while let Some(record) = records.next() {
        let [x, y, b' ', path @ ..] = record else {
            continue;
        };
        let letter = match (x, y) {
            (b'?', _) => b'?',
            (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => b'U',
            (b'R' | b'C', _) => {
                // The original path follows as a record of its own.
                records.next();
                b'R'
            }
            (b'A', _) => b'A',
            (b'D', _) | (_, b'D') => b'D',
            _ => b'M',
        };
        changes.extend_from_slice(&[letter, b' ']);
        changes.extend_from_slice(path);
        changes.push(0);
    }
    changes
}

pub(crate) fn branches(repo: &Repository) -> Result<Vec<String>, VcsError> {
    // `lstrip=2` instead of `:short`: `:short` disambiguates to `heads/x`
    // when a tag shares the name.
    let out = git(repo, &repo.root)?
        .args(["for-each-ref", "--format=%(refname:lstrip=2)", "refs/heads"])
        .stdout()?;
    Ok(lines(&out).map(string_from_bytes).collect())
}

/// `git` with output that parsers and the explore panel can rely on: no
/// signature lines, no color, no external diff tools, plain UTF-8 paths and
/// `a/` and `b/` prefixes whatever the user's config says.
fn plain_git(repo: &Repository, worktree: &Path) -> Result<EnvCommand, VcsError> {
    Ok(git(repo, worktree)?.args([
        "-c",
        "log.showSignature=false",
        "-c",
        "core.quotePath=false",
        "-c",
        "color.ui=never",
    ]))
}

/// Diff options shared by [`show`] and [`review`].
const DIFF_OPTIONS: [&str; 5] = [
    "--no-color",
    "--no-ext-diff",
    "-M",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

pub(crate) fn log(
    repo: &Repository,
    worktree: &Path,
    range: &str,
) -> Result<Vec<Commit>, VcsError> {
    let output = plain_git(repo, worktree)?
        .args([
            "log",
            "--no-color",
            "--format=%H%x1f%h%x1f%an%x1f%at%x1f%D%x1f%s%x1e",
        ])
        .arg(format!("--max-count={LOG_LIMIT}"))
        .args([range, "--"])
        .output()
        .map_err(CommandError::Spawn)?;
    if output.status.success() {
        return Ok(parse_log(&output.stdout, string_from_bytes));
    }
    // Before the first commit `HEAD` names nothing; that is an empty log.
    let born = git(repo, worktree)?
        .args(["rev-parse", "--quiet", "--verify", "HEAD"])
        .output()
        .is_ok_and(|out| out.status.success());
    if born {
        Err(VcsError::Command(CommandError::Failed {
            code: output.status.code(),
            stderr: string_from_bytes(output.stderr.trim_ascii()),
        }))
    } else {
        Ok(Vec::new())
    }
}

pub(crate) fn show(repo: &Repository, worktree: &Path, id: &str) -> Result<Vec<u8>, VcsError> {
    Ok(plain_git(repo, worktree)?
        .args([
            "show",
            // The refs on a line of their own when it has any (git 2.33
            // and later), who wrote it and when, then the message's body:
            // the panel's header already shows the short hash and the
            // subject, and the committer lines of `fuller` mostly repeat
            // the author.
            "--format=%(decorate:prefix=Refs:   ,suffix=%n)Author: %an <%ae>%nDate:   %ad%n%w(0,4,4)%+b",
            "--date=format-local:%Y-%m-%d %H:%M",
            "--stat",
            "--patch",
            "-m",
            "--first-parent",
        ])
        .args(DIFF_OPTIONS)
        .args([id, "--"])
        .stdout()?)
}

/// Candidate base refs, in the order [`default_base`] prefers them.
const BASES: [&str; 6] = [
    "refs/remotes/origin/HEAD",
    "refs/remotes/origin/main",
    "refs/remotes/origin/master",
    "refs/heads/main",
    "refs/heads/master",
    "refs/heads/trunk",
];

pub(crate) fn default_base(repo: &Repository, worktree: &Path) -> Result<Option<String>, VcsError> {
    let out = git(repo, worktree)?
        .args(["for-each-ref", "--format=%(refname)%00%(symref)"])
        .args(BASES)
        .stdout()?;
    Ok(pick_base(&out))
}

/// The preferred base from `for-each-ref` records of `refname NUL symref`.
fn pick_base(out: &[u8]) -> Option<String> {
    let found: Vec<(&[u8], &[u8])> = lines(out).map(|line| split_once(line, 0)).collect();
    let target = BASES.iter().find_map(|base| {
        let &(name, symref) = found.iter().find(|(name, _)| name == &base.as_bytes())?;
        Some(if symref.is_empty() { name } else { symref })
    })?;
    let short = target
        .strip_prefix(b"refs/remotes/")
        .or_else(|| target.strip_prefix(b"refs/heads/"))
        .unwrap_or(target);
    non_empty_string(short)
}

pub(crate) fn review(
    repo: &Repository,
    worktree: &Path,
    base: &str,
    uncommitted: bool,
) -> Result<Review, VcsError> {
    let out = match git(repo, worktree)?
        .args(["merge-base", "HEAD", base])
        .stdout()
    {
        Ok(out) => out,
        // merge-base exits 1 without a word when the two share no commit.
        // Say so when the base itself exists, rather than "exited with 1".
        Err(error) => {
            let exists = git(repo, worktree)?
                .args(["rev-parse", "--verify", "--quiet"])
                .arg(format!("{base}^{{commit}}"))
                .stdout()
                .is_ok();
            return Err(if exists {
                VcsError::NoCommonHistory(base.to_owned())
            } else {
                error.into()
            });
        }
    };
    let merge_base = string_from_bytes(trim_newline(&out));
    let commits = log(repo, worktree, &format!("{merge_base}..HEAD"))?;
    let mut diff = plain_git(repo, worktree)?
        .arg("diff")
        .args(DIFF_OPTIONS)
        .arg(&merge_base);
    if !uncommitted {
        diff = diff.arg("HEAD");
    }
    let mut patch = diff.arg("--").stdout()?;
    if uncommitted {
        let untracked = git(repo, worktree)?
            .args(["ls-files", "-z", "--others", "--exclude-standard"])
            .stdout()?;
        crate::vcs::untracked::append(&repo.env, worktree, &untracked, &mut patch)?;
    }
    Ok(Review {
        merge_base,
        commits,
        patch,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn changes_name_one_letter_per_path_and_skip_the_old_name_of_a_rename() {
        let out =
            b" M src/a.rs\0A  b.rs\0R  new.rs\0old.rs\0?? notes.txt\0 D gone.rs\0UU both.rs\0";
        assert_eq!(
            super::parse_changes(out),
            b"M src/a.rs\0A b.rs\0R new.rs\0? notes.txt\0D gone.rs\0U both.rs\0"
        );
    }

    use super::*;

    #[cfg(unix)]
    #[test]
    fn polls_run_no_command_a_submodule_names() {
        let run = |cwd: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "protocol.file.allow=always"])
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .is_ok_and(|out| out.status.success())
        };
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let (sub, top) = (dir.join("sub"), dir.join("top"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&top).unwrap();
        std::fs::write(sub.join("f"), "a").unwrap();
        if !(run(&sub, &["init", "-q"])
            && run(&sub, &["add", "f"])
            && run(&sub, &["commit", "-q", "-m", "s"])
            && run(&top, &["init", "-q"])
            && run(&top, &["submodule", "add", "-q", "../sub", "sub"])
            && run(&top, &["commit", "-q", "-m", "t"]))
        {
            eprintln!("git cannot set up a submodule here; skipped");
            return;
        }
        // What a sandbox could write into the submodule's metadata.
        let marker = |name: &str| dir.join(name);
        let inner = top.join("sub");
        let config = |key: &str, value: String| assert!(run(&inner, &["config", key, &value]));
        config(
            "filter.x.clean",
            format!("touch {}; cat", marker("filter").display()),
        );
        config(
            "core.fsmonitor",
            format!("touch {}; false", marker("fsmonitor").display()),
        );
        let gitdir = top.join(".git/modules/sub");
        std::fs::create_dir_all(gitdir.join("info")).unwrap();
        std::fs::write(gitdir.join("info/attributes"), "f filter=x\n").unwrap();
        // A file as new as the index is compared by content, through the
        // filter.
        std::fs::write(inner.join("f"), "a").unwrap();
        let repo = crate::vcs::discover(&Environment::Local, &top).unwrap();
        status(&repo, &top).unwrap();
        changes(&repo, &top).unwrap();
        review(&repo, &top, "HEAD", true).unwrap();
        assert!(!marker("filter").exists() && !marker("fsmonitor").exists());
    }

    /// Sandboxes may write hooks into a linked worktree's metadata, and
    /// find a `commondir` of `.` in the main git directory (see
    /// `runtime.rs`), which the app's git commands must take in stride.
    #[cfg(unix)]
    #[test]
    fn worktrees_come_and_go_without_hooks_and_with_a_dot_commondir() {
        let run = |cwd: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .is_ok_and(|out| out.status.success())
        };
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if !(run(&repo, &["init", "-q"])
            && run(&repo, &["commit", "-q", "--allow-empty", "-m", "a"]))
        {
            eprintln!("git is missing; skipped");
            return;
        }
        std::fs::write(repo.join(".git/commondir"), ".\n").unwrap();
        let marker = dir.join("hook-ran");
        let hook = repo.join(".git/hooks/post-checkout");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let env = Environment::Local;
        let root = |path: &Path| discover(&env, path).unwrap().map(|known| known.root);
        assert_eq!(root(&repo), Some(repo.clone()));
        let repository = crate::vcs::discover(&env, &repo).unwrap();
        let wt = dir.join("wt");
        let added = add_worktree(&repository, &wt, "b", true).unwrap();
        assert!(added.head.is_some());
        let at = dir.join("at");
        add_worktree_at(&repository, &repo, &at, "HEAD", None).unwrap();
        assert_eq!(root(&wt), Some(repo.clone()));
        // The new worktrees are listed only once the repository is read
        // again.
        let repository = crate::vcs::discover(&env, &repo).unwrap();
        let list = worktrees(&repository).unwrap();
        let mut paths: Vec<&Path> = list.iter().map(|w| w.path.as_path()).collect();
        paths[1..].sort();
        assert_eq!(paths, [repo.as_path(), at.as_path(), wt.as_path()]);
        status(&repository, &wt).unwrap();
        remove_worktree(&repository, &wt, false).unwrap();
        remove_worktree(&repository, &at, false).unwrap();
        assert!(!wt.exists() && !marker.exists());
    }

    /// git refuses to remove a worktree with a checked out submodule
    /// before its clean check, which would look inside the submodule with
    /// the configuration a sandbox wrote there.
    #[cfg(unix)]
    #[test]
    fn removing_a_worktree_runs_no_command_its_submodule_names() {
        let run = |cwd: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "protocol.file.allow=always"])
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .is_ok_and(|out| out.status.success())
        };
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let (sub, top, wt) = (dir.join("sub"), dir.join("top"), dir.join("wt"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&top).unwrap();
        std::fs::write(sub.join("f"), "a").unwrap();
        if !(run(&sub, &["init", "-q"])
            && run(&sub, &["add", "f"])
            && run(&sub, &["commit", "-q", "-m", "s"])
            && run(&top, &["init", "-q"])
            && run(&top, &["submodule", "add", "-q", "../sub", "sub"])
            && run(&top, &["commit", "-q", "-m", "t"])
            && run(&top, &["worktree", "add", "-q", "../wt"])
            && run(&wt, &["submodule", "update", "-q", "--init"]))
        {
            eprintln!("git cannot set up a submodule here; skipped");
            return;
        }
        // What a sandbox in `wt` could write into its submodule's metadata.
        let marker = dir.join("filter");
        let inner = wt.join("sub");
        let filter = format!("touch {}; cat", marker.display());
        assert!(run(&inner, &["config", "filter.x.clean", &filter]));
        let gitdir = top.join(".git/worktrees/wt/modules/sub");
        std::fs::create_dir_all(gitdir.join("info")).unwrap();
        std::fs::write(gitdir.join("info/attributes"), "f filter=x\n").unwrap();
        std::fs::write(inner.join("f"), "a").unwrap();
        let repository = crate::vcs::discover(&Environment::Local, &top).unwrap();
        assert!(remove_worktree(&repository, &wt, false).is_err());
        assert!(wt.exists() && !marker.exists());
    }

    #[test]
    fn discover_main_linked_and_bare() {
        let known = |root: &str, git_dir: &str| {
            Some(KnownRepo {
                root: root.into(),
                git_dir: git_dir.into(),
            })
        };
        assert_eq!(
            parse_discover(b"/r/.git\n/r/.git\nfalse\n/r\n", true),
            known("/r", "/r/.git")
        );
        assert_eq!(
            parse_discover(b"/r/.git/worktrees/w\n/r/.git\nfalse\n/w\n", true),
            known("/r", "/r/.git")
        );
        assert_eq!(
            parse_discover(b"/b.git\n/b.git\ntrue\n", false),
            known("/b.git", "/b.git")
        );
        // A separate git directory.
        assert_eq!(
            parse_discover(b"/s.git\n/s.git\nfalse\n/r\n", true),
            known("/r", "/s.git")
        );
        assert_eq!(parse_discover(b"", false), None);
    }

    /// The git directory recorded for a project saved before the app
    /// recorded them, read without running git.
    #[cfg(unix)]
    #[test]
    fn the_common_directory_is_found_without_git() {
        let run = |cwd: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .is_ok_and(|out| out.status.success())
        };
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let (plain, sep, linked) = (dir.join("plain"), dir.join("sep"), dir.join("linked"));
        for repo in [&plain, &sep, &linked] {
            std::fs::create_dir_all(repo).unwrap();
        }
        if !(run(&plain, &["init", "-q"])
            && run(&sep, &["init", "-q", "--separate-git-dir", "../sep.git"])
            && run(&dir, &["init", "-q", "--bare", "real.git"]))
        {
            eprintln!("git is missing; skipped");
            return;
        }
        std::os::unix::fs::symlink(dir.join("real.git"), linked.join(".git")).unwrap();
        // The same answer as git's.
        for repo in [&plain, &sep, &linked] {
            let git = discover(&Environment::Local, repo).unwrap().unwrap();
            assert_eq!(local_common_dir(repo), Some(git.git_dir), "{repo:?}");
        }
        assert_eq!(local_common_dir(&dir), None);
    }

    #[test]
    fn linked_worktrees_come_from_their_gitdir_files() {
        let admin = PathBuf::from("/r/.git/worktrees/w");
        assert_eq!(
            linked_worktree(admin.clone(), b"/w/.git\n"),
            Some((PathBuf::from("/w"), admin.clone()))
        );
        // `worktree.useRelativePaths` writes them relative to `admin`.
        assert_eq!(
            linked_worktree(admin.clone(), b"../../../../w/.git\n"),
            Some((PathBuf::from("/w"), admin.clone()))
        );
        assert_eq!(linked_worktree(admin.clone(), b"/w/other\n"), None);
        assert_eq!(linked_worktree(admin, b""), None);
        let dirs = GitDirs {
            common: "/r/.git".into(),
            linked: vec![
                ("/w".into(), "/r/.git/worktrees/w".into()),
                ("/twice".into(), "/r/.git/worktrees/a".into()),
                ("/twice".into(), "/r/.git/worktrees/b".into()),
            ],
        };
        let root = Path::new("/r");
        assert_eq!(dirs.git_dir(root, root, false), Some(Path::new("/r/.git")));
        let w = dirs.git_dir(root, Path::new("/w/"), false);
        assert_eq!(w, Some(Path::new("/r/.git/worktrees/w")));
        // A path listed twice or not at all gets no git directory.
        assert_eq!(dirs.git_dir(root, Path::new("/twice"), false), None);
        assert_eq!(dirs.git_dir(root, Path::new("/elsewhere"), false), None);
    }

    #[test]
    fn parses_worktree_list() {
        let out = b"worktree /r\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0\
worktree /w\0HEAD 2222222222222222222222222222222222222222\0detached\0locked reason\0prunable gitdir file points to non-existent location\0\0";
        let list = parse_worktrees(out).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].is_main);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert_eq!(list[1].path, PathBuf::from("/w"));
        assert!(!list[1].is_main && list[1].locked && list[1].prunable);
        assert_eq!(list[1].branch, None);
    }

    #[test]
    fn parses_status_v2() {
        let out = b"# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -3\0\
1 M. N... 100644 100644 100644 a a f1\0\
1 .M N... 100644 100644 100644 a a f2\0\
2 R. N... 100644 100644 100644 a a R100 new\0old\0\
u UU N... 100644 100644 100644 100644 a a a c\0\
? u1\0? u2\0";
        assert_eq!(
            parse_status(out),
            Status {
                branch: Some("main".into()),
                ahead: 2,
                behind: 3,
                staged: 2,
                modified: 1,
                untracked: 2,
                conflicted: 1,
            }
        );
        assert_eq!(parse_status(b"# branch.head (detached)\0").branch, None);
    }

    #[test]
    fn picks_the_preferred_base() {
        let out = b"refs/heads/main\0\nrefs/remotes/origin/HEAD\0refs/remotes/origin/trunk\n";
        assert_eq!(pick_base(out).as_deref(), Some("origin/trunk"));
        let out = b"refs/heads/master\0\nrefs/remotes/origin/master\0\n";
        assert_eq!(pick_base(out).as_deref(), Some("origin/master"));
        assert_eq!(pick_base(b"refs/heads/trunk\0\n").as_deref(), Some("trunk"));
        assert_eq!(pick_base(b""), None);
    }

    #[test]
    fn parses_remote_config() {
        let remotes =
            parse_remotes(b"remote.origin.url\nhttps://x/y.git\0remote.a.b.url\ngit@h:p\0");
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[1].name, "a.b");
        assert_eq!(remotes[1].url, "git@h:p");
    }
}
