//! Jujutsu backend, used only where the user turned it on. Worktrees are
//! jj workspaces; branches are bookmarks. A workspace's "checked-out
//! revision" is the parent of its working-copy commit (`@-`), so like git a
//! new worktree leaves the uncommitted work of its source behind.
//!
//! Status queries snapshot the working copy as `jj status` does, so new
//! files on disk count. Every other query passes `--ignore-working-copy`
//! and reads the repository as the last snapshot left it. Queries that
//! need several steps run as one `sh -c` script so a remote project pays
//! for one SSH channel, not one per step.

use std::path::{Path, PathBuf};

use cmdterm_env::{EnvCommand, Environment};

use crate::vcs::{
    Commit, LOG_LIMIT, Remote, Repository, Review, Status, VcsError, Worktree, lines, normalize,
    parse_log, path_from_bytes, split_once, string_from_bytes, trim_newline,
};

/// Shell prelude: `jj` without pager or colors, and `jjr` for the queries
/// that read the last snapshot.
const PRELUDE: &str = "jj() { command jj --no-pager --color=never \"$@\" </dev/null; }\n\
                       jjr() { jj --ignore-working-copy \"$@\"; }\n";

fn jj(env: &Environment, cwd: &Path) -> EnvCommand {
    env.command("jj")
        .args(["--no-pager", "--color=never", "--ignore-working-copy"])
        .cwd(cwd)
}

/// `sh -c PRELUDE+script sh args...` in `cwd`.
fn script(env: &Environment, cwd: &Path, body: &str) -> EnvCommand {
    let mut source = String::with_capacity(PRELUDE.len() + body.len());
    source.push_str(PRELUDE);
    source.push_str(body);
    env.command("sh")
        .args(["-c".into(), source, "sh".into()])
        .cwd(cwd)
}

/// The main workspace of the repository containing `path`.
pub(crate) fn discover(env: &Environment, path: &Path) -> Option<PathBuf> {
    let out = script(
        env,
        path,
        "r=$(jjr root) || exit 1\nprintf '%s\\n' \"$r\"\n\
         [ -f \"$r/.jj/repo\" ] && cat \"$r/.jj/repo\"\nexit 0\n",
    )
    .stdout()
    .ok()?;
    parse_discover(&out)
}

/// A secondary workspace's `.jj/repo` is a file naming the main workspace's
/// `.jj/repo` directory, relative to its own `.jj`.
fn parse_discover(out: &[u8]) -> Option<PathBuf> {
    let (root, repo) = split_once(out, b'\n');
    if root.is_empty() {
        return None;
    }
    let root = path_from_bytes(root);
    let repo = trim_newline(repo);
    if repo.is_empty() {
        return Some(root);
    }
    let target = path_from_bytes(repo);
    let store = if target.is_absolute() {
        normalize(&target)
    } else {
        normalize(&root.join(".jj").join(target))
    };
    store
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or(Some(root))
}

/// Per workspace: root, change id of `@`, bookmarks at `@`, bookmarks at
/// its parents.
const WORKSPACE_TEMPLATE: &str = r#"self.root() ++ "\x1f" ++ target.change_id() ++ "\x1f" ++ target.local_bookmarks().map(|b| b.name()).join(",") ++ "\x1f" ++ target.parents().map(|c| c.local_bookmarks().map(|b| b.name()).join(",")).join(",") ++ "\n""#;

pub(crate) fn worktrees(repo: &Repository) -> Result<Vec<Worktree>, VcsError> {
    // The workspace list, a 0x1e line, then 1 or 0 per workspace for
    // whether its directory exists.
    let out = script(
        &repo.env,
        &repo.root,
        r#"out=$(jjr workspace list -T "$1") || exit 1
printf '%s\n\036\n' "$out"
us=$(printf '\037')
printf '%s\n' "$out" | while IFS= read -r l; do
  [ -n "$l" ] || continue
  if [ -d "${l%%"$us"*}" ]; then echo 1; else echo 0; fi
done
"#,
    )
    .arg(WORKSPACE_TEMPLATE)
    .stdout()?;
    parse_worktrees(&repo.root, &out)
}

