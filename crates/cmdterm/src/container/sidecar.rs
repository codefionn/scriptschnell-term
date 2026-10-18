//! Container engine sidecars for sandboxes: a docker-in-docker or
//! podman-in-podman container per project, whose API socket the sandboxes of
//! the project's worktrees get instead of the host engine's.
//!
//! The sidecar runs privileged on the host engine, which rootless podman
//! keeps inside the user's own namespace. Containers it starts see only what
//! the sidecar has: the shared paths (the project's worktrees, at the same
//! place, so `docker run -v "$PWD:/src"` works), its own storage volume, which
//! survives restarts, and the host's registry certificates and CA bundle,
//! read-only. Its network is the host engine's, not the sandbox's.
//!
//! The engine inside listens on a socket in a host directory mounted into
//! the sidecar. Unix sockets cannot cross a VM boundary, so this works only
//! where the host engine runs containers on the host's kernel: Linux.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cmdterm_env::Environment;

use crate::container::{Engine, Error, find_program, path_hash};

/// The engine that runs inside the sidecar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarKind {
    /// `quay.io/podman/stable` running `podman system service`.
    Podman,
    /// `docker:dind` running `dockerd`.
    Docker,
}

impl SidecarKind {
    pub fn default_image(self) -> &'static str {
        match self {
            Self::Podman => "quay.io/podman/stable",
            Self::Docker => "docker.io/library/docker:dind",
        }
    }

    fn socket_name(self) -> &'static str {
        match self {
            Self::Podman => "podman.sock",
            Self::Docker => "docker.sock",
        }
    }

    /// Where the engine keeps images and containers inside the sidecar.
    fn storage(self) -> &'static str {
        match self {
            Self::Podman => "/var/lib/containers",
            Self::Docker => "/var/lib/docker",
        }
    }

    /// The service, started in the background by a shell that makes its
    /// socket `$S` reachable for the host user once it appears.
    fn service(self) -> &'static str {
        match self {
            Self::Podman => "podman system service --time=0 unix://$S",
            Self::Docker => "dockerd-entrypoint.sh dockerd --host=unix://$S",
        }
    }

    /// Where the image keeps its CA bundle.
    fn ca_bundle(self) -> &'static str {
        match self {
            Self::Podman => "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
            Self::Docker => "/etc/ssl/certs/ca-certificates.crt",
        }
    }

    /// Host paths with registry certificates and where the sidecar's engine
    /// reads them, in shell syntax for `$HOME`. Podman reads docker's
    /// directory too.
    fn cert_dirs(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Podman => &[
                ("/etc/containers/certs.d", "/etc/containers/certs.d"),
                ("/etc/docker/certs.d", "/etc/docker/certs.d"),
                (
                    "$HOME/.config/containers/certs.d",
                    "/root/.config/containers/certs.d",
                ),
            ],
            Self::Docker => &[
                ("/etc/docker/certs.d", "/etc/docker/certs.d"),
                ("/etc/containerd/certs.d", "/etc/containerd/certs.d"),
            ],
        }
    }
}

/// Host CA bundles, the first that exists is used. A host with private CAs
/// in its trust store passes them on this way.
const CA_BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
];

/// Where the socket directory is mounted inside the sidecar.
const INNER_DIR: &str = "/run/cmdterm-sidecar";

/// How long the engine inside may take to listen. A first start pulls the
/// image before this starts counting.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

pub struct SidecarOptions<'a> {
    pub host: Environment,
    /// The project's root, which names the sidecar.
    pub project: PathBuf,
    /// Host paths shared at the same place, such as the project's worktrees.
    /// Missing ones are skipped.
    pub shared: Vec<PathBuf>,
    pub kind: SidecarKind,
    /// `None` uses [`SidecarKind::default_image`].
    pub image: Option<String>,
    /// The host engine. `None` picks podman when the host has it, else
    /// docker.
    pub engine: Option<String>,
    /// A private host directory for the socket. Its path plus the socket
    /// name must fit a Unix socket address (about 100 bytes).
    pub socket_dir: PathBuf,
    pub log: &'a mut (dyn Write + Send),
}

/// A running sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sidecar {
    pub name: String,
    /// The host engine that runs it.
    pub engine: String,
    /// The engine's socket on the host.
    pub socket: PathBuf,
    /// The variables that point clients to the socket.
    pub vars: Vec<(String, String)>,
}

/// The sidecar container's name for a project root.
pub fn sidecar_name(project: &Path) -> String {
    format!("cmdterm-sidecar-{}", path_hash(project))
}

