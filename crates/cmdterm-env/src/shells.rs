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

use std::borrow::Cow;
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

    /// The full path of `name` where Windows' own search misses it: a bare
    /// name, such as `az` or `claude`, that `PATH` (`path`, else this
    /// process's) holds only as a batch file such as `az.cmd`, which npm and
    /// the Azure CLI install. Windows looks only for `name.exe`; this tries
    /// every extension of `PATHEXT` in each directory, as the shells do. A
    /// name with an extension of `PATHEXT` is looked for as it is. `None` for
    /// a path, and for a program of the Windows directories, which Windows
    /// searches before `PATH`.
    fn path_program(&self, name: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
        let text = name.to_str()?;
        if text.is_empty() || text.contains(['/', '\\', ':']) {
            return None;
        }
        let pathext = (self.var)("PATHEXT")
            .and_then(|value| value.into_string().ok())
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_owned());
        let extensions: Vec<String> = pathext
            .split(';')
            .filter(|ext| ext.starts_with('.') && ext.len() > 1)
            // Lower case, as the files are usually named.
            .map(str::to_ascii_lowercase)
            .collect();
        let has_extension = Path::new(text).extension().is_some_and(|ext| {
            let ext = format!(".{}", ext.to_string_lossy().to_ascii_lowercase());
            extensions.contains(&ext)
        });
        let candidates: Vec<String> = if has_extension {
            vec![text.to_owned()]
        } else {
            let exe = format!("{text}.exe");
            if let Some(root) = (self.var)("SystemRoot").map(PathBuf::from)
                && [root.join("System32"), root.clone()]
                    .iter()
                    .any(|dir| (self.is_file)(&dir.join(&exe)))
            {
                return None;
            }
            extensions
                .iter()
                .map(|ext| format!("{text}{ext}"))
                .collect()
        };
        let dirs: Vec<PathBuf> = match path {
            Some(path) => std::env::split_paths(path).collect(),
            None => self.path_dirs(),
        };
        dirs.iter()
            .flat_map(|dir| candidates.iter().map(move |name| dir.join(name)))
            .find(|candidate| (self.is_file)(candidate))
    }

    /// The `PATH` for `program` when it is a program from Git's `usr\bin`
    /// and `path` (the command's, else this process's) lacks that
    /// directory: Git's `mingw64\bin` and `usr\bin` in front, as Git Bash's
    /// profile puts them. Started from outside Git Bash, its `sh` otherwise
    /// finds no `sed` or `base64`, and System32's `find` in place of
    /// Git's. `None` when nothing needs to change.
    fn msys_path(&self, program: &Path, path: Option<&OsStr>) -> Option<OsString> {
        let usr_bin = program.parent()?;
        let usr = usr_bin.parent()?;
        let is = |dir: &Path, name: &str| {
            dir.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        };
        if !is(usr_bin, "bin") || !is(usr, "usr") || !(self.is_file)(&usr_bin.join("msys-2.0.dll"))
        {
            return None;
        }
        let current = path.map(OsStr::to_os_string).or_else(|| (self.var)("PATH"));
        let dirs: Vec<PathBuf> = current
            .as_deref()
            .map(|path| std::env::split_paths(path).collect())
            .unwrap_or_default();
        let same = |dir: &PathBuf| {
            dir.as_os_str()
                .to_string_lossy()
                .trim_end_matches(['\\', '/'])
                .eq_ignore_ascii_case(&usr_bin.to_string_lossy())
        };
        if dirs.iter().any(same) {
            return None;
        }
        let mingw = usr.parent()?.join("mingw64").join("bin");
        std::env::join_paths([mingw, usr_bin.to_path_buf()].into_iter().chain(dirs)).ok()
    }

    /// Whether `program` runs on the MSYS runtime of Git for Windows, or on
    /// Cygwin's: its folder, or the `usr\bin` beside it as for Git Bash's
    /// `bin\bash.exe`, holds the runtime's DLL. A bare `sh` or `bash` is
    /// the one that comes with Git, see [`local_program`].
    fn is_msys(&self, program: &OsStr) -> bool {
        let path = Path::new(program);
        let name = path.file_stem().and_then(OsStr::to_str).unwrap_or("");
        let path = if path.parent().is_none_or(|dir| dir.as_os_str().is_empty()) {
            if name.eq_ignore_ascii_case("sh") || name.eq_ignore_ascii_case("bash") {
                return true;
            }
            let exe = format!("{name}.exe");
            match self.on_path(&exe).or_else(|| self.on_path(name)) {
                Some(path) => path,
                None => return false,
            }
        } else {
            path.to_path_buf()
        };
        let Some(dir) = path.parent() else {
            return false;
        };
        let beside = dir.parent().map(|root| root.join("usr").join("bin"));
        [Some(dir.to_path_buf()), beside]
            .into_iter()
            .flatten()
            .any(|dir| {
                (self.is_file)(&dir.join("msys-2.0.dll"))
                    || (self.is_file)(&dir.join("cygwin1.dll"))
            })
    }

    /// The shell a terminal in a Windows sandbox starts for `shell`, and
    /// the name of the shell that replaced it, if one did. A program on the
    /// MSYS runtime cannot run there: the runtime keeps a section of shared
    /// memory per user that every MSYS program of that user opens for
    /// writing, which the sandbox may not, and sharing it would let the
    /// sandbox change what programs outside it read. The first native shell
    /// installed takes its place.
    fn sandbox_shell(&self, shell: Vec<OsString>) -> (Vec<OsString>, Option<&'static str>) {
        if !shell.first().is_some_and(|program| self.is_msys(program)) {
            return (shell, None);
        }
        let native = self
            .shells()
            .into_iter()
            .find(|option| matches!(option.id, "pwsh" | "powershell" | "cmd"));
        match native {
            Some(option) => (option.argv, Some(option.name)),
            None => (vec!["cmd.exe".into()], Some("Command Prompt")),
        }
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

/// The program to start for `program` on this machine, with `path` as the
/// `PATH` the command gets when it sets one. Windows has no `sh` of its
/// own, and Git for Windows puts only `git` on `PATH`, so there a bare
/// `sh`, `bash` or `tail` that `PATH` lacks becomes the one that comes with
/// Git. The app's scripts need it, as does every `term.run` of `sh` and the
/// terminal that follows a starting runtime's log with `tail`. Other bare
/// names that `PATH` holds only as a batch file, such as `az.cmd` or the
/// `claude.cmd` of npm, become its full path ([`Probe::path_program`]).
pub(crate) fn local_program<'a>(program: &'a OsStr, path: Option<&OsStr>) -> Cow<'a, OsStr> {
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        static GIT_SH: OnceLock<Option<PathBuf>> = OnceLock::new();
        static GIT_BASH: OnceLock<Option<PathBuf>> = OnceLock::new();
        static GIT_TAIL: OnceLock<Option<PathBuf>> = OnceLock::new();
        let git = match program.to_str() {
            Some("sh") => Some((&GIT_SH, "sh.exe")),
            Some("bash") => Some((&GIT_BASH, "bash.exe")),
            Some("tail") => Some((&GIT_TAIL, "tail.exe")),
            _ => None,
        };
        if let Some((cell, name)) = git
            && let Some(found) = cell.get_or_init(|| machine(|probe| probe.git_unix_tool(name)))
        {
            return Cow::Borrowed(found.as_os_str());
        }
        if let Some(found) = machine(|probe| probe.path_program(program, path)) {
            return Cow::Owned(found.into_os_string());
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    Cow::Borrowed(program)
}

