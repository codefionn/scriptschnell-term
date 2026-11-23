//! Where commands run: the local machine, a host reached over SSH, a cmdterm
//! remote machine, a Firecracker VM on this machine, or a target of an
//! environment an extension defines ([`CustomTarget`]). Two wrappers run
//! commands of another environment inside a container ([`ContainerTarget`],
//! used for devcontainers) or a sandbox ([`SandboxTarget`]: bubblewrap,
//! Seatbelt or a restricted Windows token).
//!
//! Every subsystem that touches a project (VCS queries, terminals, extension
//! `run` calls) goes through [`Environment`], so a remote project behaves like
//! a local one. SSH commands share one multiplexed connection per host
//! (`ControlMaster=auto`), so a VCS poll costs a channel open, not a handshake.
//! Remote machine commands run through the `cmdterm-remote exec` client, which
//! shares one authenticated connection per server the same way. Local VM
//! commands go through the same client to the local VM daemon.
//!
//! Terminals on a remote machine or a local VM can run in a session
//! ([`EnvCommand::session`]) that keeps running when the connection drops or
//! the app quits, and that the next terminal with the same name attaches to.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub mod appimage;
pub mod console;
mod container;
mod copy;
pub mod cpu;
mod custom;
mod dirs;
mod engines;
mod host_bridge;
pub mod host_env;
#[cfg(target_os = "linux")]
mod keyring;
mod landlock;
pub mod os_proxy;
pub mod os_secrets;
mod ports;
mod proxy;
pub mod pty;
mod sandbox;
mod seatbelt;
mod shells;
mod trust;
#[cfg(windows)]
mod windows_sandbox;
// The Windows job's UI limits, tested on every platform.
#[cfg(all(test, not(windows)))]
#[path = "windows_sandbox/ui.rs"]
mod windows_sandbox_ui;

pub use container::ContainerTarget;
pub use cpu::{CpuLimit, Share as CpuShare};
pub use custom::{
    CustomProvider, CustomTarget, SCRIPT_PLACEHOLDER, TARGET_PLACEHOLDER, custom_environment,
    set_custom_environments, valid_scheme, valid_target, valid_template,
};
pub use dirs::AppDirs;
pub use engines::{hide_engines, pass_engines};
pub use host_bridge::{
    HostBridge, host_bridge_wanted, install_host_bridge, local_bridge_program, takes_host_bridge,
};
#[cfg(target_os = "linux")]
pub use keyring::{Action as KeyringAction, KeyringBus};
pub use landlock::{BUILT_IN_BRIDGE_FLAG, SCOPE_EXE_VAR, landlock_scopes, scope_exe};
pub use ports::{ListeningPort, PortHub};
#[cfg(unix)]
pub use proxy::proxy_bridge_main;
pub use proxy::{
    AGENT_SETTINGS_GUARD, Approval, Approvals, Blocking, Blocklist, GuardKind, GuardMode,
    GuardRule, KEYRING_GUARD, LinkOpener, OPEN_HOST, OPEN_URL_GUARD, ProxyRules, SandboxProxy,
    SecretRule, builtin_guards, fake_secret, host_allowed, named_fake_secret,
    set_built_in_proxy_bridge, set_proxy_bridge, valid_host_pattern, valid_request_pattern,
};
pub use proxy::{NetworkProxy, PROXY_USER, ProxyToken, Refusal, RefusalLog};
pub use sandbox::{SandboxKind, SandboxLaunch, SandboxPolicy, SandboxTarget, set_sandbox_launcher};
pub use seatbelt::{
    SandboxWarning, sandbox_warnings_pending, take_sandbox_warnings, warm_seatbelt_probe,
};
pub use shells::{
    GIT_BASH_VAR, ShellOption, find_program, git_bash, git_sh, installed_shells, set_default_shell,
};
use shells::{git_bash_var, local_msys_path, local_program, local_shell, path_var};
#[cfg(windows)]
pub use windows_sandbox::launch_sandboxed;

/// A place commands run in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Environment {
    #[default]
    Local,
    Ssh(SshTarget),
    Remote(RemoteTarget),
    /// A Firecracker VM on this machine, run by `cmdterm-remote vmd`.
    Vm(VmTarget),
    /// A target of an environment an extension registered with
    /// `term.environment`, such as a Coder workspace.
    Custom(CustomTarget),
    Container(Box<ContainerTarget>),
    Sandbox(Box<SandboxTarget>),
}

/// A VM of the local VM daemon, by name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VmTarget {
    pub name: String,
}

/// Default TCP port of a cmdterm remote server.
pub const REMOTE_DEFAULT_PORT: u16 = 7722;

/// A cmdterm remote server, reached with the `cmdterm-remote` client
/// (`crates/cmdterm-remote`). Commands run on the server's host, or inside a
/// Firecracker VM on it when `vm` names one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoteTarget {
    /// `host:port` of the server.
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm: Option<String>,
}

impl RemoteTarget {
    /// `host` or `host:port`; a missing port means [`REMOTE_DEFAULT_PORT`].
    pub fn new(address: &str) -> Self {
        // IPv6 hosts need brackets: `[::1]` or `[::1]:7722`.
        let has_port = address.rsplit_once(':').is_some_and(|(host, port)| {
            port.parse::<u16>().is_ok() && (!host.contains(':') || host.ends_with(']'))
        });
        let address = if has_port {
            address.to_owned()
        } else {
            format!("{address}:{REMOTE_DEFAULT_PORT}")
        };
        Self { address, vm: None }
    }
}

/// Arguments of `cmdterm-remote exec` that select a remote or local VM
/// target, or `None` for other environments.
fn client_args(env: &Environment) -> Option<Vec<OsString>> {
    let client = remote_client();
    let mut args = Vec::new();
    if let Some(dir) = &client.config_dir {
        args.push("--config-dir".into());
        args.push(dir.clone().into_os_string());
    }
    args.push("exec".into());
    match env {
        Environment::Remote(target) => {
            args.push("--target".into());
            args.push(target.address.clone().into());
            if let Some(vm) = &target.vm {
                args.push("--vm".into());
                args.push(vm.clone().into());
            }
        }
        Environment::Vm(target) => {
            args.push("--local-vm".into());
            args.push(target.name.clone().into());
        }
        _ => return None,
    }
    Some(args)
}

/// How this process reaches remote machines.
#[derive(Clone, Debug)]
pub struct RemoteClient {
    /// The `cmdterm-remote` executable.
    pub program: PathBuf,
    /// Passed as `--config-dir`, so the client uses the same keys as the app.
    pub config_dir: Option<PathBuf>,
}

static REMOTE_CLIENT: OnceLock<RemoteClient> = OnceLock::new();

/// Sets the remote client for this process. Only the first call counts.
pub fn set_remote_client(client: RemoteClient) {
    let _ = REMOTE_CLIENT.set(client);
}

impl RemoteClient {
    /// A `cmdterm-remote` next to the running executable, otherwise the one
    /// on `PATH`, with its default configuration directory.
    pub fn discover() -> Self {
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|exe| {
                let name = format!("cmdterm-remote{}", std::env::consts::EXE_SUFFIX);
                Some(exe.parent()?.join(name))
            })
            .filter(|path| path.is_file());
        Self {
            program: sibling.unwrap_or_else(|| PathBuf::from("cmdterm-remote")),
            config_dir: None,
        }
    }
}

