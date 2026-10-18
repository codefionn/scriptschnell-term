//! Direct passthrough of this machine's container engines and WSL into its
//! sandboxes: [`pass_engines`] lets a sandbox talk to the engines' API
//! sockets, and [`hide_engines`] takes them away.
//!
//! The engines run outside the sandbox. A sandbox that reaches one can start
//! containers with any host path mounted and any network, so passthrough
//! trades the sandbox's limits for working `docker`, `podman`, `nerdctl` and
//! `buildctl`. The same goes for WSL interop, which runs Windows programs as
//! the Windows user.
//!
//! On Linux and macOS the sockets are found where each engine puts them by
//! default, or where `DOCKER_HOST`, `CONTAINER_HOST`, `CONTAINERD_ADDRESS`
//! or `BUILDKIT_HOST` point, and the sandbox gets the same variable set to
//! the socket it can reach. Without a docker socket, `DOCKER_HOST` points to
//! podman's, which speaks the same API. On Windows the `cmdterm-sandbox`
//! launcher relays the engines' named pipes, see [`SandboxPolicy::pipes`].

use std::path::{Path, PathBuf};

use crate::{SandboxKind, SandboxPolicy};

/// One engine's API: the variable its clients read, the URL scheme it takes
/// there, and where the engine listens by default.
struct Engine {
    var: &'static str,
    /// What precedes the socket path in the variable, `unix://` or empty.
    scheme: &'static str,
    /// Default sockets in the order clients try them. `~/` is the home
    /// directory, `$XDG/` the runtime directory.
    sockets: &'static [&'static str],
    /// The address of the engine's Windows named pipe, in the form its
    /// clients read from `var`.
    pipe: &'static str,
}

const DOCKER: Engine = Engine {
    var: "DOCKER_HOST",
    scheme: "unix://",
    sockets: &[
        "/var/run/docker.sock",
        "$XDG/docker.sock",
        "~/.docker/desktop/docker.sock",
        "~/.docker/run/docker.sock",
        "~/.colima/default/docker.sock",
        "~/.orbstack/run/docker.sock",
        "~/.rd/docker.sock",
    ],
    pipe: "npipe:////./pipe/docker_engine",
};

const PODMAN: Engine = Engine {
    var: "CONTAINER_HOST",
    scheme: "unix://",
    sockets: &[
        "$XDG/podman/podman.sock",
        "/run/podman/podman.sock",
        "~/.local/share/containers/podman/machine/podman.sock",
    ],
    pipe: "npipe:////./pipe/podman-machine-default",
};

const CONTAINERD: Engine = Engine {
    var: "CONTAINERD_ADDRESS",
    scheme: "",
    sockets: &[
        "/run/containerd/containerd.sock",
        "/run/k3s/containerd/containerd.sock",
    ],
    pipe: r"\\.\pipe\containerd-containerd",
};

const BUILDKIT: Engine = Engine {
    var: "BUILDKIT_HOST",
    scheme: "unix://",
    sockets: &[
        "$XDG/buildkit/buildkitd.sock",
        "/run/buildkit/buildkitd.sock",
    ],
    pipe: "npipe:////./pipe/buildkitd",
};

const ENGINES: [&Engine; 4] = [&DOCKER, &PODMAN, &CONTAINERD, &BUILDKIT];

/// The variable WSL sets to its interop socket, through which programs in a
/// WSL distribution start Windows programs.
const WSL_INTEROP: &str = "WSL_INTEROP";

/// Lets sandboxes of `policy` reach this machine's container engines and,
/// inside WSL, Windows. Engines that are not running are left out.
pub fn pass_engines(policy: &mut SandboxPolicy) {
    pass_with(policy, &|var| std::env::var(var).ok());
}

/// Hides this machine's container engine sockets and WSL interop from
/// sandboxes of `policy`, also where the read-only root would show them.
/// Windows sandboxes cannot write to the engines' pipes anyway.
pub fn hide_engines(policy: &mut SandboxPolicy) {
    hide_with(policy, &|var| std::env::var(var).ok());
}

/// Reads a variable of this process, or a test's.
type Vars<'a> = &'a dyn Fn(&str) -> Option<String>;

