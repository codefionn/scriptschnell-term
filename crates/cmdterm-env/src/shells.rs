//! Shells of this machine, for local terminals.
//!
//! On Windows the app finds Git Bash, PowerShell 7, Windows PowerShell,
//! the Command Prompt and WSL ([`installed_shells`]). The app's settings pick
//! the one new terminals start ([`set_default_shell`]); without a choice it
//! is `%COMSPEC%`. Other systems start `$SHELL` and find no shells here.
//!
//! Git for Windows also brings the `sh` the app's scripts run
//! ([`local_program`]) and the bash Claude Code needs, which every local
//! process learns through `CLAUDE_CODE_GIT_BASH_PATH` ([`git_bash_var`]).

// Finding shells runs on Windows only; tests run it everywhere.
#![cfg_attr(not(windows), allow(dead_code))]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// A shell new terminals can start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellOption {
    /// What settings name it by: `git-bash`, `pwsh`, `powershell`, `cmd`
    /// or `wsl`.
    pub id: &'static str,
    /// Its product name, the same in every language.
    pub name: &'static str,
    /// Program and arguments.
    pub argv: Vec<OsString>,
}

/// The variable Claude Code reads Git Bash's path from on Windows.
pub const GIT_BASH_VAR: &str = "CLAUDE_CODE_GIT_BASH_PATH";

/// What finding shells reads from the machine, so tests can stand in for
/// it.
pub(crate) struct Probe<'a> {
    pub var: &'a dyn Fn(&str) -> Option<OsString>,
    pub is_file: &'a dyn Fn(&Path) -> bool,
}

