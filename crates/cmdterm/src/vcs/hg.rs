//! Mercurial backend. Worktrees are shares (`hg share -B`, bookmarks shared
//! with the source), recorded in `<root>/.hg/cmdterm-shares`.
//!
//! Queries that need several steps run as one `sh -c` script so a remote
//! project pays for one SSH channel, not one per step. The share extension is
//! enabled on every call: shared bookmarks only resolve with it loaded.

use std::path::{Path, PathBuf};

use cmdterm_env::{EnvCommand, Environment};

use crate::vcs::{
    Commit, LOG_LIMIT, Remote, Repository, Review, Status, VcsError, Worktree, lines, network_ok,
    non_empty_string, normalize, parse_log, path_from_bytes, split_once, string_from_bytes,
    trim_newline,
};

/// Shell prelude: plain output and the share extension for every `hg`.
const PRELUDE: &str = "hg() { command hg --config extensions.share= \"$@\" </dev/null; }\n";

fn hg(env: &Environment, cwd: &Path) -> EnvCommand {
    env.command("hg")
        .args(["--config", "extensions.share="])
        .cwd(cwd)
        .var("HGPLAIN", "1")
}

/// `sh -c PRELUDE+script sh args...` in `cwd`.
fn script(env: &Environment, cwd: &Path, body: &str) -> EnvCommand {
    let mut source = String::with_capacity(PRELUDE.len() + body.len());
    source.push_str(PRELUDE);
    source.push_str(body);
    env.command("sh")
        .args(["-c".into(), source, "sh".into()])
        .cwd(cwd)
        .var("HGPLAIN", "1")
}

/// The share source (or plain repository root) containing `path`.
pub(crate) fn discover(env: &Environment, path: &Path) -> Option<PathBuf> {
    let out = script(
        env,
        path,
        "r=$(hg root) || exit 1\nprintf '%s\\n' \"$r\"\n\
         [ -r \"$r/.hg/sharedpath\" ] && cat \"$r/.hg/sharedpath\"\nexit 0\n",
    )
    .stdout()
    .ok()?;
    parse_discover(&out)
}

fn parse_discover(out: &[u8]) -> Option<PathBuf> {
    let (root, shared) = split_once(out, b'\n');
    if root.is_empty() {
        return None;
    }
    let root = path_from_bytes(root);
    let shared = trim_newline(shared);
    if shared.is_empty() {
        return Some(root);
    }
    let store = resolve_sharedpath(&root, shared);
    // sharedpath names the source's `.hg` directory.
    Some(store.parent().map(Path::to_path_buf).unwrap_or(root))
}

/// `.hg/sharedpath` may be relative to the share's `.hg` directory.
fn resolve_sharedpath(share: &Path, value: &[u8]) -> PathBuf {
    let target = path_from_bytes(value);
    if target.is_absolute() {
        normalize(&target)
    } else {
        normalize(&share.join(".hg").join(target))
    }
}

pub(crate) fn worktrees(repo: &Repository) -> Result<Vec<Worktree>, VcsError> {
    // Per entry, five lines: path, state (main | s:<sharedpath> | missing),
    // node, active bookmark, branch.
    let out = script(
        &repo.env,
        &repo.root,
        r#"root=$1
emit() {
  printf '%s\n%s\n' "$1" "$2"
  hg log -R "$1" -r . -T '{node}\n{activebookmark}\n{branch}\n' 2>/dev/null || printf '\n\n\n'
}
emit "$root" main
[ -r "$root/.hg/cmdterm-shares" ] || exit 0
while IFS= read -r p || [ -n "$p" ]; do
  [ -n "$p" ] || continue
  if [ -r "$p/.hg/sharedpath" ]; then emit "$p" "s:$(cat "$p/.hg/sharedpath")"
  else printf '%s\nmissing\n\n\n\n' "$p"; fi
done < "$root/.hg/cmdterm-shares"
"#,
    )
    .arg(&repo.root)
    .stdout()?;
    parse_worktrees(&repo.root, &out)
}

