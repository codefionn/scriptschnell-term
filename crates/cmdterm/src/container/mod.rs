//! Devcontainers for cmdterm worktrees.
//!
//! [`up`] turns a worktree on any host [`Environment`] into a running
//! container and a [`ContainerTarget`], so the worktree's terminals run
//! inside the container while VCS work stays on the host. Files are read
//! with commands on the host, so remote hosts work the same as local ones.
//! On this machine they are read directly, and programs are looked up on
//! `PATH` with `PATHEXT`, since a Windows host has neither `sh` nor `cat`
//! and runs the devcontainer CLI as `devcontainer.cmd`.
//!
//! When the devcontainer CLI (`@devcontainers/cli`) is installed on the host,
//! [`up`] delegates to it. Otherwise it runs a built-in subset of
//! devcontainer.json through the docker or podman CLI. `README.md` lists what
//! that subset covers.
//!
//! [`start_sidecar`] runs a container engine inside a container for a
//! sandboxed worktree (`sidecar.rs`).

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use cmdterm_env::{ContainerTarget, EnvCommand, Environment};

mod args;
mod config;
mod jsonc;
mod sidecar;
mod stream;

pub use config::{Build, Config, ImageSource, Lifecycle, Mount, Step};
pub use sidecar::{
    Sidecar, SidecarKind, SidecarOptions, sidecar_name, start_sidecar, stop_sidecar,
};

/// A devcontainer.json found for a worktree.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// The config file on the host.
    pub path: PathBuf,
    pub config: Config,
}

pub struct UpOptions<'a> {
    pub host: Environment,
    pub worktree: PathBuf,
    /// `None` picks `docker` if `command -v docker` succeeds on the host,
    /// else `podman`.
    pub engine: Option<String>,
    /// Remove an existing container (and rebuild the image) first.
    pub rebuild: bool,
    /// More host directories bound into a new container, as `(host,
    /// container)`, such as the app's control socket directory. An
    /// existing container keeps the mounts it was created with.
    pub mounts: Vec<(PathBuf, PathBuf)>,
    /// Build and lifecycle output goes here, line by line as it arrives.
    pub log: &'a mut (dyn io::Write + Send),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Up {
    pub target: ContainerTarget,
    /// True when this call created the container (first start or rebuild).
    pub created: bool,
    pub workspace_folder: PathBuf,
}

/// Why a devcontainer could not be found, started, or removed.
#[derive(Debug)]
pub enum Error {
    /// The worktree has no devcontainer.json.
    NoConfig { worktree: PathBuf },
    /// devcontainer.json is not valid JSON with comments, or has a key of
    /// the wrong type.
    Parse { path: PathBuf, message: String },
    /// The config uses keys only the devcontainer CLI supports.
    Unsupported {
        path: PathBuf,
        keys: Vec<&'static str>,
    },
    /// The config names neither an image nor a Dockerfile.
    NoImage { path: PathBuf },
    /// A program could not be started on the host.
    Spawn { program: String, error: io::Error },
    /// An engine command or lifecycle command exited unsuccessfully.
    Step {
        step: String,
        code: Option<i32>,
        /// The last lines the command wrote to stderr.
        detail: Vec<String>,
    },
    /// `devcontainer up` failed or printed no result.
    Cli { message: String },
    /// An engine sidecar could not start.
    Sidecar { message: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfig { worktree } => write!(
                f,
                "no devcontainer.json in {}: looked for .devcontainer/devcontainer.json, \
                 .devcontainer.json and .devcontainer/<name>/devcontainer.json",
                worktree.display()
            ),
            Self::Parse { path, message } => {
                write!(f, "could not read {}: {message}", path.display())
            }
            Self::Unsupported { path, keys } => write!(
                f,
                "{} uses {}, which cmdterm's built-in devcontainer support does not handle. \
                 Install the devcontainer CLI on the host (npm install -g @devcontainers/cli) \
                 and start the container again",
                path.display(),
                keys.join(" and ")
            ),
            Self::NoImage { path } => write!(
                f,
                "{} sets neither \"image\" nor \"build.dockerfile\". Add one of them, or \
                 install the devcontainer CLI (npm install -g @devcontainers/cli) for compose setups",
                path.display()
            ),
            Self::Spawn { program, error } => {
                write!(
                    f,
                    "could not start {program}: {error}. Is it installed on the host?"
                )
            }
            Self::Step { step, code, detail } => {
                match code {
                    Some(code) => write!(f, "{step} failed with exit code {code}")?,
                    None => write!(f, "{step} was killed by a signal")?,
                }
                if let Some(last) = detail.last() {
                    write!(f, ": {last}")?;
                }
                write!(f, ". The container log has the full output")
            }
            Self::Cli { message } => write!(f, "devcontainer up failed: {message}"),
            Self::Sidecar { message } => write!(f, "engine sidecar: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { error, .. } => Some(error),
            _ => None,
        }
    }
}