impl Probe<'_> {
    fn path_dirs(&self) -> Vec<PathBuf> {
        (self.var)("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default()
    }

    fn on_path(&self, name: &str) -> Option<PathBuf> {
        self.path_dirs()
            .into_iter()
            .map(|dir| dir.join(name))
            .find(|path| (self.is_file)(path))
    }

    fn file(&self, base: &str, rest: &[&str]) -> Option<PathBuf> {
        let mut path = PathBuf::from((self.var)(base)?);
        path.extend(rest);
        (self.is_file)(&path).then_some(path)
    }

    /// Where Git for Windows may be installed: above a `git.exe` on `PATH`
    /// (`<Git>\cmd` or `<Git>\bin`), else the default directories for all
    /// users and for one.
    fn git_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self
            .path_dirs()
            .into_iter()
            .filter(|dir| (self.is_file)(&dir.join("git.exe")))
            .filter_map(|dir| dir.parent().map(Path::to_path_buf))
            .collect();
        for (base, rest) in [
            ("ProgramFiles", &["Git"][..]),
            ("ProgramW6432", &["Git"][..]),
            ("LOCALAPPDATA", &["Programs", "Git"][..]),
        ] {
            if let Some(base) = (self.var)(base) {
                let mut root = PathBuf::from(base);
                root.extend(rest);
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        roots
    }

    /// `<Git>\bin\bash.exe`, the bash Git Bash starts. It sets up `PATH`
    /// for the Unix tools that come with Git, which `usr\bin\bash.exe`
    /// leaves to a login shell.
    pub fn git_bash(&self) -> Option<PathBuf> {
        self.git_roots()
            .into_iter()
            .map(|root| root.join("bin").join("bash.exe"))
            .find(|path| (self.is_file)(path))
    }

    /// `name` from Git's `usr\bin`, unless `PATH` has it already.
    fn git_unix_tool(&self, name: &str) -> Option<PathBuf> {
        if self.on_path(name).is_some() {
            return None;
        }
        self.git_roots()
            .into_iter()
            .map(|root| root.join("usr").join("bin").join(name))
            .find(|path| (self.is_file)(path))
    }

    /// The shells installed here, in the order the settings list them.
    pub fn shells(&self) -> Vec<ShellOption> {
        let argv = |path: PathBuf, args: &[&str]| {
            let mut argv = vec![path.into_os_string()];
            argv.extend(args.iter().map(OsString::from));
            argv
        };
        let system = |rest: &[&str]| self.file("SystemRoot", rest);
        let mut out = Vec::new();
        if let Some(bash) = self.git_bash() {
            out.push(ShellOption {
                id: "git-bash",
                name: "Git Bash",
                argv: argv(bash, &["--login", "-i"]),
            });
        }
        let pwsh = (self.on_path("pwsh.exe"))
            .or_else(|| self.file("ProgramFiles", &["PowerShell", "7", "pwsh.exe"]));
        if let Some(pwsh) = pwsh {
            out.push(ShellOption {
                id: "pwsh",
                name: "PowerShell",
                argv: argv(pwsh, &["-NoLogo"]),
            });
        }
        let powershell = ["System32", "WindowsPowerShell", "v1.0", "powershell.exe"];
        if let Some(powershell) = system(&powershell) {
            out.push(ShellOption {
                id: "powershell",
                name: "Windows PowerShell",
                argv: argv(powershell, &["-NoLogo"]),
            });
        }
        let cmd = ((self.var)("COMSPEC").map(PathBuf::from))
            .filter(|path| (self.is_file)(path))
            .or_else(|| system(&["System32", "cmd.exe"]));
        if let Some(cmd) = cmd {
            out.push(ShellOption {
                id: "cmd",
                name: "Command Prompt",
                argv: argv(cmd, &[]),
            });
        }
        if let Some(wsl) = system(&["System32", "wsl.exe"]) {
            out.push(ShellOption {
                id: "wsl",
                name: "WSL",
                argv: argv(wsl, &[]),
            });
        }
        out
    }
}

#[cfg(windows)]
fn machine<T>(read: impl FnOnce(&Probe) -> T) -> T {
    read(&Probe {
        var: &|name| std::env::var_os(name),
        is_file: &|path| path.is_file(),
    })
}

/// The shells installed on this machine, found once. Empty on systems other
/// than Windows.
pub fn installed_shells() -> &'static [ShellOption] {
    #[cfg(windows)]
    {
        static SHELLS: std::sync::OnceLock<Vec<ShellOption>> = std::sync::OnceLock::new();
        SHELLS.get_or_init(|| machine(|probe| probe.shells()))
    }
    #[cfg(not(windows))]
    {
        &[]
    }
}

/// Git Bash's `bash.exe`, found once. Always `None` on systems other than
/// Windows.
pub fn git_bash() -> Option<&'static Path> {
    #[cfg(windows)]
    {
        static BASH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        BASH.get_or_init(|| machine(|probe| probe.git_bash()))
            .as_deref()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// `CLAUDE_CODE_GIT_BASH_PATH` set to Git Bash, for processes of this
/// machine, unless the app has the variable already. `None` where Git Bash
/// is missing and on systems other than Windows.
pub(crate) fn git_bash_var() -> Option<(&'static str, &'static OsStr)> {
    let bash = git_bash()?;
    std::env::var_os(GIT_BASH_VAR)
        .is_none()
        .then_some((GIT_BASH_VAR, bash.as_os_str()))
}

/// The program to start for `program` on this machine. Windows has no `sh`
/// of its own, and Git for Windows puts only `git` on `PATH`, so there a
/// bare `sh` or `bash` that `PATH` lacks becomes the one that comes with
/// Git. The app's scripts need it, as does every `term.run` of `sh`.
pub(crate) fn local_program(program: &OsStr) -> &OsStr {
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        static GIT_SH: OnceLock<Option<PathBuf>> = OnceLock::new();
        static GIT_BASH: OnceLock<Option<PathBuf>> = OnceLock::new();
        let (cell, name) = match program.to_str() {
            Some("sh") => (&GIT_SH, "sh.exe"),
            Some("bash") => (&GIT_BASH, "bash.exe"),
            _ => return program,
        };
        if let Some(found) = cell.get_or_init(|| machine(|probe| probe.git_unix_tool(name))) {
            return found.as_os_str();
        }
    }
    program
}