fn parse_worktrees(root: &Path, out: &[u8]) -> Result<Vec<Worktree>, VcsError> {
    let source = normalize(&root.join(".hg"));
    let mut list: Vec<Worktree> = Vec::new();
    let mut lines = out.split(|&b| b == b'\n');
    while let Some(path) = lines.next() {
        if path.is_empty() {
            continue;
        }
        let mut field = || lines.next().unwrap_or_default();
        let (state, node, bookmark, branch) = (field(), field(), field(), field());
        let path = path_from_bytes(path);
        let (is_main, prunable) = match state {
            b"main" => (true, false),
            b"missing" => (false, true),
            _ => match state.strip_prefix(b"s:") {
                // Only shares that still point at this repository count.
                Some(shared) if resolve_sharedpath(&path, shared) == source => (false, false),
                Some(_) => continue,
                None => return Err(VcsError::Parse(format!("hg worktree state {state:?}"))),
            },
        };
        if list.iter().any(|w| w.path == path) {
            continue;
        }
        list.push(Worktree {
            path,
            branch: non_empty_string(bookmark).or_else(|| non_empty_string(branch)),
            head: head(node),
            is_main,
            locked: false,
            prunable,
            bare: false,
        });
    }
    if list.first().is_none_or(|w| !w.is_main) {
        return Err(VcsError::Parse(
            "hg worktree list lacks the main entry".into(),
        ));
    }
    Ok(list)
}

fn head(node: &[u8]) -> Option<String> {
    (!node.is_empty() && !node.iter().all(|&b| b == b'0')).then(|| string_from_bytes(node))
}

pub(crate) fn add_worktree(
    repo: &Repository,
    path: &Path,
    branch: &str,
    create_branch: bool,
) -> Result<Worktree, VcsError> {
    // Record the share right after creating it, so a failed update still
    // leaves it listed (and removable).
    let out = script(
        &repo.env,
        &repo.root,
        r#"set -e
root=$1 path=$2 branch=$3
hg share -B -U -q "$root" "$path"
printf '%s\n' "$path" >> "$root/.hg/cmdterm-shares"
if [ "$4" = 1 ]; then
  rev=$(hg log -R "$root" -r . -T '{node}')
  hg update -R "$path" -q -r "$rev"
  hg bookmark -R "$path" -q -- "$branch"
else
  hg update -R "$path" -q -- "$branch"
fi
hg log -R "$path" -r . -T '{node}'
"#,
    )
    .arg(&repo.root)
    .arg(path)
    .arg(branch)
    .arg(if create_branch { "1" } else { "0" })
    .stdout()?;
    Ok(Worktree {
        path: path.to_path_buf(),
        branch: Some(branch.to_owned()),
        head: head(trim_newline(&out)),
        is_main: false,
        locked: false,
        prunable: false,
        bare: false,
    })
}

/// Adds a share at the first match of `revision` as the checkout `source`
/// resolves it, so `.` names the revision checked out there.
pub(crate) fn add_worktree_at(
    repo: &Repository,
    source: &Path,
    path: &Path,
    revision: &str,
    branch: Option<&str>,
) -> Result<Worktree, VcsError> {
    // Updating to a bare node leaves no bookmark active: hg's detached state.
    let out = script(
        &repo.env,
        &repo.root,
        r#"set -e
root=$1 path=$2 source=$3 branch=$4
rev=$(hg log -R "$source" -r "$5" -l 1 -T '{node}')
[ -n "$rev" ] || { echo "no revision matches $5" >&2; exit 1; }
hg share -B -U -q "$root" "$path"
printf '%s\n' "$path" >> "$root/.hg/cmdterm-shares"
hg update -R "$path" -q -r "$rev"
if [ -n "$branch" ]; then
  hg bookmark -R "$path" -q -- "$branch"
fi
hg log -R "$path" -r . -T '{node}'
"#,
    )
    .arg(&repo.root)
    .arg(path)
    .arg(source)
    .arg(branch.unwrap_or(""))
    .arg(revision)
    .stdout()?;
    Ok(Worktree {
        path: path.to_path_buf(),
        branch: branch.map(str::to_owned),
        head: head(trim_newline(&out)),
        is_main: false,
        locked: false,
        prunable: false,
        bare: false,
    })
}

