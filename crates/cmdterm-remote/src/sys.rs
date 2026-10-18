//! The parts of the crate that differ between Unix and Windows.
//!
//! Keys, the handshake, the client's connection and the server over TCP work
//! on both. Masters, the VM daemon, forwarded sockets and Firecracker talk
//! over Unix sockets, which the standard library offers only on Unix. On
//! Windows the socket types here fail every call with
//! [`io::ErrorKind::Unsupported`], so those features report an error instead
//! of keeping the crate from building.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::process::Command;

#[cfg(windows)]
pub use self::windows::{UnixListener, UnixStream};
#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};

/// Opens new files with `mode` on Unix. Windows files inherit the ACL of
/// their directory, which for the app's directories is the user's own.
pub fn mode(options: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode)
    }
    #[cfg(windows)]
    {
        let _ = mode;
        options
    }
}

/// Binds a Unix socket at `path` that only this user can reach from the
/// moment it exists there. The socket is bound in a new mode 0700
/// directory next to `path`, made mode 0600, and then linked to `path`, so
/// there is no window between `bind` and `chmod` in which another local
/// user could connect. The umask is process-global, so changing it around
/// the bind would give directories other threads create meanwhile the
/// wrong mode. Like a plain bind, this fails when `path` exists.
/// On Windows the socket types fail every call anyway.
pub fn bind_private_socket(path: &Path) -> io::Result<UnixListener> {
    #[cfg(unix)]
    {
        let (staging, staged) = staging_dir(path)?;
        let bound = UnixListener::bind(&staged).and_then(|listener| {
            set_mode(&staged, 0o600)?;
            std::fs::hard_link(&staged, path)?;
            Ok(listener)
        });
        let _ = std::fs::remove_file(&staged);
        let _ = std::fs::remove_dir(&staging);
        bound
    }
    #[cfg(windows)]
    {
        UnixListener::bind(path)
    }
}

/// A new, empty directory only this user can enter, next to `path`, and
/// the path a socket gets in it. The names are short, so the socket's
/// path is at most a few bytes longer than `path`.
#[cfg(unix)]
fn staging_dir(path: &Path) -> io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let seed = std::process::id()
        ^ std::time::UNIX_EPOCH
            .elapsed()
            .map_or(0, |t| t.subsec_nanos());
    for _ in 0..16 {
        let n = seed.wrapping_add(NEXT.fetch_add(0x9e37_79b9, Ordering::Relaxed));
        let staging = parent.join(format!(".b{n:08x}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&staging) {
            Ok(()) => {
                // The mode asked for is narrowed by the umask, never widened.
                set_mode(&staging, 0o700)?;
                let staged = staging.join("s");
                return Ok((staging, staged));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{}: no free name for a staging directory", parent.display()),
    ))
}

/// Sets `path`'s permission bits on Unix. Does nothing on Windows.
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Starts `command` in a process group of its own, so signals aimed at the
/// parent's terminal miss it and a kill can reach everything it started.
/// Windows has no process groups in that sense; the child just has no
/// console window.
pub fn own_process_group(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
    }
}

/// Ignores hangups from the terminal of the process that started this one.
/// Windows sends none.
pub fn ignore_hangup() {
    #[cfg(unix)]
    {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        let _ = signal_hook::flag::register(
            signal_hook::consts::SIGHUP,
            Arc::new(AtomicBool::new(false)),
        );
    }
}

/// Asks process `pid` to end (SIGTERM).
pub fn terminate(pid: i32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        kill(nix::unistd::Pid::from_raw(pid), Signal::SIGTERM).map_err(io::Error::from)
    }
    #[cfg(windows)]
    {
        let _ = pid;
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// Kills the group that [`own_process_group`] started `pid` in, so nothing
/// it started outlives it. On Windows `taskkill /T` ends its process tree.
pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(pid) {
        use nix::sys::signal::{Signal, killpg};
        let _ = killpg(nix::unistd::Pid::from_raw(pid), Signal::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = cmdterm_env::Environment::Local
            .command("taskkill")
            .args(["/T", "/F", "/PID"])
            .arg(pid.to_string())
            .to_std()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Blocks until child `pid` exits but leaves it to be reaped, so a
/// [`kill_group`] until then never hits a reused pid. Windows does not reuse
/// the pid while the `Child` holds the process handle, so it returns at once.
pub fn wait_without_reaping(pid: u32) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(pid) {
        // rustix rather than nix: nix offers waitid on Linux only.
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        if let Some(pid) = Pid::from_raw(pid) {
            let _ = waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            );
        }
    }
    #[cfg(windows)]
    let _ = pid;
}

/// Exit code of a finished process: its code, or 128 plus the signal that
/// ended it on Unix.
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|signal| 128 + signal)
    };
    #[cfg(windows)]
    let signal = None;
    status.code().or(signal).unwrap_or(255)
}

#[cfg(windows)]
mod windows {
    //! Unix socket types that fail every call. `Never` keeps them from
    //! being constructed, so the I/O methods are unreachable.

    use std::io::{self, Read, Write};
    use std::net::Shutdown;
    use std::path::Path;
    use std::time::Duration;

    #[derive(Debug)]
    enum Never {}

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix sockets are not supported on Windows",
        ))
    }

    #[derive(Debug)]
    pub struct UnixStream(Never);

    impl UnixStream {
        pub fn connect(_path: impl AsRef<Path>) -> io::Result<Self> {
            unsupported()
        }

        pub fn pair() -> io::Result<(Self, Self)> {
            unsupported()
        }

        pub fn try_clone(&self) -> io::Result<Self> {
            match self.0 {}
        }

        pub fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
            match self.0 {}
        }

        pub fn set_read_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            match self.0 {}
        }

        pub fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            match self.0 {}
        }
    }

    impl Read for UnixStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            match self.0 {}
        }
    }

    impl Write for UnixStream {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            match self.0 {}
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.0 {}
        }
    }

    impl Read for &UnixStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            match self.0 {}
        }
    }

    impl Write for &UnixStream {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            match self.0 {}
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.0 {}
        }
    }

    #[derive(Debug)]
    pub struct UnixListener(Never);

    impl UnixListener {
        pub fn bind(_path: impl AsRef<Path>) -> io::Result<Self> {
            unsupported()
        }

        pub fn incoming(&self) -> std::iter::Empty<io::Result<UnixStream>> {
            match self.0 {}
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The socket answers at its path with mode 0600, nothing of the
    /// staging is left, and an existing path is never replaced.
    #[test]
    fn private_sockets_bind_mode_0600_and_replace_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sock");
        let listener = bind_private_socket(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _client = UnixStream::connect(&path).unwrap();
        listener.accept().unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["a.sock"]);

        let taken = dir.path().join("taken");
        std::fs::write(&taken, "mine").unwrap();
        assert!(bind_private_socket(&taken).is_err());
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "mine");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    /// Binding leaves the process umask alone, so directories that other
    /// threads create at the same time keep their usual mode.
    #[test]
    fn binding_leaves_other_threads_directories_alone() {
        let dir = tempfile::tempdir().unwrap();
        let binder = {
            let dir = dir.path().to_path_buf();
            std::thread::spawn(move || {
                for n in 0..200 {
                    let path = dir.join(format!("{n}.sock"));
                    drop(bind_private_socket(&path).unwrap());
                }
            })
        };
        for n in 0..200 {
            let made = dir.path().join(format!("d{n}/inner"));
            std::fs::create_dir_all(&made).unwrap();
        }
        binder.join().unwrap();
    }
}