/// Starts the project's sidecar, replacing one that runs already, and
/// waits until its engine listens.
pub fn start_sidecar(options: SidecarOptions) -> Result<Sidecar, Error> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Sidecar {
            message: "engine sidecars need Linux, where the host engine shares the \
                      host's kernel and its sockets"
                .to_owned(),
        });
    }
    let SidecarOptions {
        host,
        project,
        shared,
        kind,
        image,
        engine,
        socket_dir,
        log,
    } = options;
    let program = engine.unwrap_or_else(|| {
        if find_program(&host, "podman").is_some() {
            "podman".to_owned()
        } else {
            "docker".to_owned()
        }
    });
    let engine = Engine::new(&host, Some(&program));
    let name = sidecar_name(&project);
    let socket = socket_dir.join(kind.socket_name());
    let _ = writeln!(log, "cmdterm: starting the {kind:?} sidecar {name}");

    // A sidecar from an earlier run would hold the name.
    let _ = engine.capture(&["rm", "-f", &name], "remove the old sidecar");
    let made = host
        .command("mkdir")
        .args(["-p", "-m", "700"])
        .arg(&socket_dir)
        .output()
        .map_err(crate::container::spawn_error("mkdir"))?;
    if !made.status.success() {
        return Err(Error::Sidecar {
            message: format!("cannot create {}", socket_dir.display()),
        });
    }

    let mut args = vec![
        "run".to_owned(),
        "-d".to_owned(),
        "--rm".to_owned(),
        "--name".to_owned(),
        name.clone(),
        "--label".to_owned(),
        format!("cmdterm.sidecar={}", project.display()),
        "--privileged".to_owned(),
        "--security-opt".to_owned(),
        "label=disable".to_owned(),
        "--stop-timeout".to_owned(),
        "2".to_owned(),
        "-v".to_owned(),
        format!("{}:{INNER_DIR}", socket_dir.display()),
        "-v".to_owned(),
        format!("{name}:{}", kind.storage()),
    ];
    for path in existing(&host, shared.iter().map(|p| p.display().to_string()))? {
        args.push("-v".to_owned());
        args.push(format!("{path}:{path}"));
    }
    for (path, inside) in cert_mounts(&host, kind)? {
        args.push("-v".to_owned());
        args.push(format!("{path}:{inside}:ro"));
    }
    if kind == SidecarKind::Docker {
        // Without it dind also listens on TCP with generated certificates.
        args.extend(["-e".to_owned(), "DOCKER_TLS_CERTDIR=".to_owned()]);
    }
    let script = format!(
        "S={INNER_DIR}/{sock}; rm -f \"$S\"; {service} & pid=$!; \
         while [ ! -S \"$S\" ]; do kill -0 $pid || exit 1; sleep 0.2; done; \
         chmod 666 \"$S\"; wait $pid",
        sock = kind.socket_name(),
        service = kind.service(),
    );
    args.extend([
        "--entrypoint".to_owned(),
        "sh".to_owned(),
        image.unwrap_or_else(|| kind.default_image().to_owned()),
        "-c".to_owned(),
        script,
    ]);
    engine.stream(&args, log, "start the sidecar")?;

    let started = Instant::now();
    loop {
        let listening = host
            .command("test")
            .args(["-S"])
            .arg(&socket)
            .output()
            .is_ok_and(|out| out.status.success());
        if listening {
            break;
        }
        let running = engine.state(&name)?.is_some_and(|state| state == "running");
        if !running || started.elapsed() > READY_TIMEOUT {
            if let Ok(out) = engine.command(&["logs", "--tail", "20", &name]).output() {
                let _ = log.write_all(&out.stdout);
                let _ = log.write_all(&out.stderr);
            }
            let _ = engine.capture(&["rm", "-f", &name], "remove the sidecar");
            return Err(Error::Sidecar {
                message: if running {
                    "the engine in the sidecar did not start listening".to_owned()
                } else {
                    "the sidecar stopped before its engine listened".to_owned()
                },
            });
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = writeln!(log, "cmdterm: the sidecar listens on {}", socket.display());
    let url = format!("unix://{}", socket.display());
    let mut vars = vec![("DOCKER_HOST".to_owned(), url.clone())];
    if kind == SidecarKind::Podman {
        vars.push(("CONTAINER_HOST".to_owned(), url));
    }
    Ok(Sidecar {
        name,
        engine: program,
        socket,
        vars,
    })
}

/// Stops and removes a sidecar. Its storage volume stays for the next start.
pub fn stop_sidecar(host: &Environment, sidecar: &Sidecar) -> Result<(), Error> {
    let engine = Engine::new(host, Some(&sidecar.engine));
    engine
        .capture(&["rm", "-f", &sidecar.name], "remove the sidecar")
        .map(drop)
}

/// The registry certificate directories and the CA bundle of the host that
/// exist, with where the sidecar reads them.
fn cert_mounts(host: &Environment, kind: SidecarKind) -> Result<Vec<(String, String)>, Error> {
    let dirs = kind.cert_dirs();
    let candidates = (dirs.iter().map(|(path, _)| *path))
        .chain(CA_BUNDLES.iter().copied())
        .map(str::to_owned);
    let found = existing(host, candidates)?;
    let mut mounts = Vec::new();
    for (path, inside) in dirs {
        let expanded = expand_home(path, host);
        if let Some(path) = found.iter().find(|f| **f == expanded) {
            mounts.push((path.clone(), (*inside).to_owned()));
        }
    }
    if let Some(bundle) = CA_BUNDLES.iter().find(|b| found.iter().any(|f| f == *b)) {
        mounts.push(((*bundle).to_owned(), kind.ca_bundle().to_owned()));
    }
    Ok(mounts)
}

/// The paths among `paths` that exist on the host, with `$HOME` expanded,
/// in their order. One shell call for all of them.
fn existing(
    host: &Environment,
    paths: impl IntoIterator<Item = String>,
) -> Result<Vec<String>, Error> {
    let mut script = String::new();
    for path in paths {
        // `$HOME` expands; everything else is quoted.
        let quoted = match path.strip_prefix("$HOME") {
            Some(rest) => format!("\"$HOME\"'{}'", rest.replace('\'', r"'\''")),
            None => format!("'{}'", path.replace('\'', r"'\''")),
        };
        script.push_str(&format!("[ -e {quoted} ] && printf '%s\\n' {quoted}; "));
    }
    script.push_str("true");
    let out = host
        .command("sh")
        .args(["-c", &script])
        .output()
        .map_err(crate::container::spawn_error("sh"))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// `path` with a leading `$HOME` replaced by the host's home directory.
fn expand_home(path: &str, host: &Environment) -> String {
    let Some(rest) = path.strip_prefix("$HOME") else {
        return path.to_owned();
    };
    let home = host
        .command("sh")
        .args(["-c", "printf %s \"$HOME\""])
        .stdout()
        .map(|out| String::from_utf8_lossy(&out).into_owned())
        .unwrap_or_default();
    format!("{home}{rest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable_per_project() {
        let a = sidecar_name(Path::new("/src/a"));
        assert_eq!(a, sidecar_name(Path::new("/src/a")));
        assert_ne!(a, sidecar_name(Path::new("/src/b")));
        assert!(a.starts_with("cmdterm-sidecar-"));
    }

    #[cfg(unix)]
    #[test]
    fn existing_paths_are_found_in_order_and_quoted() {
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join("it's here");
        std::fs::create_dir(&odd).unwrap();
        let paths = [
            odd.display().to_string(),
            dir.path().join("missing").display().to_string(),
            "/".to_owned(),
        ];
        let found = existing(&Environment::Local, paths).unwrap();
        assert_eq!(found, [odd.display().to_string(), "/".to_owned()]);
    }

    /// Needs podman on the host and the network for the images.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "starts podman in podman"]
    fn a_podman_sidecar_runs_containers_that_see_only_the_worktree() {
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let worktree = dir.path().join("work");
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(worktree.join("hello"), "from the worktree").unwrap();
        std::fs::write(dir.path().join("secret"), "from the host").unwrap();
        let mut log = Vec::new();
        let sidecar = start_sidecar(SidecarOptions {
            host: Environment::Local,
            project: worktree.clone(),
            shared: vec![worktree.clone()],
            kind: SidecarKind::Podman,
            image: None,
            engine: Some("podman".into()),
            socket_dir: dir.path().join("s"),
            log: &mut log,
        })
        .unwrap_or_else(|e| panic!("{e}\n{}", String::from_utf8_lossy(&log)));
        let run = |args: &[&str]| {
            Environment::Local
                .command("podman")
                .args(args.iter().copied())
                .var("CONTAINER_HOST", &sidecar.vars[1].1)
                .output()
                .unwrap()
        };
        let mount = format!("{}:/w:ro", worktree.display());
        let out = run(&[
            "run",
            "--rm",
            "-v",
            &mount,
            "docker.io/library/alpine",
            "cat",
            "/w/hello",
        ]);
        let outside = format!("{}:/s:ro", dir.path().join("secret").display());
        let secret = run(&[
            "run",
            "--rm",
            "-v",
            &outside,
            "docker.io/library/alpine",
            "cat",
            "/s",
        ]);
        stop_sidecar(&Environment::Local, &sidecar).unwrap();
        let _ = Environment::Local
            .command("podman")
            .args(["volume", "rm", "-f", &sidecar.name])
            .output();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "from the worktree");
        // A host file outside the shared paths does not exist in the sidecar.
        assert!(!secret.status.success());
        assert_eq!(String::from_utf8_lossy(&secret.stdout), "");
    }
}