pub(crate) fn remove_worktree(repo: &Repository, path: &Path, force: bool) -> Result<(), VcsError> {
    // Refuses paths this crate did not record, so it never deletes an
    // unrelated directory.
    script(
        &repo.env,
        &repo.root,
        r#"set -e
root=$1 path=$2 f="$1/.hg/cmdterm-shares"
if ! [ -r "$f" ] || ! grep -qxF -e "$path" "$f"; then
  echo "not a worktree of $root: $path" >&2; exit 1
fi
if [ -e "$path" ]; then
  [ -r "$path/.hg/sharedpath" ] || { echo "not a mercurial share: $path" >&2; exit 1; }
  if [ "$3" != 1 ]; then
    st=$(hg status -R "$path")
    [ -z "$st" ] || { echo "$path has uncommitted changes" >&2; exit 1; }
  fi
  rm -rf -- "$path"
fi
grep -vxF -e "$path" "$f" > "$f.tmp" || true
mv -- "$f.tmp" "$f"
"#,
    )
    .arg(&repo.root)
    .arg(path)
    .arg(if force { "1" } else { "0" })
    .stdout()?;
    Ok(())
}

pub(crate) fn status(env: &Environment, worktree: &Path) -> Result<Status, VcsError> {
    // Output: bookmark \0 branch \0 conflicts \0 then `hg status -0` records.
    // `hg resolve` only runs while a merge is in progress.
    let out = script(
        env,
        worktree,
        r#"hg log -r . -T '{activebookmark}\0{branch}\0' || exit
if [ -e .hg/merge/state ] || [ -e .hg/merge/state2 ]; then
  n=$(hg resolve -l | grep -c '^U')
else n=0; fi
printf '%s\0' "${n:-0}"
hg status -0
"#,
    )
    .stdout()?;
    parse_status(&out)
}

fn parse_status(out: &[u8]) -> Result<Status, VcsError> {
    let mut records = out.split(|&b| b == 0);
    let mut field = || {
        records
            .next()
            .ok_or_else(|| VcsError::Parse("hg status output".into()))
    };
    let bookmark = field()?;
    let branch = field()?;
    let conflicted = std::str::from_utf8(field()?)
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    let mut status = Status {
        branch: non_empty_string(bookmark).or_else(|| non_empty_string(branch)),
        conflicted,
        ..Status::default()
    };
    for record in records {
        match record.first() {
            Some(b'M' | b'A' | b'R' | b'!') => status.modified += 1,
            Some(b'?') => status.untracked += 1,
            _ => {}
        }
    }
    Ok(status)
}

pub(crate) fn remotes(repo: &Repository) -> Result<Vec<Remote>, VcsError> {
    let out = hg(&repo.env, &repo.root).arg("paths").stdout()?;
    Ok(parse_paths(&out))
}

fn parse_paths(out: &[u8]) -> Vec<Remote> {
    lines(out)
        .filter_map(|line| {
            let i = line.windows(3).position(|w| w == b" = ")?;
            Some(Remote {
                name: string_from_bytes(&line[..i]),
                url: string_from_bytes(&line[i + 3..]),
            })
        })
        .collect()
}

/// `hg pull` from `remote`, else from `default`; with `update`, the
/// working copy is updated too.
pub(crate) fn fetch(
    env: &Environment,
    worktree: &Path,
    remote: Option<&str>,
    update: bool,
) -> Result<(), VcsError> {
    if let Some(remote) = remote
        && remote.starts_with('-')
    {
        return Err(VcsError::InvalidArgument(format!(
            "remote name cannot start with '-': {remote}"
        )));
    }
    let mut command = hg(env, worktree).arg("pull");
    if update {
        command = command.arg("--update");
    }
    if let Some(remote) = remote {
        command = command.arg("--").arg(remote);
    }
    network_ok(&command)
}

/// `sh -c PUSH sh [PATH]`: pushes `.` with its active bookmark. `hg push`
/// exits 1 when there is nothing to push, which is no failure here.
const PUSH: &str = r#"b=$(hg log -r . -T '{activebookmark}') || exit 2
if [ -n "$b" ]; then hg push -r . -B "$b" -- "$@"; else hg push -r . -- "$@"; fi
c=$?
[ "$c" = 1 ] && exit 0
exit "$c"
"#;

