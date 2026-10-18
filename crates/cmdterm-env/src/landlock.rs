//! Landlock scopes for bubblewrap sandboxes with the host's network.
//!
//! Abstract Unix sockets (`@name`) belong to a network namespace, not to a
//! path, so a sandbox that shares the host's network reaches every abstract
//! socket on the host, a fresh `/tmp` notwithstanding. X11 and Xwayland
//! listen at `@/tmp/.X11-unix/X0`, which lets a client type into any window
//! on the display, and some systems put the session bus and other daemons
//! at abstract addresses too.
//!
//! Landlock ABI 6 (Linux 6.12) can scope a process away from abstract
//! sockets and signals: once [`scope`] restricted it, the process and
//! everything it starts can connect or send only to abstract sockets bound
//! inside the same restriction, and signal only processes inside it. A
//! restriction applies to the calling thread and is inherited across fork
//! and exec, so the proxy bridge applies it first thing inside the sandbox
//! (`cmdterm-proxy-bridge --scope`) and then replaces itself with the shell.
//! Binding is not scoped: a sandbox can still take an unused abstract name
//! that a program outside connects to later.
//!
//! A sandbox on a cmdterm-remote server or VM runs on that machine, where
//! the client cannot name a program. The server puts the path of its own
//! bridge into [`SCOPE_EXE_VAR`] for the commands it starts, and a short
//! `sh` script at the start of the sandbox runs that bridge with
//! `--try-scope`. Older servers set no variable, so their sandboxes start
//! as before, and so do those on a kernel without ABI 6. SSH hosts and
//! hosts of extension environments get a copy of this machine's bridge
//! instead, see `host_bridge.rs`.
//!
//! Where this machine's kernel lacks ABI 6, or Landlock is off at boot,
//! sandboxes here with the host's network start without the scope. This
//! module warns once on standard error; the app also shows a notice.

use std::ffi::{OsStr, OsString};

use crate::Environment;

/// The first argument with which the app and `cmdterm-remote` run as the
/// proxy bridge. The `cmdterm-proxy-bridge` program skips it.
pub const BUILT_IN_BRIDGE_FLAG: &str = "--cmdterm-proxy-bridge";

/// The variable in which a cmdterm-remote server names its bridge for the
/// commands it starts, see [`scope_exe`].
pub const SCOPE_EXE_VAR: &str = "CMDTERM_SCOPE_EXE";

/// Runs `"$@"` under the bridge that [`SCOPE_EXE_VAR`] names, with
/// `--try-scope`, or as it is without one. The variable does not reach the
/// command.
const REMOTE_SCOPE: &str = r#"x=$CMDTERM_SCOPE_EXE; unset CMDTERM_SCOPE_EXE
if [ -n "$x" ] && [ -x "$x" ]; then
  exec "$x" --cmdterm-proxy-bridge --try-scope -- "$@"
fi
exec "$@"
"#;

/// The first Landlock ABI with scopes.
#[cfg(target_os = "linux")]
const SCOPE_ABI: u32 = 6;

/// The kernel's Landlock ABI version, or `None` without Landlock: before
/// Linux 5.13, off at boot, or not Linux.
fn abi() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        static ABI: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
        *ABI.get_or_init(sys::abi)
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// Whether this machine's kernel can keep sandboxes from abstract Unix
/// sockets and processes outside them: Linux 6.12 or later, with Landlock
/// on.
pub fn landlock_scopes() -> bool {
    #[cfg(target_os = "linux")]
    {
        abi().is_some_and(|abi| abi >= SCOPE_ABI)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = abi;
        false
    }
}

/// Keeps this process and all it starts from now on from reaching abstract
/// Unix sockets and processes outside, see the module documentation. Fails
/// without Landlock ABI 6. The bridge, its only caller, runs on Unix.
#[cfg(unix)]
pub(crate) fn scope() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        sys::scope()
    }
    #[cfg(not(target_os = "linux"))]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Landlock needs Linux",
    ))
}

/// What a cmdterm-remote server puts into [`SCOPE_EXE_VAR`]: its bridge,
/// when it has one that sandboxes started from [`REMOTE_SCOPE`] can run.
/// `None` off Linux, and in test binaries without a known bridge.
pub fn scope_exe() -> Option<&'static OsStr> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    match crate::proxy::known_proxy_bridge()? {
        [program] => Some(program),
        [program, flag] if flag == BUILT_IN_BRIDGE_FLAG => Some(program),
        _ => None,
    }
}

/// What goes before the command of a bubblewrap sandbox with the host's
/// network on `host`, so the sandbox cannot reach abstract Unix sockets
/// bound outside it, or `None` to start it as it is. This machine runs its
/// bridge with `--scope` where its kernel can, and a cmdterm-remote server
/// or VM runs [`REMOTE_SCOPE`]. SSH hosts and extension environments run
/// the copy `host_bridge.rs` put there, once it is there. Containers have
/// no bridge of ours, and a bridge here that is only a guess at `PATH`, as
/// in test binaries, counts as none.
pub(crate) fn host_network_scope(host: &Environment) -> Option<Vec<OsString>> {
    match host {
        Environment::Local => {
            let bridge = crate::proxy::known_proxy_bridge()?;
            if !landlock_scopes() {
                warn_unscoped();
                return None;
            }
            let mut args = bridge.to_vec();
            args.extend(["--scope".into(), "--".into()]);
            Some(args)
        }
        Environment::Remote(_) | Environment::Vm(_) => Some(
            ["sh", "-c", REMOTE_SCOPE, "sh"]
                .map(OsString::from)
                .to_vec(),
        ),
        Environment::Ssh(_) | Environment::Custom(_) => crate::host_bridge::scope_args(host),
        _ => None,
    }
}