/// The `PATH` a command that starts `program`, as [`local_program`] gave
/// it, needs on this machine, with `path` the `PATH` it sets: see
/// [`Probe::msys_path`]. Always `None` on systems other than Windows.
pub(crate) fn local_msys_path(program: &OsStr, path: Option<&OsStr>) -> Option<OsString> {
    #[cfg(windows)]
    {
        machine(|probe| probe.msys_path(Path::new(program), path))
    }
    #[cfg(not(windows))]
    {
        let _ = (program, path);
        None
    }
}

/// The `PATH` among a command's variables, which Windows names in any case.
pub(crate) fn path_var(vars: &[(OsString, OsString)]) -> Option<&OsStr> {
    vars.iter()
        .rev()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.as_os_str())
}

static DEFAULT_SHELL: RwLock<Option<Vec<OsString>>> = RwLock::new(None);

/// Sets the program and arguments new local terminals start, or `None` for
/// the system's shell. The app passes the shell its settings pick.
pub fn set_default_shell(argv: Option<Vec<OsString>>) {
    let argv = argv.filter(|argv| !argv.is_empty());
    *DEFAULT_SHELL.write().unwrap_or_else(|e| e.into_inner()) = argv;
}

/// The interactive shell of a terminal in a Windows sandbox: [`local_shell`],
/// or a native shell in place of one on the MSYS runtime, which says so
/// once through [`crate::take_sandbox_warnings`].
pub(crate) fn windows_sandbox_shell() -> Vec<OsString> {
    let shell = local_shell();
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static TOLD: AtomicBool = AtomicBool::new(false);
        let program = shell.first().cloned().unwrap_or_default();
        let (shell, replaced) = machine(|probe| probe.sandbox_shell(shell));
        if let Some(name) = replaced
            && !TOLD.swap(true, Ordering::Relaxed)
        {
            crate::seatbelt::warn(crate::SandboxWarning::ShellReplaced {
                shell: Path::new(&program)
                    .file_name()
                    .unwrap_or(&program)
                    .to_string_lossy()
                    .into_owned(),
                with: name,
            });
        }
        sandbox_history(shell)
    }
    #[cfg(not(windows))]
    shell
}