fn parse_worktrees(root: &Path, out: &[u8]) -> Result<Vec<Worktree>, VcsError> {
    let (list, exists) = out
        .windows(3)
        .position(|w| w == b"\n\x1e\n")
        .map(|i| (&out[..i], &out[i + 3..]))
        .unwrap_or((out, &[]));
    let mut exists = lines(exists);
    let mut worktrees: Vec<Worktree> = Vec::new();
    for line in lines(list) {
        let mut fields = line.split(|&b| b == 0x1f);
        let mut field = || fields.next().unwrap_or_default();
        let (path, change, at, parents) = (field(), field(), field(), field());
        let present = exists.next() != Some(b"0");
        let path = path_from_bytes(path);
        let is_main = path == root;
        let worktree = Worktree {
            branch: first_name(at).or_else(|| first_name(parents)),
            head: (!change.is_empty()).then(|| string_from_bytes(change)),
            is_main,
            locked: false,
            prunable: !present,
            bare: false,
            path,
        };
        if is_main {
            worktrees.insert(0, worktree);
        } else {
            worktrees.push(worktree);
        }
    }
    if worktrees.first().is_none_or(|w| !w.is_main) {
        return Err(VcsError::Parse(
            "jj workspace list lacks the main workspace".into(),
        ));
    }
    Ok(worktrees)
}

/// The first of comma-separated names.
fn first_name(names: &[u8]) -> Option<String> {
    names
        .split(|&b| b == b',')
        .find(|name| !name.is_empty())
        .map(string_from_bytes)
}

pub(crate) fn add_worktree(
    repo: &Repository,
    path: &Path,
    branch: &str,
    create_branch: bool,
) -> Result<Worktree, VcsError> {
    if create_branch {
        add_worktree_at(repo, &repo.root, path, "@-", Some(branch))
    } else {
        // The new working-copy commit sits on the bookmark, which then
        // names the worktree.
        let mut worktree = add(repo, &repo.root, path, branch, None)?;
        worktree.branch = Some(branch.to_owned());
        Ok(worktree)
    }
}

/// Adds a workspace whose working-copy commit is a new child of the first
/// match of `revision` as the workspace `source` resolves it, so `@-`
/// names the revision checked out there. With `branch`, that bookmark is
/// created at the new working-copy commit.
pub(crate) fn add_worktree_at(
    repo: &Repository,
    source: &Path,
    path: &Path,
    revision: &str,
    branch: Option<&str>,
) -> Result<Worktree, VcsError> {
    add(repo, source, path, revision, branch)
}

fn add(
    repo: &Repository,
    source: &Path,
    path: &Path,
    revision: &str,
    branch: Option<&str>,
) -> Result<Worktree, VcsError> {
    let out = script(
        &repo.env,
        &repo.root,
        r#"set -e
root=$1 path=$2 source=$3 branch=$4
rev=$(jjr -R "$source" log --no-graph -r "$5" -n 1 -T commit_id)
[ -n "$rev" ] || { echo "no revision matches $5" >&2; exit 1; }
jj -R "$root" workspace add --quiet -r "$rev" "$path"
if [ -n "$branch" ]; then
  jj -R "$path" bookmark create --quiet -r @ -- "$branch"
fi
jjr -R "$path" log --no-graph -r @ -T change_id
"#,
    )
    .arg(&repo.root)
    .arg(path)
    .arg(source)
    .arg(branch.unwrap_or(""))
    .arg(revision)
    .stdout()?;
    let change = trim_newline(&out);
    Ok(Worktree {
        path: path.to_path_buf(),
        branch: branch.map(str::to_owned),
        head: (!change.is_empty()).then(|| string_from_bytes(change)),
        is_main: false,
        locked: false,
        prunable: false,
        bare: false,
    })
}

