//! Commands inside a running container, through `docker exec` or
//! `podman exec` on the container's host environment.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{EnvCommand, Environment};

/// A running container. The devcontainer support in `cmdterm`'s `container`
/// module creates these; any container started by other means works too.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerTarget {
    /// Where the engine runs: this machine, an SSH host, a remote machine.
    pub host: Box<Environment>,
    /// `docker` or `podman`.
    pub engine: String,
    /// Container name or id.
    pub container: String,
    /// User to run as, such as a devcontainer's `remoteUser`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Host directories mounted into the container as `(host, container)`.
    /// A working directory under a host path is translated to its container
    /// path, so a worktree's terminals start in the mounted copy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<(PathBuf, PathBuf)>,
    /// Variables set for every command, such as a devcontainer's `remoteEnv`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vars: Vec<(String, String)>,
}

impl ContainerTarget {
    pub fn new(host: Environment, engine: &str, container: &str) -> Self {
        Self {
            host: Box::new(host),
            engine: engine.to_owned(),
            container: container.to_owned(),
            user: None,
            mounts: Vec::new(),
            vars: Vec::new(),
        }
    }

    /// `path` on the host as seen inside the container. Paths outside every
    /// mount are passed through. The part under the mount is joined with
    /// `/`, so a Windows host's `C:\src\app\crates` becomes
    /// `/workspaces/app/crates` and not a path with backslashes.
    pub fn container_path(&self, path: &Path) -> PathBuf {
        for (host, inside) in &self.mounts {
            if let Ok(rest) = path.strip_prefix(host) {
                let mut out = inside.as_os_str().to_owned();
                for part in rest.components() {
                    if !out.to_string_lossy().ends_with('/') {
                        out.push("/");
                    }
                    out.push(part.as_os_str());
                }
                return PathBuf::from(out);
            }
        }
        path.to_path_buf()
    }

    /// `command` as an `exec` call on the host.
    pub(crate) fn lower(&self, command: &EnvCommand, pty: bool) -> EnvCommand {
        let mut args: Vec<OsString> = vec!["exec".into(), "-i".into()];
        if pty {
            args.push("-t".into());
            args.push("-e".into());
            args.push("TERM=xterm-256color".into());
        }
        if let Some(cwd) = &command.cwd {
            args.push("-w".into());
            args.push(self.container_path(cwd).into_os_string());
        }
        if let Some(user) = &self.user {
            args.push("-u".into());
            args.push(user.into());
        }
        let vars = self
            .vars
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .chain(command.vars.iter().cloned());
        for (key, value) in vars {
            let mut pair = key;
            pair.push("=");
            pair.push(value);
            args.push("-e".into());
            args.push(pair);
        }
        args.push(self.container.clone().into());
        args.push(command.program.clone());
        args.extend(command.args.iter().cloned());
        self.host.command(&self.engine).args(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ContainerTarget {
        let mut target = ContainerTarget::new(Environment::Local, "podman", "dev");
        target.user = Some("vscode".into());
        target.mounts = vec![("/home/me/app".into(), "/workspaces/app".into())];
        target.vars = vec![("A".into(), "1".into())];
        target
    }

    fn argv(command: &std::process::Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn paths_under_a_mount_are_joined_with_slashes() {
        let mut target = target();
        assert_eq!(
            target.container_path(Path::new("/home/me/app")),
            Path::new("/workspaces/app")
        );
        assert_eq!(
            target
                .container_path(Path::new("/home/me/app/a/b"))
                .as_os_str(),
            "/workspaces/app/a/b"
        );
        target.mounts = vec![("/home/me/app".into(), "/".into())];
        assert_eq!(
            target
                .container_path(Path::new("/home/me/app/a"))
                .as_os_str(),
            "/a"
        );
        assert_eq!(
            target.container_path(Path::new("/elsewhere")),
            Path::new("/elsewhere")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_become_container_paths() {
        let mut target = target();
        target.mounts = vec![(r"C:\src\app".into(), "/workspaces/app".into())];
        assert_eq!(
            target
                .container_path(Path::new(r"C:\src\app\crates\x"))
                .as_os_str(),
            "/workspaces/app/crates/x"
        );
    }

    #[test]
    fn commands_become_exec_calls_with_translated_cwd() {
        let env = Environment::Container(Box::new(target()));
        let command = env
            .command("cargo")
            .arg("test")
            .cwd("/home/me/app/crates/x")
            .var("B", "2");
        assert_eq!(
            argv(&command.to_std()),
            [
                "podman",
                "exec",
                "-i",
                "-w",
                "/workspaces/app/crates/x",
                "-u",
                "vscode",
                "-e",
                "A=1",
                "-e",
                "B=2",
                "dev",
                "cargo",
                "test"
            ]
        );
        let pty = command.to_pty();
        let pty_argv: Vec<String> = pty
            .get_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            pty_argv[..6],
            ["podman", "exec", "-i", "-t", "-e", "TERM=xterm-256color"]
        );
    }

    #[test]
    fn containers_on_ssh_hosts_run_exec_over_ssh() {
        let mut target = target();
        target.host = Box::new(Environment::ssh("box"));
        let env = Environment::Container(Box::new(target));
        let command = env.command("ls").to_std();
        assert_eq!(command.get_program(), "ssh");
        let last = command
            .get_args()
            .last()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(last, "exec podman exec -i -u vscode -e 'A=1' dev ls");
        assert_eq!(env.base(), &Environment::ssh("box"));
        assert_eq!(env.label(), "dev");
    }

    #[test]
    fn shells_start_the_login_shell_inside() {
        let env = Environment::Container(Box::new(target()));
        let shell = env.shell(Some(Path::new("/home/me/app")));
        let argv: Vec<String> = shell
            .get_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(argv.contains(&"/workspaces/app".to_owned()), "{argv:?}");
        assert_eq!(argv[argv.len() - 3..argv.len() - 1], ["sh", "-c"]);
    }
}
