//! New-file diffs for untracked files, which neither `git diff` nor
//! `hg diff` prints. One `sh` call reads a batch of files, so a remote
//! project pays for one SSH channel per batch, not one per file.

use std::path::Path;

use cmdterm_env::Environment;

use crate::vcs::{VcsError, path_from_bytes};

/// Files larger than this show as binary instead of with their content.
const MAX_FILE: u64 = 4 * 1024 * 1024;

/// Paths per `sh` call, well below any argument length limit.
const BATCH: usize = 500;

/// git treats a file as binary when its first 8000 bytes hold a NUL.
const BINARY_PROBE: usize = 8000;

/// `sh -c SCRIPT sh MAX PATH...`: for each regular file or symlink,
/// `path NUL mode NUL size NUL` and then `size` bytes of content (a
/// symlink's target), or size `-1` without content for a file over MAX.
/// Other paths, such as unreadable files or nested repositories, are skipped.
const SCRIPT: &str = r#"export LC_ALL=C
max=$1; shift
for f do
  p=./$f
  if [ -L "$p" ]; then
    t=$(readlink "$p") || continue
    printf '%s\0120000\0%s\0%s' "$f" "${#t}" "$t"
  elif [ -f "$p" ] && [ -r "$p" ]; then
    if [ -x "$p" ]; then m=100755; else m=100644; fi
    n=$(($(wc -c < "$p")))
    if [ "$n" -gt "$max" ]; then
      printf '%s\0%s\0-1\0' "$f" "$m"
    else
      printf '%s\0%s\0%s\0' "$f" "$m" "$n"
      head -c "$n" "$p"
    fi
  fi
done
"#;

/// Appends a new-file diff in git's extended format, with `a/` and `b/`
/// prefixes, for each of the NUL-separated `paths` relative to `worktree`.
pub(crate) fn append(
    env: &Environment,
    worktree: &Path,
    paths: &[u8],
    patch: &mut Vec<u8>,
) -> Result<(), VcsError> {
    let paths: Vec<&[u8]> = paths.split(|&b| b == 0).filter(|p| !p.is_empty()).collect();
    for batch in paths.chunks(BATCH) {
        let out = env
            .command("sh")
            .args(["-c", SCRIPT, "sh"])
            .arg(MAX_FILE.to_string())
            .args(batch.iter().map(|path| path_from_bytes(path)))
            .cwd(worktree)
            .stdout()?;
        write_records(&out, patch);
    }
    Ok(())
}

/// Turns the script's records into diffs. A record cut short, as when a
/// file shrank while it was read, ends the batch.
fn write_records(mut out: &[u8], patch: &mut Vec<u8>) {
    while !out.is_empty() {
        let mut fields = out.splitn(4, |&b| b == 0);
        let (Some(path), Some(mode), Some(size), Some(rest)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return;
        };
        let content = match std::str::from_utf8(size)
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
        {
            Some(-1) => None,
            Some(size) if size >= 0 && size as usize <= rest.len() => Some(&rest[..size as usize]),
            _ => return,
        };
        out = &rest[content.map_or(0, <[u8]>::len)..];
        write_file(path, mode, content, patch);
    }
}

/// One new file's diff; `None` content is a file too large to show.
fn write_file(path: &[u8], mode: &[u8], content: Option<&[u8]>, patch: &mut Vec<u8>) {
    let line = |patch: &mut Vec<u8>, parts: &[&[u8]]| {
        parts.iter().for_each(|part| patch.extend_from_slice(part));
        patch.push(b'\n');
    };
    line(patch, &[b"diff --git a/", path, b" b/", path]);
    line(patch, &[b"new file mode ", mode]);
    let Some(content) = content.filter(|c| !c[..c.len().min(BINARY_PROBE)].contains(&0)) else {
        line(patch, &[b"Binary files /dev/null and b/", path, b" differ"]);
        return;
    };
    if content.is_empty() {
        return;
    }
    line(patch, &[b"--- /dev/null"]);
    line(patch, &[b"+++ b/", path]);
    let body = content.strip_suffix(b"\n").unwrap_or(content);
    let count = body.split(|&b| b == b'\n').count();
    line(
        patch,
        &[b"@@ -0,0 +1,", count.to_string().as_bytes(), b" @@"],
    );
    for text in body.split(|&b| b == b'\n') {
        line(patch, &[b"+", text]);
    }
    if !content.ends_with(b"\n") {
        line(patch, &[b"\\ No newline at end of file"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(out: &[u8]) -> String {
        let mut patch = Vec::new();
        write_records(out, &mut patch);
        String::from_utf8(patch).unwrap()
    }

    #[test]
    fn writes_text_binary_and_empty_files() {
        let out = b"a b\x00100644\x007\0one\ntwotwo\x00100755\x002\0x\nbin\x00100644\x002\0\0\x01e\x00100644\x000\0";
        assert_eq!(
            records(out),
            "diff --git a/a b b/a b\nnew file mode 100644\n--- /dev/null\n+++ b/a b\n\
@@ -0,0 +1,2 @@\n+one\n+two\n\\ No newline at end of file\n\
diff --git a/two b/two\nnew file mode 100755\n--- /dev/null\n+++ b/two\n\
@@ -0,0 +1,1 @@\n+x\n\
diff --git a/bin b/bin\nnew file mode 100644\nBinary files /dev/null and b/bin differ\n\
diff --git a/e b/e\nnew file mode 100644\n"
        );
    }

    #[test]
    fn large_files_show_as_binary_and_short_records_stop() {
        assert_eq!(
            records(b"big\x00100644\0-1\0cut\x00100644\x009\0abc"),
            "diff --git a/big b/big\nnew file mode 100644\nBinary files /dev/null and b/big differ\n"
        );
    }
}