/// Forgets the workspace at `path` and deletes its directory. Its last
/// snapshot stays in the repository as a commit, so nothing is lost; the
/// snapshot first takes in the files on disk. Without `force`, a failed
/// snapshot stops the removal.
pub(crate) fn remove_worktree(repo: &Repository, path: &Path, force: bool) -> Result<(), VcsError> {
    // Refuses paths that are not a workspace of this repository, so it
    // never deletes an unrelated directory.
    script(
        &repo.env,
        &repo.root,
        r#"set -e
root=$1 path=$2 us=$(printf '\037')
name=$(jjr -R "$root" workspace list -T 'self.root() ++ "\x1f" ++ name ++ "\n"' |
  while IFS= read -r l; do
    if [ "${l%%"$us"*}" = "$path" ]; then printf '%s' "${l#*"$us"}"; fi
  done)
[ -n "$name" ] || { echo "not a workspace of $root: $path" >&2; exit 1; }
if [ -d "$path" ] && ! jj -R "$path" status >/dev/null; then
  [ "$3" = 1 ] || { echo "could not snapshot $path" >&2; exit 1; }
fi
jj -R "$root" workspace forget --quiet -- "$name"
rm -rf -- "$path"
"#,
    )
    .arg(&repo.root)
    .arg(path)
    .arg(if force { "1" } else { "0" })
    .stdout()?;
    // A colocated repository forgets the git worktree too. git gets its
    // directory named, with the options of the app's other git commands,
    // so it reads no `.git` a sandbox replaced and runs nothing it names.
    let prune = format!(
        "[ -d \"$1/.git\" ] && [ ! -L \"$1/.git\" ] || exit 0
GIT_DIR=$1/.git GIT_COMMON_DIR=$1/.git exec git {} worktree prune",
        crate::vcs::git::HOST_SAFE.join(" ")
    );
    let _ = (repo.env.command("sh"))
        .args(["-c", prune.as_str(), "sh"])
        .arg(&repo.root)
        .output();
    Ok(())
}

pub(crate) fn status(env: &Environment, worktree: &Path) -> Result<Status, VcsError> {
    // Output: bookmarks at @ \0 bookmarks at its parents \0 conflicted
    // paths \0 then `jj diff --summary` lines. Only the first command
    // snapshots.
    let out = script(
        env,
        worktree,
        r#"jj log --no-graph -r @ -T 'local_bookmarks.map(|b| b.name()).join(",") ++ "\0" ++ parents.map(|c| c.local_bookmarks().map(|b| b.name()).join(",")).join(",") ++ "\0"' || exit
n=$(jjr resolve --list 2>/dev/null | wc -l)
printf '%s\0' "${n:-0}"
jjr diff --summary -r @
"#,
    )
    .stdout()?;
    parse_status(&out)
}

fn parse_status(out: &[u8]) -> Result<Status, VcsError> {
    let mut records = out.splitn(4, |&b| b == 0);
    let mut field = || {
        records
            .next()
            .ok_or_else(|| VcsError::Parse("jj status output".into()))
    };
    let at = field()?;
    let parents = field()?;
    let conflicted = std::str::from_utf8(field()?)
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    let mut status = Status {
        branch: first_name(at).or_else(|| first_name(parents)),
        conflicted,
        ..Status::default()
    };
    for line in lines(records.next().unwrap_or_default()) {
        if matches!(line.first(), Some(b'M' | b'A' | b'D' | b'R' | b'C')) {
            status.modified += 1;
        }
    }
    Ok(status)
}

pub(crate) fn remotes(repo: &Repository) -> Result<Vec<Remote>, VcsError> {
    // A repository on another backend than git has no remotes.
    let out = jj(&repo.env, &repo.root)
        .args(["git", "remote", "list"])
        .stdout()
        .unwrap_or_default();
    Ok(parse_remotes(&out))
}

fn parse_remotes(out: &[u8]) -> Vec<Remote> {
    lines(out)
        .filter_map(|line| {
            let (name, url) = split_once(line, b' ');
            (!name.is_empty() && !url.is_empty()).then(|| Remote {
                name: string_from_bytes(name),
                url: string_from_bytes(url.trim_ascii()),
            })
        })
        .collect()
}