fn spawn_error(program: &str) -> impl FnOnce(io::Error) -> Error + '_ {
    move |error| Error::Spawn {
        program: program.to_owned(),
        error,
    }
}

/// Looks for `.devcontainer/devcontainer.json`, `.devcontainer.json`, then
/// the first `.devcontainer/<dir>/devcontainer.json` in name order.
/// Variables in the config are substituted for this worktree.
pub fn find(host: &Environment, worktree: &Path) -> Result<Option<Found>, Error> {
    let fixed = [
        worktree.join(".devcontainer").join("devcontainer.json"),
        worktree.join(".devcontainer.json"),
    ];
    for path in fixed {
        if let Some(text) = read_file(host, &path)? {
            return parse_found(path, &text, worktree).map(Some);
        }
    }
    let dir = worktree.join(".devcontainer");
    let Some(mut names) = list_dir(host, &dir)? else {
        return Ok(None);
    };
    names.sort_unstable();
    for name in names {
        let path = dir.join(name).join("devcontainer.json");
        if let Some(text) = read_file(host, &path)? {
            return parse_found(path, &text, worktree).map(Some);
        }
    }
    Ok(None)
}

/// The names in `dir`, or `None` when it cannot be listed.
fn list_dir(host: &Environment, dir: &Path) -> Result<Option<Vec<String>>, Error> {
    if host.is_local() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(None);
        };
        return Ok(Some(
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect(),
        ));
    }
    let listing = host
        .command("ls")
        .arg("-1")
        .arg(dir)
        .output()
        .map_err(spawn_error("ls"))?;
    if !listing.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect(),
    ))
}

/// The file's text, or `None` when it cannot be read (missing, a
/// directory).
fn read_file(host: &Environment, path: &Path) -> Result<Option<String>, Error> {
    if host.is_local() {
        return Ok(std::fs::read(path)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()));
    }
    let output = host
        .command("cat")
        .arg(path)
        .output()
        .map_err(spawn_error("cat"))?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
}

fn parse_found(path: PathBuf, text: &str, worktree: &Path) -> Result<Found, Error> {
    match Config::parse(text, worktree, &path_hash(worktree)) {
        Ok(config) => Ok(Found { path, config }),
        Err(message) => Err(Error::Parse { path, message }),
    }
}

/// Deterministic container name for a worktree:
/// `cmdterm-<basename sanitized>-<12 hex chars of a hash of the full path>`.
pub fn container_name(worktree: &Path) -> String {
    let base = config::basename(worktree);
    let mut clean: String = base
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '_' | '.' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .take(32)
        .collect();
    if clean.is_empty() {
        clean.push_str("worktree");
    }
    format!("cmdterm-{clean}-{}", path_hash(worktree))
}

/// 12 hex chars of the FNV-1a 64 hash of the path bytes. Stable across runs
/// and builds, unlike std's hasher. Also used as `${devcontainerId}`.
fn path_hash(worktree: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in worktree.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")[..12].to_owned()
}

/// Starts the worktree's devcontainer, creating it when needed. Uses the
/// devcontainer CLI when the host has it, else the built-in subset.
pub fn up(options: UpOptions) -> Result<Up, Error> {
    match find_program(&options.host, "devcontainer") {
        Some(cli) => up_cli(options, &cli),
        None => up_builtin(options),
    }
}