/// The remote client: what [`set_remote_client`] set, otherwise
/// [`RemoteClient::discover`].
pub fn remote_client() -> &'static RemoteClient {
    REMOTE_CLIENT.get_or_init(RemoteClient::discover)
}

/// `path` made absolute with every link resolved, like
/// [`std::fs::canonicalize`], but without the `\\?\` prefix Windows puts on
/// drive paths there. `cmd.exe` cannot start in such a directory and Git
/// for Windows shows it everywhere, so paths the app stores or hands to
/// programs come from here. Network paths keep the prefix.
pub fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let real = std::fs::canonicalize(path)?;
    if cfg!(windows)
        && let Some(rest) = real.to_str().and_then(|s| s.strip_prefix(r"\\?\"))
        && rest.as_bytes().get(1) == Some(&b':')
    {
        return Ok(PathBuf::from(rest));
    }
    Ok(real)
}

/// An SSH destination. `destination` is anything `ssh` accepts, including a
/// `Host` alias from `~/.ssh/config`, so keys and jump hosts stay in the
/// user's ssh configuration.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SshTarget {
    pub destination: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Extra `ssh` arguments placed before the destination.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// The ssh executable to run. `None` means `ssh`. Useful for wrappers such
    /// as `tsh ssh`, and for tests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
}

impl SshTarget {
    /// The executable to run for this target.
    pub fn program(&self) -> &str {
        self.program.as_deref().unwrap_or("ssh")
    }

    pub fn new(destination: impl Into<String>) -> Self {
        Self {
            destination: destination.into(),
            port: None,
            options: Vec::new(),
            program: None,
        }
    }
}

/// Whether `destination` is safe to give ssh as `[user@]host`: not empty,
/// without whitespace or control characters, and with neither the user
/// nor the host starting with `-`, which ssh or a proxy command it runs
/// could read as an option. Commands also pass `--` before it.
pub fn valid_ssh_destination(destination: &str) -> bool {
    let (user, host) = match destination.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, destination),
    };
    !host.is_empty()
        && !host.starts_with('-')
        && user.is_none_or(|user| !user.is_empty() && !user.starts_with('-'))
        && !destination
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
}

impl Environment {
    pub fn ssh(destination: impl Into<String>) -> Self {
        Self::Ssh(SshTarget::new(destination))
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// Short label for the UI, such as `local` or `dev-box`.
    pub fn label(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Ssh(target) => &target.destination,
            Self::Remote(target) => target.vm.as_deref().unwrap_or(&target.address),
            Self::Vm(target) => &target.name,
            Self::Custom(target) => &target.target,
            Self::Container(target) => &target.container,
            Self::Sandbox(target) => target.host.label(),
        }
    }

    /// The machine-level environment under any container or sandbox.
    pub fn base(&self) -> &Environment {
        match self {
            Self::Container(target) => target.host.base(),
            Self::Sandbox(target) => target.host.base(),
            other => other,
        }
    }

    /// The first `limit` bytes of the file at `path`. Locally this reads the
    /// file directly; elsewhere it runs `head -c`, which GNU and busybox both
    /// have.
    pub fn read_file(&self, path: &Path, limit: u64) -> Result<Vec<u8>, CommandError> {
        if self.is_local() {
            use std::io::Read;
            let mut out = Vec::new();
            std::fs::File::open(path)
                .and_then(|file| file.take(limit).read_to_end(&mut out))
                .map_err(CommandError::Spawn)?;
            return Ok(out);
        }
        self.command("head")
            .arg("-c")
            .arg(limit.to_string())
            .arg("--")
            .arg(path)
            .stdout()
    }

    /// Starts building a command that runs `program` in this environment.
    pub fn command(&self, program: impl Into<OsString>) -> EnvCommand {
        EnvCommand {
            env: self.clone(),
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            vars: Vec::new(),
            sockets: Vec::new(),
            session: None,
            cpu: None,
        }
    }

    /// Whether terminals here can run in sessions that outlive the
    /// connection and the app: on a remote machine or a local VM, also
    /// inside a container or sandbox there.
    pub fn keeps_sessions(&self) -> bool {
        matches!(self.base(), Self::Remote(_) | Self::Vm(_))
    }