pub(crate) fn files(env: &Environment, worktree: &Path) -> Result<Vec<u8>, VcsError> {
    // jj tracks new files when it snapshots, so the tracked files of `@`
    // are every file on disk that is not ignored. Paths are relative to
    // the workspace root, which is `worktree`.
    Ok(env
        .command("jj")
        .args(["--no-pager", "--color=never", "file", "list", "-T"])
        .arg(r#"path ++ "\0""#)
        .cwd(worktree)
        .stdout()?)
}

pub(crate) fn changes(env: &Environment, worktree: &Path) -> Result<Vec<u8>, VcsError> {
    // Without `--ignore-working-copy`, so the summary sees the files on disk.
    let out = env
        .command("jj")
        .args([
            "--no-pager",
            "--color=never",
            "diff",
            "--summary",
            "-r",
            "@",
        ])
        .cwd(worktree)
        .stdout()?;
    Ok(parse_changes(&out))
}

/// `jj diff --summary` lines as [`crate::vcs::Repository::changes`]
/// records. A rename (`R dir/{old => new}`) keeps its new path.
fn parse_changes(out: &[u8]) -> Vec<u8> {
    let mut changes = Vec::with_capacity(out.len());
    for line in lines(out) {
        let [status, b' ', path @ ..] = line else {
            continue;
        };
        let letter = match status {
            b'A' => b'A',
            b'D' => b'D',
            b'R' | b'C' => b'R',
            _ => b'M',
        };
        changes.extend_from_slice(&[letter, b' ']);
        match renamed(path) {
            Some((before, new, after)) => {
                changes.extend_from_slice(before);
                changes.extend_from_slice(new);
                changes.extend_from_slice(after);
            }
            None => changes.extend_from_slice(path),
        }
        changes.push(0);
    }
    changes
}

/// The parts of `before{old => new}after` that make the new path.
fn renamed(path: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let open = path.iter().position(|&b| b == b'{')?;
    let close = open + path[open..].iter().position(|&b| b == b'}')?;
    let inner = &path[open + 1..close];
    let arrow = inner.windows(4).position(|w| w == b" => ")?;
    let mut after = &path[close + 1..];
    let new = &inner[arrow + 4..];
    // `dir/{ => sub}/file` leaves a doubled slash when one side is empty.
    if new.is_empty() && after.first() == Some(&b'/') {
        after = &after[1..];
    }
    Some((&path[..open], new, after))
}

pub(crate) fn branches(repo: &Repository) -> Result<Vec<String>, VcsError> {
    let out = jj(&repo.env, &repo.root)
        .args(["bookmark", "list", "-T"])
        .arg(r#"if(!remote, name ++ "\n")"#)
        .stdout()?;
    let mut names: Vec<String> = Vec::new();
    for line in lines(&out) {
        if !names.iter().any(|n| n.as_bytes() == line) {
            names.push(string_from_bytes(line));
        }
    }
    Ok(names)
}

/// Log records in the layout [`parse_log`] reads.
const LOG_TEMPLATE: &str = r#"commit_id ++ "\x1f" ++ change_id.short() ++ "\x1f" ++ author.name() ++ "\x1f" ++ author.timestamp().format("%s") ++ "\x1f" ++ separate(", ", local_bookmarks.map(|b| b.name()).join(", "), tags.map(|t| t.name()).join(", ")) ++ "\x1f" ++ description.first_line() ++ "\x1e""#;

/// History of a workspace: its working-copy commit and everything before,
/// without the working-copy commit while it is empty and undescribed.
pub(crate) const LOG_REVSET: &str = r#"::@ ~ (@ & empty() & description(exact:""))"#;

pub(crate) fn log(
    env: &Environment,
    worktree: &Path,
    revset: &str,
) -> Result<Vec<Commit>, VcsError> {
    let out = jj(env, worktree)
        .args(["log", "--no-graph", "-r", revset, "-n"])
        .arg(LOG_LIMIT.to_string())
        .args(["-T", LOG_TEMPLATE])
        .stdout()?;
    Ok(parse_log(&out, string_from_bytes))
}

pub(crate) fn show(env: &Environment, worktree: &Path, id: &str) -> Result<Vec<u8>, VcsError> {
    Ok(jj(env, worktree)
        .args(["show", "--stat", "--git", "-r", id])
        .stdout()?)
}

/// `trunk()`, the revset jj resolves to the default branch of the default
/// remote, unless that is the root commit because nothing matched.
pub(crate) fn default_base(env: &Environment, worktree: &Path) -> Result<Option<String>, VcsError> {
    let out = jj(env, worktree)
        .args([
            "log",
            "--no-graph",
            "-n",
            "1",
            "-r",
            "trunk() ~ root()",
            "-T",
            "commit_id",
        ])
        .stdout()?;
    Ok((!trim_newline(&out).is_empty()).then(|| "trunk()".to_owned()))
}

pub(crate) fn review(
    env: &Environment,
    worktree: &Path,
    base: &str,
    uncommitted: bool,
) -> Result<Review, VcsError> {
    // Output: merge base NUL log records NUL patch. Without `uncommitted`
    // the review ends at `@-`, leaving out the working-copy commit. The
    // first command snapshots, so the working copy is current.
    let out = script(
        env,
        worktree,
        r#"set -e
mb=$(jj log --no-graph -n 1 -r "fork_point(@ | ($1))" -T commit_id)
[ -n "$mb" ] || { printf 'no common ancestor with %s\n' "$1" >&2; exit 1; }
if [ "$2" = 1 ]; then to=@; else to=@-; fi
printf '%s\0' "$mb"
jjr log --no-graph -r "$mb..$to" -n "$3" -T "$4"
printf '\0'
jjr diff --git --from "$mb" --to "$to"
"#,
    )
    .arg(base)
    .arg(if uncommitted { "1" } else { "0" })
    .arg(LOG_LIMIT.to_string())
    .arg(LOG_TEMPLATE)
    .stdout()?;
    let (merge_base, rest) = split_once(&out, 0);
    let (log, patch) = split_once(rest, 0);
    Ok(Review {
        merge_base: string_from_bytes(merge_base),
        commits: parse_log(log, string_from_bytes),
        patch: patch.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_keep_the_new_path_of_a_rename() {
        let out = b"M src/a.rs\nA new.txt\nR src/{old.rs => new.rs}\nR {a => b}/c.rs\nD gone\n";
        assert_eq!(
            super::parse_changes(out),
            b"M src/a.rs\0A new.txt\0R src/new.rs\0R b/c.rs\0D gone\0"
        );
    }

    #[test]
    fn discover_resolves_main_workspace() {
        assert_eq!(parse_discover(b"/r\n"), Some(PathBuf::from("/r")));
        assert_eq!(
            parse_discover(b"/x/w\n../../r/.jj/repo"),
            Some(PathBuf::from("/x/r"))
        );
        assert_eq!(
            parse_discover(b"/w\n/r/.jj/repo\n"),
            Some(PathBuf::from("/r"))
        );
        assert_eq!(parse_discover(b""), None);
    }

    #[test]
    fn parses_workspaces() {
        let out = b"/w\x1fwwww\x1f\x1fmain\n/r\x1frrrr\x1ftopic,other\x1f\n/gone\x1fgggg\x1f\x1f\n\x1e\n1\n1\n0\n";
        let list = parse_worktrees(Path::new("/r"), out).unwrap();
        assert_eq!(list.len(), 3);
        assert!(list[0].is_main);
        assert_eq!(list[0].branch.as_deref(), Some("topic"));
        assert_eq!(list[1].branch.as_deref(), Some("main"));
        assert_eq!(list[1].head.as_deref(), Some("wwww"));
        assert!(list[2].prunable && list[2].branch.is_none());
        assert!(parse_worktrees(Path::new("/elsewhere"), out).is_err());
    }

    #[test]
    fn parses_status_records() {
        let status = parse_status(b"\0main\x002\0M a\nA b\nR {c => d}\n").unwrap();
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(
            (status.modified, status.untracked, status.conflicted),
            (3, 0, 2)
        );
        let status = parse_status(b"topic\0main\x000\0").unwrap();
        assert_eq!(status.branch.as_deref(), Some("topic"));
        assert_eq!(status.modified, 0);
    }

    #[test]
    fn parses_remotes() {
        assert_eq!(
            parse_remotes(b"origin https://h/r.git\nup ssh://h/r\n")[1],
            Remote {
                name: "up".into(),
                url: "ssh://h/r".into()
            }
        );
    }
}