/// Stops and removes the worktree's container. Ok if none exists. Finds
/// containers by their `cmdterm.worktree` label, so it also removes ones the
/// devcontainer CLI created. Built images stay cached.
pub fn remove(host: &Environment, worktree: &Path, engine: Option<&str>) -> Result<(), Error> {
    let engine = Engine::new(host, engine);
    let ids = engine.capture(
        &args::list_by_label(worktree),
        "list the worktree's containers",
    )?;
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Ok(());
    }
    let mut rm = vec!["rm", "-f"];
    rm.extend(ids);
    engine.capture(&rm, "remove the container").map(drop)
}

/// Like [`up`], but always uses the built-in subset, even when the
/// devcontainer CLI is installed.
pub fn up_builtin(options: UpOptions) -> Result<Up, Error> {
    let UpOptions {
        host,
        worktree,
        engine,
        rebuild,
        mounts,
        log,
    } = options;
    let found = find(&host, &worktree)?.ok_or_else(|| Error::NoConfig {
        worktree: worktree.clone(),
    })?;
    let config = &found.config;
    let keys = config.cli_only_keys();
    if !keys.is_empty() {
        return Err(Error::Unsupported {
            path: found.path,
            keys,
        });
    }
    let config_dir = found.path.parent().unwrap_or(&worktree);
    let Some(source) = config.image_source(config_dir) else {
        return Err(Error::NoImage { path: found.path });
    };
    let engine = Engine::new(&host, engine.as_deref());
    let name = container_name(&worktree);
    let user = config.user();

    let mut state = engine.state(&name)?;
    if rebuild && state.is_some() {
        engine.stream(&["rm", "-f", &name], log, "removing the old container")?;
        state = None;
    }
    let (created, env) = match state.as_deref() {
        Some("running") => (false, remote_env(&engine, &name, config)?),
        Some(state) => {
            let verb = if state == "paused" {
                "unpause"
            } else {
                "start"
            };
            engine.stream(&[verb, &name], log, "starting the container")?;
            let env = remote_env(&engine, &name, config)?;
            let lifecycle = Hooks {
                engine: &engine,
                name: &name,
                user,
                workdir: &config.workspace_folder,
                env: &env,
            };
            lifecycle.run("postStartCommand", config.post_start_command.as_ref(), log)?;
            (false, env)
        }
        None => {
            let image = match &source {
                ImageSource::Pull(image) => image.clone(),
                ImageSource::Build {
                    dockerfile,
                    context,
                    args: build_args,
                    target,
                } => {
                    let tag = format!("cmdterm-dc-{}", path_hash(&worktree));
                    let build =
                        args::build(dockerfile, context, build_args, target.as_deref(), &tag);
                    engine.stream(&build, log, "building the image")?;
                    tag
                }
            };
            let keep_id = args::is_podman(&engine.program)
                && !args::sets_userns(&config.run_args)
                && !host_is_root(&host);
            let spec = args::RunSpec {
                name: &name,
                worktree: &worktree,
                image: &image,
                keep_id,
                mounts: &mounts,
            };
            engine.stream(&args::run(config, &spec), log, "creating the container")?;
            let env = remote_env(&engine, &name, config)?;
            let lifecycle = Hooks {
                engine: &engine,
                name: &name,
                user,
                workdir: &config.workspace_folder,
                env: &env,
            };
            lifecycle.run("onCreateCommand", config.on_create_command.as_ref(), log)?;
            lifecycle.run(
                "updateContentCommand",
                config.update_content_command.as_ref(),
                log,
            )?;
            lifecycle.run(
                "postCreateCommand",
                config.post_create_command.as_ref(),
                log,
            )?;
            lifecycle.run("postStartCommand", config.post_start_command.as_ref(), log)?;
            (true, env)
        }
    };

    let workspace_folder = PathBuf::from(&config.workspace_folder);
    let mut target = ContainerTarget::new(host.clone(), &engine.program, &name);
    target.user = user.map(str::to_owned);
    target.mounts = vec![(worktree, workspace_folder.clone())];
    target.mounts.extend(mounts);
    target.vars = env;
    Ok(Up {
        target,
        created,
        workspace_folder,
    })
}

