//! Engine arguments for the built-in strategy, as pure functions so tests can
//! check them without an engine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::container::config::Config;

/// The command a container runs when `overrideCommand` is true: sleep until
/// stopped, and exit at once on SIGTERM.
pub const SLEEP_SCRIPT: &str = "trap 'exit 0' TERM; while sleep 1000 & wait $!; do :; done";

/// Label that marks a container as the devcontainer of a worktree.
pub const WORKTREE_LABEL: &str = "cmdterm.worktree";

/// `build` arguments for an image built from a Dockerfile.
pub fn build(
    dockerfile: &Path,
    context: &Path,
    build_args: &BTreeMap<String, String>,
    target: Option<&str>,
    tag: &str,
) -> Vec<String> {
    let mut args = vec!["build".to_owned(), "-f".to_owned(), display(dockerfile)];
    for (key, value) in build_args {
        args.push("--build-arg".into());
        args.push(format!("{key}={value}"));
    }
    if let Some(target) = target {
        args.push("--target".into());
        args.push(target.into());
    }
    args.push("-t".into());
    args.push(tag.into());
    args.push(display(context));
    args
}

/// Inputs to [`run`] that do not come from devcontainer.json.
pub struct RunSpec<'a> {
    pub name: &'a str,
    pub worktree: &'a Path,
    pub image: &'a str,
    /// Add `--userns=keep-id` (rootless podman).
    pub keep_id: bool,
    /// More bind mounts, as `(host, container)`.
    pub mounts: &'a [(PathBuf, PathBuf)],
}

/// `run` arguments that create the container detached.
pub fn run(config: &Config, spec: &RunSpec) -> Vec<String> {
    let worktree = display(spec.worktree);
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        spec.name.into(),
        "--label".into(),
        format!("{WORKTREE_LABEL}={worktree}"),
        "--label".into(),
        format!("devcontainer.local_folder={worktree}"),
        "--mount".into(),
        config.workspace_mount_arg(spec.worktree),
        "-w".into(),
        config.workspace_folder.clone(),
    ];
    for (key, value) in &config.container_env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    for mount in &config.mounts {
        args.push("--mount".into());
        args.push(mount.to_arg());
    }
    for (host, inside) in spec.mounts {
        args.push("--mount".into());
        args.push(bind_arg(host, inside));
    }
    if let Some(user) = &config.container_user {
        args.push("-u".into());
        args.push(user.clone());
    }
    if config.init == Some(true) {
        args.push("--init".into());
    }
    if config.privileged == Some(true) {
        args.push("--privileged".into());
    }
    for cap in &config.cap_add {
        args.push("--cap-add".into());
        args.push(cap.clone());
    }
    for opt in &config.security_opt {
        args.push("--security-opt".into());
        args.push(opt.clone());
    }
    if spec.keep_id {
        args.push("--userns=keep-id".into());
    }
    let override_command = config.override_command.unwrap_or(true);
    if override_command {
        args.push("--entrypoint".into());
        args.push("/bin/sh".into());
    }
    args.extend(config.run_args.iter().cloned());
    args.push(spec.image.into());
    if override_command {
        args.push("-c".into());
        args.push(SLEEP_SCRIPT.into());
    }
    args
}

/// Whether `runArgs` already choose a user namespace.
pub fn sets_userns(run_args: &[String]) -> bool {
    run_args.iter().any(|arg| arg.starts_with("--userns"))
}

/// `exec` arguments that run `argv` in the workspace folder as the remote
/// user, with `env` set.
pub fn exec(
    container: &str,
    user: Option<&str>,
    workdir: &str,
    env: &[(String, String)],
    argv: &[String],
) -> Vec<String> {
    let mut args = vec!["exec".to_owned()];
    if let Some(user) = user {
        args.push("-u".into());
        args.push(user.into());
    }
    args.push("-w".into());
    args.push(workdir.into());
    for (key, value) in env {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }
    args.push(container.into());
    args.extend(argv.iter().cloned());
    args
}

/// `devcontainer up` arguments for the CLI strategy.
/// `--mount` value that binds `host` at `inside`.
fn bind_arg(host: &Path, inside: &Path) -> String {
    format!(
        "type=bind,source={},target={}",
        display(host),
        display(inside)
    )
}

pub fn cli_up(
    worktree: &Path,
    rebuild: bool,
    engine: &str,
    mounts: &[(PathBuf, PathBuf)],
) -> Vec<String> {
    let worktree = display(worktree);
    let mut args = vec![
        "up".to_owned(),
        "--workspace-folder".into(),
        worktree.clone(),
        "--id-label".into(),
        format!("{WORKTREE_LABEL}={worktree}"),
    ];
    if rebuild {
        args.push("--remove-existing-container".into());
    }
    args.push("--docker-path".into());
    args.push(engine.into());
    for (host, inside) in mounts {
        args.push("--mount".into());
        args.push(bind_arg(host, inside));
    }
    args
}

