//! Pseudo terminals: a master the app reads and writes, and a child that sees
//! a terminal on its standard streams.
//!
//! This is the only place that creates PTYs and spawns their children. On Unix
//! (Linux and macOS) it uses `posix_openpt` and spawns through
//! `std::process::Command` with a `pre_exec` hook that starts a session and
//! makes the PTY its controlling terminal. On Windows it uses ConPTY.
//!
//! The reader and writer block. Callers run them on their own threads.
//! The master closes when the [`Pty`], every reader and every writer are
//! dropped, and the child then sees SIGHUP or end of input.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{Child, ChildKiller, Pty, PtyReader, PtyWriter};
#[cfg(windows)]
pub use windows::{Child, ChildKiller, Pty, PtyReader, PtyWriter};

/// The terminal name a child gets unless the builder sets or removes `TERM`.
pub const DEFAULT_TERM: &str = "xterm-256color";

/// Size of a PTY in cells, with an optional size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl PtySize {
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl Default for PtySize {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

/// A command to run in a PTY: program, arguments, environment and directory.
///
/// The child inherits this process's environment. [`env`](Self::env) and
/// [`env_remove`](Self::env_remove) change it, and [`env_clear`](Self::env_clear)
/// starts from an empty one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandBuilder {
    argv: Vec<OsString>,
    env: Vec<(OsString, Option<OsString>)>,
    clear_env: bool,
    cwd: Option<PathBuf>,
    /// The CPU limit the child starts under, where a job object applies
    /// it (Windows). On Unix the command itself carries it.
    cpu: Option<crate::CpuLimit>,
}

impl CommandBuilder {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            argv: vec![program.as_ref().to_os_string()],
            ..Self::default()
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.argv.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    /// Sets a variable, replacing an earlier setting or removal of it.
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.set_env(key.as_ref(), Some(value.as_ref().to_os_string()));
        self
    }

    /// Removes a variable from the child's environment.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.set_env(key.as_ref(), None);
        self
    }

    /// Starts from an empty environment and forgets earlier changes.
    pub fn env_clear(&mut self) -> &mut Self {
        self.clear_env = true;
        self.env.clear();
        self
    }

    /// The child's working directory. Without one, or when it is not a
    /// directory, the child starts in the home directory (`USERPROFILE` on
    /// Windows), as portable-pty did.
    pub fn cwd(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Starts the child under `cpu` on Windows: in the app's slice job and
    /// a job of its own, which [`crate::cpu::set_scope`] changes later.
    /// Unix ignores it.
    pub fn limit_cpu(&mut self, cpu: Option<crate::CpuLimit>) -> &mut Self {
        self.cpu = cpu;
        self
    }

    pub fn get_cpu(&self) -> Option<&crate::CpuLimit> {
        self.cpu.as_ref()
    }

    /// The program followed by its arguments.
    pub fn get_argv(&self) -> &Vec<OsString> {
        &self.argv
    }

    pub fn get_cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The value set for `key`, if the builder sets one.
    pub fn get_env(&self, key: impl AsRef<OsStr>) -> Option<&OsStr> {
        let key = key.as_ref();
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.as_deref())
    }

    fn set_env(&mut self, key: &OsStr, value: Option<OsString>) {
        match self.env.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.env.push((key.to_os_string(), value)),
        }
    }

    /// Whether the builder decides `TERM` itself.
    fn has_term(&self) -> bool {
        self.env.iter().any(|(k, _)| k == "TERM")
    }
}

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    code: u32,
    signal: Option<i32>,
}

impl ExitStatus {
    pub(crate) fn from_code(code: u32) -> Self {
        Self { code, signal: None }
    }

    #[cfg(unix)]
    pub(crate) fn from_signal(signal: i32) -> Self {
        Self {
            code: 128 + signal as u32,
            signal: Some(signal),
        }
    }

    pub fn success(&self) -> bool {
        self.signal.is_none() && self.code == 0
    }

    /// The exit code, or 128 plus the signal number for a child a signal ended.
    pub fn exit_code(&self) -> u32 {
        self.code
    }

    /// The signal that ended the child. Always `None` on Windows.
    pub fn signal(&self) -> Option<i32> {
        self.signal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_keeps_the_last_env_setting() {
        let mut b = CommandBuilder::new("sh");
        b.arg("-c").arg("true").env("A", "1").env("A", "2");
        b.env("B", "x").env_remove("B");
        assert_eq!(b.get_env("A"), Some(OsStr::new("2")));
        assert_eq!(b.get_env("B"), None);
        assert_eq!(b.get_argv().len(), 3);
        assert!(!b.has_term());
        b.env_remove("TERM");
        assert!(b.has_term());
        b.env_clear();
        assert!(!b.has_term());
    }
}