static DEFAULT_SHELL: RwLock<Option<Vec<OsString>>> = RwLock::new(None);

/// Sets the program and arguments new local terminals start, or `None` for
/// the system's shell. The app passes the shell its settings pick.
pub fn set_default_shell(argv: Option<Vec<OsString>>) {
    let argv = argv.filter(|argv| !argv.is_empty());
    *DEFAULT_SHELL.write().unwrap_or_else(|e| e.into_inner()) = argv;
}

/// This machine's interactive shell: what [`set_default_shell`] set, else
/// `$SHELL` (on Windows only when it is the full path of a file), else
/// `%COMSPEC%` on Windows, else `/bin/sh`.
pub(crate) fn local_shell() -> Vec<OsString> {
    if let Some(argv) = DEFAULT_SHELL
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return argv;
    }
    let fallback = || {
        if cfg!(windows) {
            std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into())
        } else {
            "/bin/sh".into()
        }
    };
    let shell = std::env::var_os("SHELL");
    // Git Bash and MSYS set `SHELL=/usr/bin/bash`, a path only their own
    // programs resolve.
    #[cfg(windows)]
    let shell = shell.filter(|shell| Path::new(shell).is_absolute() && Path::new(shell).is_file());
    vec![shell.unwrap_or_else(fallback)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn probe_with<T>(
        vars: &[(&str, &str)],
        files: &[PathBuf],
        read: impl FnOnce(&Probe) -> T,
    ) -> T {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        let var = |name: &str| vars.get(name).cloned();
        let is_file = |path: &Path| files.iter().any(|file| file == path);
        read(&Probe {
            var: &var,
            is_file: &is_file,
        })
    }

    fn path(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn shells_are_found_in_their_install_directories() {
        let git_bash = path(&["/pf", "Git", "bin", "bash.exe"]);
        let pwsh = path(&["/pf", "PowerShell", "7", "pwsh.exe"]);
        let powershell = path(&[
            "/win",
            "System32",
            "WindowsPowerShell",
            "v1.0",
            "powershell.exe",
        ]);
        let cmd = path(&["/win", "System32", "cmd.exe"]);
        let files = [
            git_bash.clone(),
            pwsh.clone(),
            powershell.clone(),
            cmd.clone(),
        ];
        let shells = probe_with(
            &[("ProgramFiles", "/pf"), ("SystemRoot", "/win")],
            &files,
            |p| p.shells(),
        );
        let found: Vec<_> = shells.iter().map(|s| (s.id, s.argv.clone())).collect();
        assert_eq!(
            found,
            [
                (
                    "git-bash",
                    vec![git_bash.into(), "--login".into(), "-i".into()]
                ),
                ("pwsh", vec![pwsh.into(), "-NoLogo".into()]),
                ("powershell", vec![powershell.into(), "-NoLogo".into()]),
                ("cmd", vec![cmd.into()]),
            ]
        );
    }

    #[test]
    fn git_on_path_leads_to_its_bash_and_sh() {
        let bash = path(&["/tools", "git", "bin", "bash.exe"]);
        let sh = path(&["/tools", "git", "usr", "bin", "sh.exe"]);
        let files = [
            path(&["/tools", "git", "cmd", "git.exe"]),
            bash.clone(),
            sh.clone(),
        ];
        let cmd_dir = path(&["/tools", "git", "cmd"]);
        let vars = [("PATH", cmd_dir.to_str().unwrap())];
        assert_eq!(probe_with(&vars, &files, |p| p.git_bash()), Some(bash));
        assert_eq!(
            probe_with(&vars, &files, |p| p.git_unix_tool("sh.exe")),
            Some(sh)
        );
        // Nothing installed: no shells at all.
        assert!(probe_with(&[], &[], |p| p.shells()).is_empty());
    }
}