    /// Ends the session `name` that a terminal of this environment ran in.
    /// Blocking; does nothing where [`Self::keeps_sessions`] is false.
    pub fn end_session(&self, name: &str) -> Result<(), CommandError> {
        let base = self.base();
        let Some(args) = client_args(base) else {
            return Ok(());
        };
        let output = new_command(&remote_client().program)
            .args(args)
            .args(["--session", name, "--end"])
            .stdin(Stdio::null())
            .output()
            .map_err(CommandError::Spawn)?;
        // 1 means the session had already ended.
        match output.status.code() {
            Some(0 | 1) => Ok(()),
            code => Err(CommandError::Failed {
                code,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
        }
    }

    /// A PTY command for an interactive shell in `cwd`. Locally this is the
    /// user's `$SHELL`; over SSH it is the remote login shell.
    pub fn shell(&self, cwd: Option<&Path>) -> pty::CommandBuilder {
        self.shell_with(cwd, &[], &[])
    }

    /// [`Self::shell`] with variables set in the environment itself, also
    /// on another machine, and Unix sockets of this machine forwarded as
    /// [`EnvCommand::forward_socket`] does.
    pub fn shell_with(
        &self,
        cwd: Option<&Path>,
        vars: &[(OsString, OsString)],
        sockets: &[(PathBuf, PathBuf)],
    ) -> pty::CommandBuilder {
        self.shell_in_session(cwd, vars, sockets, None)
    }

    /// [`Self::shell_with`] in the session `session` where
    /// [`Self::keeps_sessions`]; elsewhere the name is ignored.
    pub fn shell_in_session(
        &self,
        cwd: Option<&Path>,
        vars: &[(OsString, OsString)],
        sockets: &[(PathBuf, PathBuf)],
        session: Option<&str>,
    ) -> pty::CommandBuilder {
        self.limited_shell(cwd, vars, sockets, session, None)
    }

    /// [`Self::shell_in_session`] limited by `cpu`, see [`cpu`]. Where
    /// limits do not apply, the shell starts unlimited.
    pub fn limited_shell(
        &self,
        cwd: Option<&Path>,
        vars: &[(OsString, OsString)],
        sockets: &[(PathBuf, PathBuf)],
        session: Option<&str>,
        cpu: Option<&CpuLimit>,
    ) -> pty::CommandBuilder {
        let cpu = cpu.filter(|_| cpu::applies(self));
        match self {
            Self::Local => {
                let shell = local_shell();
                let mut builder = match cpu.filter(|_| !cpu::by_job(self)) {
                    Some(cpu) => {
                        let mut builder = pty::CommandBuilder::new("sh");
                        builder.args(cpu.sh_args());
                        builder.args(&shell);
                        builder
                    }
                    None => {
                        let mut builder = pty::CommandBuilder::new(&shell[0]);
                        builder.args(&shell[1..]);
                        builder.limit_cpu(cpu.cloned());
                        builder
                    }
                };
                if let Some(cwd) = cwd {
                    builder.cwd(cwd);
                }
                builder.env("TERM", "xterm-256color");
                if let Some((key, value)) = git_bash_var() {
                    builder.env(key, value);
                }
                for (key, value) in vars {
                    builder.env(key, value);
                }
                builder
            }
            Self::Ssh(target) => {
                let mut builder = pty::CommandBuilder::new(target.program());
                for arg in ssh_base_args(target) {
                    builder.arg(arg);
                }
                for arg in ssh_forward_args(sockets) {
                    builder.arg(arg);
                }
                builder.arg("-t");
                // `--` keeps a destination starting with `-` from being read
                // as an option.
                builder.arg("--");
                builder.arg(&target.destination);
                let mut remote = OsString::from(clean_up_sockets(sockets));
                if let Some(cwd) = cwd {
                    remote.push("cd ");
                    push_quoted(&mut remote, cwd.as_os_str());
                    remote.push(" && ");
                }
                push_vars(&mut remote, vars);
                remote.push(exec_or_clean_up(sockets));
                if let Some(cpu) = cpu {
                    remote.push(cpu.quoted());
                }
                remote.push("\"${SHELL:-/bin/sh}\" -l");
                builder.arg(remote);
                builder.env("TERM", "xterm-256color");
                builder
            }
            // The server applies the limit to the shell it starts, also on
            // Windows.
            Self::Remote(_) | Self::Vm(_) => {
                let mut builder = pty::CommandBuilder::new(&remote_client().program);
                builder.args(client_args(self).unwrap_or_default());
                builder.args(["--pty", "--shell"]);
                if let Some(session) = session {
                    builder.args(["--session", session]);
                }
                if let Some(cpu) = cpu {
                    builder.arg("--cpu");
                    builder.arg(cpu.to_spec());
                }
                if let Some(cwd) = cwd {
                    builder.arg("--cwd");
                    builder.arg(cwd);
                }
                builder.args(client_var_args(vars, sockets));
                builder.env("TERM", "xterm-256color");
                builder
            }
            Self::Custom(target) => {
                let mut remote = OsString::new();
                if let Some(cwd) = cwd {
                    remote.push("cd ");
                    push_quoted(&mut remote, cwd.as_os_str());
                    remote.push(" && ");
                }
                push_vars(&mut remote, vars);
                if let Some(cpu) = cpu {
                    remote.push("exec ");
                    remote.push(cpu.quoted());
                }
                remote.push("\"${SHELL:-/bin/sh}\" -l");
                let (program, args) = target.argv_or_missing(remote, true);
                let mut builder = pty::CommandBuilder::new(program);
                builder.args(args);
                builder.env("TERM", "xterm-256color");
                builder
            }
            // Windows has no `sh` to find the login shell with.
            Self::Sandbox(target) if target.policy.kind == SandboxKind::Windows => {
                let shell = shells::windows_sandbox_shell();
                let mut command = self.command(&shell[0]).args(&shell[1..]);
                if let Some(cwd) = cwd {
                    command = command.cwd(cwd);
                }
                command.vars.extend(vars.iter().cloned());
                command.session = session.map(str::to_owned);
                command.cpu = cpu.cloned();
                command.to_pty()
            }
            Self::Container(_) | Self::Sandbox(_) => {
                // The login shell of the user inside, found at run time.
                let mut command = self.command("sh").args(["-c", LOGIN_SHELL]);
                if let Some(cwd) = cwd {
                    command = command.cwd(cwd);
                }
                command.vars.extend(vars.iter().cloned());
                command.sockets.extend(sockets.iter().cloned());
                command.session = session.map(str::to_owned);
                command.cpu = cpu.cloned();
                command.to_pty()
            }
        }
    }

    /// Where commands in this environment reach the Unix socket `local` of
    /// this machine when they forward it with [`EnvCommand::forward_socket`],
    /// `None` when they cannot. Locally and in a sandbox here it is `local`
    /// itself. On an SSH host, a remote machine or a VM it is
    /// `/tmp/<name>.sock` there: ssh listens on it (`-R`), or the server of
    /// `cmdterm-remote` does. Extensions' environments cannot reach it. A container on this machine reaches it when
    /// one of its mounts holds `local`, at the mounted path; sandboxes of
    /// the Windows launcher cannot reach it. `name` should be unique per
    /// command, since each forward listens on its own path.
    pub fn forwarded_socket(&self, local: &Path, name: &str) -> Option<PathBuf> {
        match self {
            Self::Local => Some(local.to_path_buf()),
            Self::Ssh(_) | Self::Remote(_) | Self::Vm(_) => {
                Some(PathBuf::from(format!("/tmp/{name}.sock")))
            }
            Self::Container(target) if target.host.is_local() => {
                let inside = target.container_path(local);
                (inside != local).then_some(inside)
            }
            Self::Container(_) | Self::Custom(_) => None,
            Self::Sandbox(target) if target.policy.kind == SandboxKind::Windows => None,
            Self::Sandbox(target) => target.host.forwarded_socket(local, name),
        }
    }

    /// Whether an SSH host lets this machine forward a Unix socket to it
    /// (`AllowStreamLocalForwarding`), checked with a throwaway forward over
    /// the shared connection. `None` when the check could not tell, such as
    /// for an unreachable host. Other environments answer `Some(true)`.
    /// Blocking.
    pub fn can_forward_socket(&self, local: &Path, probe: &Path) -> Option<bool> {
        let Self::Ssh(target) = self.base() else {
            return Some(true);
        };
        let output = new_command(target.program())
            .args(ssh_base_args(target))
            .args(ssh_forward_args(&[(
                local.to_path_buf(),
                probe.to_path_buf(),
            )]))
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-T",
                "--",
            ])
            .arg(&target.destination)
            .arg(format!("rm -f -- {}", quote(&probe.to_string_lossy())))
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if output.status.success() {
            return Some(true);
        }
        String::from_utf8_lossy(&output.stderr)
            .contains("forwarding failed")
            .then_some(false)
    }
}

/// A POSIX script that execs the current user's login shell: the passwd
/// entry, else `$SHELL`, else bash, else sh.
const LOGIN_SHELL: &str = "s=$(getent passwd \"$(id -un)\" 2>/dev/null | cut -d: -f7); \
[ -x \"$s\" ] || s=$SHELL; [ -x \"$s\" ] || s=$(command -v bash) || s=/bin/sh; \
exec \"$s\" -l";

/// A command bound to an [`Environment`].
#[derive(Clone, Debug)]
pub struct EnvCommand {
    env: Environment,
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    vars: Vec<(OsString, OsString)>,
    /// Unix sockets of this machine, as `(local, inside)`, see
    /// [`Self::forward_socket`].
    sockets: Vec<(PathBuf, PathBuf)>,
    /// See [`Self::session`].
    session: Option<String>,
    /// See [`Self::limit_cpu`].
    cpu: Option<CpuLimit>,
}

impl EnvCommand {
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Working directory, interpreted on the target machine.
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn var(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.vars.push((key.into(), value.into()));
        self
    }