fn pass_with(policy: &mut SandboxPolicy, vars: Vars) {
    if cfg!(windows) {
        for engine in ENGINES {
            // A pipe the variable names wins over the default one.
            let address = vars(engine.var)
                .filter(|value| value.to_ascii_lowercase().contains("pipe"))
                .unwrap_or_else(|| engine.pipe.to_owned());
            policy.pipes.push((engine.var.into(), address));
        }
        return;
    }
    let mut docker = None;
    let mut podman = None;
    for engine in ENGINES {
        let Some(socket) = engine_socket(engine, vars) else {
            continue;
        };
        let value = format!("{}{}", engine.scheme, socket.display());
        policy.vars.push((engine.var.into(), value.clone()));
        policy.sockets.push(socket);
        if std::ptr::eq(engine, &DOCKER) {
            docker = Some(value);
        } else if std::ptr::eq(engine, &PODMAN) {
            podman = Some(value);
        }
    }
    let docker_unset = vars(DOCKER.var).is_none();
    if let (None, Some(podman), true) = (docker, podman, docker_unset) {
        policy.vars.push((DOCKER.var.into(), podman));
    }
    policy.readable.extend(docker_tls_files(vars));
    if let Some(socket) = wsl_interop(vars) {
        policy.sockets.push(socket);
    }
}

fn hide_with(policy: &mut SandboxPolicy, vars: Vars) {
    if cfg!(windows) {
        return;
    }
    // An engine started after the sandbox puts its socket where nothing is
    // mounted yet. bubblewrap sees `/run` as a scratch directory, so new
    // entries there stay out, and an engine's own directory is hidden
    // whole, socket or not. Seatbelt denies paths that do not exist yet.
    let run = PathBuf::from("/run");
    if policy.kind == SandboxKind::Bwrap && run.is_dir() && !policy.scratch_dirs.contains(&run) {
        policy.scratch_dirs.push(run.clone());
    }
    let real = |path: Option<PathBuf>| path.and_then(|p| std::fs::canonicalize(p).ok());
    let shared = [Some(run), vars("XDG_RUNTIME_DIR").map(PathBuf::from)].map(real);
    for engine in ENGINES {
        let defaults: Vec<PathBuf> = (engine.sockets.iter())
            .filter_map(|s| expand(s, vars))
            .collect();
        for dir in defaults
            .iter()
            .map(|s| real(s.parent().map(Path::to_path_buf)))
        {
            let Some(dir) = dir.filter(|dir| !shared.contains(&Some(dir.clone()))) else {
                continue;
            };
            let covered = policy.hidden_dirs.iter().any(|d| dir.starts_with(d));
            if !covered && dir.is_dir() {
                policy.hidden_dirs.push(dir);
            }
        }
        for socket in defaults.iter().cloned().chain(from_var(engine, vars)) {
            let socket = match std::fs::canonicalize(&socket) {
                Ok(socket) if is_socket(&socket) => socket,
                Ok(_) => continue,
                Err(_) if policy.kind == SandboxKind::Seatbelt => socket,
                Err(_) => continue,
            };
            let covered = policy.hidden_dirs.iter().any(|dir| socket.starts_with(dir));
            if !covered && !policy.hidden_files.contains(&socket) {
                policy.hidden_files.push(socket);
            }
        }
        policy.unset.push(engine.var.into());
    }
    if let Some(socket) = wsl_interop(vars) {
        // Every WSL process has its own socket next to this one.
        match socket.parent().filter(|dir| *dir != Path::new("/")) {
            Some(dir) => policy.hidden_dirs.push(dir.to_path_buf()),
            None => policy.hidden_files.push(socket),
        }
        policy.unset.push(WSL_INTEROP.into());
    }
}

/// The client certificates of a docker daemon reached over TLS, which sit
/// in the hidden `~/.docker` unless `DOCKER_CERT_PATH` names another place.
/// Registry certificates need nothing: the engines read them on the host.
fn docker_tls_files(vars: Vars) -> Vec<PathBuf> {
    let tls = vars("DOCKER_TLS_VERIFY").is_some_and(|v| !v.is_empty())
        || vars(DOCKER.var).is_some_and(|host| host.starts_with("tcp://"));
    let dir = vars("DOCKER_CERT_PATH")
        .map(PathBuf::from)
        .or_else(|| expand("~/.docker", vars));
    let Some(dir) = dir.filter(|_| tls) else {
        return Vec::new();
    };
    ["ca.pem", "cert.pem", "key.pem"]
        .iter()
        .filter_map(|name| std::fs::canonicalize(dir.join(name)).ok())
        .collect()
}

/// The socket `engine` listens on: the one its variable names, otherwise
/// the first default that exists. Symlinks are resolved, because sandboxes
/// mount it at its real place, which a hidden directory may cover.
fn engine_socket(engine: &Engine, vars: Vars) -> Option<PathBuf> {
    // A TCP or SSH address needs no socket; the variable passes as is.
    if vars(engine.var).is_some() {
        return from_var(engine, vars).and_then(real_socket);
    }
    (engine.sockets.iter())
        .filter_map(|s| expand(s, vars))
        .find_map(real_socket)
}

