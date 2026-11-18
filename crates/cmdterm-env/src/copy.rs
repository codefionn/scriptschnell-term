//! Copying files from this machine into any environment.
//!
//! A local `tar -c` streams into `tar -x` run in the target, so the same
//! code reaches SSH hosts, remote machines, VMs, containers and sandboxes.

use std::io;
use std::path::Path;
use std::process::Stdio;
use std::thread;

use crate::{CommandError, Environment};

/// Creates the destination directory, expanding a leading `~` to the
/// target's home, then unpacks stdin into it. Git for Windows' `tar` cannot
/// change into a path such as `C:\x`, so where `cygpath` exists the path
/// becomes `/c/x` first.
const UNPACK: &str = "d=$1; case $d in '~') d=$HOME ;; '~/'*) d=$HOME/${d#'~/'} ;; esac; \
if command -v cygpath >/dev/null 2>&1; then d=$(cygpath -u -- \"$d\") || exit 1; fi; \
mkdir -p \"$d\" && exec tar -xf - -C \"$d\"";

impl Environment {
    /// Copies the local file or directory `source` into the directory
    /// `dest_dir` of this environment, keeping its name. `dest_dir` may start
    /// with `~`. Paths matching an `exclude` pattern (tar `--exclude`
    /// syntax) are left out.
    pub fn copy_in(
        &self,
        source: &Path,
        dest_dir: &str,
        exclude: &[&str],
    ) -> Result<(), CommandError> {
        let parent = source
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = source.file_name().ok_or_else(|| {
            CommandError::Spawn(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has no file name", source.display()),
            ))
        })?;
        let mut pack = Environment::Local
            .command("tar")
            .args(exclude.iter().map(|pattern| format!("--exclude={pattern}")))
            .arg("-cf")
            .arg("-")
            .arg("-C")
            .arg(parent)
            .arg(name)
            .to_std();
        pack.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut pack = pack.spawn().map_err(CommandError::Spawn)?;
        let mut unpack = self
            .command("sh")
            .args(["-c", UNPACK, "sh", dest_dir])
            .to_std();
        unpack
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut unpack = match unpack.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = pack.kill();
                let _ = pack.wait();
                return Err(CommandError::Spawn(error));
            }
        };
        // Both sides' stderr is read while they run: a tar that warns more
        // than a pipe holds would otherwise block forever.
        let pack_err = drain_tail(pack.stderr.take().expect("piped stderr"));
        let unpack_err = drain_tail(unpack.stderr.take().expect("piped stderr"));
        let mut tar_out = pack.stdout.take().expect("piped stdout");
        let mut sink = unpack.stdin.take().expect("piped stdin");
        let pump = thread::spawn(move || io::copy(&mut tar_out, &mut sink));
        let unpacked = unpack.wait().map_err(CommandError::Spawn)?;
        let _ = pump.join();
        let packed = pack.wait().map_err(CommandError::Spawn)?;
        let failure = |code: Option<i32>, stderr: Vec<u8>| CommandError::Failed {
            code,
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        };
        let pack_err = pack_err.join().unwrap_or_default();
        let unpack_err = unpack_err.join().unwrap_or_default();
        // A failed target side, such as an ssh that could not connect, also
        // ends the local tar with a broken pipe, so its error comes first.
        // The local tar's own message follows when it wrote one.
        if !unpacked.success() {
            let mut stderr = unpack_err;
            if !packed.success() && !pack_err.is_empty() {
                stderr.extend_from_slice(b"\n");
                stderr.extend_from_slice(&pack_err);
            }
            return Err(failure(unpacked.code(), stderr));
        }
        if !packed.success() {
            return Err(failure(packed.code(), pack_err));
        }
        Ok(())
    }
}

/// How much of a side's stderr [`copy_in`](Environment::copy_in) keeps:
/// the last bytes, which hold the error that ended it.
const STDERR_TAIL: usize = 16 * 1024;

/// Reads `pipe` to its end on a thread and returns its last
/// [`STDERR_TAIL`] bytes.
fn drain_tail(mut pipe: impl io::Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut tail = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    tail.extend_from_slice(&chunk[..n]);
                    if tail.len() > 2 * STDERR_TAIL {
                        tail.drain(..tail.len() - STDERR_TAIL);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if tail.len() > STDERR_TAIL {
            tail.drain(..tail.len() - STDERR_TAIL);
        }
        tail
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_and_files_arrive_with_their_names() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let tree = src.path().join("conf");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("sub/a.txt"), "a").unwrap();
        std::fs::write(tree.join("skip.log"), "x").unwrap();
        std::fs::write(src.path().join("single"), "s").unwrap();
        let dest = dst.path().join("new/dir");
        let dest = dest.to_str().unwrap();
        Environment::Local.copy_in(&tree, dest, &["*.log"]).unwrap();
        Environment::Local
            .copy_in(&src.path().join("single"), dest, &[])
            .unwrap();
        let out = dst.path().join("new/dir");
        assert_eq!(
            std::fs::read_to_string(out.join("conf/sub/a.txt")).unwrap(),
            "a"
        );
        assert!(!out.join("conf/skip.log").exists());
        assert_eq!(std::fs::read_to_string(out.join("single")).unwrap(), "s");
    }

    #[test]
    fn a_failed_target_reports_its_own_error() {
        let src = tempfile::tempdir().unwrap();
        // More than a pipe holds, so the local tar is still writing when
        // the target side gives up.
        std::fs::write(src.path().join("big"), vec![7u8; 1 << 20]).unwrap();
        let blocker = src.path().join("blocker");
        std::fs::write(&blocker, "").unwrap();
        let dest = blocker.join("under-a-file");
        let error = Environment::Local
            .copy_in(&src.path().join("big"), dest.to_str().unwrap(), &[])
            .unwrap_err();
        let CommandError::Failed { code, stderr } = &error else {
            panic!("{error}");
        };
        assert!(code.is_some_and(|code| code != 0), "{error}");
        assert!(stderr.contains("mkdir"), "{error}");
    }

    /// A tar that warns about every one of thousands of files it cannot
    /// read writes more than a pipe holds to stderr.
    #[cfg(unix)]
    #[test]
    fn noisy_tars_do_not_hang() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            eprintln!("root reads every file; skipped");
            return;
        }
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let tree = src.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        for n in 0..2500 {
            let file = tree.join(format!("unreadable-file-number-{n:05}"));
            std::fs::write(&file, "").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let error = Environment::Local
            .copy_in(&tree, dst.path().to_str().unwrap(), &[])
            .unwrap_err();
        let CommandError::Failed { stderr, .. } = &error else {
            panic!("{error}");
        };
        assert!(stderr.len() <= STDERR_TAIL, "{}", stderr.len());
        assert!(stderr.contains("unreadable-file-number-"), "{error}");
    }

    #[test]
    fn missing_sources_fail() {
        let error = Environment::Local
            .copy_in(Path::new("/nonexistent/cmdterm"), "/tmp", &[])
            .unwrap_err();
        assert!(matches!(error, CommandError::Failed { .. }), "{error}");
    }
}