fn up_cli(options: UpOptions, cli: &str) -> Result<Up, Error> {
    let UpOptions {
        host,
        worktree,
        engine,
        rebuild,
        mounts,
        log,
    } = options;
    let engine = Engine::new(&host, engine.as_deref());
    let existed = !rebuild
        && !engine
            .capture(
                &args::list_by_label(&worktree),
                "list the worktree's containers",
            )?
            .trim()
            .is_empty();
    let cli_args = args::cli_up(&worktree, rebuild, &engine.program, &mounts);
    let _ = writeln!(log, "> devcontainer {}", cli_args.join(" "));
    let command = host.command(cli).args(&cli_args);
    let ran = stream::run(&command, log, false).map_err(spawn_error("devcontainer"))?;
    let result = match parse_cli_result(&ran.stdout) {
        Some(result) => result.map_err(|message| Error::Cli { message })?,
        None => {
            let mut message = match ran.status.code() {
                Some(code) => format!("it exited with code {code} and printed no result"),
                None => "it was killed by a signal".to_owned(),
            };
            if let Some(last) = ran.stderr_tail.last() {
                message.push_str(": ");
                message.push_str(last);
            }
            return Err(Error::Cli { message });
        }
    };
    // The CLI does not report remoteEnv, so read it from the config. A parse
    // error here is not fatal: the CLI accepted the file.
    let found = find(&host, &worktree).ok().flatten();
    let env = match &found {
        Some(found) => remote_env(&engine, &result.container_id, &found.config)?,
        None => Vec::new(),
    };
    let workspace_folder = match (result.workspace_folder.is_empty(), &found) {
        (true, Some(found)) => PathBuf::from(&found.config.workspace_folder),
        (true, None) => PathBuf::from(format!("/workspaces/{}", config::basename(&worktree))),
        (false, _) => PathBuf::from(&result.workspace_folder),
    };
    let mut target = ContainerTarget::new(host.clone(), &engine.program, &result.container_id);
    target.user = result.remote_user;
    target.mounts = vec![(worktree, workspace_folder.clone())];
    target.mounts.extend(mounts);
    target.vars = env;
    Ok(Up {
        target,
        created: !existed,
        workspace_folder,
    })
}

#[derive(Debug, PartialEq)]
struct CliResult {
    container_id: String,
    remote_user: Option<String>,
    workspace_folder: String,
}

/// The JSON result line of `devcontainer up`, searched from the end.
/// `None` when no line has an `outcome`.
fn parse_cli_result(stdout: &[String]) -> Option<Result<CliResult, String>> {
    let value = stdout.iter().rev().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        value.get("outcome").is_some().then_some(value)
    })?;
    let text = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    if text("outcome").as_deref() != Some("success") {
        let message = [text("message"), text("description")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(": ");
        return Some(Err(if message.is_empty() {
            "it reported an error without a message".to_owned()
        } else {
            message
        }));
    }
    let Some(container_id) = text("containerId") else {
        return Some(Err("its result has no containerId".to_owned()));
    };
    Some(Ok(CliResult {
        container_id,
        remote_user: text("remoteUser"),
        workspace_folder: text("remoteWorkspaceFolder").unwrap_or_default(),
    }))
}