/// The path in `engine`'s variable, if it names a Unix socket.
fn from_var(engine: &Engine, vars: Vars) -> Option<PathBuf> {
    let value = vars(engine.var)?;
    let path = match value.strip_prefix("unix://") {
        Some(path) => path,
        None if value.starts_with('/') => &value,
        None => return None,
    };
    Some(PathBuf::from(path))
}

fn wsl_interop(vars: Vars) -> Option<PathBuf> {
    real_socket(PathBuf::from(vars(WSL_INTEROP)?))
}

fn real_socket(path: PathBuf) -> Option<PathBuf> {
    let path = std::fs::canonicalize(path).ok()?;
    is_socket(&path).then_some(path)
}

#[cfg(unix)]
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

#[cfg(not(unix))]
fn is_socket(_: &Path) -> bool {
    false
}

/// `path` with `~/` and `$XDG/` replaced, if the variable they need is set.
fn expand(path: &str, vars: Vars) -> Option<PathBuf> {
    let (var, rest) = if let Some(rest) = path.strip_prefix("~/") {
        ("HOME", rest)
    } else if let Some(rest) = path.strip_prefix("$XDG/") {
        ("XDG_RUNTIME_DIR", rest)
    } else {
        return Some(PathBuf::from(path));
    };
    Some(PathBuf::from(vars(var)?).join(rest))
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;
    use std::os::unix::net::UnixListener;

    use super::*;

    /// A home and runtime directory with podman's rootless socket, and a
    /// docker socket outside both.
    struct Machine {
        dir: tempfile::TempDir,
        _listeners: Vec<UnixListener>,
    }

    impl Machine {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("run/podman")).unwrap();
            std::fs::create_dir_all(dir.path().join("home")).unwrap();
            let listeners = ["run/podman/podman.sock", "docker.sock"]
                .map(|s| UnixListener::bind(dir.path().join(s)).unwrap());
            Self {
                dir,
                _listeners: listeners.into(),
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            std::fs::canonicalize(self.dir.path()).unwrap().join(name)
        }

        fn vars(&self, extra: &[(&str, String)]) -> HashMap<String, String> {
            let mut vars = HashMap::from([
                ("HOME".to_owned(), self.path("home").display().to_string()),
                (
                    "XDG_RUNTIME_DIR".to_owned(),
                    self.path("run").display().to_string(),
                ),
            ]);
            vars.extend(extra.iter().map(|(k, v)| ((*k).to_owned(), v.clone())));
            vars
        }
    }

    fn policy() -> SandboxPolicy {
        SandboxPolicy::local(vec![], &[], true)
    }

    fn var<'a>(policy: &'a SandboxPolicy, name: &str) -> Option<&'a str> {
        let mut values = policy.vars.iter().filter(|(k, _)| k == name);
        let value = values.next().map(|(_, v)| v.as_str());
        assert!(values.next().is_none(), "{name} is set twice");
        value
    }

    #[test]
    fn podman_stands_in_for_a_missing_docker() {
        let machine = Machine::new();
        let vars = machine.vars(&[]);
        let mut policy = policy();
        pass_with(&mut policy, &|v| vars.get(v).cloned());
        let podman = machine.path("run/podman/podman.sock");
        let url = format!("unix://{}", podman.display());
        assert_eq!(var(&policy, "CONTAINER_HOST"), Some(url.as_str()));
        // Unless the host has /var/run/docker.sock, docker falls back to podman.
        if !Path::new("/var/run/docker.sock").exists() {
            assert_eq!(var(&policy, "DOCKER_HOST"), Some(url.as_str()));
        }
        assert!(policy.sockets.contains(&podman));
    }

    #[test]
    fn the_variables_name_the_sockets() {
        let machine = Machine::new();
        let docker = machine.path("docker.sock");
        let vars = machine.vars(&[
            ("DOCKER_HOST", format!("unix://{}", docker.display())),
            ("CONTAINERD_ADDRESS", "/missing/containerd.sock".to_owned()),
            ("BUILDKIT_HOST", "tcp://10.0.0.1:1234".to_owned()),
        ]);
        let mut policy = policy();
        pass_with(&mut policy, &|v| vars.get(v).cloned());
        let url = format!("unix://{}", docker.display());
        assert_eq!(var(&policy, "DOCKER_HOST"), Some(url.as_str()));
        // A variable that names no socket here is left as the host has it.
        assert_eq!(var(&policy, "CONTAINERD_ADDRESS"), None);
        assert_eq!(var(&policy, "BUILDKIT_HOST"), None);
        assert!(policy.sockets.contains(&docker));
    }

    #[test]
    fn a_tcp_docker_host_is_not_replaced_by_podman() {
        let machine = Machine::new();
        let vars = machine.vars(&[("DOCKER_HOST", "tcp://10.0.0.1:2375".to_owned())]);
        let mut policy = policy();
        pass_with(&mut policy, &|v| vars.get(v).cloned());
        assert_eq!(var(&policy, "DOCKER_HOST"), None);
        assert!(var(&policy, "CONTAINER_HOST").is_some());
    }

    #[test]
    fn off_hides_sockets_the_root_would_show() {
        let machine = Machine::new();
        let docker = machine.path("docker.sock");
        let vars = machine.vars(&[("DOCKER_HOST", format!("unix://{}", docker.display()))]);
        let mut policy = policy();
        policy.hidden_dirs.push(machine.path("run"));
        hide_with(&mut policy, &|v| vars.get(v).cloned());
        assert!(policy.hidden_files.contains(&docker));
        // The hidden runtime directory covers podman's socket already.
        assert!(
            !policy
                .hidden_files
                .contains(&machine.path("run/podman/podman.sock"))
        );
        assert!(policy.unset.iter().any(|v| v == "DOCKER_HOST"));
        assert!(policy.sockets.is_empty());
    }

    #[test]
    fn off_hides_engines_that_start_after_the_sandbox() {
        let machine = Machine::new();
        // BuildKit is installed but not running: its directory has no
        // socket yet.
        std::fs::create_dir_all(machine.path("run/buildkit")).unwrap();
        let vars = machine.vars(&[]);
        let mut policy = policy();
        policy.kind = SandboxKind::Bwrap;
        hide_with(&mut policy, &|v| vars.get(v).cloned());
        assert!(policy.hidden_dirs.contains(&machine.path("run/buildkit")));
        assert!(policy.hidden_dirs.contains(&machine.path("run/podman")));
        // The runtime directory holds more than engines.
        assert!(!policy.hidden_dirs.contains(&machine.path("run")));
        if Path::new("/run").is_dir() {
            assert_eq!(policy.scratch_dirs, [PathBuf::from("/run")]);
        }
        // Seatbelt denies a socket that is not there yet.
        let mut policy = self::policy();
        policy.kind = SandboxKind::Seatbelt;
        hide_with(&mut policy, &|v| vars.get(v).cloned());
        assert!(policy.scratch_dirs.is_empty());
        assert!(
            (policy.hidden_files).contains(&machine.path("home/.rd/docker.sock")),
            "{:?}",
            policy.hidden_files
        );
    }

    #[test]
    fn wsl_interop_passes_or_hides() {
        let machine = Machine::new();
        std::fs::create_dir_all(machine.path("wsl")).unwrap();
        let socket = machine.path("wsl/7_interop");
        let _listener = UnixListener::bind(&socket).unwrap();
        let vars = machine.vars(&[("WSL_INTEROP", socket.display().to_string())]);
        let mut policy = policy();
        pass_with(&mut policy, &|v| vars.get(v).cloned());
        assert!(policy.sockets.contains(&socket));
        let mut policy = self::policy();
        hide_with(&mut policy, &|v| vars.get(v).cloned());
        assert!(policy.hidden_dirs.contains(&machine.path("wsl")));
        assert!(policy.unset.iter().any(|v| v == "WSL_INTEROP"));
    }

    #[test]
    fn a_tls_daemon_gets_its_client_certificates() {
        let machine = Machine::new();
        let certs = machine.path("home/.docker");
        std::fs::create_dir_all(&certs).unwrap();
        for name in ["ca.pem", "cert.pem", "key.pem"] {
            std::fs::write(certs.join(name), "").unwrap();
        }
        let tcp = machine.vars(&[("DOCKER_HOST", "tcp://10.0.0.1:2376".to_owned())]);
        let mut policy = policy();
        pass_with(&mut policy, &|v| tcp.get(v).cloned());
        assert!(policy.readable.contains(&certs.join("key.pem")));
        assert_eq!(policy.readable.len(), 3);
        // A local socket needs no certificates.
        let local = machine.vars(&[]);
        let mut policy = self::policy();
        pass_with(&mut policy, &|v| local.get(v).cloned());
        assert!(policy.readable.is_empty());
    }

    #[test]
    fn default_paths_expand() {
        let vars = |v: &str| (v == "HOME").then(|| "/h".to_owned());
        assert_eq!(
            expand("/run/x.sock", &vars),
            Some(PathBuf::from("/run/x.sock"))
        );
        assert_eq!(expand("~/a.sock", &vars), Some(PathBuf::from("/h/a.sock")));
        assert_eq!(expand("$XDG/a.sock", &vars), None);
    }
}