    /// Lets the command connect to the Unix socket `local` of this machine
    /// at `inside`, the path [`Environment::forwarded_socket`] gave. The
    /// forward lasts as long as the command, and as long as the shared SSH
    /// connection over SSH. Environments that cannot forward ignore it.
    pub fn forward_socket(mut self, local: impl Into<PathBuf>, inside: impl Into<PathBuf>) -> Self {
        self.sockets.push((local.into(), inside.into()));
        self
    }

    /// Runs the PTY command in the session `name`, which keeps running
    /// when the connection drops or this process exits; the next command
    /// with the same name attaches to it, and gets the output it missed.
    /// Only where [`Environment::keeps_sessions`]; elsewhere, and for
    /// commands without a PTY, the name is ignored. Pick a name no other
    /// terminal uses, and end it with [`Environment::end_session`].
    pub fn session(mut self, name: impl Into<String>) -> Self {
        self.session = Some(name.into());
        self
    }

    /// Starts the command limited by `cpu`, see [`cpu`], on the machine it
    /// runs on. A sandbox's whole process tree is limited from outside; a
    /// devcontainer's processes run under its engine, so the limit applies
    /// inside, without systemd there. Windows ignores it.
    pub fn limit_cpu(mut self, cpu: Option<CpuLimit>) -> Self {
        self.cpu = cpu;
        self
    }

    /// The command as `sh` running [`cpu::SCRIPT`] with this command after
    /// it, when it has a limit that applies; else itself.
    fn cpu_applied(&self) -> Option<EnvCommand> {
        // Windows takes the limit through the PTY's job, a remote machine
        // through its server.
        let cpu = self.cpu.as_ref().filter(|_| {
            cpu::applies(&self.env) && !cpu::by_job(&self.env) && !cpu::by_server(&self.env)
        })?;
        let mut args: Vec<OsString> = cpu.sh_args().into();
        args.push(self.program.clone());
        args.extend(self.args.iter().cloned());
        Some(EnvCommand {
            program: "sh".into(),
            args,
            cpu: None,
            ..self.clone()
        })
    }

    /// The same command, expressed in the wrapped environment, for
    /// containers and sandboxes.
    fn lowered(&self, pty: bool) -> Option<EnvCommand> {
        let mut lowered = match &self.env {
            Environment::Container(target) => match self.cpu_applied() {
                Some(applied) => target.lower(&applied, pty),
                None => target.lower(self, pty),
            },
            // The limit applies at the base, outside the sandbox. A Windows
            // sandbox's launcher takes the cap itself, as its job cannot
            // join the slice.
            Environment::Sandbox(target) => {
                let mut lowered = target.lower(self, pty);
                lowered.cpu = self.cpu.clone().filter(|_| {
                    cpu::applies(&self.env) && target.policy.kind != SandboxKind::Windows
                });
                lowered
            }
            _ => return None,
        };
        lowered.session = self.session.clone();
        Some(lowered)
    }

    /// The process that will actually be spawned on this machine.
    pub fn to_std(&self) -> Command {
        if let Some(lowered) = self.lowered(false) {
            return lowered.to_std();
        }
        if let Some(applied) = self.cpu_applied() {
            return applied.to_std();
        }
        match &self.env {
            Environment::Local => {
                let program = local_program(&self.program, path_var(&self.vars));
                let msys_path = local_msys_path(&program, path_var(&self.vars));
                let mut command = new_command(program);
                add_local_args(&mut command, &self.args);
                if let Some(cwd) = &self.cwd {
                    command.current_dir(cwd);
                }
                if appimage::strips_runtime_vars() {
                    for var in appimage::RUNTIME_VARS {
                        command.env_remove(var);
                    }
                }
                if let Some((key, value)) = git_bash_var() {
                    command.env(key, value);
                }
                command.envs(self.vars.iter().map(|(k, v)| (k, v)));
                if let Some(path) = msys_path {
                    command.env("PATH", path);
                }
                command
            }
            Environment::Ssh(target) => {
                let mut command = new_command(target.program());
                command.args(ssh_base_args(target));
                command.args(ssh_forward_args(&self.sockets));
                // Fail instead of prompting: background polls have no TTY.
                command.args(["-o", "BatchMode=yes", "-T", "--"]);
                command.arg(&target.destination);
                command.arg(self.remote_script());
                command
            }
            Environment::Remote(_) | Environment::Vm(_) => {
                let mut command = new_command(&remote_client().program);
                command.args(self.remote_client_args(false));
                command
            }
            Environment::Custom(target) => {
                let (program, args) = target.argv_or_missing(self.remote_script(), false);
                let mut command = new_command(program);
                command.args(args);
                command
            }
            Environment::Container(_) | Environment::Sandbox(_) => {
                unreachable!("lowered above")
            }
        }
    }
}

/// A process of this machine that opens no console window of its own,
/// see [`console::no_window`].
fn new_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    console::no_window(&mut command);
    command
}

/// Adds `args` to a local `command`. On Windows the standard library quotes
/// an argument only when it holds a space or tab, so one such as `a\nb` would
/// reach the program unquoted, and the MSYS runtime of Git's `sh` splits it
/// at the line break. Those arguments are quoted here instead.
fn add_local_args(command: &mut Command, args: &[OsString]) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        for arg in args {
            if arg
                .as_encoded_bytes()
                .iter()
                .any(|b| matches!(b, b'\n' | b'\r'))
            {
                command.raw_arg(windows_sandbox::quoted_arg(arg));
            } else {
                command.arg(arg);
            }
        }
    }
    #[cfg(not(windows))]
    command.args(args);
}

impl EnvCommand {
    /// Runs to completion and captures stdout and stderr. Stdin is closed.
    pub fn output(&self) -> io::Result<Output> {
        self.to_std().stdin(Stdio::null()).output()
    }