/// How to start the program `name` on the host, `None` when it has none.
/// Elsewhere this runs `command -v <name>`; on this machine it searches
/// `PATH`, and on Windows returns the full path, so `devcontainer.cmd` and
/// other batch files start.
fn find_program(host: &Environment, name: &str) -> Option<String> {
    if host.is_local() {
        return local_program(name, std::env::var_os("PATH")?, cfg!(windows));
    }
    host.command("sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .is_ok_and(|output| output.status.success())
        .then(|| name.to_owned())
}

/// [`find_program`] on this machine, with `path` as `PATH`. With `windows`
/// every extension of `PATHEXT` is tried and the full path returned;
/// otherwise the name, once an executable file of that name is on `path`.
fn local_program(name: &str, path: std::ffi::OsString, windows: bool) -> Option<String> {
    let extensions: Vec<String> = if windows {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .filter(|ext| !ext.is_empty())
            // `.cmd` rather than PATHEXT's `.CMD`, as the files are named.
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for extension in &extensions {
            let candidate = dir.join(format!("{name}{extension}"));
            if !is_executable(&candidate) {
                continue;
            }
            return Some(if windows {
                candidate.to_string_lossy().into_owned()
            } else {
                name.to_owned()
            });
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn host_is_root(host: &Environment) -> bool {
    // Windows has no `id`, and Docker Desktop maps users itself.
    if cfg!(windows) && host.is_local() {
        return false;
    }
    host.command("id")
        .arg("-u")
        .stdout()
        .is_ok_and(|out| out.trim_ascii() == b"0")
}

/// The container engine CLI on the host.
struct Engine<'a> {
    host: &'a Environment,
    program: String,
}

impl<'a> Engine<'a> {
    fn new(host: &'a Environment, engine: Option<&str>) -> Self {
        let program = match engine {
            Some(engine) => engine.to_owned(),
            None => find_program(host, "docker")
                .or_else(|| find_program(host, "podman"))
                .unwrap_or_else(|| "podman".to_owned()),
        };
        Self { host, program }
    }

    fn command<S: AsRef<str>>(&self, args: &[S]) -> EnvCommand {
        self.host
            .command(&self.program)
            .args(args.iter().map(|a| a.as_ref().to_owned()))
    }

    /// Runs to completion and returns stdout.
    fn capture<S: AsRef<str>>(&self, args: &[S], step: &str) -> Result<String, Error> {
        let output = self
            .command(args)
            .output()
            .map_err(spawn_error(&self.program))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Step {
                step: step.to_owned(),
                code: output.status.code(),
                detail: stderr.lines().map(str::to_owned).collect(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs with all output going to `log`.
    fn stream<S: AsRef<str>>(
        &self,
        args: &[S],
        log: &mut dyn io::Write,
        step: &str,
    ) -> Result<(), Error> {
        let joined: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
        let _ = writeln!(log, "> {} {}", self.program, joined.join(" "));
        let ran =
            stream::run(&self.command(args), log, true).map_err(spawn_error(&self.program))?;
        if ran.status.success() {
            Ok(())
        } else {
            Err(Error::Step {
                step: step.to_owned(),
                code: ran.status.code(),
                detail: ran.stderr_tail,
            })
        }
    }

    /// The container's state (`running`, `exited`, ...), or `None` when it
    /// does not exist.
    fn state(&self, name: &str) -> Result<Option<String>, Error> {
        let output = self
            .command(&["inspect", "--format", "{{.State.Status}}", name])
            .output()
            .map_err(spawn_error(&self.program))?;
        Ok(output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
    }
}

/// Runs lifecycle commands inside the container.
struct Hooks<'a> {
    engine: &'a Engine<'a>,
    name: &'a str,
    user: Option<&'a str>,
    workdir: &'a str,
    env: &'a [(String, String)],
}

impl Hooks<'_> {
    fn run(
        &self,
        key: &str,
        command: Option<&Lifecycle>,
        log: &mut dyn io::Write,
    ) -> Result<(), Error> {
        let Some(command) = command else {
            return Ok(());
        };
        for (label, argv) in command.commands() {
            let step = match label {
                Some(label) => format!("{key} \"{label}\""),
                None => key.to_owned(),
            };
            let exec = args::exec(self.name, self.user, self.workdir, self.env, &argv);
            self.engine.stream(&exec, log, &step)?;
        }
        Ok(())
    }
}

/// Where [`pass_host_env`] writes the app's CAs in the container.
const HOST_CA: &str = "/tmp/.cmdterm-host-ca.pem";

/// Gives the container what the app's environment says about telemetry
/// and trusted CAs, see [`cmdterm_env::host_env`]: the telemetry variables,
/// and the certificates of the CA variables, added to the container's trust
/// store (which needs root or passwordless sudo there) and written to
/// [`HOST_CA`] for `NODE_EXTRA_CA_CERTS`. The CA variables the app has set
/// point to the container's bundle. Variables the devcontainer sets win.
pub fn pass_host_env(target: &mut ContainerTarget, log: &mut dyn io::Write) {
    let certs = cmdterm_env::host_env::ca_certificates();
    let (mut bundle, mut extra) = (None, None);
    if !certs.is_empty() {
        let pem = cmdterm_env::host_env::to_pem(&certs);
        let env = Environment::Container(Box::new(target.clone()));
        match env.trust_certificate("cmdterm-host-ca", pem.as_bytes()) {
            Ok(path) if !path.is_empty() => bundle = Some(path),
            Ok(_) => {}
            Err(error) => {
                let _ = writeln!(
                    log,
                    "cmdterm: the host's CAs are not in the container's trust store: {error}"
                );
            }
        }
        let written = env
            .command("sh")
            .args(["-c", "cat > \"$1\"", "sh", HOST_CA])
            .stdout_with_input(pem.as_bytes());
        match written {
            Ok(_) => extra = Some(HOST_CA),
            Err(error) => {
                let _ = writeln!(
                    log,
                    "cmdterm: could not write the host's CAs to {HOST_CA}: {error}"
                );
            }
        }
    }
    let ca_set: Vec<&str> = (cmdterm_env::host_env::CA_FILE_VARS.iter().copied())
        .filter(|name| std::env::var_os(name).is_some_and(|v| Path::new(&v).is_file()))
        .collect();
    let vars = host_vars(
        &target.vars,
        cmdterm_env::host_env::telemetry_vars(),
        &ca_set,
        bundle.as_deref(),
        extra,
    );
    target.vars.extend(vars);
}

/// The variables [`pass_host_env`] adds to `own`: `telemetry`, then each CA
/// variable in `ca_set` pointed at `bundle`, and `NODE_EXTRA_CA_CERTS` at
/// `extra`. None of `own` is replaced.
fn host_vars(
    own: &[(String, String)],
    telemetry: Vec<(String, String)>,
    ca_set: &[&str],
    bundle: Option<&str>,
    extra: Option<&str>,
) -> Vec<(String, String)> {
    let mut out = telemetry;
    if let Some(bundle) = bundle {
        for name in ca_set.iter().filter(|name| **name != "NODE_EXTRA_CA_CERTS") {
            out.push(((*name).to_owned(), bundle.to_owned()));
        }
    }
    if let Some(extra) = extra {
        out.push(("NODE_EXTRA_CA_CERTS".into(), extra.to_owned()));
    }
    out.retain(|(key, _)| !own.iter().any(|(k, _)| k == key));
    out
}

/// `remoteEnv` with unset entries dropped and `${containerEnv:VAR}` read
/// from the running container.
fn remote_env(
    engine: &Engine,
    container: &str,
    config: &Config,
) -> Result<Vec<(String, String)>, Error> {
    let pairs: Vec<(String, String)> = config
        .remote_env
        .iter()
        .filter_map(|(key, value)| Some((key.clone(), value.clone()?)))
        .collect();
    if !pairs
        .iter()
        .any(|(_, value)| value.contains("${containerEnv:"))
    {
        return Ok(pairs);
    }
    let output = engine.capture(
        &["exec", container, "env"],
        "reading the container environment",
    )?;
    let inside: HashMap<&str, &str> = output
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let lookup = |name: &str| {
        let var = name.strip_prefix("containerEnv:")?;
        let (var, default) = var.split_once(':').unwrap_or((var, ""));
        Some(inside.get(var).copied().unwrap_or(default).to_owned())
    };
    Ok(pairs
        .into_iter()
        .map(|(key, value)| (key, config::substitute_by(&value, &lookup)))
        .collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_variables_point_at_the_container_bundle_and_keep_its_own() {
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        let own = [
            pair("OTEL_SERVICE_NAME", "mine"),
            pair("CURL_CA_BUNDLE", "/own.pem"),
        ];
        let telemetry = vec![
            pair("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
            pair("OTEL_SERVICE_NAME", "host"),
        ];
        let ca_set = ["SSL_CERT_FILE", "CURL_CA_BUNDLE", "NODE_EXTRA_CA_CERTS"];
        let vars = super::host_vars(
            &own,
            telemetry.clone(),
            &ca_set,
            Some("/b.crt"),
            Some("/x.pem"),
        );
        assert_eq!(
            vars,
            [
                pair("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
                pair("SSL_CERT_FILE", "/b.crt"),
                pair("NODE_EXTRA_CA_CERTS", "/x.pem"),
            ]
        );
        // Without a trust store only the extra CAs reach Node.
        let vars = super::host_vars(&[], Vec::new(), &ca_set, None, Some("/x.pem"));
        assert_eq!(vars, [pair("NODE_EXTRA_CA_CERTS", "/x.pem")]);
    }

    use super::*;

    #[test]
    fn container_names_are_stable_and_clean() {
        let name = container_name(Path::new("/home/me/My Repo (2)"));
        assert_eq!(name, container_name(Path::new("/home/me/My Repo (2)")));
        let (prefix, hash) = name.rsplit_once('-').unwrap();
        assert_eq!(prefix, "cmdterm-my-repo--2-");
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // Same basename, different path: different name.
        assert_ne!(
            container_name(Path::new("/a/app")),
            container_name(Path::new("/b/app"))
        );
        // Fixed value, so a change to the hash shows up here.
        assert_eq!(path_hash(Path::new("")), "cbf29ce48422");
        assert!(container_name(Path::new("/")).starts_with("cmdterm-worktree-"));
        let long = container_name(Path::new(&format!("/x/{}", "a".repeat(100))));
        assert_eq!(long.len(), "cmdterm-".len() + 32 + 1 + 12);
    }

    #[test]
    fn cli_result_parsing() {
        let lines = |s: &str| s.lines().map(str::to_owned).collect::<Vec<_>>();
        let ok = parse_cli_result(&lines(
            "noise\n{\"outcome\":\"success\",\"containerId\":\"abc\",\"remoteUser\":\"node\",\"remoteWorkspaceFolder\":\"/workspaces/app\"}",
        ));
        assert_eq!(
            ok,
            Some(Ok(CliResult {
                container_id: "abc".into(),
                remote_user: Some("node".into()),
                workspace_folder: "/workspaces/app".into(),
            }))
        );
        let err = parse_cli_result(&lines(
            "{\"outcome\":\"error\",\"message\":\"Command failed\",\"description\":\"build broke\"}",
        ));
        assert_eq!(err, Some(Err("Command failed: build broke".into())));
        assert_eq!(parse_cli_result(&lines("hello\n{\"a\":1}")), None);
    }

    #[cfg(unix)]
    #[test]
    fn programs_are_found_on_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), dir.path()]).unwrap();
        assert_eq!(local_program("tool", path.clone(), false), None);
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            local_program("tool", path.clone(), false).as_deref(),
            Some("tool")
        );
        assert_eq!(local_program("other", path, false), None);
    }

    #[cfg(windows)]
    #[test]
    fn batch_files_are_found_by_their_full_path() {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("devcontainer.cmd");
        std::fs::write(&cli, "@echo off\r\n").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(
            local_program("devcontainer", path, true),
            Some(cli.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn unsupported_message_tells_how_to_fix() {
        let err = Error::Unsupported {
            path: "/r/.devcontainer/devcontainer.json".into(),
            keys: vec!["features"],
        };
        let text = err.to_string();
        assert!(text.contains("features"), "{text}");
        assert!(text.contains("npm install -g @devcontainers/cli"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn find_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let host = Environment::Local;
        assert_eq!(find(&host, root).unwrap(), None);

        write(
            ".devcontainer/zeta/devcontainer.json",
            r#"{"name": "zeta"}"#,
        );
        write(
            ".devcontainer/alpha/devcontainer.json",
            r#"{"name": "alpha"}"#,
        );
        write(".devcontainer/empty/README", "");
        let found = find(&host, root).unwrap().unwrap();
        assert_eq!(found.config.name.as_deref(), Some("alpha"));
        assert_eq!(
            found.path,
            root.join(".devcontainer/alpha/devcontainer.json")
        );

        write(".devcontainer.json", r#"{"name": "top"}"#);
        assert_eq!(
            find(&host, root).unwrap().unwrap().config.name.as_deref(),
            Some("top")
        );

        write(
            ".devcontainer/devcontainer.json",
            "{\"name\": \"main\", // c\n}",
        );
        assert_eq!(
            find(&host, root).unwrap().unwrap().config.name.as_deref(),
            Some("main")
        );

        write(".devcontainer/devcontainer.json", "{nope");
        assert!(matches!(find(&host, root), Err(Error::Parse { .. })));
    }
}