/// `shell` with PowerShell's history kept in the sandbox's own `TEMP`.
/// PSReadLine keeps it under `AppData\Roaming`, which the sandbox may not
/// write, and says so after every command otherwise.
fn sandbox_history(mut shell: Vec<OsString>) -> Vec<OsString> {
    let powershell = shell.first().is_some_and(|program| {
        let stem = Path::new(program).file_stem().unwrap_or_default();
        stem.eq_ignore_ascii_case("pwsh") || stem.eq_ignore_ascii_case("powershell")
    });
    // Other arguments may already run a command or a file.
    if powershell
        && shell[1..]
            .iter()
            .all(|arg| arg.eq_ignore_ascii_case("-NoLogo"))
    {
        shell.extend([
            "-NoExit".into(),
            "-Command".into(),
            "if (Get-Command Set-PSReadLineOption -ErrorAction Ignore) { \
             Set-PSReadLineOption -HistorySavePath (Join-Path $env:TEMP 'PSReadLine_history.txt') }"
                .into(),
        ]);
    }
    shell
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
    fn sandboxes_start_a_native_shell_for_one_on_msys() {
        let git_bash = path(&["/pf", "Git", "bin", "bash.exe"]);
        let msys = path(&["/pf", "Git", "usr", "bin", "msys-2.0.dll"]);
        let usr_bash = path(&["/pf", "Git", "usr", "bin", "bash.exe"]);
        let pwsh = path(&["/pf", "PowerShell", "7", "pwsh.exe"]);
        let cmd = path(&["/win", "System32", "cmd.exe"]);
        let files = [
            git_bash.clone(),
            msys,
            usr_bash.clone(),
            pwsh.clone(),
            cmd.clone(),
        ];
        let vars = [("ProgramFiles", "/pf"), ("SystemRoot", "/win")];
        probe_with(&vars, &files, |p| {
            assert!(p.is_msys(git_bash.as_os_str()));
            assert!(p.is_msys(usr_bash.as_os_str()));
            assert!(p.is_msys("bash".as_ref()));
            assert!(!p.is_msys(pwsh.as_os_str()));
            assert!(!p.is_msys(cmd.as_os_str()));
            let (shell, replaced) = p.sandbox_shell(vec![git_bash.clone().into(), "-i".into()]);
            assert_eq!(shell, [OsString::from(&pwsh), "-NoLogo".into()]);
            assert_eq!(replaced, Some("PowerShell"));
            let native = vec![cmd.clone().into_os_string()];
            assert_eq!(p.sandbox_shell(native.clone()), (native, None));
        });
    }

    #[test]
    fn powershell_in_sandboxes_keeps_history_in_temp() {
        let shell = sandbox_history(vec![
            path(&["C:", "pf", "pwsh.exe"]).into(),
            "-NoLogo".into(),
        ]);
        assert_eq!(shell[2..4], ["-NoExit", "-Command"]);
        assert!(shell[4].to_string_lossy().contains("$env:TEMP"));
        let script = vec!["PowerShell.EXE".into(), "-File".into(), "x.ps1".into()];
        assert_eq!(sandbox_history(script.clone()), script);
        let cmd = vec![OsString::from("cmd.exe")];
        assert_eq!(sandbox_history(cmd.clone()), cmd);
        assert!(sandbox_history(Vec::new()).is_empty());
    }

    #[test]
    fn batch_files_on_path_are_found_by_their_extension() {
        let az = path(&["/tools", "azure", "wbin", "az.cmd"]);
        let claude = path(&["/home", "npm", "claude.cmd"]);
        let claude_sh = path(&["/home", "npm", "claude"]);
        let git = path(&["/tools", "git", "cmd", "git.exe"]);
        let explorer = path(&["/win", "explorer.exe"]);
        let files = [
            az.clone(),
            claude.clone(),
            claude_sh,
            git.clone(),
            explorer.clone(),
        ];
        let dirs = std::env::join_paths([
            path(&["/tools", "azure", "wbin"]),
            path(&["/home", "npm"]),
            path(&["/tools", "git", "cmd"]),
            path(&["/win"]),
        ])
        .unwrap();
        let dirs = dirs.to_str().unwrap();
        let vars = [
            ("PATH", dirs),
            ("PATHEXT", ".COM;.EXE;.BAT;.CMD;.PS1"),
            ("SystemRoot", "/win"),
        ];
        probe_with(&vars, &files, |p| {
            let find = |name: &str| p.path_program(name.as_ref(), None);
            assert_eq!(find("az"), Some(az.clone()));
            assert_eq!(find("az.cmd"), Some(az.clone()));
            // The batch file, not the shell script npm puts beside it.
            assert_eq!(find("claude"), Some(claude.clone()));
            assert_eq!(find("git"), Some(git.clone()));
            // Windows finds these itself.
            assert_eq!(find("explorer"), None);
            assert_eq!(find(az.to_str().unwrap()), None);
            assert_eq!(find("missing"), None);
            // A `PATH` the command sets replaces the app's.
            let only_npm = path(&["/home", "npm"]);
            assert_eq!(
                p.path_program("az".as_ref(), Some(only_npm.as_os_str())),
                None
            );
        });
    }

    #[test]
    fn gits_sh_gets_gits_tools_on_path() {
        let sh = path(&["/git", "usr", "bin", "sh.exe"]);
        let dll = path(&["/git", "usr", "bin", "msys-2.0.dll"]);
        let usr_bin = path(&["/git", "usr", "bin"]);
        let mingw = path(&["/git", "mingw64", "bin"]);
        let win = path(&["/win"]);
        let system = std::env::join_paths([&win]).unwrap();
        let system = system.to_str().unwrap();
        probe_with(&[("PATH", system)], &[sh.clone(), dll], |p| {
            let expected = std::env::join_paths([&mingw, &usr_bin, &win]).unwrap();
            assert_eq!(p.msys_path(&sh, None), Some(expected));
            // A `PATH` with Git's tools stays.
            let has = std::env::join_paths([&usr_bin, &win]).unwrap();
            assert_eq!(p.msys_path(&sh, Some(&has)), None);
            // The command's own `PATH` is the one that grows.
            let own = std::env::join_paths([path(&["/mine"])]).unwrap();
            let grown = std::env::join_paths([&mingw, &usr_bin, &path(&["/mine"])]).unwrap();
            assert_eq!(p.msys_path(&sh, Some(&own)), Some(grown));
            // Other programs and a `usr\bin` without the runtime stay.
            assert_eq!(p.msys_path(&path(&["/win", "cmd.exe"]), None), None);
            assert_eq!(
                p.msys_path(&path(&["/other", "usr", "bin", "sh.exe"]), None),
                None
            );
        });
    }

    #[test]
    fn path_is_found_among_variables_in_any_case() {
        let vars = vec![
            (OsString::from("Path"), OsString::from("a")),
            (OsString::from("HOME"), OsString::from("h")),
        ];
        assert_eq!(path_var(&vars), Some(OsStr::new("a")));
        assert_eq!(path_var(&vars[1..]), None);
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