    /// [`Self::output`], but kills the command when it has not exited
    /// within `timeout` and fails with [`io::ErrorKind::TimedOut`]. For
    /// commands that cross a network that may be gone, such as a laptop's
    /// after it woke up somewhere else.
    pub fn output_within(&self, timeout: Duration) -> io::Result<Output> {
        let mut child = self
            .to_std()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let deadline = Instant::now() + timeout;
        // Read from threads: a command that prints more than a pipe holds
        // would otherwise never exit.
        let read = |pipe: Option<Box<dyn io::Read + Send>>| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_end(&mut bytes);
                }
                bytes
            })
        };
        let stdout = read(child.stdout.take().map(|p| Box::new(p) as _));
        let stderr = read(child.stderr.take().map(|p| Box::new(p) as _));
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no answer within {} seconds", timeout.as_secs()),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        Ok(Output {
            status,
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        })
    }

    /// Runs to completion and returns stdout when the exit status is zero.
    pub fn stdout(&self) -> Result<Vec<u8>, CommandError> {
        let output = self.output().map_err(CommandError::Spawn)?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(CommandError::Failed {
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }

    /// [`Self::stdout`] with `input` written to the command's stdin.
    pub fn stdout_with_input(&self, input: &[u8]) -> Result<Vec<u8>, CommandError> {
        let mut child = self
            .to_std()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(CommandError::Spawn)?;
        // Write from a thread: a command that prints before it reads all
        // of its input would otherwise wait on a full pipe.
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let output = std::thread::scope(|scope| {
            scope.spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(input);
            });
            child.wait_with_output()
        })
        .map_err(CommandError::Spawn)?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(CommandError::Failed {
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }

    /// A PTY command for this invocation, for commands run inside a terminal.
    pub fn to_pty(&self) -> pty::CommandBuilder {
        if let Some(lowered) = self.lowered(true) {
            return lowered.to_pty();
        }
        if let Some(applied) = self.cpu_applied() {
            return applied.to_pty();
        }
        match &self.env {
            Environment::Local => {
                let program = local_program(&self.program, path_var(&self.vars));
                let msys_path = local_msys_path(&program, path_var(&self.vars));
                let mut builder = pty::CommandBuilder::new(program);
                builder.args(&self.args);
                builder.limit_cpu(self.cpu.clone().filter(|_| cpu::by_job(&self.env)));
                if let Some(cwd) = &self.cwd {
                    builder.cwd(cwd);
                }
                if appimage::strips_runtime_vars() {
                    for var in appimage::RUNTIME_VARS {
                        builder.env_remove(var);
                    }
                }
                if let Some((key, value)) = git_bash_var() {
                    builder.env(key, value);
                }
                for (key, value) in &self.vars {
                    builder.env(key, value);
                }
                if let Some(path) = msys_path {
                    builder.env("PATH", path);
                }
                builder.env("TERM", "xterm-256color");
                builder
            }
            Environment::Ssh(target) => {
                let mut builder = pty::CommandBuilder::new(target.program());
                for arg in ssh_base_args(target) {
                    builder.arg(arg);
                }
                for arg in ssh_forward_args(&self.sockets) {
                    builder.arg(arg);
                }
                builder.arg("-t");
                builder.arg("--");
                builder.arg(&target.destination);
                builder.arg(self.remote_script());
                builder.env("TERM", "xterm-256color");
                builder
            }
            Environment::Remote(_) | Environment::Vm(_) => {
                let mut builder = pty::CommandBuilder::new(&remote_client().program);
                builder.args(self.remote_client_args(true));
                builder.env("TERM", "xterm-256color");
                builder
            }
            Environment::Custom(target) => {
                let (program, args) = target.argv_or_missing(self.remote_script(), true);
                let mut builder = pty::CommandBuilder::new(program);
                builder.args(args);
                builder.env("TERM", "xterm-256color");
                builder
            }
            Environment::Container(_) | Environment::Sandbox(_) => {
                unreachable!("lowered above")
            }
        }
    }

    /// `cmdterm-remote` arguments that run this command on `target`.
    fn remote_client_args(&self, pty: bool) -> Vec<OsString> {
        let mut args = client_args(&self.env).unwrap_or_default();
        if pty {
            args.push("--pty".into());
            if let Some(session) = &self.session {
                args.push("--session".into());
                args.push(session.into());
            }
            if let Some(cpu) = &self.cpu {
                args.push("--cpu".into());
                args.push(cpu.to_spec().into());
            }
        }
        if let Some(cwd) = &self.cwd {
            args.push("--cwd".into());
            args.push(cwd.clone().into_os_string());
        }
        args.extend(client_var_args(&self.vars, &self.sockets));
        args.push("--".into());
        args.push(self.program.clone());
        args.extend(self.args.iter().cloned());
        args
    }

    /// The single shell string sent to the remote `sh`.
    fn remote_script(&self) -> OsString {
        let mut script = OsString::from(clean_up_sockets(&self.sockets));
        if let Some(cwd) = &self.cwd {
            script.push("cd ");
            push_quoted(&mut script, cwd.as_os_str());
            script.push(" && ");
        }
        push_vars(&mut script, &self.vars);
        script.push(exec_or_clean_up(&self.sockets));
        push_quoted(&mut script, &self.program);
        for arg in &self.args {
            script.push(" ");
            push_quoted(&mut script, arg);
        }
        script
    }
}

/// A command that could not start or exited unsuccessfully.
#[derive(Debug)]
pub enum CommandError {
    Spawn(io::Error),
    Failed { code: Option<i32>, stderr: String },
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "could not start command: {error}"),
            Self::Failed { code, stderr } => match code {
                Some(code) => write!(f, "command exited with {code}: {stderr}"),
                None => write!(f, "command killed by signal: {stderr}"),
            },
        }
    }
}

impl std::error::Error for CommandError {}

fn quote(value: &str) -> String {
    shell_words::quote(value).into_owned()
}

/// How a remote script starts its command: `exec`, or with forwarded
/// sockets a plain call, so the `trap` that [`clean_up_sockets`] sets
/// removes them when it ends. sshd leaves the socket files behind.
fn exec_or_clean_up(sockets: &[(PathBuf, PathBuf)]) -> &'static str {
    if sockets.is_empty() { "exec " } else { "" }
}

/// `trap 'rm -f -- PATHS' EXIT HUP INT TERM; ` for the remote ends of
/// `sockets`, empty without any.
fn clean_up_sockets(sockets: &[(PathBuf, PathBuf)]) -> String {
    if sockets.is_empty() {
        return String::new();
    }
    let mut remove = String::from("rm -f --");
    for (_, inside) in sockets {
        remove.push(' ');
        remove.push_str(&quote(&inside.to_string_lossy()));
    }
    format!("trap {} EXIT HUP INT TERM; ", quote(&remove))
}

/// Appends `value` quoted for a POSIX shell. On Unix, bytes that are not
/// UTF-8 go through unchanged inside single quotes, so a file name in
/// another encoding reaches the far side as it is. Elsewhere they become
/// U+FFFD.
fn push_quoted(script: &mut OsString, value: &std::ffi::OsStr) {
    if let Some(text) = value.to_str() {
        script.push(quote(text));
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let mut bytes = std::mem::take(script).into_vec();
        bytes.push(b'\'');
        for &byte in value.as_bytes() {
            if byte == b'\'' {
                bytes.extend_from_slice(b"'\\''");
            } else {
                bytes.push(byte);
            }
        }
        bytes.push(b'\'');
        *script = OsString::from_vec(bytes);
    }
    #[cfg(not(unix))]
    script.push(quote(&value.to_string_lossy()));
}