/// `ps` arguments that list the ids of every container of a worktree.
pub fn list_by_label(worktree: &Path) -> Vec<String> {
    vec![
        "ps".into(),
        "-a".into(),
        "-q".into(),
        "--filter".into(),
        format!("label={WORKTREE_LABEL}={}", display(worktree)),
    ]
}

/// Whether `engine` (a name or a path) is podman.
pub fn is_podman(engine: &str) -> bool {
    PathBuf::from(engine)
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("podman"))
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::config::Config;

    fn config(text: &str) -> Config {
        Config::parse_with(text, Path::new("/src/app"), "id", &|_| None).unwrap()
    }

    #[test]
    fn build_arguments() {
        let args = build(
            Path::new("/src/app/.devcontainer/Dockerfile"),
            Path::new("/src/app"),
            &[
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), "x y".to_owned()),
            ]
            .into(),
            Some("dev"),
            "cmdterm-dc-0123",
        );
        assert_eq!(
            args,
            [
                "build",
                "-f",
                "/src/app/.devcontainer/Dockerfile",
                "--build-arg",
                "A=1",
                "--build-arg",
                "B=x y",
                "--target",
                "dev",
                "-t",
                "cmdterm-dc-0123",
                "/src/app"
            ]
        );
    }

    #[test]
    fn run_arguments_with_defaults() {
        let spec = RunSpec {
            name: "cmdterm-app-1",
            worktree: Path::new("/src/app"),
            image: "alpine",
            keep_id: false,
            mounts: &[],
        };
        assert_eq!(
            run(&config(r#"{"image": "alpine"}"#), &spec),
            [
                "run",
                "-d",
                "--name",
                "cmdterm-app-1",
                "--label",
                "cmdterm.worktree=/src/app",
                "--label",
                "devcontainer.local_folder=/src/app",
                "--mount",
                "type=bind,source=/src/app,target=/workspaces/app",
                "-w",
                "/workspaces/app",
                "--entrypoint",
                "/bin/sh",
                "alpine",
                "-c",
                SLEEP_SCRIPT,
            ]
        );
    }

    #[test]
    fn run_arguments_with_everything() {
        let config = config(
            r#"{
            "image": "alpine",
            "workspaceFolder": "/w",
            "workspaceMount": "source=${localWorkspaceFolder},target=/w,type=bind,consistency=cached",
            "containerEnv": {"A": "1"},
            "mounts": ["type=volume,source=v,target=/v", {"source": "/s", "target": "/t", "type": "bind"}],
            "containerUser": "dev",
            "init": true,
            "privileged": true,
            "capAdd": ["SYS_PTRACE"],
            "securityOpt": ["seccomp=unconfined"],
            "runArgs": ["--network=host"],
            "overrideCommand": false
        }"#,
        );
        let spec = RunSpec {
            name: "n",
            worktree: Path::new("/src/app"),
            image: "img",
            keep_id: true,
            mounts: &[],
        };
        assert_eq!(
            run(&config, &spec)[8..],
            [
                "--mount",
                "source=/src/app,target=/w,type=bind,consistency=cached",
                "-w",
                "/w",
                "-e",
                "A=1",
                "--mount",
                "type=volume,source=v,target=/v",
                "--mount",
                "type=bind,source=/s,target=/t",
                "-u",
                "dev",
                "--init",
                "--privileged",
                "--cap-add",
                "SYS_PTRACE",
                "--security-opt",
                "seccomp=unconfined",
                "--userns=keep-id",
                "--network=host",
                "img",
            ]
        );
    }

    #[test]
    fn exec_arguments() {
        assert_eq!(
            exec(
                "c",
                Some("vscode"),
                "/w",
                &[("K".into(), "v".into())],
                &["sh".into(), "-c".into(), "make".into()]
            ),
            [
                "exec", "-u", "vscode", "-w", "/w", "-e", "K=v", "c", "sh", "-c", "make"
            ]
        );
        assert_eq!(
            exec("c", None, "/w", &[], &["ls".into()]),
            ["exec", "-w", "/w", "c", "ls"]
        );
    }

    #[test]
    fn cli_arguments() {
        assert_eq!(
            cli_up(Path::new("/src/app"), true, "podman", &[]),
            [
                "up",
                "--workspace-folder",
                "/src/app",
                "--id-label",
                "cmdterm.worktree=/src/app",
                "--remove-existing-container",
                "--docker-path",
                "podman"
            ]
        );
    }

    #[test]
    fn engine_and_userns_checks() {
        assert!(is_podman("podman"));
        assert!(is_podman("/usr/bin/podman"));
        assert!(!is_podman("/usr/bin/docker"));
        assert!(sets_userns(&["--userns=host".into()]));
        assert!(sets_userns(&["--userns".into(), "host".into()]));
        assert!(!sets_userns(&["--network=host".into()]));
    }
}
