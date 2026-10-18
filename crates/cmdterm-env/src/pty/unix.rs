//! The Unix PTY: `posix_openpt` for the master and a `pre_exec` hook that makes
//! the slave the child's controlling terminal.
//!
//! The hook calls `setsid` and `TIOCSCTTY` and nothing else. Both are
//! async-signal-safe and neither allocates. `std::process::Command` already
//! resets the signal mask and SIGPIPE before it runs the hook, and exec resets
//! handlers. A child still inherits ignored signals and open descriptors above 2
//! that are not close-on-exec. This process opens its own descriptors
//! close-on-exec.

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};

use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, WaitOptions, WaitStatus};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcgetpgrp, tcsetwinsize};

use super::{CommandBuilder, DEFAULT_TERM, ExitStatus, PtySize};

fn winsize(size: PtySize) -> Winsize {
    Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.pixel_width,
        ws_ypixel: size.pixel_height,
    }
}

/// The master side of a PTY.
///
/// The slave opens when a child spawns and closes in this process right after,
/// so the master reads end of input once the child and everything it started
/// are gone.
pub struct Pty {
    master: Arc<File>,
}

impl Pty {
    pub fn open(size: PtySize) -> io::Result<Pty> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY;
        let master = openpt(flags)?;
        // Where `posix_openpt` has no close-on-exec flag, a child forked by
        // another thread between these calls can inherit the master.
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
        grantpt(&master)?;
        unlockpt(&master)?;
        tcsetwinsize(&master, winsize(size))?;
        Ok(Pty {
            master: Arc::new(File::from(master)),
        })
    }

    /// Starts `command` with the PTY as its controlling terminal and its
    /// standard streams.
    ///
    /// The child gets `TERM` set to [`DEFAULT_TERM`] unless the builder sets or
    /// removes it. A working directory that is not a directory is replaced by
    /// `$HOME`, so a terminal opens for a worktree that was deleted.
    pub fn spawn(&self, command: &CommandBuilder) -> io::Result<Child> {
        let program = command
            .argv
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
        let name = ptsname(self.master.as_ref(), Vec::new())?;
        let slave: OwnedFd = rustix::fs::open(
            OsStr::from_bytes(name.to_bytes()),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;

        let mut cmd = Command::new(program);
        cmd.args(&command.argv[1..]);
        if command.clear_env {
            cmd.env_clear();
        }
        for (key, value) in &command.env {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            };
        }
        if !command.has_term() {
            cmd.env("TERM", DEFAULT_TERM);
        }
        // Without a usable directory the child starts in the home directory,
        // as a login terminal does, not in the app's own directory.
        match &command.cwd {
            Some(cwd) if cwd.is_dir() => {
                cmd.current_dir(cwd);
            }
            _ => {
                if let Some(home) = std::env::var_os("HOME") {
                    cmd.current_dir(PathBuf::from(home));
                }
            }
        }
        cmd.stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: the hook runs between fork and exec, so it may only call
        // async-signal-safe functions and must not allocate. `setsid` and the
        // `TIOCSCTTY` ioctl on descriptor 0 (the slave, which `Command` has
        // already duplicated onto it) are plain system calls. The hook captures
        // nothing.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setsid()?;
                rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
                Ok(())
            });
        }
        let spawned = cmd.spawn()?;
        let pid = Pid::from_raw(spawned.id() as i32)
            .ok_or_else(|| io::Error::other("child has no process id"))?;
        // The child is reaped by `Child::wait`, not by the std handle.
        drop(spawned);
        Ok(Child {
            shared: Arc::new(Shared {
                pid,
                status: Mutex::new(None),
            }),
        })
    }

    /// A blocking reader for the master. It reads end of input, not an error,
    /// once the child side is gone.
    pub fn reader(&self) -> io::Result<PtyReader> {
        Ok(PtyReader {
            master: self.master.clone(),
        })
    }

    /// A blocking writer for the master: bytes written arrive as typed input.
    pub fn writer(&self) -> io::Result<PtyWriter> {
        Ok(PtyWriter {
            master: self.master.clone(),
        })
    }

    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        Ok(tcsetwinsize(self.master.as_fd(), winsize(size))?)
    }

    /// The foreground process group of the terminal, if there is one.
    pub fn foreground_group(&self) -> Option<i32> {
        tcgetpgrp(self.master.as_fd())
            .ok()
            .map(|pid| pid.as_raw_nonzero().get())
    }
}

pub struct PtyReader {
    master: Arc<File>,
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match (&*self.master).read(buf) {
                // Linux reports EIO when the last slave descriptor closes.
                Err(e) if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => {
                    return Ok(0);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }
}

pub struct PtyWriter {
    master: Arc<File>,
}

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.master).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Shared {
    pid: Pid,
    /// Set once the child is reaped. The killer checks it under the lock, so
    /// it does not signal a process id that may belong to someone else.
    status: Mutex<Option<ExitStatus>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Option<ExitStatus>> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn kill(&self) -> io::Result<()> {
        self.signal(Signal::HUP)
    }

    fn signal(&self, signal: Signal) -> io::Result<()> {
        let status = self.lock();
        if status.is_some() {
            return Ok(());
        }
        match rustix::process::kill_process(self.pid, signal) {
            Err(rustix::io::Errno::SRCH) => Ok(()),
            other => Ok(other?),
        }
    }
}

fn to_status(status: WaitStatus) -> ExitStatus {
    match status.terminating_signal() {
        Some(signal) => ExitStatus::from_signal(signal),
        None => ExitStatus::from_code(status.exit_status().unwrap_or(0) as u32),
    }
}

/// A running child. Dropping it neither kills nor reaps the process.
pub struct Child {
    shared: Arc<Shared>,
}

impl Child {
    pub fn pid(&self) -> u32 {
        self.shared.pid.as_raw_nonzero().get() as u32
    }

    /// Blocks until the child ends.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            // Wait without reaping, so the process id stays taken until
            // `try_wait` reaps it under the lock the killer checks.
            match rustix::process::waitid(
                WaitId::Pid(self.shared.pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            ) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// The status if the child has ended, without blocking.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut stored = self.shared.lock();
        if let Some(status) = *stored {
            return Ok(Some(status));
        }
        match rustix::process::waitpid(Some(self.shared.pid), WaitOptions::NOHANG) {
            Ok(Some((_, status))) => {
                let status = to_status(status);
                *stored = Some(status);
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Sends SIGHUP, as closing a terminal window does, and SIGKILL if the
    /// child is still running a quarter second later.
    pub fn kill(&mut self) -> io::Result<()> {
        self.shared.kill()?;
        for _ in 0..5 {
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        self.shared.signal(Signal::KILL)
    }

    /// A handle other threads can use to end the child.
    pub fn killer(&self) -> ChildKiller {
        ChildKiller {
            shared: self.shared.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ChildKiller {
    shared: Arc<Shared>,
}

impl ChildKiller {
    /// Sends SIGHUP. It does nothing once the child has been reaped.
    pub fn kill(&self) -> io::Result<()> {
        self.shared.kill()
    }
}