/// Whether `name` can be assigned in a POSIX shell: `[A-Za-z_][A-Za-z0-9_]*`.
fn shell_identifier(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `KEY='value' ` for each variable, in front of a remote `exec`. A name
/// that is not a shell identifier would change the script, so it is left
/// out with a warning.
fn push_vars(script: &mut OsString, vars: &[(OsString, OsString)]) {
    for (key, value) in vars {
        if !shell_identifier(key) {
            eprintln!(
                "scriptschnell-term: not a variable name, left out: {}",
                quote(&key.to_string_lossy())
            );
            continue;
        }
        script.push(key);
        script.push("=");
        push_quoted(script, value);
        script.push(" ");
    }
}

/// `--env KEY=VALUE` and `--forward INSIDE=LOCAL` for `cmdterm-remote exec`.
fn client_var_args(vars: &[(OsString, OsString)], sockets: &[(PathBuf, PathBuf)]) -> Vec<OsString> {
    let mut args = Vec::new();
    for (key, value) in vars {
        let mut pair = key.clone();
        pair.push("=");
        pair.push(value);
        args.push("--env".into());
        args.push(pair);
    }
    for (local, inside) in sockets {
        let mut pair = inside.clone().into_os_string();
        pair.push("=");
        pair.push(local);
        args.push("--forward".into());
        args.push(pair);
    }
    args
}

/// `-R INSIDE:LOCAL` for each socket. sshd creates the remote socket
/// readable only by the user (`StreamLocalBindMask`).
fn ssh_forward_args(sockets: &[(PathBuf, PathBuf)]) -> Vec<OsString> {
    let mut args = Vec::new();
    for (local, inside) in sockets {
        let mut pair = inside.clone().into_os_string();
        pair.push(":");
        pair.push(local);
        args.push("-R".into());
        args.push(pair);
    }
    args
}

/// Arguments shared by every ssh invocation to `target`: connection sharing
/// plus the user's port and options.
/// Without a private control directory, connections are not shared.
fn ssh_base_args(target: &SshTarget) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(dir) = control_dir() {
        args.extend([
            "-o".to_owned(),
            "ControlMaster=auto".to_owned(),
            "-o".to_owned(),
            format!("ControlPath={}", dir.join("%C").display()),
            "-o".to_owned(),
            "ControlPersist=10m".to_owned(),
        ]);
    }
    if let Some(port) = target.port {
        args.push("-p".to_owned());
        args.push(port.to_string());
    }
    args.extend(target.options.iter().cloned());
    args
}

/// The longest control directory whose sockets still fit a Unix socket
/// path on macOS, which allows 104 bytes with the closing NUL: `%C`
/// expands to 40 hex digits, and ssh first binds the path with a dot and
/// 16 random characters after it.
#[cfg(unix)]
const CONTROL_DIR_MAX: usize = 104 - 1 - (1 + 40 + 1 + 16);

/// Directory for ssh control sockets: `cmdterm-ssh` in the runtime
/// directory, else `/tmp/cmdterm-ssh-<uid>`, which stays short where the
/// temporary directory is long, as on macOS, else `~/.ssh/cmdterm`. The
/// last one is for when another user made the `/tmp` one first: nobody
/// else can make it. `None` when none is a private directory of this
/// user's that fits [`CONTROL_DIR_MAX`].
#[cfg(unix)]
fn control_dir() -> Option<PathBuf> {
    let uid = rustix::process::geteuid().as_raw();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join("cmdterm-ssh"));
    let fallback = PathBuf::from(format!("/tmp/cmdterm-ssh-{uid}"));
    let home = std::env::var_os("HOME")
        .filter(|dir| !dir.is_empty())
        .and_then(|home| home_control_dir(Path::new(&home), uid));
    pick_control_dir(runtime.into_iter().chain([fallback]).chain(home), uid)
}

/// `~/.ssh/cmdterm` when `~/.ssh` is a directory of `uid`'s that nobody
/// else may write to, so no other user can make the directory in it.
/// `~/.ssh` itself is never created.
#[cfg(unix)]
fn home_control_dir(home: &Path, uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let ssh = home.join(".ssh");
    let meta = std::fs::symlink_metadata(&ssh).ok()?;
    (meta.file_type().is_dir() && meta.uid() == uid && meta.mode() & 0o022 == 0)
        .then(|| ssh.join("cmdterm"))
}

/// The first of `candidates` that fits [`CONTROL_DIR_MAX`] and is, or
/// could be made, a private directory of `uid`'s.
#[cfg(unix)]
fn pick_control_dir(candidates: impl IntoIterator<Item = PathBuf>, uid: u32) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find(|dir| dir.as_os_str().len() <= CONTROL_DIR_MAX && private_dir(dir, uid).is_ok())
}