/// Pushes the checked-out revision, see [`Repository::push`].
pub(crate) fn push(
    repo: &Repository,
    worktree: &Path,
    remote: Option<&str>,
) -> Result<String, VcsError> {
    if let Some(remote) = remote
        && remote.starts_with('-')
    {
        return Err(VcsError::InvalidArgument(format!(
            "remote name cannot start with '-': {remote}"
        )));
    }
    let mut command = script(&repo.env, worktree, PUSH);
    let name = match remote {
        Some(remote) => {
            command = command.arg(remote);
            remote.to_owned()
        }
        None => {
            let paths = remotes(repo)?;
            let push = paths.iter().any(|path| path.name == "default-push");
            (if push { "default-push" } else { "default" }).to_owned()
        }
    };
    network_ok(&command)?;
    Ok(name)
}

pub(crate) fn files(env: &Environment, worktree: &Path) -> Result<Vec<u8>, VcsError> {
    // Modified, added, clean and unknown: every file on disk that is not
    // ignored. Without patterns hg prints paths relative to the root of the
    // working copy, which is `worktree`.
    Ok(hg(env, worktree)
        .args(["status", "-macu", "--no-status", "-0"])
        .stdout()?)
}

pub(crate) fn changes(env: &Environment, worktree: &Path) -> Result<Vec<u8>, VcsError> {
    let out = hg(env, worktree)
        .args(["status", "-mardu", "-0"])
        .stdout()?;
    Ok(parse_changes(&out))
}

/// `S path` records of `hg status -0` as
/// [`crate::vcs::Repository::changes`] records: removed (`R`) and missing
/// (`!`) files count as deleted.
fn parse_changes(out: &[u8]) -> Vec<u8> {
    let mut changes = Vec::with_capacity(out.len());
    for record in out.split(|&b| b == 0) {
        let [status, b' ', path @ ..] = record else {
            continue;
        };
        let letter = match status {
            b'R' | b'!' => b'D',
            b'A' => b'A',
            b'?' => b'?',
            _ => b'M',
        };
        changes.extend_from_slice(&[letter, b' ']);
        changes.extend_from_slice(path);
        changes.push(0);
    }
    changes
}

pub(crate) fn branches(repo: &Repository) -> Result<Vec<String>, VcsError> {
    let out = script(&repo.env, &repo.root, "hg branches -q && hg bookmarks -q\n").stdout()?;
    let mut names: Vec<String> = Vec::new();
    for line in lines(&out) {
        let line = line.trim_ascii();
        if !line.is_empty() && !names.iter().any(|n| n.as_bytes() == line) {
            names.push(string_from_bytes(line));
        }
    }
    Ok(names)
}

/// Log records in the layout [`parse_log`] reads.
const LOG_TEMPLATE: &str = r"{node}\x1f{node|short}\x1f{author|person}\x1f{date|hgdate}\x1f{bookmarks} {tags}\x1f{desc|firstline}\x1e";