fn warn_unscoped() {
    #[cfg(target_os = "linux")]
    {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!(
                "cmdterm-env: this kernel has no Landlock ABI {SCOPE_ABI} (Linux 6.12), so \
                 sandboxes with the host's network can reach the host's abstract Unix \
                 sockets, such as X11's"
            );
        });
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod sys {
    use std::ffi::{c_int, c_long, c_ulong};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    // The Landlock calls have these numbers on every architecture but
    // Alpha and MIPS. There the call fails with ENOSYS, which reads as no
    // Landlock.
    const SYS_LANDLOCK_CREATE_RULESET: c_long = 444;
    const SYS_LANDLOCK_RESTRICT_SELF: c_long = 446;
    const LANDLOCK_CREATE_RULESET_VERSION: c_long = 1 << 0;
    const LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    const LANDLOCK_SCOPE_SIGNAL: u64 = 1 << 1;
    const PR_SET_NO_NEW_PRIVS: c_int = 38;

    /// `struct landlock_ruleset_attr` as of ABI 6.
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    unsafe extern "C" {
        fn syscall(number: c_long, ...) -> c_long;
        fn prctl(option: c_int, ...) -> c_int;
    }

    pub(super) fn abi() -> Option<u32> {
        // SAFETY: with the version flag the kernel reads no attribute.
        let version = unsafe {
            syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                std::ptr::null::<RulesetAttr>(),
                0 as c_long,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        u32::try_from(version).ok().filter(|&version| version > 0)
    }

    pub(super) fn scope() -> io::Result<()> {
        // bwrap sets it already. Without it an unprivileged process may not
        // restrict itself.
        // SAFETY: plain integer arguments.
        if unsafe {
            prctl(
                PR_SET_NO_NEW_PRIVS,
                1 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // Handling no file system or network access leaves both as they
        // are; only the scopes restrict.
        let attr = RulesetAttr {
            handled_access_fs: 0,
            handled_access_net: 0,
            scoped: LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL,
        };
        // SAFETY: `attr` lives across the call and the size is its own.
        let fd = unsafe {
            syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &raw const attr,
                size_of::<RulesetAttr>() as c_long,
                0 as c_long,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel just returned this descriptor, close-on-exec,
        // and nothing else owns it.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as c_int) };
        // SAFETY: plain integer arguments.
        let done = unsafe {
            syscall(
                SYS_LANDLOCK_RESTRICT_SELF,
                ruleset.as_raw_fd() as c_long,
                0 as c_long,
            )
        };
        if done != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RemoteTarget, SandboxKind, SandboxPolicy, SandboxTarget, SshTarget, VmTarget};

    fn lowered(host: Environment) -> Vec<String> {
        let mut policy = SandboxPolicy::local(vec![], &[], true);
        policy.kind = SandboxKind::Bwrap;
        policy.host_network = true;
        let target = SandboxTarget::new(host.clone(), policy);
        let command = host.command("ls").arg("-l");
        let lowered = target.lower(&command, true);
        assert_eq!(lowered.program, "bwrap");
        let args = lowered
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned());
        let args: Vec<String> = args.collect();
        let end = args.iter().position(|a| a == "--").unwrap();
        args[end + 1..].to_vec()
    }

    #[test]
    fn remote_machines_and_vms_start_host_network_sandboxes_under_their_bridge() {
        let script = ["sh", "-c", REMOTE_SCOPE, "sh", "ls", "-l"];
        let remote = Environment::Remote(RemoteTarget::new("box"));
        assert_eq!(lowered(remote), script);
        let vm = Environment::Vm(VmTarget { name: "dev".into() });
        assert_eq!(lowered(vm), script);
        // The script names the variable and the flag the server sets up.
        assert!(REMOTE_SCOPE.contains(&format!("${SCOPE_EXE_VAR}")));
        assert!(REMOTE_SCOPE.contains(&format!("unset {SCOPE_EXE_VAR}")));
        assert!(REMOTE_SCOPE.contains(&format!("\"$x\" {BUILT_IN_BRIDGE_FLAG} --try-scope --")));

        // An SSH host without a copy of the bridge (`host_bridge.rs`).
        let ssh: SshTarget = serde_json::from_str(r#"{"destination": "box"}"#).unwrap();
        assert_eq!(lowered(Environment::Ssh(ssh)), ["ls", "-l"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_remote_script_runs_the_command_with_or_without_a_bridge() {
        let run = |exe: Option<&str>| {
            let mut command = Environment::Local
                .command("sh")
                .args(["-c", REMOTE_SCOPE, "sh", "sh", "-c"])
                .arg(format!("echo \"[${{{SCOPE_EXE_VAR}-unset}}]\" \"$0\""))
                .arg("arg");
            if let Some(exe) = exe {
                command = command.var(SCOPE_EXE_VAR, exe);
            }
            String::from_utf8(command.stdout().unwrap()).unwrap()
        };
        // Without a server's bridge, or with one that is gone, as it is.
        assert_eq!(run(None), "[unset] arg\n");
        assert_eq!(run(Some("/nonexistent/cmdterm-remote")), "[unset] arg\n");
        // `echo` stands in for the bridge: it prints the flags and the
        // command it got. (A script written here could fail to start with
        // ETXTBSY while another test thread forks.)
        let echo = ["/usr/bin/echo", "/bin/echo"]
            .into_iter()
            .find(|path| std::path::Path::new(path).is_file())
            .unwrap();
        assert_eq!(
            run(Some(echo)),
            format!(
                "{BUILT_IN_BRIDGE_FLAG} --try-scope -- sh -c echo \"[${{{SCOPE_EXE_VAR}-unset}}]\" \"$0\" arg\n"
            )
        );
    }
}