/// Creates `dir` with mode 0700, or checks that it is a directory `uid`
/// owns, not a symlink, and takes access away from others if it has to.
/// Another user could have made it first in the shared `/tmp`.
#[cfg(unix)]
fn private_dir(dir: &Path, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() || meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not this user's own directory", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Directory for ssh control sockets. Created on first use.
#[cfg(not(unix))]
fn control_dir() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cmdterm-ssh");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_script_quotes_cwd_vars_and_args() {
        let command = Environment::ssh("box")
            .command("git")
            .args(["log", "--format=%H %s"])
            .cwd("/srv/my repo")
            .var("GIT_PAGER", "cat");
        assert_eq!(
            command.remote_script(),
            "cd '/srv/my repo' && GIT_PAGER=cat exec git log '--format=%H %s'"
        );
    }

    #[test]
    fn shells_carry_variables_and_forwards_to_every_machine() {
        let vars = [("SCRIPTSCHNELL_TERM".into(), "1".into())];
        let local = PathBuf::from("/run/app.sock");
        let argv = |env: &Environment| {
            let inside = env.forwarded_socket(&local, "s1").unwrap();
            let sockets = [(local.clone(), inside)];
            let builder = env.shell_with(Some(Path::new("/srv/a")), &vars, &sockets);
            (builder.get_argv().iter())
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let ssh = argv(&Environment::ssh("box"));
        let at = ssh.iter().position(|a| a == "-R").unwrap();
        assert_eq!(ssh[at + 1], "/tmp/s1.sock:/run/app.sock");
        assert_eq!(
            ssh.last().unwrap(),
            "trap 'rm -f -- /tmp/s1.sock' EXIT HUP INT TERM; \
             cd /srv/a && SCRIPTSCHNELL_TERM=1 \"${SHELL:-/bin/sh}\" -l"
        );
        let remote = argv(&Environment::Remote(RemoteTarget::new("box")));
        let tail = [
            "--env",
            "SCRIPTSCHNELL_TERM=1",
            "--forward",
            "/tmp/s1.sock=/run/app.sock",
        ];
        assert_eq!(remote[remote.len() - tail.len()..], tail);
        let container = Environment::Container(Box::new(ContainerTarget::new(
            Environment::Local,
            "podman",
            "dev",
        )));
        assert_eq!(container.forwarded_socket(&local, "s1"), None);
        let Environment::Container(mut target) = container else {
            unreachable!()
        };
        target
            .mounts
            .push(("/run".into(), "/run/scriptschnell-term".into()));
        let mounted = Environment::Container(target);
        assert_eq!(
            mounted.forwarded_socket(&local, "s1"),
            Some(PathBuf::from("/run/scriptschnell-term/app.sock"))
        );
        assert_eq!(
            Environment::Local.forwarded_socket(&local, "s1"),
            Some(local.clone())
        );
    }

    /// Against a real sshd: `CMDTERM_TEST_SSH_CONFIG` names an ssh config
    /// file, `CMDTERM_TEST_SSH_HOST` a host in it that forwards Unix sockets
    /// and `CMDTERM_TEST_SSH_NO_FORWARD` one that does not. Both must be
    /// this machine, so the test can connect to the forwarded end.
    #[cfg(unix)]
    #[test]
    #[ignore = "needs an sshd on this machine"]
    fn ssh_forwards_reach_this_machine_and_go_away() {
        use std::io::{Read, Write};
        use std::os::unix::net::{UnixListener, UnixStream};
        let config = std::env::var("CMDTERM_TEST_SSH_CONFIG").unwrap();
        let host = |var: &str| {
            let mut target = SshTarget::new(std::env::var(var).unwrap());
            target.options = vec!["-F".into(), config.clone()];
            Environment::Ssh(target)
        };
        let (env, refusing) = (
            host("CMDTERM_TEST_SSH_HOST"),
            host("CMDTERM_TEST_SSH_NO_FORWARD"),
        );
        let dir = tempfile::Builder::new()
            .prefix("ct")
            .tempdir_in("/tmp")
            .unwrap();
        let local = dir.path().join("l.sock");
        let listener = UnixListener::bind(&local).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let _ = stream.unwrap().write_all(b"hello");
            }
        });
        let probe = dir.path().join("probe.sock");
        assert_eq!(env.can_forward_socket(&local, &probe), Some(true));
        assert!(!probe.exists(), "the probe cleans up");
        assert_eq!(refusing.can_forward_socket(&local, &probe), Some(false));

        let inside = env.forwarded_socket(&local, "ct-test").unwrap();
        let mut child = env
            .command("sleep")
            .arg("3")
            .forward_socket(&local, &inside)
            .to_std()
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !inside.exists() {
            assert!(std::time::Instant::now() < deadline, "no forward");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut reply = String::new();
        UnixStream::connect(&inside)
            .unwrap()
            .read_to_string(&mut reply)
            .unwrap();
        assert_eq!(reply, "hello");
        assert!(child.wait().unwrap().success());
        assert!(!inside.exists(), "the remote script removes its socket");
    }

    #[test]
    fn local_command_runs() {
        let out = Environment::Local
            .command("sh")
            .args(["-c", "printf hi"])
            .stdout()
            .unwrap();
        assert_eq!(out, b"hi");
    }

    #[test]
    fn failed_command_reports_stderr() {
        let error = Environment::Local
            .command("sh")
            .args(["-c", "echo nope >&2; exit 3"])
            .stdout()
            .unwrap_err();
        assert_eq!(error.to_string(), "command exited with 3: nope");
    }

    #[test]
    fn environment_round_trips_as_json() {
        let env = Environment::ssh("user@host");
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(json, r#"{"kind":"ssh","destination":"user@host"}"#);
        assert_eq!(serde_json::from_str::<Environment>(&json).unwrap(), env);
    }

    #[test]
    fn program_round_trips_and_defaults_to_ssh() {
        let mut target = SshTarget::new("h");
        assert_eq!(target.program(), "ssh");
        target.program = Some("/bin/shim".into());
        let json = serde_json::to_string(&Environment::Ssh(target.clone())).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"ssh","destination":"h","program":"/bin/shim"}"#
        );
        assert_eq!(
            serde_json::from_str::<Environment>(&json).unwrap(),
            Environment::Ssh(target)
        );
    }

    #[cfg(unix)]
    #[test]
    fn commands_that_hang_are_killed_at_the_deadline() {
        let quick = Environment::Local
            .command("sh")
            .args(["-c", "echo hi; echo oops >&2"])
            .output_within(Duration::from_secs(10))
            .unwrap();
        assert_eq!(quick.stdout, b"hi\n");
        assert_eq!(quick.stderr, b"oops\n");

        let started = Instant::now();
        let error = Environment::Local
            .command("sleep")
            .arg("30")
            .output_within(Duration::from_millis(200))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn remote_target_defaults_the_port_and_round_trips() {
        assert_eq!(RemoteTarget::new("box").address, "box:7722");
        assert_eq!(RemoteTarget::new("box:9000").address, "box:9000");
        assert_eq!(RemoteTarget::new("[::1]").address, "[::1]:7722");
        assert_eq!(RemoteTarget::new("[::1]:1").address, "[::1]:1");
        let mut target = RemoteTarget::new("box");
        target.vm = Some("ci".into());
        let env = Environment::Remote(target);
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(json, r#"{"kind":"remote","address":"box:7722","vm":"ci"}"#);
        assert_eq!(serde_json::from_str::<Environment>(&json).unwrap(), env);
        assert_eq!(env.label(), "ci");
    }

    #[test]
    fn remote_commands_go_through_the_client() {
        let env = Environment::Remote(RemoteTarget::new("box"));
        let command = env.command("git").arg("status").cwd("/srv/a").var("K", "v");
        let std = command.to_std();
        assert_eq!(std.get_program(), remote_client().program.as_os_str());
        let args: Vec<_> = std
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let tail = [
            "exec", "--target", "box:7722", "--cwd", "/srv/a", "--env", "K=v", "--", "git",
            "status",
        ];
        assert_eq!(args[args.len() - tail.len()..], tail);
        let argv = command.to_pty().get_argv().clone();
        assert!(argv.iter().any(|a| a == "--pty"));
        let shell = env.shell(Some(Path::new("/srv/a"))).get_argv().clone();
        assert!(shell.iter().any(|a| a == "--shell"));
    }

    #[test]
    fn cpu_limits_wrap_the_command_where_it_runs() {
        let limit = CpuLimit::new("t1", CpuShare::ALL, 100);
        let strings = |argv: &Vec<OsString>| -> Vec<String> {
            argv.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        // Here: sh runs the script, then the program.
        let local = Environment::Local
            .command("claude")
            .arg("-c")
            .limit_cpu(Some(limit.clone()));
        let argv = strings(local.to_pty().get_argv());
        if cfg!(unix) {
            assert_eq!(argv[0], "sh");
            assert_eq!(argv[1], "-c");
            assert_eq!(argv[2], cpu::SCRIPT);
            assert_eq!(argv[argv.len() - 3..], ["--", "claude", "-c"]);
        } else {
            assert_eq!(argv.len(), 2);
        }
        // Over SSH the script runs on the host, in front of its shell.
        let ssh = Environment::ssh("box");
        let shell = strings(
            ssh.limited_shell(None, &[], &[], None, Some(&limit))
                .get_argv(),
        );
        let remote = shell.last().unwrap();
        assert!(remote.starts_with("exec sh -c "), "{remote}");
        assert!(
            remote.ends_with(" scriptschnell-term-t1 0 100 100 -- \"${SHELL:-/bin/sh}\" -l"),
            "{remote}"
        );
        // A remote machine's server applies the limit to its login shell,
        // so it works on Windows servers too.
        let env = Environment::Remote(RemoteTarget::new("box"));
        let shell = strings(
            env.limited_shell(None, &[], &[], Some("s1"), Some(&limit))
                .get_argv(),
        );
        assert!(shell.iter().any(|a| a == "--shell"));
        assert!(shell.windows(2).any(|w| w == ["--session", "s1"]));
        assert!(
            shell
                .windows(2)
                .any(|w| w == ["--cpu", "scriptschnell-term-t1:0:100:100"])
        );
        let command = env
            .command("claude")
            .session("s1")
            .limit_cpu(Some(limit.clone()));
        let argv = strings(command.to_pty().get_argv());
        assert!(
            argv.windows(2)
                .any(|w| w == ["--cpu", "scriptschnell-term-t1:0:100:100"])
        );
        assert_eq!(argv.last().unwrap(), "claude");
        // Without a limit nothing changes.
        let plain = strings(env.limited_shell(None, &[], &[], None, None).get_argv());
        assert!(plain.iter().any(|a| a == "--shell"));
    }

    #[test]
    fn sessions_reach_the_client_only_for_terminals_that_keep_them() {
        let env = Environment::Remote(RemoteTarget::new("box"));
        assert!(env.keeps_sessions());
        assert!(Environment::Vm(VmTarget { name: "dev".into() }).keeps_sessions());
        assert!(!Environment::Local.keeps_sessions());
        assert!(!Environment::ssh("h").keeps_sessions());
        let has_session = |argv: &[OsString]| {
            argv.windows(2)
                .any(|pair| pair[0] == "--session" && pair[1] == "s1")
        };
        let command = env.command("htop").session("s1");
        assert!(has_session(command.to_pty().get_argv()));
        let args: Vec<OsString> = command.to_std().get_args().map(Into::into).collect();
        assert!(!has_session(&args), "only PTY commands keep sessions");
        let shell = env.shell_in_session(None, &[], &[], Some("s1"));
        assert!(has_session(shell.get_argv()));
        assert!(!has_session(env.shell(None).get_argv()));
        let local = Environment::Local.shell_in_session(None, &[], &[], Some("s1"));
        assert!(!has_session(local.get_argv()));
    }

    #[test]
    fn local_vm_commands_select_the_vm() {
        let env = Environment::Vm(VmTarget { name: "dev".into() });
        let args: Vec<_> = env
            .command("ls")
            .to_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let tail = ["exec", "--local-vm", "dev", "--", "ls"];
        assert_eq!(args[args.len() - tail.len()..], tail);
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(json, r#"{"kind":"vm","name":"dev"}"#);
    }

    #[test]
    fn all_ssh_call_sites_use_the_program() {
        let mut target = SshTarget::new("h");
        target.port = Some(2222);
        target.program = Some("shim".into());
        let env = Environment::Ssh(target);
        let command = env.command("ls").cwd("/x");
        let std = command.to_std();
        assert_eq!(std.get_program(), "shim");
        let args: Vec<_> = std
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args[args.len() - 8..],
            [
                "-p",
                "2222",
                "-o",
                "BatchMode=yes",
                "-T",
                "--",
                "h",
                "cd /x && exec ls"
            ]
        );
        assert_eq!(command.to_pty().get_argv()[0], "shim");
        assert_eq!(env.shell(None).get_argv()[0], "shim");
    }

    #[test]
    fn ssh_destinations_come_after_a_double_dash() {
        let env = Environment::ssh("-oProxyCommand=x");
        let strings = |argv: Vec<OsString>| -> Vec<String> {
            argv.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let command = env.command("ls");
        let std = command.to_std();
        let argvs = [
            strings(std.get_args().map(Into::into).collect()),
            strings(command.to_pty().get_argv().clone()),
            strings(env.shell(None).get_argv().clone()),
        ];
        for argv in argvs {
            let at = argv.iter().position(|a| a == "-oProxyCommand=x").unwrap();
            assert_eq!(argv[at - 1], "--", "{argv:?}");
        }
    }

    #[test]
    fn ssh_destinations_that_read_as_options_are_refused() {
        assert!(valid_ssh_destination("box"));
        assert!(valid_ssh_destination("me@box.example"));
        assert!(valid_ssh_destination("me@100.64.0.2"));
        assert!(valid_ssh_destination("a-b@c-d"));
        assert!(!valid_ssh_destination(""));
        assert!(!valid_ssh_destination("-oProxyCommand=x"));
        assert!(!valid_ssh_destination("me@-oProxyCommand=x"));
        assert!(!valid_ssh_destination("-me@box"));
        assert!(!valid_ssh_destination("@box"));
        assert!(!valid_ssh_destination("me@"));
        assert!(!valid_ssh_destination("me@box x"));
        assert!(!valid_ssh_destination("box\n"));
    }

    #[cfg(unix)]
    #[test]
    fn control_dirs_are_short_and_private() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let uid = rustix::process::geteuid().as_raw();
        let root = tempfile::Builder::new()
            .prefix("ct")
            .tempdir_in("/tmp")
            .unwrap();
        // A long runtime directory is passed over for a short one.
        let long = root.path().join("x".repeat(CONTROL_DIR_MAX));
        let short = root.path().join("s");
        assert_eq!(
            pick_control_dir([long.clone(), short.clone()], uid),
            Some(short.clone())
        );
        assert!(!long.exists());
        let mode = |dir: &Path| std::fs::metadata(dir).unwrap().mode() & 0o777;
        assert_eq!(mode(&short), 0o700);
        // One of ours that others can enter is closed.
        std::fs::set_permissions(&short, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&short, uid).is_ok());
        assert_eq!(mode(&short), 0o700);
        // Another user's directory, a symlink and a file are refused.
        assert!(private_dir(&short, uid + 1).is_err());
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&short, &link).unwrap();
        assert!(private_dir(&link, uid).is_err());
        let file = root.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(pick_control_dir([link, file], uid), None);
        // The fallback fits.
        assert!(format!("/tmp/cmdterm-ssh-{}", u32::MAX).len() <= CONTROL_DIR_MAX);
    }

    /// When another user made the `/tmp` directory first, `~/.ssh/cmdterm`
    /// is used, but only in a `~/.ssh` that nobody else may write to.
    #[cfg(unix)]
    #[test]
    fn control_dirs_fall_back_to_the_home_directory() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let uid = rustix::process::geteuid().as_raw();
        let home = tempfile::Builder::new()
            .prefix("ct")
            .tempdir_in("/tmp")
            .unwrap();
        assert_eq!(home_control_dir(home.path(), uid), None, "no ~/.ssh");
        assert!(!home.path().join(".ssh").exists(), "~/.ssh is not made");
        let ssh = home.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(home_control_dir(home.path(), uid), None, "others may write");
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(home_control_dir(home.path(), uid + 1), None, "not ours");
        let ours = home_control_dir(home.path(), uid).unwrap();
        assert_eq!(ours, ssh.join("cmdterm"));
        // A `/tmp` path that is not our directory, as when another user
        // made it, is passed over for the home one.
        let taken = home.path().join("taken");
        std::os::unix::fs::symlink(home.path(), &taken).unwrap();
        assert_eq!(
            pick_control_dir([taken, ours.clone()], uid),
            Some(ours.clone())
        );
        assert_eq!(std::fs::metadata(&ours).unwrap().mode() & 0o777, 0o700);
        assert!(ours.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn remote_scripts_keep_bytes_that_are_not_utf8() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let name = OsString::from_vec(b"caf\xe9 it's".to_vec());
        let command = Environment::ssh("box").command("cat").arg(&name);
        assert_eq!(
            command.remote_script().as_bytes(),
            b"exec cat 'caf\xe9 it'\\''s'"
        );
        // The shell reads it back as the same bytes.
        let out = Environment::Local
            .command("sh")
            .arg("-c")
            .arg({
                let mut script = OsString::from("printf %s ");
                push_quoted(&mut script, &name);
                script
            })
            .stdout()
            .unwrap();
        assert_eq!(out, name.as_bytes());
    }

    #[test]
    fn remote_scripts_leave_out_names_that_are_not_identifiers() {
        let command = Environment::ssh("box")
            .command("env")
            .var("GOOD_1", "a")
            .var("$(touch x)", "b")
            .var("1BAD", "c")
            .var("_ok", "d");
        assert_eq!(command.remote_script(), "GOOD_1=a _ok=d exec env");
    }
}