/// Bookmarks and tags as one comma-separated list, without `tip`.
fn refs(names: &[u8]) -> String {
    let mut out = String::new();
    for name in names.split(u8::is_ascii_whitespace) {
        if name.is_empty() || name == b"tip" {
            continue;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(&String::from_utf8_lossy(name));
    }
    out
}

pub(crate) fn log(
    env: &Environment,
    worktree: &Path,
    revset: &str,
) -> Result<Vec<Commit>, VcsError> {
    let out = hg(env, worktree)
        .args(["log", "-r", revset, "-l"])
        .arg(LOG_LIMIT.to_string())
        .args(["-T", LOG_TEMPLATE])
        .stdout()?;
    Ok(parse_log(&out, refs))
}

pub(crate) fn show(env: &Environment, worktree: &Path, id: &str) -> Result<Vec<u8>, VcsError> {
    Ok(hg(env, worktree)
        .args(["log", "-v", "--stat", "-p", "--git", "-r", id])
        .stdout()?)
}

/// `revision` as a revset string literal.
fn revset_string(revision: &str) -> String {
    let mut out = String::with_capacity(revision.len() + 2);
    out.push('\'');
    for ch in revision.chars() {
        if matches!(ch, '\'' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

pub(crate) fn review(
    env: &Environment,
    worktree: &Path,
    base: &str,
    uncommitted: bool,
) -> Result<Review, VcsError> {
    // Output: merge base NUL log records NUL patch.
    let out = script(
        env,
        worktree,
        r#"set -e
mb=$(hg log -r "ancestor(., $1)" -T '{node}')
[ -n "$mb" ] || { printf 'no common ancestor with %s\n' "$2" >&2; exit 1; }
printf '%s\0' "$mb"
hg log -r "reverse(only(., $mb))" -l "$4" -T "$5"
printf '\0'
if [ "$3" = 1 ]; then hg diff --git -r "$mb"; else hg diff --git -r "$mb" -r .; fi
"#,
    )
    .arg(revset_string(base))
    .arg(base)
    .arg(if uncommitted { "1" } else { "0" })
    .arg(LOG_LIMIT.to_string())
    .arg(LOG_TEMPLATE)
    .stdout()?;
    let (merge_base, rest) = split_once(&out, 0);
    let (log, patch) = split_once(rest, 0);
    let mut patch = patch.to_vec();
    if uncommitted {
        let untracked = hg(env, worktree).args(["status", "-un0"]).stdout()?;
        crate::vcs::untracked::append(env, worktree, &untracked, &mut patch)?;
    }
    Ok(Review {
        merge_base: string_from_bytes(merge_base),
        commits: parse_log(log, refs),
        patch,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn changes_count_removed_and_missing_files_as_deleted() {
        let out = b"M a.rs\0A b.rs\0R c.rs\0! d.rs\0? e.rs\0";
        assert_eq!(
            super::parse_changes(out),
            b"M a.rs\0A b.rs\0D c.rs\0D d.rs\0? e.rs\0"
        );
    }

    use super::*;

    #[test]
    fn discover_resolves_share_source() {
        assert_eq!(parse_discover(b"/r\n"), Some(PathBuf::from("/r")));
        assert_eq!(parse_discover(b"/s\n/r/.hg"), Some(PathBuf::from("/r")));
        assert_eq!(
            parse_discover(b"/x/s\n../../r/.hg"),
            Some(PathBuf::from("/x/r"))
        );
        assert_eq!(parse_discover(b""), None);
    }

    #[test]
    fn parses_worktree_entries() {
        let out = b"/r\nmain\nabc\nbm\ndefault\n\
/s\ns:/r/.hg\ndef\n\nfeature\n\
/gone\nmissing\n\n\n\n\
/other\ns:/elsewhere/.hg\n123\n\ndefault\n";
        let list = parse_worktrees(Path::new("/r"), out).unwrap();
        assert_eq!(list.len(), 3);
        assert!(list[0].is_main);
        assert_eq!(list[0].branch.as_deref(), Some("bm"));
        assert_eq!(list[1].branch.as_deref(), Some("feature"));
        assert!(list[2].prunable);
    }

    #[test]
    fn parses_status_records() {
        let status = parse_status(b"\0default\x002\0M a\0A b\0? c\0").unwrap();
        assert_eq!(status.branch.as_deref(), Some("default"));
        assert_eq!(
            (status.modified, status.untracked, status.conflicted),
            (2, 1, 2)
        );
    }

    #[test]
    fn quotes_revset_strings_and_joins_refs() {
        assert_eq!(revset_string("main"), "'main'");
        assert_eq!(revset_string(r"a'b\c"), r"'a\'b\\c'");
        assert_eq!(refs(b"feature  tip v1.0"), "feature, v1.0");
        assert_eq!(refs(b" tip"), "");
    }

    #[test]
    fn parses_paths() {
        let remotes = parse_paths(b"default = https://h/r\nup = ssh://h//r\n");
        assert_eq!(
            remotes[1],
            Remote {
                name: "up".into(),
                url: "ssh://h//r".into()
            }
        );
    }
}
